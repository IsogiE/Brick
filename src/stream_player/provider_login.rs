//! Personal, memory-only provider sessions. No browser cookie import or export.
use crate::streams::Provider;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use url::Url;

#[cfg(target_os = "linux")]
#[path = "provider_login/linux.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "provider_login/windows.rs"]
mod platform;

/// Owned by one signed-in Brick account. Retire it before changing accounts.
/// Provider state is personal; guild data and Brick credentials never enter it.
#[derive(Default)]
pub struct ProviderSessions {
    contexts: RefCell<[Option<Rc<Context>>; 2]>,
    closed: Cell<bool>,
}

impl ProviderSessions {
    /// Open personal viewing sign-in without loading a guild page or video.
    /// A successful open does not establish whether the provider accepted login.
    pub fn open_login(
        &self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        provider: Provider,
    ) -> Result<(), String> {
        if self.closed.get() {
            return Err("This provider session has ended.".into());
        }
        let owner = frame
            .window_handle()
            .map_err(|_| "The provider sign-in window could not find Brick.")?;
        match owner.as_raw() {
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_) => {
                super::initialize_gtk()?;
            }
            #[cfg(target_os = "windows")]
            RawWindowHandle::Win32(_) => (),
            _ => return Err("Provider sign-in needs Brick's desktop window.".into()),
        }
        let context = self.context(provider)?;
        let result = context.open_with(|| {
            platform::Window::from_frame(&context.platform, &context.provider, frame, ctx)
        });
        if result.is_err() {
            // Failed first opens must not retain a newly allocated anonymous
            // context/keeper. Existing media or explicit sessions remain owned.
            self.release_unused(&context);
        }
        result
    }

    pub fn session_started(&self, provider: &Provider) -> bool {
        self.contexts.borrow()[index(provider)]
            .as_ref()
            .is_some_and(|context| context.attempted())
    }

    pub fn login_open_for(&self, provider: &Provider) -> bool {
        let context = self.contexts.borrow()[index(provider)].clone();
        context.is_some_and(|context| context.window_open())
    }

    pub fn close_login(&self, provider: &Provider) {
        let context = self.contexts.borrow()[index(provider)].clone();
        if let Some(context) = context {
            context.close_window();
        }
    }

    #[cfg(test)]
    pub(crate) fn ended_for_test(&self) -> bool {
        self.closed.get()
    }

    pub fn login_open(&self) -> bool {
        let contexts = self.contexts.borrow().clone();
        contexts
            .iter()
            .flatten()
            .any(|context| context.window_open())
    }

    pub(super) fn release_unused(&self, context: &Rc<Context>) {
        // Anonymous playback keeps the existing prompt resource teardown. A
        // user-requested login retains its memory-only session for this run.
        if !context.attempted() && Rc::strong_count(context) == 2 {
            let removed = {
                let mut contexts = self.contexts.borrow_mut();
                let slot = &mut contexts[index(&context.provider)];
                if slot
                    .as_ref()
                    .is_some_and(|saved| Rc::ptr_eq(saved, context))
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(context) = removed {
                context.retire();
            }
        }
    }
    pub fn disconnect(&self, provider: &Provider) {
        let context = self.contexts.borrow_mut()[index(provider)].take();
        if let Some(context) = context {
            context.retire();
        }
    }

    pub fn close(&self) {
        self.closed.set(true);
        let contexts = std::mem::take(&mut *self.contexts.borrow_mut());
        for context in contexts.into_iter().flatten() {
            context.retire();
        }
    }

    pub(super) fn context(&self, provider: Provider) -> Result<Rc<Context>, String> {
        if self.closed.get() {
            return Err("This provider session has ended.".into());
        }
        let mut contexts = self.contexts.borrow_mut();
        let slot = &mut contexts[index(&provider)];
        if slot.is_none() {
            *slot = Some(Rc::new(Context::new(provider)?));
        }
        Ok(Rc::clone(slot.as_ref().expect("Context was initialized")))
    }
}

