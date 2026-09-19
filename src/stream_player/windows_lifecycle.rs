//! Failure cleanup for Wry's Windows child construction and WebView2 processes.
use super::provider_login;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    rc::Weak,
    sync::{Arc, Mutex},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        DestroyWindow, EnumChildWindows, GetClassNameW, GetParent, GetWindowThreadProcessId,
        ShowWindow, SW_HIDE,
    },
};
use wry::{WebView, WebViewBuilder, WebViewExtWindows};

// Wry 0.57 creates WRY_WEBVIEW before creating the browser/controller. Its
// error path has no InnerWebView to drop, leaving a native child over egui.
// Only children created by this synchronous build, on this thread and under
// this exact owner are eligible for cleanup. Existing players are untouched.
struct Construction {
    owner: HWND,
    before: Vec<HWND>,
    armed: bool,
}

fn children(owner: HWND) -> Vec<HWND> {
    struct Found {
        owner: HWND,
        windows: Vec<HWND>,
    }
    unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> i32 {
        // SAFETY: EnumChildWindows calls synchronously with this stack value.
        let found = unsafe { &mut *(data as *mut Found) };
        let mut class = [0u16; 64];
        unsafe {
            let len = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
            if GetParent(window) == found.owner
                && GetWindowThreadProcessId(window, std::ptr::null_mut()) == GetCurrentThreadId()
                && len > 0
                && String::from_utf16_lossy(&class[..len as usize]) == "WRY_WEBVIEW"
            {
                found.windows.push(window);
            }
        }
        1
    }
    let mut found = Found {
        owner,
        windows: Vec::new(),
    };
    unsafe {
        EnumChildWindows(owner, Some(collect), &mut found as *mut Found as LPARAM);
    }
    found.windows
}

impl Construction {
    fn new(owner: HWND) -> Self {
        Self {
            owner,
            before: children(owner),
            armed: true,
        }
    }
}

impl Drop for Construction {
    fn drop(&mut self) {
        if self.armed {
            for child in children(self.owner) {
                if !self.before.contains(&child) {
                    // SAFETY: owned direct Wry child on its creating UI thread.
                    unsafe {
                        ShowWindow(child, SW_HIDE);
                        DestroyWindow(child);
                    }
                }
            }
        }
    }
}

pub(super) fn build_child(
    builder: WebViewBuilder<'_>,
    parent: &impl HasWindowHandle,
) -> wry::Result<WebView> {
    let RawWindowHandle::Win32(handle) = parent.window_handle()?.as_raw() else {
        return Err(wry::Error::UnsupportedWindowHandle);
    };
    let mut construction = Construction::new(handle.hwnd.get() as HWND);
    let view = builder.build_as_child(parent)?;
    construction.armed = false;
    Ok(view)
}

pub(super) fn watch_failures(
    view: &WebView,
    failure: Arc<Mutex<Option<String>>>,
    context: Weak<provider_login::Context>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use webview2_com::{Microsoft::Web::WebView2::Win32::*, ProcessFailedEventHandler};
    let ctx = ctx.clone();
    let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            unsafe {
                args.ProcessFailedKind(&mut kind)?;
            }
            if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED {
                if let Some(context) = context.upgrade() {
                    context.browser_failed();
                }
            }
            if matches!(
                kind,
                COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
                    | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
                    | COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED
                    | COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE
            ) {
                if let Ok(mut failure) = failure.lock() {
                    *failure =
                        Some("The stream player stopped unexpectedly. Please try again.".into());
                }
                ctx.request_repaint();
            }
            // WebView2 recovers GPU/utility processes itself.
        }
        Ok(())
    }));
    let mut registration = 0;
    unsafe {
        view.webview()
            .add_ProcessFailed(&handler, &mut registration)
    }
    .map_err(|_| "The stream player could not monitor its browser.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, IsWindow, RegisterClassW, UnregisterClassW, WNDCLASSW,
            WS_CHILD,
        },
    };

    #[test]
    fn failed_construction_removes_only_new_wry_children() {
        unsafe {
            let class: Vec<u16> = "WRY_WEBVIEW\0".encode_utf16().collect();
            let static_class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let instance = GetModuleHandleW(std::ptr::null());
            let registered = RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            });
            let make = |name: *const u16, owner: HWND| {
                CreateWindowExW(
                    0,
                    name,
                    std::ptr::null(),
                    if owner.is_null() { 0 } else { WS_CHILD },
                    0,
                    0,
                    32,
                    32,
                    owner,
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                )
            };
            let owner = make(static_class.as_ptr(), std::ptr::null_mut());
            assert!(!owner.is_null());
            let existing = make(class.as_ptr(), owner);
            for _ in 0..32 {
                let guard = Construction::new(owner);
                let orphan = make(class.as_ptr(), owner);
                let unrelated = make(static_class.as_ptr(), owner);
                assert!(!orphan.is_null());
                drop(guard);
                assert_eq!(IsWindow(orphan), 0);
                assert_ne!(IsWindow(existing), 0);
                assert_ne!(IsWindow(unrelated), 0);
                DestroyWindow(unrelated);
            }
            let mut success = Construction::new(owner);
            let retained = make(class.as_ptr(), owner);
            success.armed = false;
            drop(success);
            assert_ne!(IsWindow(retained), 0);
            DestroyWindow(owner);
            if registered != 0 {
                UnregisterClassW(class.as_ptr(), instance);
            }
        }
    }
}
