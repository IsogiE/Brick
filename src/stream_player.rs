//! The native app owns navigation and authentication; this child only renders media.

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use url::Url;
use wry::{
    dpi::{PhysicalPosition, PhysicalSize},
    http::{header::AUTHORIZATION, HeaderMap, HeaderValue},
    NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder,
};

const WRAPPER_LOAD_TIMEOUT: Duration = Duration::from_secs(25);

pub struct StreamPlayer {
    webview: Option<WebView>,
    bounds: [i32; 4],
    loaded: Arc<AtomicBool>,
    created: Instant,
    failure: Arc<Mutex<Option<String>>>,
}

impl StreamPlayer {
    pub fn new(
        frame: &eframe::Frame,
        url: &str,
        token: &str,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<Self, String> {
        let created = Instant::now();
        let player_url = validated_player_url(url)?;
        let bounds = physical_bounds(rect, pixels_per_point)?;
        if token.is_empty() || token.len() > 2048 {
            return Err("Sign in to Discord again to watch this stream.".to_string());
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "The Discord session cannot open this stream.".to_string())?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);

        let handle = frame
            .window_handle()
            .map_err(|_| "The stream player could not access the Brick window.".to_string())?;
        match handle.as_raw() {
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_) => initialize_gtk()?,
            #[cfg(target_os = "windows")]
            RawWindowHandle::Win32(_) => (),
            _ => return Err(
                "The stream player needs an X11/XWayland window on Linux or WebView2 on Windows."
                    .to_string(),
            ),
        }

        // Wry creates a WebKit process during construction. Configure its private
        // context before that happens, then use the supported related-view hook
        // to give the media child this sandboxed context.
        #[cfg(target_os = "linux")]
        let linux_seed = {
            use webkit2gtk::WebContextExt;
            let context = webkit2gtk::WebContext::new_ephemeral();
            context.set_sandbox_enabled(true);
            webkit2gtk::WebView::with_context(&context)
        };

        // Build an empty, private child first so every navigation guard is installed
        // before the single authenticated navigation starts. No token enters HTML,
        // JavaScript, the URL, a custom protocol, or a global resource interceptor.
        let loaded = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let wrapper_loaded = Arc::clone(&loaded);
        let wrapper_url = player_url.to_string();
        let builder = WebViewBuilder::new()
            .with_bounds(wry_bounds(bounds))
            .with_incognito(true)
            .with_autoplay(true)
            .with_devtools(false)
            .with_clipboard(false)
            .with_hotkeys_zoom(false)
            .with_focused(false)
            .with_background_color((18, 20, 25, 255))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .with_on_page_load_handler(move |event, destination| {
                #[cfg(test)]
                eprintln!(
                    "Stream page event {}: {}",
                    if matches!(event, PageLoadEvent::Finished) {
                        "Finished"
                    } else {
                        "Started"
                    },
                    diagnostic_destination(&destination)
                );
                if matches!(event, PageLoadEvent::Finished) && destination == wrapper_url {
                    wrapper_loaded.store(true, Ordering::Relaxed);
                }
            });

        #[cfg(target_os = "windows")]
        let builder = {
            use wry::WebViewBuilderExtWindows;
            // Override Wry's default flags so WebView2 keeps SmartScreen enabled.
            builder.with_additional_browser_args("--autoplay-policy=no-user-gesture-required")
        };

        // WebView2 invokes this for top-level navigations; provider iframe requests
        // are separate. Block redirects and links away from the protected wrapper.
        #[cfg(not(target_os = "linux"))]
        let builder = {
            let allowed = player_url.to_string();
            builder.with_navigation_handler(move |destination| destination == allowed)
        };

        #[cfg(target_os = "linux")]
        let builder = {
            use wry::WebViewBuilderExtUnix;
            builder.with_related_view(linux_seed.clone())
        };

