//! Task-owned native startup geometry probe; never loads Brick settings or accounts.
//! Run `native_window_geometry fit` or `native_window_geometry legacy`.
#[cfg(target_os = "windows")]
// This harnessless probe imports the production unit-test module without running it.
#[allow(dead_code, unused_imports)]
#[path = "../src/window_geometry.rs"]
mod window_geometry;

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("Windows startup geometry probe is skipped on this platform.");
}

#[cfg(target_os = "windows")]
fn main() {
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use std::sync::{Arc, Mutex};
    use windows_sys::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
        UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsWindowVisible},
    };

    fn snapshot(hwnd: HWND) -> serde_json::Value {
        let mut outer = RECT::default();
        let mut client = RECT::default();
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            assert_ne!(GetWindowRect(hwnd, &mut outer), 0);
            assert_ne!(GetClientRect(hwnd, &mut client), 0);
            assert_ne!(
                GetMonitorInfoW(
                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                    &mut monitor
                ),
                0
            );
        }
        let work = monitor.rcWork;
        serde_json::json!({
            "outer": [outer.left, outer.top, outer.right, outer.bottom],
            "inner": [client.right - client.left, client.bottom - client.top],
            "workArea": [work.left, work.top, work.right, work.bottom],
            "fitsWorkArea": outer.left >= work.left && outer.top >= work.top
                && outer.right <= work.right && outer.bottom <= work.bottom
        })
    }

    struct Probe {
        hwnd: isize,
        before: serde_json::Value,
        after: serde_json::Value,
        scale_factor: f64,
        fitted: bool,
        result: Arc<Mutex<Option<serde_json::Value>>>,
    }

    impl eframe::App for Probe {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            ui.label("Brick isolated native startup geometry probe");
            // Measure the actual first shown HWND before any fixture arrangement.
            if unsafe { IsWindowVisible(self.hwnd as HWND) } != 0 {
                let shown = snapshot(self.hwnd as HWND);
                if self.fitted {
                    assert_eq!(
                        shown["fitsWorkArea"], true,
                        "Initial outer window does not fit"
                    );
                }
                *self.result.lock().unwrap() = Some(serde_json::json!({
                    "passed": true,
                    "fitApplied": self.fitted,
                    "scaleFactor": self.scale_factor,
                    "beforeFit": self.before,
                    "afterFitBeforeShow": self.after,
                    "firstShown": shown
                }));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(20));
            }
        }
    }

    let fitted = match std::env::args().nth(1).as_deref() {
        None | Some("fit") => true,
        Some("legacy") => false,
        Some(mode) => panic!("Unknown startup geometry mode: {mode}"),
    };
    let result = Arc::new(Mutex::new(None));
    let result_slot = result.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Brick isolated startup geometry probe")
            .with_app_id("dev.isogi.brick.startup-geometry-probe")
            .with_inner_size(window_geometry::INITIAL_INNER_SIZE)
            .with_min_inner_size(window_geometry::MIN_INNER_SIZE)
            .with_clamp_size_to_monitor_size(true)
            .with_active(false),
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Brick startup geometry probe",
        options,
        Box::new(move |cc| {
            let RawWindowHandle::Win32(handle) = cc.window_handle()?.as_raw() else {
                panic!("Expected Windows HWND");
            };
            let hwnd = handle.hwnd.get();
            assert_eq!(
                unsafe { IsWindowVisible(hwnd as HWND) },
                0,
                "Probe must fit before first show"
            );
            let before = snapshot(hwnd as HWND);
            if fitted {
                window_geometry::fit_initial_window(cc)?;
            }
            assert_eq!(
                unsafe { IsWindowVisible(hwnd as HWND) },
                0,
                "Startup fit unexpectedly showed the hidden window"
            );
            let after = snapshot(hwnd as HWND);
            if fitted {
                assert_eq!(
                    after["fitsWorkArea"], true,
                    "Hidden startup placement does not fit"
                );
            }
            Ok(Box::new(Probe {
                hwnd,
                before,
                after,
                scale_factor: cc.winit_window().unwrap().scale_factor(),
                fitted,
                result: result_slot,
            }))
        }),
    )
    .unwrap();
    println!(
        "{}",
        result
            .lock()
            .unwrap()
            .take()
            .expect("Window never reached first show")
    );
}