impl Drop for ProviderSessions {
    fn drop(&mut self) {
        self.close();
    }
}

fn index(provider: &Provider) -> usize {
    match provider {
        Provider::Twitch => 0,
        Provider::Youtube => 1,
    }
}

pub(super) struct Context {
    pub provider: Provider,
    active: Cell<bool>,
    attempted: Cell<bool>,
    login_requested: Cell<bool>,
    window: RefCell<Option<platform::Window>>,
    pub platform: platform::Context,
}

impl Context {
    fn new(provider: Provider) -> Result<Self, String> {
        Ok(Self {
            provider,
            active: Cell::new(true),
            attempted: Cell::new(false),
            login_requested: Cell::new(false),
            window: RefCell::new(None),
            platform: platform::Context::new()?,
        })
    }

    pub fn active(&self) -> bool {
        self.active.get()
    }
    pub fn attempted(&self) -> bool {
        self.attempted.get()
    }

    pub fn request_login(&self, destination: &str, user_gesture: bool, has_auth: bool) -> bool {
        if !self.active.get()
            || !user_gesture
            || has_auth
            || !login_destination(&self.provider, destination)
        {
            return false;
        }
        // One bounded pending action, consumed by native UI after this browser
        // callback returns. The requested URL is never retained or followed.
        self.login_requested.set(true);
        true
    }

    pub fn take_login_request(&self) -> bool {
        self.login_requested.replace(false) && self.active.get()
    }

    pub fn open(&self, player: &wry::WebView, ctx: &egui::Context) -> Result<(), String> {
        self.open_with(|| platform::Window::new(&self.platform, &self.provider, player, ctx))
    }

    fn open_with(
        &self,
        create: impl FnOnce() -> Result<platform::Window, String>,
    ) -> Result<(), String> {
        if !self.active.get() {
            return Err("This provider session has ended.".into());
        }
        if let Some(window) = self.window.borrow().as_ref().filter(|w| w.open()) {
            window.present();
            return Ok(());
        }
        self.close_window();
        let window = create()?;
        *self.window.borrow_mut() = Some(window);
        self.attempted.set(true);
        Ok(())
    }

    pub fn window_open(&self) -> bool {
        let closed = {
            let mut window = self.window.borrow_mut();
            if window.as_ref().is_some_and(|window| !window.open()) {
                window.take()
            } else {
                None
            }
        };
        // Native controller teardown may pump messages. Do not retain a RefMut
        // while dropping a window and entering those platform callbacks.
        drop(closed);
        self.window.borrow().is_some()
    }

    pub fn close_window(&self) {
        let window = self.window.borrow_mut().take();
        drop(window);
    }

    fn retire(&self) {
        if !self.active.replace(false) {
            return;
        }
        self.login_requested.set(false);
        self.close_window();
        self.platform.retire();
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.retire();
    }
}

pub(super) fn provider(url: &Url) -> Option<Provider> {
    match url.path().rsplit('/').next()? {
        "youtube" => Some(Provider::Youtube),
        "twitch" => Some(Provider::Twitch),
        _ => None,
    }
}

pub(super) fn watch_requests(
    view: &wry::WebView,
    context: &Rc<Context>,
    ctx: &egui::Context,
) -> Result<(), String> {
    platform::watch_requests(view, Rc::downgrade(context), ctx)
}

pub(super) fn login_destination(provider: &Provider, destination: &str) -> bool {
    if !allowed_document(provider, destination) {
        return false;
    }
    let Ok(url) = Url::parse(destination) else {
        return false;
    };
    match provider {
        Provider::Youtube => {
            matches!(
                url.host_str(),
                Some("accounts.google.com" | "accounts.youtube.com")
            ) || (url.host_str() == Some("www.youtube.com")
                && url.path().trim_end_matches('/') == "/signin")
        }
        Provider::Twitch => {
            matches!(url.host_str(), Some("id.twitch.tv" | "passport.twitch.tv"))
                || (url.host_str() == Some("www.twitch.tv")
                    && url.path().trim_end_matches('/') == "/login")
        }
    }
}

