use super::{allowed_document, start_url, title};
use crate::streams::Provider;
use eframe::egui;
use gtk::prelude::*;
use std::cell::RefCell;
use webkit2gtk::{
    DownloadExt, FileChooserRequestExt, NavigationPolicyDecisionExt, PermissionRequestExt,
    PolicyDecisionExt, SettingsExt, URIRequestExt, WebContextExt, WebViewExt,
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
}

impl Window {
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
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(560, 700);
        window.set_title(&title(provider, start_url(provider)));
        window.add(&view);
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
        view.connect_load_changed(move |view, event| {
            if event == webkit2gtk::LoadEvent::Committed {
                if let Some(window) = title_window.upgrade() {
                    window.set_title(&title(
                        &title_provider,
                        view.uri().as_deref().unwrap_or_default(),
                    ));
                }
            }
        });
        context.register(&view);
        view.load_uri(start);
        window.show_all();
        Ok(Self { window, view })
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
        let window = Window::new_at(
            &youtube.platform,
            &Provider::Youtube,
            &format!("{origin}/login"),
            fixture_origin,
            &egui::Context::default(),
        )
        .unwrap();
        wait_for(|| {
            window
                .view
                .title()
                .is_some_and(|title| title.starts_with('{'))
        });
        let login_state: serde_json::Value =
            serde_json::from_str(&window.view.title().unwrap()).unwrap();
        assert_eq!(login_state["ipc"], false);
        assert_eq!(login_state["bridge"], false);
        assert_eq!(login_state["opener"], false);
        *youtube.window.borrow_mut() = Some(window);
        youtube.attempted.set(true);
        assert!(sessions.login_open());
        youtube.close_window();
        assert!(!sessions.login_open());

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
        other_account.close();
        stopped.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        eprintln!("Native provider sessions: sandbox/private confirmed; shared POV state; no login IPC/scripts/opener; provider/account isolation; disconnect and stale-context retirement passed");
    }
}
