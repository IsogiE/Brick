use super::{allowed_document, start_url, title};
use crate::streams::Provider;
use eframe::egui;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use webkit2gtk::{
    DownloadExt, FileChooserRequestExt, HardwareAccelerationPolicy, NavigationPolicyDecisionExt,
    PermissionRequestExt, PolicyDecisionExt, SettingsExt, URIRequestExt, WebContextExt, WebViewExt,
    WebsiteDataManagerExt,
};

pub(super) fn watch_requests(
    view: &wry::WebView,
    context: std::rc::Weak<super::Context>,
    ctx: &egui::Context,
) -> Result<(), String> {
    use wry::WebViewExtUnix;
    let ctx = ctx.clone();
    // Install before the normal media policy so a recognized account action
    // can be queued while its navigation is still denied in the media view.
    view.webview()
        .connect_decide_policy(move |_, decision, kind| {
            if !matches!(
                kind,
                webkit2gtk::PolicyDecisionType::NavigationAction
                    | webkit2gtk::PolicyDecisionType::NewWindowAction
            ) {
                return false;
            }
            let requested = decision
                .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                .and_then(|decision| decision.navigation_action())
                .is_some_and(|action| {
                    action.request().is_some_and(|request| {
                        let auth = request
                            .http_headers()
                            .is_some_and(|headers| headers.one("Authorization").is_some());
                        request.uri().is_some_and(|uri| {
                            context.upgrade().is_some_and(|context| {
                                context.request_login(&uri, action.is_user_gesture(), auth)
                            })
                        })
                    })
                });
            if requested {
                decision.ignore();
                ctx.request_repaint();
            }
            requested
        });
    Ok(())
}

pub(crate) struct Context {
    pub context: webkit2gtk::WebContext,
    views: RefCell<Vec<gtk::glib::WeakRef<webkit2gtk::WebView>>>,
}

impl Context {
    pub fn new() -> Result<Self, String> {
        let context = webkit2gtk::WebContext::new_ephemeral();
        context.set_sandbox_enabled(true);
        if !context.is_ephemeral() || !context.is_sandbox_enabled() {
            return Err("The provider session needs the browser sandbox.".into());
        }
        if let Some(manager) = context.website_data_manager() {
            manager.set_persistent_credential_storage_enabled(false);
        }
        // Provider downloads never leave this private context.
        context.connect_download_started(|_, download| {
            download.cancel();
        });
        Ok(Self {
            context,
            views: RefCell::new(Vec::new()),
        })
    }

    pub fn register(&self, view: &webkit2gtk::WebView) {
        let mut views = self.views.borrow_mut();
        views.retain(|view| view.upgrade().is_some());
        views.push(view.downgrade());
    }

    pub fn retire(&self) {
        let views = std::mem::take(&mut *self.views.borrow_mut());
        for view in views.into_iter().filter_map(|view| view.upgrade()) {
            view.stop_loading();
            view.set_is_muted(true);
            view.terminate_web_process();
            view.hide();
        }
    }
}

pub(super) struct Window {
    window: gtk::Window,
    view: webkit2gtk::WebView,
    _load_status: Rc<LoadStatus>,
}

struct LoadStatus {
    panel: gtk::Box,
    spinner: gtk::Spinner,
    label: gtk::Label,
    retry: gtk::Button,
    failed: Cell<bool>,
}

impl LoadStatus {
    fn new() -> Self {
        let panel = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        panel.set_margin_start(12);
        panel.set_margin_end(12);
        panel.set_margin_top(12);
        panel.set_margin_bottom(12);
        let spinner = gtk::Spinner::new();
        let label = gtk::Label::new(Some("Couldn't connect"));
        let retry = gtk::Button::with_label("Try again");
        panel.pack_start(&spinner, false, false, 0);
        panel.pack_start(&label, true, true, 0);
        panel.pack_end(&retry, false, false, 0);
        Self {
            panel,
            spinner,
            label,
            retry,
            failed: Cell::new(false),
        }
    }

    fn loading(&self) {
        self.failed.set(false);
        self.panel.show_all();
        self.label.hide();
        self.retry.hide();
        self.spinner.start();
    }

    fn fail(&self) {
        self.failed.set(true);
        self.panel.show_all();
        self.spinner.stop();
        self.spinner.hide();
    }

