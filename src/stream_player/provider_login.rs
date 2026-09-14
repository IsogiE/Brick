//! Personal provider sessions with OS-protected persistence of Brick viewing cookies.
mod session;
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

#[cfg(target_os = "windows")]
pub(super) use platform::Registration;

/// Owned by one signed-in Brick account. Retire it before changing accounts.
/// Provider state is personal; guild data and Brick credentials never enter it.
pub struct ProviderSessions {
    contexts: RefCell<[Option<Rc<Context>>; 2]>,
    closed: Cell<bool>,
    jars: [Rc<session::Jar>; 2],
}

/// Returning home does not mean the provider has finished setting its cookies.
/// Observe a fresh browser session after that return before closing its page.
#[derive(Default)]
pub(super) struct LoginCompletion {
    navigation: Cell<u64>,
    returned: Cell<bool>,
    observed: Cell<bool>,
    hidden_since: Cell<Option<std::time::Instant>>,
}

impl LoginCompletion {
    fn started(&self) {
        self.navigation.set(self.navigation.get().wrapping_add(1));
        self.returned.set(false);
        self.observed.set(false);
    }

    fn finished(&self, returned: bool) {
        self.returned.set(returned);
    }

    fn probe(self: &Rc<Self>) -> Option<(Rc<Self>, u64)> {
        self.returned
            .get()
            .then(|| (self.clone(), self.navigation.get()))
    }

    fn observe(&self, navigation: u64, signed_in: bool) {
        if self.returned.get() && self.navigation.get() == navigation {
            self.observed.set(signed_in);
        }
    }

    fn ready(&self) -> bool {
        self.returned.get() && self.observed.get()
    }

    fn hide_return(&self) {
        if self.hidden_since.get().is_none() {
            self.hidden_since.set(Some(std::time::Instant::now()));
        }
    }

    fn reveal(&self) -> bool {
        self.hidden_since.take().is_some()
    }

    fn reveal_due(&self, now: std::time::Instant) -> bool {
        self.hidden_since.get().is_some_and(|since| {
            now.saturating_duration_since(since) >= std::time::Duration::from_secs(30)
        })
    }
}

impl Default for ProviderSessions {
    fn default() -> Self {
        Self {
            contexts: RefCell::new([None, None]),
            closed: Cell::new(false),
            jars: [
                Rc::new(session::Jar::memory(Provider::Twitch)),
                Rc::new(session::Jar::memory(Provider::Youtube)),
            ],
        }
    }
}

impl ProviderSessions {
    pub fn for_account(account: &str) -> Self {
        Self {
            jars: [
                Rc::new(session::Jar::new(Provider::Twitch, account)),
                Rc::new(session::Jar::new(Provider::Youtube, account)),
            ],
            contexts: RefCell::new([None, None]),
            closed: Cell::new(false),
        }
    }