        let built = builder.build_as_child(frame);
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            // SAFETY: this unparented blank seed is owned here and never used
            // again. The child already retains its own context/process reference.
            unsafe {
                linux_seed.destroy();
            }
        }
        let webview = built.map_err(|_| {
            "The stream player could not start. Check that WebView2 (Windows) or WebKitGTK 4.1 (Linux) is installed."
                .to_string()
        })?;
        let player = Self {
            webview: Some(webview),
            bounds,
            loaded,
            created,
            failure,
        };
        let webview = player
            .webview
            .as_ref()
            .expect("The player has just been created");
        #[cfg(target_os = "linux")]
        {
            protect_linux_navigation(webview, player_url.as_str())?;
            watch_linux_failures(webview, player_url.as_str(), Arc::clone(&player.failure));
        }
        #[cfg(target_os = "windows")]
        protect_windows_permissions(webview)?;

        // Wry uses WebKit's load_request / WebView2 NavigateWithWebResourceRequest:
        // these headers belong to this request, not subsequent iframe resources.
        webview
            .load_url_with_headers(player_url.as_str(), headers)
            .map_err(|_| "The stream player could not load this stream.".to_string())?;
        Ok(player)
    }

    pub fn set_bounds(&mut self, rect: egui::Rect, pixels_per_point: f32) -> Result<(), String> {
        let bounds = physical_bounds(rect, pixels_per_point)?;
        if bounds != self.bounds {
            self.webview
                .as_ref()
                .ok_or_else(|| "The stream player has closed.".to_string())?
                .set_bounds(wry_bounds(bounds))
                .map_err(|_| "The stream player could not resize.".to_string())?;
            self.bounds = bounds;
        }
        Ok(())
    }

    /// Provider playback errors stay in the official iframe. Only a failed native
    /// process or a wrapper that never finishes loading closes the native child.
    pub fn failure(&self) -> Option<String> {
        if let Some(message) = self.failure.lock().ok().and_then(|failure| failure.clone()) {
            return Some(message);
        }
        if !self.loaded.load(Ordering::Relaxed) && self.created.elapsed() >= WRAPPER_LOAD_TIMEOUT {
            return Some("The stream player took too long to load. Please try again.".to_string());
        }
        None
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn diagnostic_html(&self) {
        use webkit2gtk::WebViewExt;
        use wry::WebViewExtUnix;
        if let Some(webview) = &self.webview {
            webview.webview().load_html("<!doctype html><html><body style='margin:0;background:#ff00ff;color:white;font:36px sans-serif'><div style='height:100px;background:#008080'>Brick native rendering diagnostic</div><p>Local HTML draws without provider requests.</p></body></html>", None);
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn diagnostic_drop_probe(&self) -> Option<Box<dyn Fn() -> bool>> {
        use gtk::prelude::*;
        use wry::WebViewExtUnix;
        let weak = self.webview.as_ref()?.webview().downgrade();
        Some(Box::new(move || weak.upgrade().is_none()))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn diagnostic_click(&self, x: f64, y: f64) -> Result<(), String> {
        use gtk::{glib::translate::ToGlibPtr, prelude::*};
        use wry::WebViewExtUnix;
        let webview = self
            .webview
            .as_ref()
            .ok_or("The diagnostic player closed")?;
        let view = webview.webview();
        let window = view
            .window()
            .ok_or("The diagnostic player has no GDK window")?;
        let device = view
            .display()
            .default_seat()
            .and_then(|seat| seat.pointer());
        let (_, root_x, root_y) = window.origin();
        view.grab_focus();
        for kind in [
            gtk::gdk::EventType::ButtonPress,
            gtk::gdk::EventType::ButtonRelease,
        ] {
            let mut event = gtk::gdk::Event::new(kind);
            event.set_device(device.as_ref());
            event.set_source_device(device.as_ref());
            let mut event = event
                .downcast::<gtk::gdk::EventButton>()
                .map_err(|_| "Invalid diagnostic event")?;
            let button = event.as_mut();
            button.window = window.to_glib_full();
            button.send_event = 1;
            button.time = gtk::gdk::ffi::GDK_CURRENT_TIME as u32;
            button.x = x;
            button.y = y;
            button.x_root = root_x as f64 + x;
            button.y_root = root_y as f64 + y;
            button.button = 1;
            button.state = if kind == gtk::gdk::EventType::ButtonRelease {
                gtk::gdk::ffi::GDK_BUTTON1_MASK
            } else {
                0
            };
            eprintln!("Stream diagnostic {kind:?} handled={}", view.event(&event));
        }
        Ok(())
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn diagnostic_details(&self) {
        use gtk::prelude::*;
        use webkit2gtk::WebViewExt;
        use wry::WebViewExtUnix;
        if let Some(webview) = &self.webview {
            let view = webview.webview();
            eprintln!("Stream GTK allocation {:?}, mapped={}, drawable={}, loading={}, progress={}, parent={:?}", view.allocation(), view.is_mapped(), view.is_drawable(), view.is_loading(), view.estimated_load_progress(), view.parent().map(|parent| parent.allocation()));
            let _ = webview.evaluate_script_with_callback(
                "JSON.stringify({visibility:document.visibilityState,focus:document.hasFocus()})",
                |result| eprintln!("Stream document visibility: {result}"),
            );
        }
    }
}

impl Drop for StreamPlayer {
    fn drop(&mut self) {
        let Some(webview) = self.webview.take() else {
            return;
        };
        // X11 unmap/destroy requests are buffered. The native app stops pumping
        // GTK as soon as this player closes, so hide and flush explicitly before
        // returning to another tab or the sign-in screen.
        let _ = webview.set_visible(false);
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            use webkit2gtk::WebViewExt;
            use wry::WebViewExtUnix;
            let view = webview.webview();
            view.stop_loading();
            view.set_is_muted(true);
            // Each player has its own private context and no other windows.
            // End its renderer now instead of retaining a background media process.
            view.terminate_web_process();
            view.hide();
            let display = view.display();
            drop(view);
            drop(webview);
            display.flush();
            pump_events();
            display.flush();
        }
    }
}

fn validated_player_url(value: &str) -> Result<Url, String> {
    validate_player_address(value, option_env!("BRICK_PRESENCE_API_URL").unwrap_or(""))
}

fn validate_player_address(value: &str, configured: &str) -> Result<Url, String> {
    let invalid = || "The stream player address is invalid.".to_string();
    let url = Url::parse(value).map_err(|_| invalid())?;
    let base = Url::parse(configured).map_err(|_| invalid())?;
    let allowed_transport =
        url.scheme() == "https" || (url.scheme() == "http" && url.host_str() == Some("127.0.0.1"));
    let member = url.path().strip_prefix("/v1/streams/player/");
    if !allowed_transport
        || url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !member.is_some_and(|path| {
            let mut segments = path.split('/');
            let id = segments.next().unwrap_or_default();
            !id.is_empty()
                && id.len() <= 32
                && id.bytes().all(|byte| byte.is_ascii_digit())
                && matches!(segments.next(), Some("twitch" | "youtube"))
                && segments.next().is_none()
        })
    {
        return Err(invalid());
    }
    Ok(url)
}

fn physical_bounds(rect: egui::Rect, scale: f32) -> Result<[i32; 4], String> {
    if !rect.is_finite()
        || !scale.is_finite()
        || scale <= 0.0
        || rect.width() <= 0.0
        || rect.height() <= 0.0
    {
        return Err("The stream player needs more room in the Brick window.".to_string());
    }
    Ok([
        (rect.min.x.max(0.0) * scale).round() as i32,
        (rect.min.y.max(0.0) * scale).round() as i32,
        (rect.width() * scale).round().max(1.0) as i32,
        (rect.height() * scale).round().max(1.0) as i32,
    ])
}

fn wry_bounds(bounds: [i32; 4]) -> wry::Rect {
    wry::Rect {
        position: PhysicalPosition::new(bounds[0], bounds[1]).into(),
        size: PhysicalSize::new(bounds[2] as u32, bounds[3] as u32).into(),
    }
}

#[cfg(target_os = "linux")]
fn initialize_gtk() -> Result<(), String> {
    use gtk::prelude::*;
    if !gtk::is_initialized() {
        // Eframe already selected X11. Match it even in a Wayland desktop session;
        // Wry's X11 child requires GDK's X11 display rather than its Wayland one.
        gtk::gdk::set_allowed_backends("x11");
        gtk::init()
            .map_err(|_| "The stream player could not connect to the desktop.".to_string())?;
    }
    if !gtk::is_initialized_main_thread()
        || !gtk::gdk::Display::default()
            .is_some_and(|display| display.type_().name() == "GdkX11Display")
    {
        return Err("The stream player needs Brick to run through X11/XWayland.".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn protect_linux_navigation(webview: &WebView, player_url: &str) -> Result<(), String> {
    use gtk::prelude::*;
    use webkit2gtk::{
        HardwareAccelerationPolicy, NavigationPolicyDecision, NavigationPolicyDecisionExt,
        PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType, SettingsExt, URIRequestExt,
        UserContentManagerExt, WebContextExt, WebViewExt,
    };
    use wry::WebViewExtUnix;

    let view = webview.webview();
    let sandboxed = view
        .context()
        .is_some_and(|context| context.is_sandbox_enabled() && context.is_ephemeral());
    #[cfg(test)]
    eprintln!("Stream WebKit sandbox enabled={sandboxed}");
    if !sandboxed {
        return Err(
            "The stream player cannot start because its browser sandbox is disabled.".to_string(),
        );
    }
    if let Some(manager) = view.user_content_manager() {
        // This media child has no native JavaScript API. Wry registers an unused
        // IPC callback that strongly captures WebView, while WebView owns this
        // manager. Remove that reference cycle before loading provider content.
        manager.unregister_script_message_handler("ipc");
        if let Some(signal) =
            gtk::glib::subclass::SignalId::lookup("script-message-received", manager.type_())
        {
            use gtk::glib::{gobject_ffi, translate::IntoGlib};
            // SAFETY: manager owns a live GObject on the GTK thread. The match is
            // solely its signal id; null function/data pointers are ignored by
            // G_SIGNAL_MATCH_ID. This private manager has no application handlers.
            unsafe {
                gobject_ffi::g_signal_handlers_disconnect_matched(
                    manager.as_ptr().cast(),
                    gobject_ffi::G_SIGNAL_MATCH_ID,
                    signal.into_glib(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }
    if let Some(settings) = WebViewExt::settings(&view) {
        // The user already selected this stream in the native sidebar. Permit
        // the official iframe's muted autoplay without a second browser click.
        settings.set_media_playback_requires_user_gesture(false);
        // XWayland child windows cannot reliably share WebKit's GBM buffers with
        // every GPU driver. Software compositing keeps the embedded player visible.
        settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
    }
    let allowed = player_url.to_string();
    // WebKit calls this for subframes too. Permit only official media frames,
    // and explicitly reject any attempted forwarding of the Brick bearer header.
    view.connect_decide_policy(move |_, decision, kind| {
        if kind != PolicyDecisionType::NavigationAction {
            return false;
        }
        let permitted = decision
            .downcast_ref::<NavigationPolicyDecision>()
            .and_then(|policy| policy.navigation_action())
            .and_then(|action| {
                let request = action.request()?;
                let destination = request.uri()?;
                let has_authorization = request
                    .http_headers()
                    .is_some_and(|headers| headers.one("Authorization").is_some());
                Some({
                    let permitted = destination.as_str() == allowed
                        || (!has_authorization
                            && !action.is_user_gesture()
                            && allowed_provider_frame(&destination));
                    #[cfg(test)]
                    eprintln!(
                        "Stream policy permitted={permitted}: {}",
                        diagnostic_destination(&destination)
                    );
                    permitted
                })
            })
            .unwrap_or(false);
        if permitted {
            decision.use_();
        } else {
            decision.ignore();
        }
        true
    });
    view.connect_permission_request(|_, request| {
        request.deny();
        true
    });
    view.connect_context_menu(|_, _, _, _| true);
    Ok(())
}

#[cfg(target_os = "windows")]
fn protect_windows_permissions(webview: &WebView) -> Result<(), String> {
    use webview2_com::{
        Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_AUTOPLAY,
            COREWEBVIEW2_PERMISSION_STATE_ALLOW, COREWEBVIEW2_PERMISSION_STATE_DENY,
        },
        PermissionRequestedEventHandler,
    };
    use wry::WebViewExtWindows;

    // Selecting the stream permits playback alone. Camera, microphone, location,
    // notifications, files and clipboard remain denied. Capture no session token.
    let handler = PermissionRequestedEventHandler::create(Box::new(|_, arguments| {
        if let Some(arguments) = arguments {
            unsafe {
                let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                arguments.PermissionKind(&mut kind)?;
                arguments.SetState(if kind == COREWEBVIEW2_PERMISSION_KIND_AUTOPLAY {
                    COREWEBVIEW2_PERMISSION_STATE_ALLOW
                } else {
                    COREWEBVIEW2_PERMISSION_STATE_DENY
                })?;
            }
        }
        Ok(())
    }));
    let mut registration = 0;
    // SAFETY: Wry's COM view and handler are used on the native UI thread. The
    // view retains the handler until its controller closes on StreamPlayer drop.
    unsafe {
        webview
            .webview()
            .add_PermissionRequested(&handler, &mut registration)
    }
    .map_err(|_| "The stream player could not protect browser permissions.".to_string())
}

#[cfg(target_os = "linux")]
fn watch_linux_failures(webview: &WebView, player_url: &str, failure: Arc<Mutex<Option<String>>>) {
    use webkit2gtk::WebViewExt;
    use wry::WebViewExtUnix;

    let view = webview.webview();
    #[cfg(test)]
    view.connect_load_changed(|view, event| {
        eprintln!(
            "Stream GTK load {event:?}: {}",
            view.uri()
                .map(|uri| diagnostic_destination(&uri))
                .unwrap_or_default()
        );
    });
    let wrapper_url = player_url.to_string();
    let load_failure = Arc::clone(&failure);
    view.connect_load_failed(move |_, _, destination, _| {
        #[cfg(test)]
        eprintln!(
            "Stream GTK load failed: {}",
            diagnostic_destination(destination)
        );
        if destination == wrapper_url {
            if let Ok(mut failure) = load_failure.lock() {
                failure.get_or_insert_with(|| {
                    "The stream player could not connect. Check your connection and try again."
                        .to_string()
                });
            }
            // Native egui shows the error; do not render a WebKit error document
            // that could include request details or the protected player address.
            return true;
        }
        false
    });
    view.connect_web_process_terminated(move |_, reason| {
        #[cfg(test)]
        eprintln!("Stream GTK web process terminated: {reason:?}");
        #[cfg(not(test))]
        let _ = reason;
        if let Ok(mut failure) = failure.lock() {
            failure.get_or_insert_with(|| {
                "The stream player stopped unexpectedly. Please try again.".to_string()
            });
        }
    });
}

#[cfg(test)]
fn diagnostic_destination(value: &str) -> String {
    Url::parse(value)
        .map(|url| {
            format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap_or_default(),
                url.path()
            )
        })
        .unwrap_or_else(|_| "Invalid URL".to_string())
}

#[cfg(any(target_os = "linux", test))]
fn allowed_provider_frame(value: &str) -> bool {
    if value == "about:blank" {
        return true;
    }
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some("player.twitch.tv") => matches!(url.path(), "/" | "/embed-error.html"),
        Some("www.youtube.com" | "www.youtube-nocookie.com") => url.path().starts_with("/embed/"),
        _ => false,
    }
}

/// Call while a player is open, alongside a roughly 30 Hz egui repaint. This does
/// not initialize GTK and never waits for an event; dropping StreamPlayer removes
/// the child and ends its media session.
pub fn pump_events() {
    #[cfg(target_os = "linux")]
    if gtk::is_initialized_main_thread() {
        let started = std::time::Instant::now();
        for _ in 0..8 {
            if !gtk::events_pending() || started.elapsed() >= std::time::Duration::from_millis(3) {
                break;
            }
            gtk::main_iteration_do(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_timeout_does_not_interrupt_a_loaded_player() {
        let player = StreamPlayer {
            webview: None,
            bounds: [0; 4],
            loaded: Arc::new(AtomicBool::new(false)),
            created: Instant::now() - WRAPPER_LOAD_TIMEOUT,
            failure: Arc::new(Mutex::new(None)),
        };
        assert!(player.failure().unwrap().contains("too long"));
        player.loaded.store(true, Ordering::Relaxed);
        assert!(player.failure().is_none());
        *player.failure.lock().unwrap() =
            Some("The stream player stopped unexpectedly.".to_string());
        assert!(player.failure().unwrap().contains("stopped unexpectedly"));
    }

    #[test]
    fn bearer_navigation_stays_on_configured_protected_endpoint() {
        let base = "https://brick.example";
        assert!(validate_player_address(
            "https://brick.example/v1/streams/player/12345/twitch",
            base
        )
        .is_ok());
        assert!(validate_player_address(
            "http://127.0.0.1:8787/v1/streams/player/12345/youtube",
            "http://127.0.0.1:8787"
        )
        .is_ok());
        for destination in [
            "https://brick.example/v1/streams/player/12345/unknown",
            "https://brick.example/v1/streams/player/12345/twitch/other",
            "https://brick.example/v1/streams/player/12345/twitch?token=secret",
            "https://brick.example/v1/streams/player/12345/youtube#secret",
            "https://evil.example/v1/streams/player/12345",
            "https://brick.example.evil.example/v1/streams/player/12345",
            "https://token@brick.example/v1/streams/player/12345",
            "https://brick.example/v1/streams/player/12345?token=secret",
            "https://brick.example/v1/streams/player/12345#secret",
            "https://brick.example/v1/streams/player/12345/other",
            "https://brick.example/v1/streams/player/member",
            "https://brick.example/v1/streams/player/",
            "https://brick.example/v1/roster",
            "http://brick.example/v1/streams/player/12345",
        ] {
            assert!(validate_player_address(destination, base).is_err());
        }
        assert!(validate_player_address(
            "http://remote.example/v1/streams/player/12345",
            "http://remote.example"
        )
        .is_err());
    }

    #[test]
    fn media_navigation_rejects_non_provider_and_lookalike_urls() {
        assert!(allowed_provider_frame(
            "https://player.twitch.tv/?channel=example&parent=brick.example"
        ));
        assert!(allowed_provider_frame(
            "https://www.youtube.com/embed/abcdefghijk"
        ));
        for destination in [
            "https://player.twitch.tv.evil.example/",
            "https://www.youtube.com/watch?v=abcdefghijk",
            "https://www.youtube.com@evil.example/embed/abcdefghijk",
            "http://player.twitch.tv/",
            "file:///tmp/stream.html",
            "javascript:alert(1)",
            "https://player.twitch.tv:8443/",
        ] {
            assert!(!allowed_provider_frame(destination));
        }
    }
}