    fn finished(&self) {
        self.spinner.stop();
        if !self.failed.get() {
            self.panel.hide();
        }
    }
}

impl Window {
    pub fn from_frame(
        context: &Context,
        provider: &Provider,
        _frame: &eframe::Frame,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        Self::new_at(
            context,
            provider,
            start_url(provider),
            allowed_document,
            ctx,
        )
    }

    pub fn new(
        context: &Context,
        provider: &Provider,
        _player: &wry::WebView,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        Self::new_at(
            context,
            provider,
            start_url(provider),
            allowed_document,
            ctx,
        )
    }

    fn new_at(
        context: &Context,
        provider: &Provider,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        // Only WebContext is shared. Do not inherit the media view's scripts,
        // message handlers, headers, settings, capture code or document opener.
        let manager = webkit2gtk::UserContentManager::new();
        let view = webkit2gtk::WebView::builder()
            .web_context(&context.context)
            .user_content_manager(&manager)
            .build();
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_enable_developer_extras(false);
            settings.set_enable_fullscreen(false);
            settings.set_enable_page_cache(false);
            // Match the existing player: XWayland cannot reliably share GBM
            // buffers with every GPU driver. This changes compositing only.
            settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(560, 700);
        window.set_title(&title(provider, start_url(provider)));
        let contents = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let load_status = Rc::new(LoadStatus::new());
        contents.pack_start(&load_status.panel, false, false, 0);
        contents.pack_start(&view, true, true, 0);
        window.add(&contents);
        let retry_view = view.downgrade();
        let retry_start = start.to_owned();
        load_status.retry.connect_clicked(move |_| {
            if let Some(view) = retry_view.upgrade() {
                // Retry only this window's original fixed provider destination.
                // Never replay a failed redirect or a URL from an error string.
                view.load_uri(&retry_start);
            }
        });
        let closed = ctx.clone();
        window.connect_delete_event(move |window, _| {
            // Owned native widget; all Rust references remain on this UI thread.
            unsafe {
                window.destroy();
            }
            closed.request_repaint();
            gtk::glib::Propagation::Stop
        });
        view.connect_permission_request(|_, request| {
            request.deny();
            true
        });
        view.connect_run_file_chooser(|_, request| {
            request.cancel();
            true
        });
        view.connect_context_menu(|_, _, _, _| true);
        view.connect_create(|_, _| None);
        let provider_for_policy = provider.clone();
        view.connect_decide_policy(move |_, decision, kind| {
            if kind != webkit2gtk::PolicyDecisionType::NavigationAction {
                return false;
            }
            let allowed = decision
                .downcast_ref::<webkit2gtk::NavigationPolicyDecision>()
                .and_then(|decision| decision.navigation_action())
                .and_then(|action| action.request())
                .is_some_and(|request| {
                    let no_auth = request
                        .http_headers()
                        .is_none_or(|headers| headers.one("Authorization").is_none());
                    no_auth
                        && request
                            .uri()
                            .is_some_and(|uri| permits(&provider_for_policy, &uri))
                });
            if allowed {
                decision.use_();
            } else {
                decision.ignore();
            }
            true
        });
        let title_window = window.downgrade();
        let title_provider = provider.clone();
        let status = Rc::downgrade(&load_status);
        view.connect_load_changed(move |view, event| {
            if let Some(status) = status.upgrade() {
                match event {
                    webkit2gtk::LoadEvent::Started => status.loading(),
                    webkit2gtk::LoadEvent::Finished => status.finished(),
                    _ => (),
                }
            }
            if event == webkit2gtk::LoadEvent::Committed {
                if let Some(window) = title_window.upgrade() {
                    window.set_title(&title(
                        &title_provider,
                        view.uri().as_deref().unwrap_or_default(),
                    ));
                }
            }
        });
        let status = Rc::downgrade(&load_status);
        view.connect_load_failed(move |_, _, _, error| {
            // Navigating again, closing, or enforcing the origin policy can
            // cancel a load without a connection failure.
            if error.matches(webkit2gtk::NetworkError::Cancelled)
                || error.matches(webkit2gtk::PolicyError::FrameLoadInterruptedByPolicyChange)
            {
                return true;
            }
            if let Some(status) = status.upgrade() {
                status.fail();
            }
            // Keep provider URLs and raw transport errors out of HTML or logs.
            // The native strip owns the error and retry UI.
            true
        });
        let status = Rc::downgrade(&load_status);
        view.connect_web_process_terminated(move |_, _| {
            if let Some(status) = status.upgrade() {
                status.fail();
            }
        });
        context.register(&view);
        view.load_uri(start);
        window.show_all();
        load_status.loading();
        Ok(Self {
            window,
            view,
            _load_status: load_status,
        })
    }

