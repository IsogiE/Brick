//! Give an egui text click keyboard focus without hiding or pausing WebView2.
use eframe::egui;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetGUIThreadInfo, IsChild, GA_ROOT, GUITHREADINFO,
};
use wry::{WebView, WebViewExtWindows};

pub(super) fn update(view: &WebView, ctx: &egui::Context) {
    // A native child keeps keyboard focus when its clipped-out parent receives
    // mouse input. Transfer it only for an actual click into an egui text field;
    // a stale text selection must never take focus back from provider controls.
    if !ctx.input(|input| input.pointer.any_pressed()) || !ctx.text_edit_focused() {
        return;
    }
    let mut child = windows::Win32::Foundation::HWND::default();
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: all handles belong to this live WebView and its UI thread. Only
    // transfer focus inside the already-active app; never activate another app.
    unsafe {
        if view.controller().ParentWindow(&mut child).is_err()
            || child.0.is_null()
            || GetGUIThreadInfo(0, &mut info) == 0
            || info.hwndFocus.is_null()
            || (info.hwndFocus != child.0 && IsChild(child.0, info.hwndFocus) == 0)
            || GetForegroundWindow() != GetAncestor(child.0, GA_ROOT)
        {
            return;
        }
    }
    let _ = view.focus_parent();
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        PostMessageW, SetForegroundWindow, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MOUSEMOVE,
    };

    #[test]
    #[ignore = "requires an interactive Windows desktop and WebView2"]
    fn native_overlay_search_receives_typing_after_provider_focus() {
        struct App {
            view: Option<WebView>,
            clip: super::super::Controller,
            text: String,
            phase: u8,
            phase_at: Instant,
            started: Instant,
            outcome: Arc<Mutex<Option<Result<(), String>>>>,
        }
        impl eframe::App for App {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                let ctx = ui.ctx().clone();
                if self.outcome.lock().unwrap().is_some() {
                    return;
                }
                if self.started.elapsed() > Duration::from_secs(15) {
                    *self.outcome.lock().unwrap() = Some(Err(format!(
                        "Keyboard focus phase {} timed out",
                        self.phase
                    )));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                let RawWindowHandle::Win32(handle) = frame.window_handle().unwrap().as_raw() else {
                    panic!("Expected Windows");
                };
                let parent = handle.hwnd.get() as *mut std::ffi::c_void;
                if self.view.is_none() {
                    self.view = Some(wry::WebViewBuilder::new()
                        .with_bounds(crate::stream_player::wry_bounds([20, 40, 600, 360]))
                        .with_html("<html style='background:#21664e'><input autofocus placeholder='Provider controls'></html>")
                        .build_as_child(frame).unwrap());
                    unsafe {
                        SetForegroundWindow(parent);
                    }
                }
                let view = self.view.as_ref().unwrap();
                let mut search = egui::Rect::NOTHING;
                egui::Window::new("Edit cooldowns")
                    .fixed_pos(egui::pos2(70.0, 90.0))
                    .resizable(false)
                    .show(&ctx, |ui| {
                        search = ui
                            .add(
                                egui::TextEdit::singleline(&mut self.text)
                                    .hint_text("Search spells…")
                                    .desired_width(300.0),
                            )
                            .rect;
                    });
                self.clip.update(view, &ctx, [20, 40, 600, 360]);
                let mut info = GUITHREADINFO {
                    cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                    ..Default::default()
                };
                unsafe {
                    assert_ne!(GetGUIThreadInfo(0, &mut info), 0);
                }
                let elapsed = self.phase_at.elapsed();
                match self.phase {
                    0 if elapsed > Duration::from_secs(1) => {
                        view.focus().unwrap();
                        self.phase = 1;
                        self.phase_at = Instant::now();
                    }
                    1 if elapsed > Duration::from_millis(300) => {
                        assert_ne!(
                            info.hwndFocus, parent,
                            "The provider must own keyboard focus before the text click"
                        );
                        unsafe {
                            assert_ne!(IsChild(parent, info.hwndFocus), 0);
                        }
                        let point = search.center() * ctx.pixels_per_point();
                        let position =
                            ((point.y as i32) << 16 | (point.x as i32 & 0xffff)) as isize;
                        unsafe {
                            assert_ne!(PostMessageW(parent, WM_MOUSEMOVE, 0, position), 0);
                            assert_ne!(PostMessageW(parent, WM_LBUTTONDOWN, 1, position), 0);
                            assert_ne!(PostMessageW(parent, WM_LBUTTONUP, 0, position), 0);
                        }
                        self.phase = 2;
                        self.phase_at = Instant::now();
                    }
                    2 if elapsed > Duration::from_millis(300) => {
                        assert!(
                            ctx.text_edit_focused(),
                            "The overlay text field must receive the click"
                        );
                        assert_eq!(
                            info.hwndFocus, parent,
                            "Typing must reach egui instead of the native browser"
                        );
                        for character in "apotheosis".chars() {
                            unsafe {
                                PostMessageW(
                                    info.hwndFocus,
                                    WM_KEYDOWN,
                                    character.to_ascii_uppercase() as usize,
                                    1,
                                );
                                PostMessageW(info.hwndFocus, WM_CHAR, character as usize, 1);
                                PostMessageW(
                                    info.hwndFocus,
                                    WM_KEYUP,
                                    character.to_ascii_uppercase() as usize,
                                    0xc0000001u32 as isize,
                                );
                            }
                        }
                        self.phase = 3;
                        self.phase_at = Instant::now();
                    }
                    3 if elapsed > Duration::from_millis(300) => {
                        assert_eq!(self.text, "apotheosis");
                        view.focus().unwrap();
                        self.phase = 4;
                        self.phase_at = Instant::now();
                    }
                    4 if elapsed > Duration::from_millis(300) => {
                        assert_ne!(
                            info.hwndFocus, parent,
                            "A prior text selection must not keep stealing provider focus"
                        );
                        unsafe {
                            assert_ne!(IsChild(parent, info.hwndFocus), 0);
                        }
                        *self.outcome.lock().unwrap() = Some(Ok(()));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => (),
                }
                ctx.request_repaint_after(Duration::from_millis(33));
            }
        }
        let outcome = Arc::new(Mutex::new(None));
        let saved = outcome.clone();
        eframe::run_native(
            "Brick overlay keyboard test",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 480.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    use winit::platform::windows::EventLoopBuilderExtWindows;
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            },
            Box::new(move |_| {
                Ok(Box::new(App {
                    view: None,
                    clip: Default::default(),
                    text: String::new(),
                    phase: 0,
                    phase_at: Instant::now(),
                    started: Instant::now(),
                    outcome,
                }))
            }),
        )
        .unwrap();
        assert_eq!(saved.lock().unwrap().take(), Some(Ok(())));
    }
}