fn start_url(provider: &Provider) -> &'static str {
    match provider {
        Provider::Youtube => "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F",
        Provider::Twitch => "https://www.twitch.tv/login",
    }
}

/// Account documents never navigate into Brick or arbitrary destinations.
/// Subresource requests retain the browser's normal origin/TLS protections.
fn allowed_document(provider: &Provider, destination: &str) -> bool {
    let Ok(url) = Url::parse(destination) else {
        return false;
    };
    if destination.len() > 8192
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    match provider {
        Provider::Youtube => matches!(
            url.host_str(),
            Some(
                "accounts.google.com"
                    | "accounts.youtube.com"
                    | "www.youtube.com"
                    | "www.google.com"
            )
        ),
        Provider::Twitch => matches!(
            url.host_str(),
            Some("www.twitch.tv" | "id.twitch.tv" | "passport.twitch.tv")
        ),
    }
}

fn title(provider: &Provider, destination: &str) -> String {
    let origin = Url::parse(destination)
        .ok()
        .filter(|_| allowed_document(provider, destination))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "Connecting…".into());
    format!("{} sign-in — {}", provider.label(), origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_account_holder_cannot_create_new_provider_contexts() {
        // This must fail before any native browser/GTK initialization. An Rc
        // held by an old account operation cannot resurrect its viewer login.
        let sessions = Rc::new(ProviderSessions::default());
        let stale = Rc::clone(&sessions);
        sessions.close();
        stale.disconnect(&Provider::Youtube);
        for provider in [Provider::Youtube, Provider::Twitch] {
            assert!(stale.context(provider).is_err());
        }
        assert!(!stale.login_open());
    }

    #[test]
    fn login_documents_are_exact_provider_https_origins() {
        for provider in [Provider::Twitch, Provider::Youtube] {
            assert!(allowed_document(&provider, start_url(&provider)));
            for value in [
                "http://www.youtube.com/",
                "https://accounts.google.com.evil.test/",
                "https://accounts.google.com@evil.test/",
                "https://token@accounts.google.com/",
                "https://www.youtube.com:8443/",
                "file:///tmp/account",
                "javascript:alert(1)",
                "https://brick.lusaggo.com/v1/guilds",
                "about:blank",
                "data:text/html,hello",
            ] {
                assert!(!allowed_document(&provider, value), "{value}");
            }
        }
        assert!(!allowed_document(
            &Provider::Youtube,
            start_url(&Provider::Twitch)
        ));
        assert!(!allowed_document(
            &Provider::Twitch,
            start_url(&Provider::Youtube)
        ));
    }

    #[test]
    fn native_title_never_displays_query_credentials_or_provider_document_title() {
        let value = title(
            &Provider::Youtube,
            "https://accounts.google.com/ServiceLogin?secret=fixture#private",
        );
        assert_eq!(value, "YouTube sign-in — https://accounts.google.com");
        assert!(!title(&Provider::Youtube, "https://evil.test/?secret=fixture").contains("evil"));
    }

    #[test]
    fn only_provider_account_destinations_request_native_login() {
        for provider in [Provider::Youtube, Provider::Twitch] {
            assert!(login_destination(&provider, start_url(&provider)));
            for value in [
                "https://www.youtube.com/watch?v=fixture",
                "https://www.twitch.tv/streamer",
                "https://accounts.google.com.evil.test/",
                "https://token@accounts.google.com/",
                "https://accounts.google.com:8443/",
                "javascript:alert(1)",
                "https://brick.lusaggo.com/v1/login",
            ] {
                assert!(!login_destination(&provider, value), "{value}");
            }
        }
        assert!(!login_destination(
            &Provider::Twitch,
            start_url(&Provider::Youtube)
        ));
        assert!(!login_destination(
            &Provider::Youtube,
            start_url(&Provider::Twitch)
        ));
    }
}