    pub fn open(&self) -> bool {
        self.window.is_visible()
    }
    pub fn present(&self) {
        self.window.present();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.view.stop_loading();
        self.view.set_is_muted(true);
        // Other POVs share the context. Destroy this window, not their process.
        unsafe {
            self.window.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProviderSessions;
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::{Duration, Instant},
    };

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Native provider fixture timed out"
            );
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn fixture_origin(_: &Provider, value: &str) -> bool {
        url::Url::parse(value).is_ok_and(|url| {
            url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")
                && url.username().is_empty()
                && url.password().is_none()
        })
    }

    #[test]
    #[ignore = "requires an isolated X11 desktop with WebKit sandbox and network namespace"]
    fn native_shared_session_keeps_account_provider_and_disconnect_boundaries() {
        assert_eq!(std::env::var("BRICK_PROVIDER_FIXTURE").as_deref(), Ok("1"));
        assert!(!std::path::Path::new("/home/lucas").exists());
        gtk::init().unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", server.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let stop_server = stopped.clone();
        let worker = thread::spawn(move || {
            while !stop_server.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = server.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 8192];
                let count = socket.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                if request.starts_with("GET /transport-error ") {
                    continue;
                }
                let login = request.starts_with("GET /login ");
                let cookie = request.lines().any(|line| {
                    line.to_ascii_lowercase().starts_with("cookie:")
                        && line.contains("brick_fixture_session=fixture-only")
                });
                let set = if login {
                    "Set-Cookie: brick_fixture_session=fixture-only; HttpOnly; SameSite=Strict; Path=/\r\n"
                } else {
                    ""
                };
                let script = if login {
                    "localStorage.setItem('fixture-login','fixture-only');"
                } else {
                    ""
                };
                let body = format!("<!doctype html><script>{script}document.title=JSON.stringify({{cookie:{cookie},storage:localStorage.getItem('fixture-login')==='fixture-only',ipc:typeof window.webkit?.messageHandlers?.ipc!=='undefined',bridge:typeof window.brickMedia!=='undefined',opener:window.opener!==null}});</script>");
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\n{set}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        struct Stop(Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop = Stop(stopped.clone());
        let sessions = ProviderSessions::default();
        let youtube = sessions.context(Provider::Youtube).unwrap();
        assert!(youtube.platform.context.is_sandbox_enabled());
        assert!(youtube.platform.context.is_ephemeral());
        let destination = super::super::start_url(&Provider::Youtube);
        assert!(!youtube.request_login(destination, false, false));
        assert!(!youtube.request_login(destination, true, true));
        assert!(!youtube.take_login_request());
        assert!(youtube.request_login(destination, true, false));
        assert!(youtube.request_login(destination, true, false));
        assert!(youtube.take_login_request());
        assert!(!youtube.take_login_request());
        // Home opens the first native view: no media child or authenticated
        // wrapper is required to create this personal session.
        assert!(!sessions.session_started(&Provider::Youtube));
        youtube
            .open_with(|| {
                Window::new_at(
                    &youtube.platform,
                    &Provider::Youtube,
                    &format!("{origin}/login"),
                    fixture_origin,
                    &egui::Context::default(),
                )
            })
            .unwrap();
        let login_state: serde_json::Value = {
            let window = youtube.window.borrow();
            let window = window.as_ref().unwrap();
            assert_eq!(
                WebViewExt::settings(&window.view)
                    .unwrap()
                    .hardware_acceleration_policy(),
                HardwareAccelerationPolicy::Never
            );
            wait_for(|| {
                window
                    .view
                    .title()
                    .is_some_and(|title| title.starts_with('{'))
            });
            window.view.load_uri(&format!("{origin}/transport-error"));
            wait_for(|| window._load_status.failed.get());
            assert!(window._load_status.panel.is_visible());
            assert!(window._load_status.retry.is_visible());
            window._load_status.retry.emit_clicked();
            wait_for(|| {
                !window._load_status.failed.get()
                    && !window.view.is_loading()
                    && window.view.title().is_some_and(|title| {
                        serde_json::from_str::<serde_json::Value>(&title).is_ok()
                    })
            });
            assert_eq!(
                window.view.uri().as_deref(),
                Some(format!("{origin}/login").as_str())
            );
            assert!(!window._load_status.panel.is_visible());
            serde_json::from_str(&window.view.title().unwrap()).unwrap()
        };
        assert_eq!(login_state["ipc"], false);
        assert_eq!(login_state["bridge"], false);
        assert_eq!(login_state["opener"], false);
        assert!(sessions.session_started(&Provider::Youtube));
        assert!(!sessions.session_started(&Provider::Twitch));
        assert!(sessions.login_open());
        assert!(sessions.login_open_for(&Provider::Youtube));
        youtube
            .open_with(|| panic!("An open login must reuse its existing native window"))
            .unwrap();
        sessions.close_login(&Provider::Youtube);
        assert!(!sessions.login_open());
        assert!(sessions.session_started(&Provider::Youtube));

        let state = |context: &super::super::Context| {
            let view = webkit2gtk::WebView::with_context(&context.platform.context);
            context.platform.register(&view);
            view.load_uri(&format!("{origin}/state"));
            wait_for(|| view.title().is_some_and(|title| title.starts_with('{')));
            let state: serde_json::Value = serde_json::from_str(&view.title().unwrap()).unwrap();
            unsafe {
                view.destroy();
            }
            state
        };
        let second_pov = sessions.context(Provider::Youtube).unwrap();
        assert!(std::rc::Rc::ptr_eq(&youtube, &second_pov));
        assert_eq!(state(&second_pov)["cookie"], true);
        assert_eq!(state(&second_pov)["storage"], true);
        // Another POV still sees the session after the previous widget closes.
        assert_eq!(state(&second_pov)["cookie"], true);
        let twitch = sessions.context(Provider::Twitch).unwrap();
        assert_eq!(state(&twitch)["cookie"], false);
        assert_eq!(state(&twitch)["storage"], false);
        let other_account = ProviderSessions::default();
        let anonymous = other_account.context(Provider::Twitch).unwrap();
        assert!(anonymous
            .open_with(|| Err("synthetic failed open".into()))
            .is_err());
        assert!(!other_account.session_started(&Provider::Twitch));
        other_account.release_unused(&anonymous);
        assert!(!anonymous.active());
        assert_eq!(
            state(&other_account.context(Provider::Youtube).unwrap())["cookie"],
            false
        );
        let reopened = Window::new_at(
            &youtube.platform,
            &Provider::Youtube,
            &format!("{origin}/state"),
            fixture_origin,
            &egui::Context::default(),
        )
        .unwrap();
        *youtube.window.borrow_mut() = Some(reopened);
        assert!(sessions.login_open());
        sessions.disconnect(&Provider::Youtube);
        assert!(!youtube.active());
        assert!(!youtube.request_login(destination, true, false));
        assert!(!youtube.take_login_request());
        assert!(!youtube.window_open());
        assert!(twitch.active());
        let replacement = sessions.context(Provider::Youtube).unwrap();
        assert!(!std::rc::Rc::ptr_eq(&replacement, &youtube));
        assert_eq!(state(&replacement)["cookie"], false);
        assert_eq!(state(&replacement)["storage"], false);
        // Held references from old operations remain retired after replacement.
        assert!(!second_pov.active());
        sessions.close();
        assert!(!replacement.active());
        assert!(!twitch.active());
        assert!(!sessions.session_started(&Provider::Youtube));
        assert!(!sessions.login_open_for(&Provider::Youtube));
        assert!(youtube
            .open_with(|| panic!("A retired session cannot reopen"))
            .is_err());
        other_account.close();
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        eprintln!("Native provider sessions: sandbox/private confirmed; shared POV state; no login IPC/scripts/opener; provider/account isolation; disconnect and stale-context retirement passed");
    }
}