    pub fn signed_in(&self, provider: &Provider) -> bool {
        self.jars[index(provider)].signed_in()
    }
    pub fn ready(&self, provider: &Provider) -> bool {
        self.jars[index(provider)].ready()
    }
    pub fn error(&self) -> Option<String> {
        self.jars.iter().find_map(|jar| jar.error())
    }
    pub fn tick(&self, ctx: &egui::Context) {
        if self.closed.get() {
            return;
        }
        for jar in &self.jars {
            jar.tick();
        }
        let contexts = self.contexts.borrow().clone();
        if contexts.iter().any(Option::is_some) {
            super::pump_events();
            for context in contexts.iter().flatten() {
                context.poll_session(ctx);
            }
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
        if self.jars.iter().any(|jar| !jar.ready()) {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

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

    #[cfg(test)]
    pub fn session_started(&self, provider: &Provider) -> bool {
        self.contexts.borrow()[index(provider)]
            .as_ref()
            .is_some_and(|context| context.attempted())
    }

    #[cfg(test)]
    pub fn login_open_for(&self, provider: &Provider) -> bool {
        let context = self.contexts.borrow()[index(provider)].clone();
        context.is_some_and(|context| context.window_open())
    }

    #[cfg(all(test, target_os = "linux"))]
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
        // user-requested login retains its native session for this run. The
        // separate protected jar can restore a retired playback context.
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
        self.jars[index(provider)].clear();
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
        if !self.ready(&provider) {
            return Err("Your viewing login is still being restored. Please try again.".into());
        }
        if let Some(context) = self.contexts.borrow()[index(&provider)].clone() {
            return Ok(context);
        }
        let context = Rc::new(Context::new(
            provider.clone(),
            self.jars[index(&provider)].clone(),
        )?);
        // Cookie restoration may pump native events; hold no RefCell borrow.
        context.platform.restore(&context.jar.cookies())?;
        if self.closed.get() {
            context.retire();
            return Err("This provider session has ended.".into());
        }
        self.contexts.borrow_mut()[index(&provider)] = Some(context.clone());
        Ok(context)
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
    jar: Rc<session::Jar>,
    polling: Cell<bool>,
    next_poll: Cell<Option<std::time::Instant>>,
}

impl Context {
    fn new(provider: Provider, jar: Rc<session::Jar>) -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        let platform = platform::Context::new(&provider)?;
        #[cfg(target_os = "linux")]
        let platform = platform::Context::new()?;
        Ok(Self {
            provider,
            active: Cell::new(true),
            attempted: Cell::new(false),
            login_requested: Cell::new(false),
            window: RefCell::new(None),
            platform,
            jar,
            polling: Cell::new(false),
            next_poll: Cell::new(None),
        })
    }

    fn poll_session(self: &Rc<Self>, ctx: &egui::Context) {
        if !self.active() {
            return;
        }
        let completed = self
            .window
            .borrow()
            .as_ref()
            .is_some_and(|window| window.completion.ready());
        if completed {
            // Close on the UI tick, outside the native cookie callback.
            self.close_window();
        }
        let now = std::time::Instant::now();
        if let Some(window) = self.window.borrow().as_ref() {
            if window.completion.reveal_due(now) {
                // A failed or interrupted return must not leave an invisible
                // window indefinitely. Keep it available for retry or close.
                window.reveal();
            }
        }
        if self.polling.get() || self.next_poll.get().is_some_and(|at| at > now) {
            return;
        }
        self.polling.set(true);
        self.next_poll
            .set(Some(now + std::time::Duration::from_secs(2)));
        let weak = Rc::downgrade(self);
        let repaint = ctx.clone();
        let completion = self
            .window
            .borrow()
            .as_ref()
            .and_then(|window| window.completion.probe());
        self.platform.read_cookies(&self.provider, move |result| {
            if let Some(context) = weak.upgrade().filter(|context| context.active()) {
                context.polling.set(false);
                if let Ok(cookies) = result {
                    context.jar.observe(cookies);
                    if let Some((completion, navigation)) = completion {
                        completion.observe(navigation, context.jar.signed_in());
                    }
                    repaint.request_repaint();
                }
            }
        });
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

/// The fixed login entry points return to the provider's home page. This is
/// only a possible return; fresh cookie observation still gates window closure.
fn returned_to_provider(provider: &Provider, destination: &str) -> bool {
    if !allowed_document(provider, destination) {
        return false;
    }
    let Ok(url) = Url::parse(destination) else {
        return false;
    };
    url.path() == "/"
        && match provider {
            Provider::Youtube => url.host_str() == Some("www.youtube.com"),
            Provider::Twitch => url.host_str() == Some("www.twitch.tv"),
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
        Provider::Youtube => {
            matches!(
                url.host_str(),
                Some("accounts.youtube.com" | "www.youtube.com" | "www.google.com")
            ) || url.host_str().is_some_and(google_account_host)
        }
        Provider::Twitch => matches!(
            url.host_str(),
            // Twitch's own login scripts load a supporting document here.
            // It has no Brick bridge and remains isolated from YouTube.
            Some("www.twitch.tv" | "id.twitch.tv" | "passport.twitch.tv" | "k.twitchcdn.net")
        ),
    }
}

fn google_account_host(host: &str) -> bool {
    // Google documents regional accounts.google.[country] hosts for sign-in:
    // https://support.google.com/chrome/a/answer/6334001
    // Snapshot of https://www.google.com/supported_domains, 2026-09-14.
    // Match only the accounts host on an enumerated domain, never google.* or
    // arbitrary subdomains. Redirects still require HTTPS and no credentials.
    host.strip_prefix("accounts").is_some_and(|domain| {
        include_str!("provider_login/google-domains.txt")
            .lines()
            .any(|known| known == domain)
    })
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
    fn login_return_waits_for_a_fresh_successful_cookie_observation() {
        let completion = Rc::new(LoginCompletion::default());
        completion.started();
        assert!(completion.probe().is_none());
        completion.finished(true);
        assert!(!completion.ready());
        let (probe, navigation) = completion.probe().unwrap();
        probe.observe(navigation, false);
        assert!(!completion.ready());
        probe.observe(navigation, true);
        assert!(completion.ready());
    }

    #[test]
    fn old_navigation_and_old_window_probes_cannot_complete_a_new_login() {
        let old = Rc::new(LoginCompletion::default());
        old.started();
        old.finished(true);
        let (probe, navigation) = old.probe().unwrap();
        old.started();
        old.finished(true);
        probe.observe(navigation, true);
        assert!(!old.ready());
        let replacement = Rc::new(LoginCompletion::default());
        replacement.started();
        replacement.finished(true);
        let (probe, navigation) = old.probe().unwrap();
        probe.observe(navigation, true);
        assert!(old.ready());
        assert!(!replacement.ready());
    }

    #[test]
    fn hidden_return_can_be_retried_and_never_hides_indefinitely() {
        let completion = LoginCompletion::default();
        completion.hide_return();
        let since = completion.hidden_since.get().unwrap();
        assert!(!completion.reveal_due(since));
        assert!(completion.reveal_due(since + std::time::Duration::from_secs(30)));
        assert!(!completion.ready());
        assert!(completion.reveal());
        assert!(!completion.reveal());
        assert!(!completion.reveal_due(since + std::time::Duration::from_secs(60)));
    }

    #[test]
    fn only_the_matching_provider_home_page_ends_login() {
        for (provider, home) in [
            (Provider::Youtube, "https://www.youtube.com/"),
            (Provider::Twitch, "https://www.twitch.tv/"),
        ] {
            assert!(returned_to_provider(&provider, home));
            assert!(!returned_to_provider(&provider, start_url(&provider)));
            for destination in [
                "https://accounts.google.com/",
                "https://accounts.youtube.com/",
                "https://www.youtube.com/signin",
                "https://www.twitch.tv/login?error=fixture",
                "https://www.youtube.com.evil.test/",
                "https://www.twitch.tv.evil.test/",
                "https://token@www.youtube.com/",
                "https://www.twitch.tv:8443/",
                "http://www.youtube.com/",
                "about:blank",
            ] {
                assert!(
                    !returned_to_provider(&provider, destination),
                    "{destination}"
                );
            }
        }
        assert!(!returned_to_provider(
            &Provider::Youtube,
            "https://www.twitch.tv/"
        ));
        assert!(!returned_to_provider(
            &Provider::Twitch,
            "https://www.youtube.com/"
        ));
    }

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
    fn google_regional_account_redirects_keep_exact_host_boundaries() {
        for host in [
            "accounts.google.com",
            "accounts.google.nl",
            "accounts.google.co.uk",
            "accounts.google.com.au",
        ] {
            assert!(allowed_document(
                &Provider::Youtube,
                &format!("https://{host}/accounts/SetSID?fixture=1")
            ));
            assert!(!allowed_document(
                &Provider::Twitch,
                &format!("https://{host}/")
            ));
        }
        for host in [
            "accounts.google.evil",
            "accounts.google.nl.evil.test",
            "evil.accounts.google.nl",
            "accounts.google.com.attacker.test",
            "accountsgoogle.nl",
            "www.google.nl",
            "accounts.google.zip",
        ] {
            assert!(
                !allowed_document(
                    &Provider::Youtube,
                    &format!("https://{host}/accounts/SetSID")
                ),
                "{host}"
            );
        }
        assert!(!allowed_document(
            &Provider::Youtube,
            "http://accounts.google.nl/accounts/SetSID"
        ));
        assert!(!allowed_document(
            &Provider::Youtube,
            "https://user@accounts.google.nl/accounts/SetSID"
        ));
        assert!(!allowed_document(
            &Provider::Youtube,
            "https://accounts.google.nl:8443/accounts/SetSID"
        ));
    }

    #[test]
    fn twitch_login_support_document_does_not_expand_login_entry_points() {
        let support = "https://k.twitchcdn.net/fixture";
        assert!(allowed_document(&Provider::Twitch, support));
        assert!(!allowed_document(&Provider::Youtube, support));
        assert!(!login_destination(&Provider::Twitch, support));
        for destination in [
            "https://k.twitchcdn.net.evil.test/",
            "https://user@k.twitchcdn.net/",
            "http://k.twitchcdn.net/",
            "https://k.twitchcdn.net:8443/",
        ] {
            assert!(!allowed_document(&Provider::Twitch, destination));
        }
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
