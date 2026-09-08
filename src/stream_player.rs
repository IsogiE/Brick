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

use crate::stream_preferences::{PreferenceBridge, Preferences};

const WRAPPER_LOAD_TIMEOUT: Duration = Duration::from_secs(25);

pub struct StreamPlayer {
    webview: Option<WebView>,
    bounds: [i32; 4],
    loaded: Arc<AtomicBool>,
    created: Instant,
    failure: Arc<Mutex<Option<String>>>,
    preferences: Option<PreferenceBridge>,
    #[cfg(target_os = "linux")]
    preference_handler: Option<(webkit2gtk::UserContentManager, gtk::glib::SignalHandlerId)>,
}

impl StreamPlayer {
    pub fn new(
        frame: &eframe::Frame,
        ctx: &egui::Context,
        url: &str,
        token: &str,
        rect: egui::Rect,
        pixels_per_point: f32,
        preferences: Option<Preferences>,
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
        let browser_ctx = ctx.clone();
        let preferences = preferences
            .filter(|_| player_url.path().ends_with("/twitch"))
            .map(|preferences| PreferenceBridge::new(player_url.as_str(), preferences));
        let builder = WebViewBuilder::new()
            .with_bounds(wry_bounds(bounds))
            .with_incognito(true)
            .with_autoplay(true)
            .with_devtools(false)
            .with_clipboard(false)
            .with_hotkeys_zoom(false)
            .with_focused(false)
            .with_background_color((18, 20, 25, 255))
            .with_new_window_req_handler(move |destination, _| {
                open_provider_window(&browser_ctx, &destination)
            })
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

        let builder = if let Some(preferences) = &preferences {
            let (relay, capture) = preferences.scripts(&player_url.origin().ascii_serialization());
            builder
                .with_initialization_script_for_main_only(relay, true)
                .with_initialization_script_for_main_only(capture, false)
        } else {
            builder
        };

        #[cfg(target_os = "windows")]
        let builder = {
            use wry::WebViewBuilderExtWindows;
            let builder = if let Some(preferences) = &preferences {
                let preferences = preferences.clone();
                builder.with_ipc_handler(move |request| {
                    preferences.receive(&request.uri().to_string(), request.body());
                })
            } else {
                builder
            };
            // Override Wry's default flags so WebView2 keeps SmartScreen enabled.
            builder.with_additional_browser_args("--autoplay-policy=no-user-gesture-required")
        };

        // WebView2 invokes this for top-level navigations; provider iframe requests
        // are separate. Public provider links leave through the system browser;
        // the child itself must stay on the protected wrapper.
        #[cfg(not(target_os = "linux"))]
        let builder = {
            let allowed = player_url.to_string();
            let ctx = ctx.clone();
            builder.with_navigation_handler(move |destination| {
                if destination == allowed {
                    true
                } else {
                    open_provider_link(&ctx, &destination);
                    false
                }
            })
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
            preferences,
            #[cfg(target_os = "linux")]
            preference_handler: None,
        };
        #[cfg(target_os = "linux")]
        let mut player = player;
        let webview = player
            .webview
            .as_ref()
            .expect("The player has just been created");
        #[cfg(target_os = "linux")]
        {
            protect_linux_navigation(webview, player_url.as_str(), ctx)?;
            watch_linux_failures(webview, player_url.as_str(), Arc::clone(&player.failure));
            if let Some(preferences) = &player.preferences {
                player.preference_handler = attach_linux_preferences(webview, preferences.clone());
            }
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
        if let Some(preferences) = &self.preferences {
            preferences.close();
        }
        #[cfg(target_os = "linux")]
        if let Some((manager, handler)) = self.preference_handler.take() {
            use gtk::prelude::*;
            use webkit2gtk::UserContentManagerExt;
            manager.unregister_script_message_handler("brickConsent");
            manager.disconnect(handler);
        }
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

fn open_provider_window(ctx: &egui::Context, destination: &str) -> NewWindowResponse {
    open_provider_link(ctx, destination);
    // Never create another embedded browser, even for a valid provider link.
    NewWindowResponse::Deny
}

fn open_provider_link(ctx: &egui::Context, destination: &str) {
    let Ok(url) = Url::parse(destination) else {
        return;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(
            url.host_str(),
            Some(
                "twitch.tv"
                    | "www.twitch.tv"
                    | "m.twitch.tv"
                    | "clips.twitch.tv"
                    | "youtube.com"
                    | "www.youtube.com"
                    | "m.youtube.com"
                    | "youtu.be"
            )
        )
    {
        return;
    }
    // Eframe opens these using the system's default browser, just like native
    // hyperlinks. Only the public URL crosses over, never the wrapper's headers.
    ctx.open_url(egui::OpenUrl::new_tab(url.as_str()));
    ctx.request_repaint();
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
fn remove_unused_linux_ipc(view: &webkit2gtk::WebView) -> Result<(), String> {
    use gtk::prelude::*;
    use webkit2gtk::{UserContentManagerExt, WebViewExt};

    if let Some(manager) = view.user_content_manager() {
        // Wry registers an unused general-purpose IPC callback that strongly
        // captures WebView, while WebView owns this manager. Remove that cycle
        // before adding the narrow, weak preference callback below.
        manager.unregister_script_message_handler("ipc");
        if let Some(signal) =
            gtk::glib::subclass::SignalId::lookup("script-message-received", manager.type_())
        {
            disconnect_linux_signal_handlers(manager.upcast_ref(), signal)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn disconnect_linux_signal_handlers(
    object: &gtk::glib::Object,
    signal: gtk::glib::subclass::SignalId,
) -> Result<(), String> {
    use gtk::glib::{gobject_ffi, object::ObjectType, translate::IntoGlib};

    // Older GLib silently ignores an ID-only disconnect_matched request. Find
    // each registration instead; handler_find supports this mask on Ubuntu's
    // GLib too. This private manager has only Wry's callback at this point.
    for attempt in 0..=8 {
        // SAFETY: object remains alive on its GTK thread. MATCH_ID ignores the
        // null closure/function/data arguments; only returned live IDs are used.
        let handler = unsafe {
            gobject_ffi::g_signal_handler_find(
                object.as_ptr(),
                gobject_ffi::G_SIGNAL_MATCH_ID,
                signal.into_glib(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if handler == 0 {
            return Ok(());
        }
        if attempt == 8 {
            break;
        }
        unsafe { gobject_ffi::g_signal_handler_disconnect(object.as_ptr(), handler) };
    }
    Err("The stream player could not release its browser callbacks.".to_string())
}

#[cfg(target_os = "linux")]
fn attach_linux_preferences(
    webview: &WebView,
    preferences: PreferenceBridge,
) -> Option<(webkit2gtk::UserContentManager, gtk::glib::SignalHandlerId)> {
    use gtk::prelude::*;
    use webkit2gtk::{UserContentManagerExt, WebViewExt};
    use wry::WebViewExtUnix;

    let view = webview.webview();
    let manager = view.user_content_manager()?;
    let weak = view.downgrade();
    let handler =
        manager.connect_script_message_received(Some("brickConsent"), move |_, result| {
            // WebKit reports only the top-level URL for this signal. The protected
            // wrapper separately checks the browser's iframe origin and source and
            // adds its private nonce. Never trust an origin claimed in JSON.
            if let (Some(view), Some(value)) = (weak.upgrade(), result.js_value()) {
                if let Some(uri) = view.uri() {
                    preferences.receive(uri.as_str(), &value.to_string());
                }
            }
        });
    if manager.register_script_message_handler("brickConsent") {
        Some((manager, handler))
    } else {
        manager.disconnect(handler);
        None
    }
}

#[cfg(target_os = "linux")]
fn protect_linux_navigation(
    webview: &WebView,
    player_url: &str,
    ctx: &egui::Context,
) -> Result<(), String> {
    use gtk::prelude::*;
    use webkit2gtk::{
        HardwareAccelerationPolicy, NavigationPolicyDecision, NavigationPolicyDecisionExt,
        PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType, SettingsExt, URIRequestExt,
        WebContextExt, WebViewExt,
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
    remove_unused_linux_ipc(&view)?;
    if let Some(settings) = WebViewExt::settings(&view) {
        // The user already selected this stream in the native sidebar. Permit
        // the official iframe's muted autoplay without a second browser click.
        settings.set_media_playback_requires_user_gesture(false);
        // XWayland child windows cannot reliably share WebKit's GBM buffers with
        // every GPU driver. Software compositing keeps the embedded player visible.
        settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
    }
    let allowed = player_url.to_string();
    let ctx = ctx.clone();
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
                    if !permitted && !has_authorization && action.is_user_gesture() {
                        open_provider_link(&ctx, &destination);
                    }
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
    fn provider_popups_open_in_the_system_browser_without_creating_a_webview() {
        for destination in [
            "https://www.twitch.tv/guildmate?tt_content=channel_name&tt_medium=embed",
            "https://twitch.tv/guildmate",
            "https://m.twitch.tv/guildmate",
            "https://clips.twitch.tv/ExampleClip",
            "https://www.youtube.com/watch?v=abcdefghijk&feature=emb_logo&t=30",
            "https://youtube.com/live/abcdefghijk",
            "https://m.youtube.com/watch?v=abcdefghijk",
            "https://youtu.be/abcdefghijk?t=30",
        ] {
            let ctx = egui::Context::default();
            assert!(matches!(
                open_provider_window(&ctx, destination),
                NewWindowResponse::Deny
            ));
            ctx.output(|output| {
                assert_eq!(output.commands.len(), 1, "{destination}");
                let egui::OutputCommand::OpenUrl(open) = &output.commands[0] else {
                    panic!("Expected a system browser request for {destination}");
                };
                assert_eq!(open.url, destination);
                assert!(open.new_tab);
            });
        }
    }

    #[test]
    fn provider_popups_reject_private_and_non_provider_destinations() {
        for destination in [
            "https://brick.example/v1/streams/player/12345/twitch",
            "https://www.twitch.tv.evil.example/guildmate",
            "https://www.youtube.com@evil.example/watch?v=abcdefghijk",
            "https://token@www.twitch.tv/guildmate",
            "https://www.twitch.tv:8443/guildmate",
            "https://evil.example/",
            "http://www.twitch.tv/guildmate",
            "javascript:alert(1)",
            "file:///tmp/stream.html",
            "twitch://stream/guildmate",
            "about:blank",
            "not a URL",
        ] {
            let ctx = egui::Context::default();
            assert!(matches!(
                open_provider_window(&ctx, destination),
                NewWindowResponse::Deny
            ));
            assert!(
                ctx.output(|output| output.commands.is_empty()),
                "{destination}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signal_cleanup_releases_captured_references_on_older_glib() {
        use gtk::glib::{prelude::*, Object};

        // Plain GObject needs no display. This runs on Ubuntu's older GLib in
        // ordinary CI and detects the otherwise silent ID-only cleanup failure.
        let object = Object::new::<Object>();
        let keepalive = Arc::new(());
        let weak = Arc::downgrade(&keepalive);
        for _ in 0..2 {
            let captured = keepalive.clone();
            object.connect_notify_local(None, move |_, _| {
                std::hint::black_box(&captured);
            });
        }
        drop(keepalive);
        assert!(weak.upgrade().is_some());
        let signal = gtk::glib::subclass::SignalId::lookup("notify", object.type_()).unwrap();
        disconnect_linux_signal_handlers(&object, signal).unwrap();
        assert!(weak.upgrade().is_none());
        disconnect_linux_signal_handlers(&object, signal).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires its own X11 display and /tmp/brick-consent-native profile"]
    fn native_preference_relay_survives_reopen_and_releases_webkit() {
        use gtk::prelude::*;
        use std::{
            fs,
            io::{Read, Write},
            net::TcpListener,
            thread,
            time::{SystemTime, UNIX_EPOCH},
        };
        use webkit2gtk::{WebContextExt, WebViewExt};
        use wry::{WebViewBuilderExtUnix, WebViewExtUnix};

        fn owned_webkit_processes() -> Vec<u32> {
            let processes: Vec<_> = fs::read_dir("/proc")
                .unwrap()
                .flatten()
                .filter_map(|entry| {
                    let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                    let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
                    let end = stat.rfind(')')?;
                    let parent = stat[end + 2..]
                        .split_whitespace()
                        .nth(1)?
                        .parse::<u32>()
                        .ok()?;
                    let webkit = stat.contains("(WebKit");
                    Some((pid, parent, webkit))
                })
                .collect();
            let mut selected = std::collections::HashSet::from([std::process::id()]);
            loop {
                let before = selected.len();
                for (pid, parent, _) in &processes {
                    if selected.contains(parent) {
                        selected.insert(*pid);
                    }
                }
                if selected.len() == before {
                    break;
                }
            }
            processes
                .into_iter()
                .filter_map(|(pid, _, webkit)| (webkit && selected.contains(&pid)).then_some(pid))
                .collect()
        }

        fn process_is_running(pid: u32) -> bool {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rfind(')').map(|end| stat[end + 2..].starts_with('Z')))
                .is_some_and(|zombie| !zombie)
        }

        let profile = crate::addon::config_dir().unwrap();
        assert!(profile.starts_with(std::env::temp_dir().join("brick-consent-native")));
        fs::create_dir_all(&profile).unwrap();
        let saved = profile.join("stream-preferences.json");
        let _ = fs::remove_file(&saved);
        gtk::init().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let wrapper = format!("http://127.0.0.1:{port}/wrapper");
        let provider_origin = format!("http://localhost:{port}");
        let first = Arc::new(AtomicBool::new(true));
        let stopped = Arc::new(AtomicBool::new(false));
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 60_000;
        let fixture_value =
            format!("{{\"loggedIn\":{{}},\"loggedOut\":{{\"Gambling\":{expiry}}}}}");
        let server_first = first.clone();
        let server_stopped = stopped.clone();
        let provider_url = format!("{provider_origin}/provider");
        let expected = fixture_value.clone();
        let server = thread::spawn(move || {
            while !server_stopped.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let count = socket.read(&mut request).unwrap_or(0);
                let provider =
                    String::from_utf8_lossy(&request[..count]).starts_with("GET /provider ");
                let body = if provider {
                    let acknowledgement = if server_first.load(Ordering::Relaxed) {
                        format!("localStorage.setItem('content-classification-labels-acknowledged',{});", serde_json::to_string(&expected).unwrap())
                    } else {
                        String::new()
                    };
                    format!("<!doctype html><script>{acknowledgement}parent.postMessage({{kind:'fixture-report',value:localStorage.getItem('content-classification-labels-acknowledged')}},'*');</script>")
                } else {
                    format!("<!doctype html><script>addEventListener('message',e=>{{if(e.data.kind==='fixture-report')document.title=e.data.value||'missing'}})</script><iframe src='{provider_url}'></iframe>")
                };
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            }
        });
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(640, 480);

        for cycle in 0..3 {
            let context = webkit2gtk::WebContext::new_ephemeral();
            context.set_sandbox_enabled(true);
            let seed = webkit2gtk::WebView::with_context(&context);
            let bridge = PreferenceBridge::new(&wrapper, Preferences::load());
            let (relay, capture) = bridge.scripts(&format!("http://127.0.0.1:{port}"));
            // Only this ignored test substitutes a localhost provider. No real
            // Twitch page is loaded and no consent is fabricated for a service.
            let webview = WebViewBuilder::new()
                .with_incognito(true)
                .with_related_view(seed.clone())
                .with_initialization_script_for_main_only(
                    relay.replace("https://player.twitch.tv", &provider_origin),
                    true,
                )
                .with_initialization_script_for_main_only(
                    capture.replace("https://player.twitch.tv", &provider_origin),
                    false,
                )
                .build_gtk(&window)
                .unwrap();
            unsafe {
                seed.destroy();
            }
            drop(seed);
            drop(context);
            remove_unused_linux_ipc(&webview.webview()).unwrap();
            let handler = attach_linux_preferences(&webview, bridge.clone()).unwrap();
            let view = webview.webview();
            let weak = view.downgrade();
            let weak_context = view.context().unwrap().downgrade();
            assert!(view.context().unwrap().is_sandbox_enabled());
            let player = StreamPlayer {
                webview: Some(webview),
                bounds: [0; 4],
                loaded: Arc::new(AtomicBool::new(true)),
                created: Instant::now(),
                failure: Arc::new(Mutex::new(None)),
                preferences: Some(bridge),
                preference_handler: Some(handler),
            };
            player.webview.as_ref().unwrap().load_url(&wrapper).unwrap();
            window.show_all();
            let deadline = Instant::now() + Duration::from_secs(12);
            while Instant::now() < deadline {
                pump_events();
                if view.title().as_deref() == Some(fixture_value.as_str()) && saved.is_file() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                view.title().as_deref(),
                Some(fixture_value.as_str()),
                "cycle {cycle} did not capture/restore through the real iframe"
            );
            assert_eq!(
                fs::read_to_string(&saved).unwrap(),
                format!("{{\"Gambling\":{expiry}}}")
            );
            first.store(false, Ordering::Relaxed);
            let children = owned_webkit_processes();
            assert!(
                !children.is_empty(),
                "the native fixture created no WebKit processes"
            );
            drop(view);
            drop(player);
            for _ in 0..400 {
                pump_events();
                if weak.upgrade().is_none()
                    && weak_context.upgrade().is_none()
                    && !children.iter().any(|pid| process_is_running(*pid))
                {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                weak.upgrade().is_none(),
                "cycle {cycle} retained its widget"
            );
            assert!(
                weak_context.upgrade().is_none(),
                "cycle {cycle} retained its private context"
            );
            assert!(
                !children.iter().any(|pid| process_is_running(*pid)),
                "cycle {cycle} retained its owned WebKit processes: {children:?}"
            );
            eprintln!("Native preference cycle {cycle}: original expiry restored; widget/context and all {} owned WebKit processes released", children.len());
        }
        window.close();
        stopped.store(true, Ordering::Relaxed);
        server.join().unwrap();
    }

    #[test]
    fn load_timeout_does_not_interrupt_a_loaded_player() {
        let player = StreamPlayer {
            webview: None,
            preferences: None,
            #[cfg(target_os = "linux")]
            preference_handler: None,
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
