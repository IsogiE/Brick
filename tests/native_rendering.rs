//! Isolated native regression for Windows window movement and Glow surface lifetime.
//! Run each policy in its own process: `native_rendering optimized` and `native_rendering legacy`.
//! This creates only test windows and never loads Brick settings, accounts, or workers.

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("Native Windows rendering regression is skipped on this platform.");
}

#[cfg(target_os = "windows")]
fn main() {
    windows::run();
}

#[cfg(target_os = "windows")]
mod windows {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
            Arc, Mutex,
        },
        thread,
        time::{Duration, Instant},
    };

    use eframe::{egui, glow::HasContext};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{RedrawWindow, RDW_INVALIDATE},
        UI::WindowsAndMessaging::{
            GetWindowRect, PostMessageW, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE,
            SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE,
        },
    };

    const ROOT_COLOR: egui::Color32 = egui::Color32::from_rgb(30, 120, 70);

    #[derive(Default)]
    struct State {
        frames: AtomicUsize,
        key_events: AtomicUsize,
        screenshots: AtomicUsize,
        revision: AtomicUsize,
        seen_revision: AtomicUsize,
        show_child: AtomicBool,
        child_frames: AtomicUsize,
        last_size: Mutex<egui::Vec2>,
    }

    struct App {
        state: Arc<State>,
    }

    impl eframe::App for App {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.state.frames.fetch_add(1, SeqCst);
            let revision = self.state.revision.load(SeqCst);
            self.state.seen_revision.store(revision, SeqCst);
            *self.state.last_size.lock().unwrap() = ui.ctx().content_rect().size();
            ui.input(|input| {
                for event in &input.events {
                    match event {
                        egui::Event::Key { key: egui::Key::F8, pressed: true, .. } => {
                            self.state.key_events.fetch_add(1, SeqCst);
                        }
                        egui::Event::Screenshot { image, .. } => {
                            let [width, height] = image.size;
                            let pixel = image.pixels[(height / 2) * width + width / 2];
                            for (actual, expected) in pixel.to_array()[..3]
                                .iter()
                                .zip(ROOT_COLOR.to_array()[..3].iter())
                            {
                                assert!(actual.abs_diff(*expected) <= 3,
                                    "Root framebuffer was not restored after child surface use: {pixel:?}");
                            }
                            self.state.screenshots.fetch_add(1, SeqCst);
                        }
                        _ => {}
                    }
                }
            });
            ui.painter().rect_filled(ui.max_rect(), 0.0, ROOT_COLOR);
            ui.label(format!(
                "Isolated rendering regression: revision {revision}"
            ));
            if self.state.show_child.load(SeqCst) {
                let state = self.state.clone();
                ui.ctx().show_viewport_deferred(
                    egui::ViewportId::from_hash_of("rendering-regression-child"),
                    egui::ViewportBuilder::default()
                        .with_title("Brick rendering regression child")
                        .with_position([640.0, 80.0])
                        .with_inner_size([220.0, 160.0])
                        .with_active(false),
                    move |ui, _class| {
                        state.child_frames.fetch_add(1, SeqCst);
                        ui.painter()
                            .rect_filled(ui.max_rect(), 0.0, egui::Color32::BLUE);
                        ui.label("Independent GL surface");
                    },
                );
            }
        }
    }

    fn wait_for(label: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "Timed out waiting for {label}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn settle(state: &State) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut previous = state.frames.load(SeqCst);
        let mut unchanged = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(20));
            let current = state.frames.load(SeqCst);
            if previous != current {
                previous = current;
                unchanged = Instant::now();
            }
            if unchanged.elapsed() >= Duration::from_millis(250) {
                return;
            }
            assert!(Instant::now() < deadline, "Static window kept repainting");
        }
    }

    fn drive(hwnd: HWND, ctx: &egui::Context, state: &State, legacy: bool) {
        wait_for("initial frame", || state.frames.load(SeqCst) > 0);
        settle(state);
        let before = state.frames.load(SeqCst);
        for step in 0..32 {
            // Only our test HWND is moved. Do not move the user's pointer or inject global input.
            unsafe {
                assert_ne!(
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        80 + step * 3,
                        80,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER
                    ),
                    0
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
        settle(state);
        let movement_frames = state.frames.load(SeqCst) - before;
        if legacy {
            assert!(
                movement_frames >= 16,
                "Legacy control did not reproduce movement repainting: {movement_frames}"
            );
        } else {
            assert_eq!(
                movement_frames, 0,
                "Pure native movement requested unnecessary full frames"
            );
        }
        let mut rect = RECT::default();
        unsafe {
            assert_ne!(GetWindowRect(hwnd, &mut rect), 0);
        }
        assert_eq!(
            (rect.left, rect.top),
            (173, 80),
            "Native position handling was lost"
        );

        let before = state.frames.load(SeqCst);
        unsafe {
            assert_ne!(PostMessageW(hwnd, WM_MOUSEMOVE, 0, (40 << 16) | 40), 0);
        }
        wait_for("pointer repaint", || state.frames.load(SeqCst) > before);
        unsafe {
            assert_ne!(PostMessageW(hwnd, WM_KEYDOWN, 0x77, 0x0042_0001), 0);
            assert_ne!(
                PostMessageW(hwnd, WM_KEYUP, 0x77, 0xc042_0001_u32 as isize),
                0
            );
        }
        wait_for("keyboard input", || state.key_events.load(SeqCst) > 0);
        settle(state);

        let old_size = *state.last_size.lock().unwrap();
        unsafe {
            assert_ne!(
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    173,
                    80,
                    rect.right - rect.left + 96,
                    rect.bottom - rect.top + 64,
                    SWP_NOACTIVATE | SWP_NOZORDER
                ),
                0
            );
        }
        wait_for("resized content", || {
            state.last_size.lock().unwrap().x > old_size.x + 20.0
        });
        settle(state);

        let before = state.frames.load(SeqCst);
        unsafe {
            assert_ne!(
                RedrawWindow(hwnd, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE),
                0
            );
        }
        wait_for("real OS paint", || state.frames.load(SeqCst) > before);
        settle(state);
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
        thread::sleep(Duration::from_millis(100));
        let before = state.frames.load(SeqCst);
        unsafe {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        wait_for("restored paint", || state.frames.load(SeqCst) > before);
        settle(state);

        state.revision.store(1, SeqCst);
        ctx.request_repaint_of(egui::ViewportId::ROOT);
        wait_for("new app content", || state.seen_revision.load(SeqCst) == 1);
        settle(state);

        // Repeatedly share the context with a different surface, drop it, recreate
        // the same viewport ID, and read the root framebuffer after repainting.
        for _ in 0..4 {
            let child_before = state.child_frames.load(SeqCst);
            state.show_child.store(true, SeqCst);
            ctx.request_repaint_of(egui::ViewportId::ROOT);
            wait_for("child surface paint", || {
                state.child_frames.load(SeqCst) > child_before
            });
            state.show_child.store(false, SeqCst);
            ctx.request_repaint_of(egui::ViewportId::ROOT);
            settle(state);
            let screenshot_before = state.screenshots.load(SeqCst);
            ctx.send_viewport_cmd_to(
                egui::ViewportId::ROOT,
                egui::ViewportCommand::Screenshot(egui::UserData::default()),
            );
            wait_for("root framebuffer screenshot", || {
                state.screenshots.load(SeqCst) > screenshot_before
            });
            settle(state);
        }
        println!("{{\"policy\":\"{}\",\"move_events\":32,\"movement_frames\":{},\"viewport_cycles\":4,\"input_resize_os_paint_restore_content\":\"passed\"}}",
            if legacy { "legacy" } else { "optimized" }, movement_frames);
    }

    pub fn run() {
        let legacy = match std::env::args().nth(1).as_deref() {
            None | Some("optimized") => false,
            Some("legacy") => true,
            Some(other) => panic!("Unknown rendering policy: {other}"),
        };
        let state = Arc::new(State::default());
        let worker = Arc::new(Mutex::new(None));
        let worker_slot = worker.clone();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("Brick isolated rendering regression")
                .with_app_id("dev.isogi.brick.rendering-regression")
                .with_position([80.0, 80.0])
                .with_inner_size([440.0, 300.0])
                .with_active(false),
            repaint_on_window_move: legacy,
            ..Default::default()
        };
        eframe::run_native(
            "Brick rendering regression",
            options,
            Box::new(move |cc| {
                let RawWindowHandle::Win32(handle) = cc.window_handle().unwrap().as_raw() else {
                    panic!("Expected Win32 window");
                };
                let hwnd = handle.hwnd.get();
                if let Some(gl) = &cc.gl {
                    unsafe {
                        println!(
                            "GL vendor: {}; renderer: {}; version: {}",
                            gl.get_parameter_string(eframe::glow::VENDOR),
                            gl.get_parameter_string(eframe::glow::RENDERER),
                            gl.get_parameter_string(eframe::glow::VERSION)
                        );
                    }
                }
                let ctx = cc.egui_ctx.clone();
                let worker_state = state.clone();
                *worker_slot.lock().unwrap() = Some(thread::spawn(move || {
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        drive(hwnd as HWND, &ctx, &worker_state, legacy)
                    }));
                    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
                    outcome
                }));
                Ok(Box::new(App { state }))
            }),
        )
        .unwrap();
        let outcome = worker.lock().unwrap().take().unwrap().join().unwrap();
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
}
