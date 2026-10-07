//! Explicitly opt-in real titlebar drag for an owned, interactive QA desktop.
//! Never run this mode on a user's active desktop: it temporarily controls the pointer.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, VK_LBUTTON, VK_RBUTTON,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowRect,
            GetWindowThreadProcessId, PostMessageW, SendMessageW, SetCursorPos,
            SetForegroundWindow, SetWindowPos, WindowFromPoint, GUITHREADINFO, GUI_INMOVESIZE,
            HTCAPTION, HWND_NOTOPMOST, HWND_TOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOMOVE, SWP_NOSIZE,
            WM_CANCELMODE, WM_NCHITTEST,
        },
    },
};

fn mouse_button(flags: u32) -> bool {
    // SAFETY: INPUT is a plain Win32 tagged union initialized as mouse input.
    unsafe {
        let mut input: INPUT = std::mem::zeroed();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = flags;
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) == 1
    }
}

fn in_move_loop(hwnd: HWND) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: Both handles and the correctly-sized output belong to this test.
    unsafe {
        GetGUIThreadInfo(
            GetWindowThreadProcessId(hwnd, std::ptr::null_mut()),
            &mut info,
        ) != 0
            && info.flags & GUI_INMOVESIZE != 0
            && info.hwndMoveSize == hwnd
    }
}

fn restore(hwnd: isize, previous: isize, cursor: POINT) {
    let _ = mouse_button(MOUSEEVENTF_LEFTUP);
    // SAFETY: Best-effort recovery touches only the test window and restores
    // the foreground/cursor values recorded immediately before this test.
    unsafe {
        PostMessageW(hwnd as HWND, WM_CANCELMODE, 0, 0);
        SetCursorPos(cursor.x, cursor.y);
        SetWindowPos(
            hwnd as HWND,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS,
        );
        if previous != 0 {
            SetForegroundWindow(previous as HWND);
        }
    }
}

struct Recovery {
    complete: Arc<AtomicBool>,
    watchdog: Option<thread::JoinHandle<()>>,
    hwnd: isize,
    previous: isize,
    cursor: POINT,
}

impl Drop for Recovery {
    fn drop(&mut self) {
        if !self.complete.swap(true, SeqCst) {
            restore(self.hwnd, self.previous, self.cursor);
        }
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.join().expect("Input-recovery watchdog panicked");
        }
    }
}

pub fn drag(hwnd: HWND, frames: impl Fn() -> usize, settle: impl Fn()) -> usize {
    assert_eq!(std::env::var("BRICK_RENDERING_QA_DESKTOP").as_deref(), Ok("1"),
        "Modal pointer control requires BRICK_RENDERING_QA_DESKTOP=1 on an owned interactive QA desktop");
    // SAFETY: Read-only mouse-state check; do not interrupt an existing physical drag.
    unsafe {
        assert!(
            GetAsyncKeyState(i32::from(VK_LBUTTON)) >= 0
                && GetAsyncKeyState(i32::from(VK_RBUTTON)) >= 0,
            "A mouse button is already held"
        );
    }
    let mut cursor = POINT::default();
    let mut original = RECT::default();
    let previous;
    unsafe {
        assert_ne!(GetCursorPos(&mut cursor), 0);
        assert_ne!(GetWindowRect(hwnd, &mut original), 0);
        previous = GetForegroundWindow() as isize;
    }
    let complete = Arc::new(AtomicBool::new(false));
    let watched = complete.clone();
    let window = hwnd as isize;
    let watchdog = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(6);
        while !watched.load(SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !watched.swap(true, SeqCst) {
            restore(window, previous, cursor);
        }
    });
    let recovery = Recovery {
        complete,
        watchdog: Some(watchdog),
        hwnd: window,
        previous,
        cursor,
    };
    unsafe {
        assert_ne!(
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE),
            0
        );
        SetForegroundWindow(hwnd);
    }
    let x = original.left + 100;
    let y = (original.top + 8..original.top + 64)
        .find(|y| {
            let point = (x as u16 as u32 | ((*y as u16 as u32) << 16)) as isize;
            // Ask Windows where its actual caption is, including current theme/DPI.
            unsafe { SendMessageW(hwnd, WM_NCHITTEST, 0, point) == HTCAPTION as isize }
        })
        .expect("The test window has no native titlebar hit region");
    unsafe {
        assert_ne!(SetCursorPos(x, y), 0);
        assert_eq!(
            WindowFromPoint(POINT { x, y }),
            hwnd,
            "Refusing pointer input: another window covers the test titlebar"
        );
    }
    assert!(
        mouse_button(MOUSEEVENTF_LEFTDOWN),
        "Could not press test drag button"
    );
    unsafe {
        assert_ne!(SetCursorPos(x + 12, y), 0);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !in_move_loop(hwnd) {
        assert!(
            Instant::now() < deadline,
            "Native titlebar move loop did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    settle(); // Finish focus/hover transitions before measuring movement-only frames.
    let before = frames();
    for step in 1..=32 {
        assert!(in_move_loop(hwnd), "Native titlebar move loop ended early");
        unsafe {
            assert_ne!(SetCursorPos(x + 12 + step * 3, y), 0);
        }
        thread::sleep(Duration::from_millis(25));
    }
    settle();
    let painted = frames() - before;
    let mut moved = RECT::default();
    unsafe {
        assert_ne!(GetWindowRect(hwnd, &mut moved), 0);
    }
    assert!(
        moved.left >= original.left + 80,
        "Actual titlebar drag did not move the window"
    );
    drop(recovery); // Release pointer capture even when subsequent assertions fail.
    let deadline = Instant::now() + Duration::from_secs(2);
    while in_move_loop(hwnd) {
        assert!(Instant::now() < deadline, "Native move loop did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    painted
}
