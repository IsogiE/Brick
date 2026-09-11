//! Expand the existing media child inside Brick without changing its window.

use eframe::egui;
use std::sync::{Arc, Mutex};
use wry::WebView;

#[derive(Default)]
struct State {
    active: bool,
    enabled: bool,
}

#[derive(Clone)]
struct Bridge {
    state: Arc<Mutex<State>>,
    ctx: egui::Context,
}

impl Bridge {
    fn request(&self, fullscreen: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if (fullscreen && !state.enabled) || state.active == fullscreen {
            return;
        }
        state.active = fullscreen;
        drop(state);
        // Only the app layout changes. Its current size, position, maximized
        // state and operating-system fullscreen mode belong to the user.
        self.ctx.request_repaint();
    }
}

pub(super) struct Controller {
    bridge: Bridge,
    #[cfg(target_os = "linux")]
    linux: std::cell::RefCell<Option<LinuxBinding>>,
}

#[cfg(target_os = "linux")]
struct LinuxBinding {
    manager: webkit2gtk::UserContentManager,
    handler: gtk::glib::SignalHandlerId,
    script: webkit2gtk::UserScript,
}

impl Controller {
    pub fn new(ctx: &egui::Context) -> Self {
        Self {
            bridge: Bridge {
                state: Arc::new(Mutex::new(State {
                    enabled: true,
                    ..State::default()
                })),
                ctx: ctx.clone(),
            },
            #[cfg(target_os = "linux")]
            linux: std::cell::RefCell::new(None),
        }
    }

    pub fn active(&self) -> bool {
        self.bridge.state.lock().is_ok_and(|state| state.active)
    }

    #[cfg(test)]
    pub fn enter(&self) {
        self.bridge.request(true);
    }

    pub fn set_enabled(&self, enabled: bool) {
        if let Ok(mut state) = self.bridge.state.lock() {
            state.enabled = enabled;
        }
        if !enabled {
            self.bridge.request(false);
        }
    }

    pub fn exit(&self, view: Option<&WebView>) {
        self.bridge.request(false);
        if let Some(view) = view {
            // Fullscreen from a nested provider also marks its parent iframe in
            // the wrapper document. Exit only that document's fullscreen state.
            let _ = view.evaluate_script(
                "if(document.fullscreenElement){document.exitFullscreen().catch(()=>{});}else if(document.webkitFullscreenElement){document.webkitExitFullscreen();}",
            );
        }
    }

    #[cfg(target_os = "linux")]
    pub fn attach(&self, view: &WebView) -> Result<(), String> {
        use gtk::prelude::*;
        use webkit2gtk::{SettingsExt, UserContentManagerExt, WebViewExt};
        use wry::WebViewExtUnix;

        let view = view.webview();
        if let Some(settings) = WebViewExt::settings(&view) {
            // Even an unexpected native request must not reach the broken
            // fullscreen implementation in the bundled WebKitGTK 2.50.4.
            settings.set_enable_fullscreen(false);
        }
        let manager = view
            .user_content_manager()
            .ok_or("The player could not enable fullscreen controls.")?;
        let bridge = self.bridge.clone();
        let handler =
            manager.connect_script_message_received(Some("brickFullscreen"), move |_, result| {
                // This channel can only change this player's layout. It carries no
                // URLs, commands, credentials, or access to application data.
                if let Some(value) = result.js_value() {
                    match value.to_string().as_str() {
                        "enter" => bridge.request(true),
                        "exit" => bridge.request(false),
                        _ => (),
                    }
                }
            });
        if !manager.register_script_message_handler("brickFullscreen") {
            manager.disconnect(handler);
            return Err("The player could not enable fullscreen controls.".to_string());
        }
        let script = webkit2gtk::UserScript::new(
            include_str!("fullscreen.js"),
            webkit2gtk::UserContentInjectedFrames::AllFrames,
            webkit2gtk::UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&script);
        *self.linux.borrow_mut() = Some(LinuxBinding {
            manager,
            handler,
            script,
        });
        let bridge = self.bridge.clone();
        view.connect_key_press_event(move |view, event| {
            if event.keyval() == gtk::gdk::keys::constants::Escape {
                bridge.request(false);
                // GTK can receive Escape before the document. Keep provider
                // state in sync as well as restoring the native child bounds.
                #[allow(deprecated)]
                view.run_javascript(
                    "document.exitFullscreen?.()",
                    None::<&gtk::gio::Cancellable>,
                    |_| {},
                );
            }
            gtk::glib::Propagation::Proceed
        });
        Ok(())
    }

