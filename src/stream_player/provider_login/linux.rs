use super::{allowed_document, returned_to_provider, start_url, title};
use crate::streams::Provider;
use eframe::egui;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use webkit2gtk::{
    CookieManagerExt, DownloadExt, FileChooserRequestExt, HardwareAccelerationPolicy,
    NavigationPolicyDecisionExt, PermissionRequestExt, PolicyDecisionExt, SettingsExt,
    URIRequestExt, WebContextExt, WebViewExt, WebsiteDataManagerExt,
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
    pub(super) fn restore(&self, cookies: &[super::session::Cookie]) -> Result<(), String> {
        if cookies.is_empty() {
            return Ok(());
        }
        let manager = self
            .context
            .cookie_manager()
            .ok_or("The viewing session could not be restored.")?;
        let pending = Rc::new(Cell::new(cookies.len()));
        let failed = Rc::new(Cell::new(false));
        let cancellation = webkit2gtk::gio::Cancellable::new();
        for saved in cookies {
            let mut cookie =
                soup::Cookie::new(&saved.name, &saved.value, &saved.domain, &saved.path, -1);
            cookie.set_secure(saved.secure);
            cookie.set_http_only(saved.http_only);
            cookie.set_same_site_policy(match saved.same_site {
                1 => soup::SameSitePolicy::Lax,
                2 => soup::SameSitePolicy::Strict,
                _ => soup::SameSitePolicy::None,
            });
            if let Some(expires) = saved.expires {
                cookie.set_expires(
                    &gtk::glib::DateTime::from_unix_utc(expires)
                        .map_err(|_| "The saved viewing session has an invalid expiry.")?,
                );
            }
            let pending = pending.clone();
            let failed = failed.clone();
            manager.add_cookie(&mut cookie, Some(&cancellation), move |result| {
                if result.is_err() {
                    failed.set(true);
                }
                pending.set(pending.get().saturating_sub(1));
            });
        }
        // Populate the private jar before any provider or media navigation.
        // Only this blank context exists here, and no RefCell borrow is held.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let main = gtk::glib::MainContext::default();
        while pending.get() != 0 && std::time::Instant::now() < deadline {
            while main.pending() {
                main.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if pending.get() != 0 || failed.get() {
            cancellation.cancel();
            return Err("The viewing session could not be restored. Please try again.".into());
        }
        Ok(())
    }

    pub(super) fn read_cookies(
        &self,
        provider: &Provider,
        done: impl FnOnce(Result<Vec<super::session::Cookie>, ()>) + 'static,
    ) {
        let Some(manager) = self.context.cookie_manager() else {
            done(Err(()));
            return;
        };
        let uri = match provider {
            Provider::Youtube => "https://www.youtube.com/",
            Provider::Twitch => "https://www.twitch.tv/",
        };
        manager.cookies(uri, None::<&webkit2gtk::gio::Cancellable>, move |result| {
            done(result.map_err(|_| ()).map(|cookies| {
                cookies
                    .into_iter()
                    .filter_map(|mut cookie| {
                        Some(super::session::Cookie {
                            name: cookie.name()?.into(),
                            value: cookie.value()?.into(),
                            domain: cookie.domain()?.into(),
                            path: cookie.path()?.into(),
                            expires: cookie.expires().map(|at| at.to_unix()),
                            secure: cookie.is_secure(),
                            http_only: cookie.is_http_only(),
                            same_site: match cookie.same_site_policy() {
                                soup::SameSitePolicy::Lax => 1,
                                soup::SameSitePolicy::Strict => 2,
                                _ => 0,
                            },
                        })
                    })
                    .collect()
            }));
        });
    }

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
    pub completion: Rc<super::LoginCompletion>,
    window: gtk::Window,
    closed: Rc<Cell<bool>>,
    view: webkit2gtk::WebView,
    _load_status: Rc<LoadStatus>,
}

struct LoadStatus {
    panel: gtk::EventBox,
    spinner: gtk::Spinner,
    label: gtk::Label,
    retry: gtk::Button,
    failed: Cell<bool>,
}

impl LoadStatus {
    fn new() -> Self {
        let panel = gtk::EventBox::new();
        // Native loading chrome only. Provider documents retain their own UI.
        let style = gtk::CssProvider::new();
        style
            .load_from_data(b"* { background-color: #15181d; color: #cbd5e1; }")
            .expect("Static provider loading style");
        panel
            .style_context()
            .add_provider(&style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);
        content.set_margin_start(24);
        content.set_margin_end(24);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(24, 24);
        spinner.set_halign(gtk::Align::Center);
        let label = gtk::Label::new(Some("Couldn't connect"));
        let retry = gtk::Button::with_label("Try again");
        retry.set_size_request(120, 36);
        content.pack_start(&spinner, false, false, 0);
        content.pack_start(&label, false, false, 0);
        content.pack_start(&retry, false, false, 0);
        panel.add(&content);
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
        frame: &eframe::Frame,
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let parent = match frame
            .window_handle()
            .map_err(|_| "The provider window could not find Brick.")?
            .as_raw()
        {
            RawWindowHandle::Xlib(handle) => handle.window,
            RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
            _ => return Err("Provider sign-in needs Brick's X11 window.".into()),
        };
        let window = Self::new_at(
            context,
            provider,
            start_url(provider),
            allowed_document,
            returned_to_provider,
            ctx,
        )?;
        window.attach_parent(parent)?;
        Ok(window)
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
            returned_to_provider,
            ctx,
        )
    }

    fn new_at(
        context: &Context,
        provider: &Provider,
        start: &str,
        permits: fn(&Provider, &str) -> bool,
        returned: fn(&Provider, &str) -> bool,
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
        view.set_background_color(&gtk::gdk::RGBA::new(
            21.0 / 255.0,
            24.0 / 255.0,
            29.0 / 255.0,
            1.0,
        ));
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(520, 640);
        window.set_position(gtk::WindowPosition::Center);
        window.set_type_hint(gtk::gdk::WindowTypeHint::Dialog);
        window.set_title(&title(provider, start_url(provider)));
        let contents = gtk::Overlay::new();
        let load_status = Rc::new(LoadStatus::new());
        contents.add(&view);
        contents.add_overlay(&load_status.panel);
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
        let closed = Rc::new(Cell::new(false));
        let close_state = closed.clone();
        window.connect_destroy(move |_| close_state.set(true));
        let close_repaint = ctx.clone();
        window.connect_delete_event(move |window, _| {
            // Owned native widget; all Rust references remain on this UI thread.
            unsafe {
                window.destroy();
            }
            close_repaint.request_repaint();
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
        let completion = Rc::new(super::LoginCompletion::default());
        let load_completion = completion.clone();
        let title_provider = provider.clone();
        let status = Rc::downgrade(&load_status);
        let finished = ctx.clone();
        view.connect_load_changed(move |view, event| {
            if event == webkit2gtk::LoadEvent::Started {
                load_completion.started();
            }
            // LoadChanged is for the main document. Hide at start/redirect
            // (and commit as a fallback) before the home page can paint.
            if matches!(
                event,
                webkit2gtk::LoadEvent::Started
                    | webkit2gtk::LoadEvent::Redirected
                    | webkit2gtk::LoadEvent::Committed
            ) {
                let home = view
                    .uri()
                    .is_some_and(|uri| returned(&title_provider, &uri));
                if let Some(window) = title_window.upgrade() {
                    if home {
                        load_completion.hide_return();
                        window.hide();
                    } else if load_completion.reveal() {
                        window.show();
                    }
                }
            }
            if let Some(status) = status.upgrade() {
                match event {
                    webkit2gtk::LoadEvent::Started => status.loading(),
                    webkit2gtk::LoadEvent::Finished => status.finished(),
                    _ => (),
                }
            }
            if event == webkit2gtk::LoadEvent::Finished {
                load_completion.finished(
                    status.upgrade().is_some_and(|status| !status.failed.get())
                        && view
                            .uri()
                            .is_some_and(|uri| returned(&title_provider, &uri)),
                );
                finished.request_repaint();
            }
            if event == webkit2gtk::LoadEvent::Committed {
                if let Some(window) = title_window.upgrade() {
                    let next = title(&title_provider, view.uri().as_deref().unwrap_or_default());
                    if window.title().as_deref() != Some(&next) {
                        window.set_title(&next);
                    }
                }
            }
        });
        let status = Rc::downgrade(&load_status);
        let failed_window = window.downgrade();
        let failed_completion = completion.clone();
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
            if failed_completion.reveal() {
                if let Some(window) = failed_window.upgrade() {
                    window.show();
                }
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
            completion,
            window,
            closed,
            view,
            _load_status: load_status,
        })
    }

    fn attach_parent(&self, parent: u64) -> Result<(), String> {
        use gtk::glib::translate::{from_glib_full, ToGlibPtr};
        use std::ffi::c_ulong;
        #[link(name = "gdk-3")]
        unsafe extern "C" {
            fn gdk_x11_window_foreign_new_for_display(
                display: *mut gtk::gdk::ffi::GdkDisplay,
                window: c_ulong,
            ) -> *mut gtk::gdk::ffi::GdkWindow;
        }
        let child = self
            .window
            .window()
            .ok_or("The provider window could not attach to Brick.")?;
        // The frame supplies a live X11 owner. GDK owns this foreign wrapper;
        // dropping it does not destroy Brick's eframe window.
        let native = unsafe {
            gdk_x11_window_foreign_new_for_display(
                child.display().to_glib_none().0,
                parent as c_ulong,
            )
        };
        if native.is_null() {
            return Err("The provider window could not attach to Brick.".into());
        }
        let owner: gtk::gdk::Window = unsafe { from_glib_full(native) };
        child.set_transient_for(&owner);
        Ok(())
    }

    pub fn open(&self) -> bool {
        !self.closed.get()
    }
    pub fn reveal(&self) {
        if self.completion.reveal() && self.open() {
            self.window.show();
        }
    }
    pub fn present(&self) {
        self.reveal();
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
        // Exercise the real private cookie manager without contacting providers.
        // Only synthetic values exist inside this network-isolated fixture.
        let restored = Context::new().unwrap();
        let saved = ["SID", "HSID"].map(|name| super::super::session::Cookie {
            name: name.into(),
            value: "synthetic-provider-session".into(),
            domain: ".youtube.com".into(),
            path: "/".into(),
            expires: Some(super::super::session::now() + 3600),
            secure: true,
            http_only: true,
            same_site: 1,
        });
        restored.restore(&saved).unwrap();
        let read = Rc::new(RefCell::new(None));
        let completed = read.clone();
        restored.read_cookies(&Provider::Youtube, move |result| {
            *completed.borrow_mut() = Some(result)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while read.borrow().is_none() && Instant::now() < deadline {
            crate::stream_player::pump_events();
            thread::sleep(Duration::from_millis(5));
        }
        let cookies = read
            .borrow_mut()
            .take()
            .expect("Cookie manager completed")
            .unwrap();
        assert!(super::super::session::signed_in(
            &Provider::Youtube,
            &cookies,
            super::super::session::now()
        ));
        assert!(cookies
            .iter()
            .all(|cookie| cookie.secure && cookie.http_only && cookie.same_site == 1));
        assert!(!super::super::session::signed_in(
            &Provider::Twitch,
            &cookies,
            super::super::session::now()
        ));
        drop(restored);
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
                    |_, value| value.ends_with("/returned"),
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
        youtube
            .open_with(|| {
                Window::new_at(
                    &youtube.platform,
                    &Provider::Youtube,
                    &format!("{origin}/login"),
                    fixture_origin,
                    |_, value| value.ends_with("/returned"),
                    &egui::Context::default(),
                )
            })
            .unwrap();
        {
            let window = youtube.window.borrow();
            let window = window.as_ref().unwrap();
            wait_for(|| !window.view.is_loading());
            window.view.load_uri(&format!("{origin}/returned"));
            wait_for(|| window.completion.returned.get());
            assert!(!window.window.is_visible());
            assert!(window.open());
        }
        assert!(sessions.login_open());
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
            |_, value| value.ends_with("/returned"),
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
        assert_delayed_login_completion(&origin);
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        eprintln!("Native provider sessions: sandbox/private confirmed; shared POV state; no login IPC/scripts/opener; provider/account isolation; disconnect and stale-context retirement passed");
    }

    // No credentials or provider traffic: cookie creation is deliberately delayed
    // until after the return page finishes, reproducing post-verification setup.
    fn assert_delayed_login_completion(origin: &str) {
        let ctx = egui::Context::default();
        for provider in [Provider::Youtube, Provider::Twitch] {
            let sessions = ProviderSessions::default();
            let context = sessions.context(provider.clone()).unwrap();
            context
                .open_with(|| {
                    Window::new_at(
                        &context.platform,
                        &provider,
                        &format!("{origin}/login"),
                        fixture_origin,
                        |_, value| value.ends_with("/returned"),
                        &ctx,
                    )
                })
                .unwrap();
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                wait_for(|| !window.view.is_loading());
                window.view.load_uri(&format!("{origin}/returned"));
                wait_for(|| window.completion.returned.get());
                assert!(!(window.window.is_visible()));
                assert!(window.open(), "hiding must keep the browser alive");
            }
            context.poll_session(&ctx);
            wait_for(|| !context.polling.get());
            context.poll_session(&ctx);
            assert!(context.window_open());
            assert!(!sessions.signed_in(&provider));
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                window
                    .completion
                    .hidden_since
                    .set(Some(Instant::now() - Duration::from_secs(31)));
            }
            context.poll_session(&ctx);
            {
                let window = context.window.borrow();
                let window = window.as_ref().unwrap();
                assert!(
                    window.window.is_visible(),
                    "an unsuccessful return remains accessible"
                );
                window.view.load_uri(&format!("{origin}/returned"));
                wait_for(|| {
                    window.completion.returned.get()
                        && window.completion.hidden_since.get().is_some()
                });
            }
            let (domain, names) = match provider {
                Provider::Youtube => (".youtube.com", vec!["SID", "HSID"]),
                Provider::Twitch => (".twitch.tv", vec!["auth-token"]),
            };
            let cookies: Vec<_> = names
                .into_iter()
                .map(|name| super::super::session::Cookie {
                    name: name.into(),
                    value: "synthetic-delayed-login".into(),
                    domain: domain.into(),
                    path: "/".into(),
                    expires: Some(super::super::session::now() + 3600),
                    secure: true,
                    http_only: true,
                    same_site: 1,
                })
                .collect();
            context.platform.restore(&cookies).unwrap();
            context.next_poll.set(None);
            wait_for(|| {
                context.poll_session(&ctx);
                !context.window_open()
            });
            assert!(sessions.signed_in(&provider));
            sessions.close();
            eprintln!("{}: hidden return stayed alive until fresh auth cookies; retry timeout and delayed sign-in passed", provider.label());
        }
    }
}