    #[cfg(target_os = "windows")]
    pub fn attach(&self, view: &WebView) -> Result<(), String> {
        use webview2_com::{
            AcceleratorKeyPressedEventHandler, ContainsFullScreenElementChangedEventHandler,
        };
        use wry::WebViewExtWindows;

        let bridge = self.bridge.clone();
        let fullscreen =
            ContainsFullScreenElementChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(view) = sender {
                    let mut contains = windows::core::BOOL::default();
                    // SAFETY: this callback runs on the controller's UI thread.
                    unsafe { view.ContainsFullScreenElement(&mut contains)? };
                    bridge.request(contains.as_bool());
                }
                Ok(())
            }));
        let bridge = self.bridge.clone();
        let keyboard = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let mut key = 0;
                // Leave this unhandled so the provider also exits DOM fullscreen.
                unsafe { args.VirtualKey(&mut key)? };
                if key == 0x1b {
                    bridge.request(false);
                }
            }
            Ok(())
        }));
        let mut registration = 0;
        // SAFETY: all COM objects stay on the native UI thread. The WebView
        // retains callbacks until StreamPlayer closes its controller.
        unsafe {
            view.webview()
                .add_ContainsFullScreenElementChanged(&fullscreen, &mut registration)
                .and_then(|_| {
                    view.controller()
                        .add_AcceleratorKeyPressed(&keyboard, &mut registration)
                })
        }
        .map_err(|_| "The player could not enable fullscreen controls.".to_string())
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        // Late callbacks from a closed or replaced child cannot re-enter.
        self.set_enabled(false);
        #[cfg(target_os = "linux")]
        if let Some(binding) = self.linux.get_mut().take() {
            use gtk::prelude::*;
            use webkit2gtk::UserContentManagerExt;
            binding.manager.remove_script(&binding.script);
            binding
                .manager
                .unregister_script_message_handler("brickFullscreen");
            binding.manager.disconnect(binding.handler);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_window_commands(ctx: &egui::Context) {
        for output in ctx.end_pass().viewport_output.values() {
            assert!(
                output.commands.is_empty(),
                "Video expansion must not change the outer window: {:?}",
                output.commands
            );
        }
    }

    #[test]
    fn repeated_provider_events_only_change_the_expanded_layout() {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput::default());
        let control = Controller::new(&ctx);
        control.bridge.request(true);
        control.bridge.request(true);
        assert!(control.active());
        control.exit(None);
        control.bridge.request(false);
        assert!(!control.active());
        assert_no_window_commands(&ctx);
    }

    #[test]
    fn existing_fullscreen_is_preserved_after_video_exits() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .fullscreen = Some(true);
        ctx.begin_pass(input);
        let control = Controller::new(&ctx);
        control.bridge.request(true);
        control.exit(None);
        assert_no_window_commands(&ctx);
    }

    #[test]
    fn hiding_or_dropping_player_blocks_late_fullscreen_callbacks() {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput::default());
        let control = Controller::new(&ctx);
        control.bridge.request(true);
        control.set_enabled(false);
        control.bridge.request(true);
        assert!(!control.active());
        control.set_enabled(true);
        control.bridge.request(true);
        let delayed = control.bridge.clone();
        drop(control);
        delayed.request(true);
        assert!(!delayed.state.lock().unwrap().active);
        assert_no_window_commands(&ctx);
    }

    #[test]
    fn outer_window_mode_does_not_control_expanded_layout() {
        let ctx = egui::Context::default();
        let control = Controller::new(&ctx);
        ctx.begin_pass(egui::RawInput::default());
        control.enter();
        assert!(control.active());
        assert_no_window_commands(&ctx);
        for fullscreen in [true, false] {
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .fullscreen = Some(fullscreen);
            ctx.begin_pass(input);
            assert!(
                control.active(),
                "Changing window mode does not leave the expanded view"
            );
            assert_no_window_commands(&ctx);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    #[ignore = "requires an isolated desktop (X11 or Windows); no credentials"]
    fn native_expand_and_escape_preserve_window_and_restore_child_bounds() {
        #[cfg(target_os = "linux")]
        use gtk::{glib::translate::ToGlibPtr, prelude::*};
        use std::{
            cell::Cell,
            time::{Duration, Instant},
        };
        #[cfg(target_os = "windows")]
        use winit::platform::windows::EventLoopBuilderExtWindows;
        #[cfg(target_os = "linux")]
        use winit::platform::x11::EventLoopBuilderExtX11;
        use wry::WebViewBuilder;
        #[cfg(target_os = "linux")]
        use wry::WebViewExtUnix;

        #[cfg(target_os = "linux")]
        fn key_escape(view: &WebView, _frame: &eframe::Frame) {
            let view = view.webview();
            let window = view.window().unwrap();
            let device = view
                .display()
                .default_seat()
                .and_then(|seat| seat.keyboard());
            for kind in [
                gtk::gdk::EventType::KeyPress,
                gtk::gdk::EventType::KeyRelease,
            ] {
                let mut event = gtk::gdk::Event::new(kind);
                event.set_device(device.as_ref());
                let mut event = event.downcast::<gtk::gdk::EventKey>().unwrap();
                let key = event.as_mut();
                key.window = window.to_glib_full();
                key.send_event = 1;
                key.time = gtk::gdk::ffi::GDK_CURRENT_TIME as u32;
                key.keyval = *gtk::gdk::keys::constants::Escape;
                view.event(&event);
            }
        }

        #[cfg(target_os = "windows")]
        fn key_escape(view: &WebView, frame: &eframe::Frame) {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetGUIThreadInfo, IsChild, PostMessageW, SetForegroundWindow, GUITHREADINFO,
                WM_KEYDOWN, WM_KEYUP,
            };
            let RawWindowHandle::Win32(handle) = frame.window_handle().unwrap().as_raw() else {
                panic!("Expected the test's Windows handle");
            };
            let parent = handle.hwnd.get() as *mut std::ffi::c_void;
            // Only focus and send keys to this test's owned native window/view.
            unsafe {
                SetForegroundWindow(parent);
            }
            view.focus().unwrap();
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                assert_ne!(GetGUIThreadInfo(0, &mut info), 0);
                assert!(
                    !info.hwndFocus.is_null()
                        && (info.hwndFocus == parent || IsChild(parent, info.hwndFocus) != 0)
                );
                assert_ne!(
                    PostMessageW(info.hwndFocus, WM_KEYDOWN, 0x1b, 0x0001_0001),
                    0
                );
                assert_ne!(
                    PostMessageW(info.hwndFocus, WM_KEYUP, 0x1b, 0xc001_0001_u32 as isize),
                    0
                );
            }
        }

        #[derive(Debug, PartialEq, Eq)]
        struct WindowGeometry {
            id: winit::window::WindowId,
            inner_size: winit::dpi::PhysicalSize<u32>,
            outer_size: winit::dpi::PhysicalSize<u32>,
            position: winit::dpi::PhysicalPosition<i32>,
            maximized: bool,
            fullscreen: bool,
        }
        impl WindowGeometry {
            fn read(frame: &eframe::Frame) -> Self {
                let window = frame.winit_window().unwrap();
                Self {
                    id: window.id(),
                    inner_size: window.inner_size(),
                    outer_size: window.outer_size(),
                    position: window
                        .outer_position()
                        .expect("The isolated test window position is available"),
                    maximized: window.is_maximized(),
                    fullscreen: window.fullscreen().is_some(),
                }
            }
        }
        struct App {
            player: Option<super::super::StreamPlayer>,
            outcome: Arc<Mutex<Result<bool, String>>>,
            baseline: Option<WindowGeometry>,
            provider_url: Option<String>,
            trusted_click: bool,
            cycles: u8,
            completed: u8,
            exit_mode: String,
            phase: u8,
            phase_started: Instant,
            started: Instant,
        }
        impl eframe::App for App {
            fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
                super::super::pump_events();
                let Some(player) = self.player.as_mut() else {
                    return;
                };
                if self.provider_url.is_some() {
                    player.poll_playback(ctx);
                }
                let geometry = WindowGeometry::read(frame);
                if let Some(baseline) = &self.baseline {
                    assert_eq!(&geometry, baseline, "Expanding video must preserve the same outer window, size, position and mode");
                }
                let normal = super::super::physical_bounds(
                    egui::Rect::from_min_size(egui::pos2(30.0, 60.0), egui::vec2(680.0, 380.0)),
                    ctx.pixels_per_point(),
                )
                .unwrap();
                let expanded = player.bounds[2] > normal[2] && player.bounds[3] > normal[3];
                let elapsed = self.phase_started.elapsed();
                if self.started.elapsed()
                    > Duration::from_secs(if self.trusted_click { 45 } else { 15 })
                {
                    *self.outcome.lock().unwrap() = Err(format!(
                        "Expansion phase {} timed out (active={}, bounds={:?}, window={geometry:?})",
                        self.phase, player.is_fullscreen(), player.bounds,
                    ));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else if self.phase == 0 && elapsed > Duration::from_millis(800) {
                    self.baseline = Some(geometry);
                    if !self.trusted_click {
                        player.enter_fullscreen();
                    }
                    self.phase = 1;
                    self.phase_started = Instant::now();
                } else if self.phase == 1
                    && player.is_fullscreen()
                    && expanded
                    && elapsed > Duration::from_millis(150)
                {
                    match self.exit_mode.as_str() {
                        "provider" => (), // The driver clicks Twitch's own exit control.
                        "brick" => player.exit_fullscreen(),
                        "escape" => key_escape(player.webview.as_ref().unwrap(), frame),
                        _ => panic!("Unknown fullscreen test exit mode"),
                    }
                    self.phase = 2;
                    self.phase_started = Instant::now();
                } else if self.phase == 2
                    && !player.is_fullscreen()
                    && player.bounds == normal
                    && elapsed > Duration::from_millis(150)
                {
                    if self.trusted_click && self.completed + 1 < self.cycles {
                        self.completed += 1;
                        self.phase = 1;
                        self.phase_started = Instant::now();
                        return;
                    }
                    if self.trusted_click {
                        *self.outcome.lock().unwrap() = Ok(true);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    player.enter_fullscreen();
                    self.phase = 3;
                    self.phase_started = Instant::now();
                } else if self.phase == 3
                    && player.is_fullscreen()
                    && expanded
                    && elapsed > Duration::from_millis(150)
                {
                    player.exit_fullscreen();
                    self.phase = 4;
                    self.phase_started = Instant::now();
                } else if self.phase == 4
                    && !player.is_fullscreen()
                    && player.bounds == normal
                    && elapsed > Duration::from_millis(150)
                {
                    *self.outcome.lock().unwrap() = Ok(true);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ctx.request_repaint_after(Duration::from_millis(33));
            }

            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                let ctx = ui.ctx().clone();
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                    if let Some(player) = &self.player {
                        player.exit_fullscreen();
                    }
                }
                let fullscreen = self.player.as_ref().is_some_and(|p| p.is_fullscreen());
                egui::CentralPanel::default().frame(egui::Frame::NONE).show_inside(ui, |ui| {
                    ui.label(if fullscreen { "Exit expanded view · Esc" } else { "Native video expansion test" });
                    let rect = if fullscreen { ui.available_rect_before_wrap() } else { egui::Rect::from_min_size(egui::pos2(30.0, 60.0), egui::vec2(680.0, 380.0)) };
                    if let Some(player) = self.player.as_mut() {
                        player.set_bounds(rect, ctx.pixels_per_point()).unwrap();
                    } else if let Some(url) = &self.provider_url {
                        self.player = Some(super::super::StreamPlayer::new(frame, &ctx, url, "local-test-101", rect, ctx.pixels_per_point(), None).unwrap());
                        self.player.as_ref().unwrap().webview.as_ref().unwrap().focus_parent().unwrap();
                    } else {
                        #[cfg(target_os = "linux")]
                        super::super::initialize_gtk().unwrap();
                        // This local media surface verifies native expansion without
                        // requiring the provider DOM fullscreen/user-activation API.
                        let view = WebViewBuilder::new().with_bounds(super::super::wry_bounds(super::super::physical_bounds(rect, ctx.pixels_per_point()).unwrap())).with_focused(true).build_as_child(frame).unwrap();
                        let fullscreen = Controller::new(&ctx);
                        fullscreen.attach(&view).unwrap();
                        view.load_html("<!doctype html><style>html,body{margin:0;height:100%;background:#123;color:white}</style><button style='margin:20px;width:300px;height:100px' onclick='document.documentElement.requestFullscreen().catch(e=>document.title=e.message)'>Expand through trusted click</button>").unwrap();
                        #[cfg(target_os = "linux")]
                        if let Some(settings) = webkit2gtk::WebViewExt::settings(&view.webview()) { webkit2gtk::SettingsExt::set_hardware_acceleration_policy(&settings, webkit2gtk::HardwareAccelerationPolicy::Never); }
                        view.focus_parent().unwrap();
                        let player = super::super::StreamPlayer {
                            _cache_usage: crate::cache_maintenance::PlayerLease::new(),
                            #[cfg(target_os = "windows")]
                            _web_context: wry::WebContext::default(),
                            webview: Some(view), allowed_url: Arc::new(Mutex::new(String::new())), bounds: super::super::physical_bounds(rect, ctx.pixels_per_point()).unwrap(), visible: Cell::new(true), loaded: Arc::new(std::sync::atomic::AtomicBool::new(true)), created: Instant::now(), failure: Arc::new(Mutex::new(None)), preferences: None, playback_state: Arc::new(Mutex::new(super::super::PlaybackState::default())), last_state_poll: None, state_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)), queued_command: None, ready_since: None, pending_seek: None, pending_playback: None, command_retried: false, capture: super::super::capture::Controller::default(), fullscreen, occlusion: super::super::occlusion::Controller::default(),
                            #[cfg(target_os = "linux")]
                            preference_handler: None,
                        };
                        self.player = Some(player);
                    }
                });
            }
        }
        let trusted_click = std::env::var("BRICK_EXPANSION_CLICK_TEST").as_deref() == Ok("1");
        let provider_url = std::env::var("BRICK_EXPANSION_PROVIDER")
            .ok()
            .map(|provider| {
                assert!(
                    trusted_click,
                    "Provider expansion requires actual trusted input"
                );
                let member = match provider.as_str() {
                    "youtube" => "101",
                    "twitch" => "103",
                    _ => panic!("Unknown local provider fixture"),
                };
                let path = std::env::var_os("BRICK_REPLAY_FIXTURE")
                    .expect("Explicit local replay fixture is required");
                let bytes = std::fs::read(path).unwrap();
                assert!(bytes.len() < 65_536);
                let replay: crate::warcraftlogs::Replay = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(replay.provider.key(), provider);
                assert!(replay.available_seconds > 60);
                let mut url = url::Url::parse("http://127.0.0.1:18083").unwrap();
                url.set_path(&format!("/v1/streams/player/{member}/{provider}"));
                url.query_pairs_mut()
                    .append_pair(
                        "at",
                        &replay
                            .available_seconds
                            .saturating_sub(1)
                            .min(2000)
                            .to_string(),
                    )
                    .append_pair("broadcast", &replay.broadcast_id);
                url.to_string()
            });
        let outcome = Arc::new(Mutex::new(Ok(false)));
        let observed = outcome.clone();
        eframe::run_native(
            "Brick video expansion test",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 520.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    #[cfg(target_os = "linux")]
                    builder.with_x11().with_any_thread(true);
                    #[cfg(target_os = "windows")]
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            },
            Box::new(move |_| {
                Ok(Box::new(App {
                    player: None,
                    outcome: observed,
                    baseline: None,
                    provider_url,
                    trusted_click,
                    cycles: std::env::var("BRICK_EXPANSION_CYCLES")
                        .ok()
                        .map(|s| s.parse::<u8>().unwrap().clamp(1, 20))
                        .unwrap_or(1),
                    completed: 0,
                    exit_mode: std::env::var("BRICK_EXPANSION_EXIT")
                        .unwrap_or_else(|_| "escape".into()),
                    phase: 0,
                    phase_started: Instant::now(),
                    started: Instant::now(),
                }))
            }),
        )
        .unwrap();
        assert_eq!(*outcome.lock().unwrap(), Ok(true));
    }
}
