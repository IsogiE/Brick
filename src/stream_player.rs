//! The native app owns navigation and authentication; this child only renders media.

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::Cell,
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

mod capture;
mod fullscreen;
pub use capture::FrameCapture;

const WRAPPER_LOAD_TIMEOUT: Duration = Duration::from_secs(25);
const COMMAND_RETRY_AFTER: Duration = Duration::from_secs(4);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const VISIBLE_STATE_INTERVAL: Duration = Duration::from_millis(100);
const HIDDEN_STATE_INTERVAL: Duration = Duration::from_millis(500);
const STATE_POLL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
enum StatePoll {
    Waiting(Duration),
    Pending,
    Started,
}

fn state_poll_interval(visible: bool) -> Duration {
    if visible {
        VISIBLE_STATE_INTERVAL
    } else {
        HIDDEN_STATE_INTERVAL
    }
}

fn schedule_state_poll(ctx: &egui::Context, wait: Duration) {
    // egui subtracts its predicted frame time from every repaint delay. These
    // are real SDK deadlines, so compensate that subtraction; otherwise the
    // final fraction of each 100 ms interval repeatedly requests immediate frames.
    let frame_time =
        ctx.input(|input| Duration::try_from_secs_f32(input.predicted_dt).unwrap_or_default());
    ctx.request_repaint_after(wait.saturating_add(frame_time));
}

fn begin_state_poll(
    last: &mut Option<Instant>,
    pending: &AtomicBool,
    now: Instant,
    visible: bool,
) -> StatePoll {
    // A slow renderer keeps its one outstanding request. Navigation replaces
    // this document's flag, and a callback or submission error releases it.
    if pending.load(Ordering::Relaxed) {
        return StatePoll::Pending;
    }
    let interval = state_poll_interval(visible);
    if let Some(wait) = last.and_then(|at| interval.checked_sub(now.saturating_duration_since(at)))
    {
        if !wait.is_zero() {
            return StatePoll::Waiting(wait);
        }
    }
    if pending
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return StatePoll::Pending;
    }
    *last = Some(now);
    StatePoll::Started
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct PlaybackState {
    pub ready: bool,
    pub seconds: f64,
    pub playing: bool,
    pub buffering: bool,
    /// Provider Pause event evidence; never substitutes for settled SDK state.
    pub pause_intent: bool,
    /// Provider Play opposing the wrapper's paused intent; not a settled ACK.
    pub play_intent: bool,
    #[serde(skip)]
    pub seeking: Option<f64>,
    /// A native action awaiting a fresh provider acknowledgement.
    #[serde(skip)]
    pub playback_intent: Option<bool>,
    #[serde(skip)]
    polled_at: Option<Instant>,
    #[serde(skip)]
    poll_finished_at: Option<Instant>,
}
impl PlaybackState {
    pub fn is_fresh(&self) -> bool {
        self.polled_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
    }

    /// The sample's SDK request must have started after the native action.
    pub fn is_fresh_since(&self, since: Instant) -> bool {
        self.is_fresh() && self.polled_at.is_some_and(|at| at >= since)
    }

    /// A new provider gesture needs a poll strictly later than the command
    /// barrier. Equal clock ticks cannot establish which action happened first.
    pub fn is_fresh_after(&self, since: Instant) -> bool {
        self.is_fresh() && self.polled_at.is_some_and(|at| at > since)
    }

    /// The SDK read happened between the native request and its callback.
    /// This brackets an observation; it is not a decoded frame timestamp.
    pub(crate) fn observation_window(&self) -> Option<[Instant; 2]> {
        let start = self.polled_at?;
        let end = self.poll_finished_at?;
        (start <= end).then_some([start, end])
    }

    #[cfg(test)]
    pub(crate) fn mark_polled_now(&mut self) {
        self.mark_polled_at(Instant::now());
    }

    #[cfg(test)]
    pub(crate) fn mark_polled_at(&mut self, at: Instant) {
        self.polled_at = Some(at);
        self.poll_finished_at = Some(at);
    }

    #[cfg(test)]
    pub(crate) fn mark_polled_between(&mut self, start: Instant, end: Instant) {
        self.polled_at = Some(start);
        self.poll_finished_at = Some(end);
    }
}
#[derive(Clone, Copy)]
pub enum PlaybackCommand {
    Seek(f64),
    SeekPaused(f64),
    Play,
    Pause,
}

pub struct StreamPlayer {
    webview: Option<WebView>,
    allowed_url: Arc<Mutex<String>>,
    bounds: [i32; 4],
    visible: Cell<bool>,
    loaded: Arc<AtomicBool>,
    created: Instant,
    failure: Arc<Mutex<Option<String>>>,
    preferences: Option<PreferenceBridge>,
    playback_state: Arc<Mutex<PlaybackState>>,
    last_state_poll: Option<Instant>,
    state_pending: Arc<AtomicBool>,
    queued_command: Option<PlaybackCommand>,
    ready_since: Option<Instant>,
    pending_seek: Option<(f64, bool, Instant)>,
    pending_playback: Option<(bool, Instant)>,
    command_retried: bool,
    capture: capture::Controller,
    fullscreen: fullscreen::Controller,
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
        let paused = player_url
            .query_pairs()
            .any(|(key, value)| key == "paused" && value == "1");
        let initial_seek = player_url
            .query_pairs()
            .find(|(key, _)| key == "at")
            .and_then(|(_, value)| value.parse::<f64>().ok());
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
        let allowed_url = Arc::new(Mutex::new(player_url.to_string()));
        let wrapper_url = Arc::clone(&allowed_url);
        let browser_ctx = ctx.clone();
        let preferences = Some(PreferenceBridge::new(
            player_url.as_str(),
            preferences.unwrap_or_else(Preferences::in_memory),
        ));
        let builder = WebViewBuilder::new()
            .with_bounds(wry_bounds(bounds))
            .with_incognito(true)
            .with_autoplay(true)
            .with_devtools(false)
            .with_clipboard(false)
            .with_hotkeys_zoom(false)
            // Opening a stream is a native user action. Give the embedded
            // player's keyboard controls focus when its child is created.
            .with_focused(true)
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
                if matches!(event, PageLoadEvent::Finished)
                    && wrapper_url.lock().is_ok_and(|url| destination == *url)
                {
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
            let allowed = Arc::clone(&allowed_url);
            let ctx = ctx.clone();
            builder.with_navigation_handler(move |destination| {
                if allowed.lock().is_ok_and(|url| destination == *url) {
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
            allowed_url,
            bounds,
            visible: Cell::new(true),
            loaded,
            created,
            failure,
            preferences,
            playback_state: Arc::new(Mutex::new(PlaybackState::default())),
            last_state_poll: None,
            state_pending: Arc::new(AtomicBool::new(false)),
            queued_command: Some(if paused {
                PlaybackCommand::Pause
            } else {
                PlaybackCommand::Play
            }),
            ready_since: None,
            pending_seek: initial_seek.map(|target| (target, !paused, created)),
            pending_playback: initial_seek.map(|_| (!paused, created)),
            command_retried: false,
            capture: capture::Controller::default(),
            fullscreen: fullscreen::Controller::new(ctx),
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
            protect_linux_navigation(webview, Arc::clone(&player.allowed_url), ctx)?;
            watch_linux_failures(
                webview,
                Arc::clone(&player.allowed_url),
                Arc::clone(&player.failure),
            );
            if let Some(preferences) = &player.preferences {
                player.preference_handler = attach_linux_preferences(webview, preferences.clone());
            }
        }
        #[cfg(target_os = "windows")]
        protect_windows_permissions(webview)?;
        player.fullscreen.attach(webview)?;

        #[cfg(target_os = "linux")]
        if ctx.input(|input| input.viewport().focused.unwrap_or(false)) {
            // Wry's focused option focuses the GTK widget, but its X11 media
            // container remains a separate native window. Activate that owned
            // container when Brick is already active so WebKit can start media
            // in response to opening a stream. Never activate an inactive app.
            webview
                .focus_parent()
                .map_err(|_| "The stream player could not receive focus.".to_string())?;
        }

        // Wry uses WebKit's load_request / WebView2 NavigateWithWebResourceRequest:
        // these headers belong to this request, not subsequent iframe resources.
        webview
            .load_url_with_headers(player_url.as_str(), headers)
            .map_err(|_| "The stream player could not load this stream.".to_string())?;
        Ok(player)
    }

    /// Keep one media process when moving between provider POVs. Every new
    /// wrapper still receives its own authenticated, strictly validated request.
    pub fn can_reuse_for_replay(&self) -> bool {
        self.preferences.is_some()
            && self.allowed_url.lock().is_ok_and(|url| {
                Url::parse(&url).is_ok_and(|url| {
                    matches!(url.path().rsplit('/').next(), Some("youtube" | "twitch"))
                })
            })
    }

    pub fn load_replay(
        &mut self,
        ctx: &egui::Context,
        value: &str,
        token: &str,
    ) -> Result<(), String> {
        let url = validated_player_url(value)?;
        if !self.can_reuse_for_replay() {
            return Err("This recording needs a new player.".into());
        }
        let target = url
            .query_pairs()
            .find(|(key, _)| key == "at")
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .ok_or("The recording has no replay position.")?;
        let resume = !url
            .query_pairs()
            .any(|(key, value)| key == "paused" && value == "1");
        let identity = |url: &Url| {
            let recording = url
                .query_pairs()
                .find(|(key, _)| key == "recording")
                .map(|(_, value)| value.into_owned());
            let broadcast = url
                .query_pairs()
                .find(|(key, _)| key == "broadcast")
                .map(|(_, value)| value.into_owned());
            (recording, broadcast)
        };
        let same_video = self
            .allowed_url
            .lock()
            .ok()
            .and_then(|url| Url::parse(&url).ok())
            .is_some_and(|previous| {
                previous.path().rsplit('/').next() == url.path().rsplit('/').next()
                    && identity(&previous).1.is_some()
                    && identity(&previous) == identity(&url)
            });
        if same_video {
            return self.command(if resume {
                PlaybackCommand::Seek(target)
            } else {
                PlaybackCommand::SeekPaused(target)
            });
        }
        self.exit_fullscreen();
        if token.is_empty() || token.len() > 2048 {
            return Err("Sign in to Discord again to watch this stream.".into());
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "The Discord session cannot open this stream.")?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        if !self
            .preferences
            .as_ref()
            .is_some_and(|bridge| bridge.retarget(url.as_str()))
        {
            return Err("The player could not protect recording preferences.".into());
        }
        self.capture.cancel();
        *self
            .allowed_url
            .lock()
            .map_err(|_| "The player could not change recording.")? = url.to_string();
        self.created = Instant::now();
        self.loaded.store(false, Ordering::Relaxed);
        // Old document callbacks retain the old state, so they cannot overwrite
        // the selected POV's position or acknowledge its pending seek.
        self.playback_state = Arc::new(Mutex::new(PlaybackState::default()));
        self.state_pending = Arc::new(AtomicBool::new(false));
        self.last_state_poll = None;
        self.ready_since = None;
        self.pending_seek = Some((target, resume, self.created));
        self.pending_playback = Some((resume, self.created));
        self.command_retried = false;
        self.queued_command = Some(if resume {
            PlaybackCommand::Play
        } else {
            PlaybackCommand::Pause
        });
        if let Ok(mut failure) = self.failure.lock() {
            *failure = None;
        }
        let webview = self.webview.as_ref().ok_or("The player has closed.")?;
        #[cfg(target_os = "linux")]
        if ctx.input(|input| input.viewport().focused.unwrap_or(false)) {
            webview
                .focus_parent()
                .map_err(|_| "The stream player could not receive focus.")?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = ctx;
        webview
            .load_url_with_headers(url.as_str(), headers)
            .map_err(|_| "The player could not change recording.".into())
    }

    pub fn set_visible(&self, visible: bool) {
        if !visible {
            self.capture.cancel();
            self.exit_fullscreen();
        }
        self.fullscreen.set_enabled(visible);
        if self.visible.get() == visible {
            return;
        }
        if let Some(webview) = &self.webview {
            if webview.set_visible(visible).is_ok() {
                self.visible.set(visible);
            }
        }
    }
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.active()
    }

    #[cfg(test)]
    pub fn enter_fullscreen(&self) {
        if self.visible.get() {
            self.capture.cancel();
            self.fullscreen.enter();
        }
    }

    pub fn exit_fullscreen(&self) {
        if self.fullscreen.active() {
            self.capture.cancel();
            self.fullscreen.exit(self.webview.as_ref());
        }
    }
    pub fn playback_state(&self) -> PlaybackState {
        let mut state = self
            .playback_state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        state.seeking = self.pending_seek.map(|(target, _, _)| target);
        state.playback_intent = self.pending_playback.map(|(playing, _)| playing);
        state
    }
    pub fn poll_playback(&mut self, ctx: &egui::Context) {
        let visible =
            self.visible.get() && ctx.input(|input| input.viewport().visible().unwrap_or(true));
        if let Some(view) = &self.webview {
            self.capture
                .tick(view, &self.playback_state(), visible, ctx);
        }
        if newer_provider_pause(
            &self.playback_state(),
            self.pending_playback,
            self.pending_seek.is_some(),
        ) {
            // The user's provider Pause supersedes an older native Play. This
            // cancels that action; it does not acknowledge a paused SDK frame.
            self.pending_playback = None;
            self.queued_command = None;
            self.command_retried = false;
        }
        // Forward the user's native replay action when the provider is ready.
        // This also preserves a seek made while the first document is loading.
        if self.playback_state().ready {
            let ready_since = self.ready_since.get_or_insert_with(Instant::now);
            // WebKit reports SDK readiness before initial media setup finishes.
            // Forward once after that setup, preserving any newer native action.
            if ready_since.elapsed() >= Duration::from_millis(500) {
                if let Some(command) = self.queued_command.take() {
                    if let Err(error) = self.command(command) {
                        if let Ok(mut failure) = self.failure.lock() {
                            *failure = Some(error);
                        }
                    }
                }
            }
        }
        if let Some((target, resume, requested)) = self.pending_seek {
            let state = self.playback_state();
            if seek_acknowledged(target, resume, requested, &state) {
                self.pending_seek = None;
            }
        }
        if let Some((playing, requested)) = self.pending_playback {
            let state = self.playback_state();
            if self.pending_seek.is_none() && playback_acknowledged(playing, requested, &state) {
                self.pending_playback = None;
            }
        }
        if let Some(command) = self.recover_pending_command() {
            if let (Some(webview), Ok(call)) = (&self.webview, playback_call(command)) {
                let _ = webview
                    .evaluate_script(&format!("if(window.brickMedia) window.brickMedia.{call}"));
            }
        }
        // Wry queues scripts during navigation without retaining their callbacks.
        // Wait for this document to load before requesting a result.
        if !self.loaded.load(Ordering::Relaxed) {
            return;
        }
        let polled_at = Instant::now();
        match begin_state_poll(
            &mut self.last_state_poll,
            &self.state_pending,
            polled_at,
            visible,
        ) {
            StatePoll::Waiting(wait) => {
                schedule_state_poll(ctx, wait);
                return;
            }
            StatePoll::Pending => return,
            StatePoll::Started => (),
        }
        // Windows need not repaint at the video frame rate. Schedule the next
        // bounded state read explicitly, including while paused so a provider
        // Play gesture is noticed promptly. This reads SDK state, not frames.
        schedule_state_poll(ctx, state_poll_interval(visible));
        let state = self.playback_state.clone();
        let pending = self.state_pending.clone();
        let ctx = ctx.clone();
        let Some(webview) = &self.webview else {
            self.state_pending.store(false, Ordering::Relaxed);
            return;
        };
        if webview
            .evaluate_script_with_callback(
                "JSON.stringify(window.brickMedia ? window.brickMedia.state() : null)",
                move |value| {
                    let poll_finished_at = Instant::now();
                    pending.store(false, Ordering::Relaxed);
                    if value.len() > 4096 {
                        return;
                    }
                    let decoded = serde_json::from_str::<String>(&value).unwrap_or(value);
                    if let Ok(mut next) = serde_json::from_str::<PlaybackState>(&decoded) {
                        next.polled_at = Some(polled_at);
                        next.poll_finished_at = Some(poll_finished_at);
                        if next.seconds.is_finite() && (0.0..=604800.0).contains(&next.seconds) {
                            if publish_playback_state(&state, next) {
                                ctx.request_repaint();
                            }
                        }
                    }
                },
            )
            .is_err()
        {
            self.state_pending.store(false, Ordering::Relaxed);
        }
    }
    pub fn command(&mut self, command: PlaybackCommand) -> Result<(), String> {
        let call = playback_call(command)?;
        let webview = self.webview.as_ref().ok_or("The player has closed.")?;
        self.capture.cancel();
        self.command_retried = false;
        // A new native action always supersedes startup or a seek queued while
        // loading, even if the provider became ready between this frame's ticks.
        self.queued_command = None;
        self.pending_playback = Some((
            matches!(command, PlaybackCommand::Play | PlaybackCommand::Seek(_)),
            Instant::now(),
        ));
        match command {
            PlaybackCommand::Seek(target) => {
                self.pending_seek = Some((target, true, Instant::now()))
            }
            PlaybackCommand::SeekPaused(target) => {
                self.pending_seek = Some((target, false, Instant::now()))
            }
            PlaybackCommand::Pause | PlaybackCommand::Play => {
                if let Some((target, _, _)) = self.pending_seek {
                    self.pending_seek = Some((
                        target,
                        matches!(command, PlaybackCommand::Play),
                        Instant::now(),
                    ));
                }
            }
        }
        if !self.playback_state().ready {
            // Pause/play while loading must keep a newer requested position.
            self.queued_command = Some(match self.pending_seek {
                Some((target, true, _)) => PlaybackCommand::Seek(target),
                Some((target, false, _)) => PlaybackCommand::SeekPaused(target),
                None => command,
            });
            return Ok(());
        }
        webview
            .evaluate_script(&format!("if(window.brickMedia) window.brickMedia.{call}"))
            .map_err(|_| "The player couldn't change playback.".into())
    }

    pub fn set_bounds(&mut self, rect: egui::Rect, pixels_per_point: f32) -> Result<(), String> {
        let bounds = physical_bounds(rect, pixels_per_point)?;
        if bounds != self.bounds {
            self.capture.cancel();
            self.webview
                .as_ref()
                .ok_or_else(|| "The stream player has closed.".to_string())?
                .set_bounds(wry_bounds(bounds))
                .map_err(|_| "The stream player could not resize.".to_string())?;
            self.bounds = bounds;
        }
        Ok(())
    }

    /// A command timeout is recoverable and must not destroy a loaded player.
    /// Only failures of the native wrapper itself replace the media view.
    pub fn failure(&self) -> Option<String> {
        if let Some(message) = self.failure.lock().ok().and_then(|failure| failure.clone()) {
            return Some(message);
        }
        if self.state_pending.load(Ordering::Relaxed)
            && self
                .last_state_poll
                .is_some_and(|at| at.elapsed() >= STATE_POLL_TIMEOUT)
        {
            // A hung renderer must not strand the controls forever. Let the
            // existing bounded wrapper recovery handle failure; never queue
            // another evaluation behind its unfinished request.
            return Some("The stream player stopped responding.".into());
        }
        if !self.loaded.load(Ordering::Relaxed) && self.created.elapsed() >= WRAPPER_LOAD_TIMEOUT {
            return Some("The stream player took too long to load. Please try again.".to_string());
        }
        if self.pending_seek.is_some()
            && !self.playback_state().ready
            && self.created.elapsed() >= WRAPPER_LOAD_TIMEOUT
        {
            return Some(
                "The recording player took too long to become ready. Please try again.".into(),
            );
        }
        None
    }

    fn recover_pending_command(&mut self) -> Option<PlaybackCommand> {
        let (playing, requested) = self.pending_playback?;
        let elapsed = requested.elapsed();
        if elapsed >= COMMAND_TIMEOUT {
            // Keep the decoded video and its real SDK position. An unfulfilled
            // request is never acknowledged by pretending it reached its target.
            self.pending_seek = None;
            self.pending_playback = None;
            self.queued_command = None;
            self.capture.cancel();
            return None;
        }
        let state = self.playback_state();
        if self.command_retried
            || elapsed < COMMAND_RETRY_AFTER
            || !self.visible.get()
            || !self.loaded.load(Ordering::Relaxed)
            || !state.ready
            || !state.is_fresh_since(requested)
            || state.buffering
        {
            return None;
        }
        // A provider can ignore a seek during its own startup or state change.
        // Retry once in the same iframe; never reload or spawn another decoder.
        self.command_retried = true;
        Some(match self.pending_seek {
            Some((target, true, _)) => PlaybackCommand::Seek(target),
            Some((target, false, _)) => PlaybackCommand::SeekPaused(target),
            None if playing => PlaybackCommand::Play,
            None => PlaybackCommand::Pause,
        })
    }

    /// Opt-in capture of this media child, with a fresh SDK timing bracket.
    /// Continue polling playback while it runs, then consume the result once.
    pub fn request_frame_capture(&self, ctx: &egui::Context) -> bool {
        self.webview.as_ref().is_some_and(|view| {
            self.capture.request(
                view,
                &self.playback_state(),
                self.visible.get() && ctx.input(|input| input.viewport().visible().unwrap_or(true)),
                self.bounds,
                ctx,
            )
        })
    }

    pub fn take_frame_capture(&self) -> Option<Result<FrameCapture, String>> {
        self.capture.take()
    }

    pub fn frame_capture_pending(&self) -> bool {
        self.capture.pending()
    }

    pub fn cancel_frame_capture(&self) {
        self.capture.cancel();
    }

    /// Compare with FrameCapture.generation before accepting downstream OCR.
    pub fn frame_capture_generation(&self) -> u64 {
        self.capture.generation()
    }

    #[cfg(all(test, target_os = "linux"))]
    pub fn diagnostic_html(&self) {
        use webkit2gtk::WebViewExt;
        use wry::WebViewExtUnix;
        if let Some(webview) = &self.webview {
            webview.webview().load_html("<!doctype html><html><body style='margin:0;background:#ff00ff;color:white;font:36px sans-serif'><div style='height:100px;background:#008080'>Brick native rendering diagnostic</div><p>Local HTML draws without provider requests.</p></body></html>", None);
        }
    }

    #[cfg(test)]
    pub fn diagnostic_provider_command(&self, command: PlaybackCommand) -> Result<(), String> {
        let (twitch, youtube) = match command {
            PlaybackCommand::Play => (3, "playVideo"),
            PlaybackCommand::Pause => (2, "pauseVideo"),
            _ => return Err("The diagnostic provider action must be Play or Pause".into()),
        };
        // Exercise the provider SDK's actual command/event path. Calling
        // brickMedia here would replace native intent and hide provider races.
        let script = format!(
            r#"(() => {{
                const frame = document.querySelector('iframe');
                if (!frame) return;
                const origin = new URL(frame.src).origin;
                if (origin === 'https://player.twitch.tv') {{
                    frame.contentWindow.postMessage({{namespace:'twitch-embed-player-proxy',eventName:{twitch},params:null}}, origin);
                }} else if (origin === 'https://www.youtube.com' || origin === 'https://www.youtube-nocookie.com') {{
                    frame.contentWindow.postMessage(JSON.stringify({{event:'command',func:'{youtube}',args:[]}}), origin);
                }}
            }})()"#
        );
        self.webview
            .as_ref()
            .ok_or("The diagnostic player closed")?
            .evaluate_script(&script)
            .map_err(|_| "The diagnostic provider command failed".to_string())
    }

    #[cfg(test)]
    pub fn diagnostic_player_id(&self) -> Option<String> {
        self.webview.as_ref().map(|view| view.id().to_string())
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
            eprintln!(
                "Stream GTK allocation {:?}, mapped={}, drawable={}, loading={}, progress={}, parent={:?}",
                view.allocation(),
                view.is_mapped(),
                view.is_drawable(),
                view.is_loading(),
                view.estimated_load_progress(),
                view.parent().map(|parent| parent.allocation())
            );
            let _ = webview.evaluate_script_with_callback(
                "JSON.stringify({visibility:document.visibilityState,focus:document.hasFocus()})",
                |result| eprintln!("Stream document visibility: {result}"),
            );
        }
    }
}

fn playback_call(command: PlaybackCommand) -> Result<String, String> {
    match command {
        PlaybackCommand::Seek(seconds) | PlaybackCommand::SeekPaused(seconds) => {
            if !seconds.is_finite() || !(0.0..=604800.0).contains(&seconds) {
                return Err("Invalid replay position.".into());
            }
            Ok(format!(
                "seek({seconds:.3},{})",
                matches!(command, PlaybackCommand::Seek(_))
            ))
        }
        PlaybackCommand::Play => Ok("play()".into()),
        PlaybackCommand::Pause => Ok("pause()".into()),
    }
}

fn newer_provider_pause(
    state: &PlaybackState,
    pending: Option<(bool, Instant)>,
    seeking: bool,
) -> bool {
    let Some((true, requested)) = pending else {
        return false;
    };
    !seeking
        && state.ready
        && state.pause_intent
        && !state.playing
        && state.is_fresh_after(requested)
}

fn publish_playback_state(state: &Mutex<PlaybackState>, next: PlaybackState) -> bool {
    let Some(polled_at) = next.polled_at else {
        return false;
    };
    let Ok(mut state) = state.lock() else {
        return false;
    };
    // Native observation epochs, never provider JSON, order callbacks.
    if state.polled_at.is_some_and(|previous| previous > polled_at) {
        return false;
    }
    *state = next;
    true
}

fn seek_acknowledged(target: f64, resume: bool, requested: Instant, state: &PlaybackState) -> bool {
    // A delayed SDK callback may describe the correct target but an obsolete
    // playback state. Require both a post-command request and a recent sample.
    state.is_fresh_since(requested) && seek_landed(target, resume, requested.elapsed(), state)
}

fn seek_landed(target: f64, resume: bool, elapsed: Duration, state: &PlaybackState) -> bool {
    state.ready
        && state.playing == resume
        && !state.buffering
        && if resume {
            // A busy provider can return its first useful sample after video
            // has advanced. This only acknowledges the observed SDK position;
            // the UI continues to display that position, never a local clock.
            (target - 0.25..=target + (elapsed.as_secs_f64() + 0.5).clamp(2.0, 15.0))
                .contains(&state.seconds)
        } else {
            (state.seconds - target).abs() <= 0.25
        }
}

fn playback_acknowledged(playing: bool, requested: Instant, state: &PlaybackState) -> bool {
    state.is_fresh_since(requested) && state.ready && !state.buffering && state.playing == playing
}

impl Drop for StreamPlayer {
    fn drop(&mut self) {
        self.capture.cancel();
        self.fullscreen.set_enabled(false);
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
        || !valid_playback_query(&url)
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

fn valid_playback_query(url: &Url) -> bool {
    if url.query().is_none() {
        return true;
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let recording_count = pairs.iter().filter(|(key, _)| key == "recording").count();
    let valid_recording = recording_count <= 1
        && pairs
            .iter()
            .filter(|(key, _)| key == "recording")
            .all(|(_, value)| match url.path().rsplit('/').next() {
                Some("twitch") => {
                    (1..=30).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
                }
                Some("youtube") => {
                    value.len() == 11
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                }
                _ => false,
            });
    if !valid_recording {
        return false;
    }
    if recording_count == 1 && pairs.len() == 1 {
        return true;
    }
    (pairs.len() == 2 + recording_count || pairs.len() == 3 + recording_count)
        && pairs.iter().filter(|(k, _)| k == "at").count() == 1
        && pairs.iter().filter(|(k, _)| k == "broadcast").count() == 1
        && pairs.iter().filter(|(k, _)| k == "paused").count() <= 1
        && pairs.iter().all(|(key, value)| match key.as_ref() {
            "at" => {
                let (whole, fractional) = value
                    .split_once('.')
                    .map_or((value.as_ref(), None), |(whole, fractional)| {
                        (whole, Some(fractional))
                    });
                (1..=7).contains(&whole.len())
                    && whole.bytes().all(|b| b.is_ascii_digit())
                    && fractional.is_none_or(|part| {
                        (1..=3).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit())
                    })
                    && value
                        .parse::<f64>()
                        .is_ok_and(|s| s.is_finite() && (0.0..=604800.0).contains(&s))
            }
            "paused" => value == "1",
            "recording" => true, // Provider-specific value and uniqueness checked above.
            "broadcast" => {
                !value.is_empty()
                    && value.len() <= 30
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }
            _ => false,
        })
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
    allowed: Arc<Mutex<String>>,
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
        // POV changes use authenticated navigation, never browser Back/Forward.
        // Do not retain previous provider documents and their media players in
        // the page cache. The normal resource cache remains available for SDKs.
        settings.set_enable_page_cache(false);
        // The user already selected this stream in the native sidebar. Permit
        // the official iframe's muted autoplay without a second browser click.
        settings.set_media_playback_requires_user_gesture(false);
        // XWayland child windows cannot reliably share WebKit's GBM buffers with
        // every GPU driver. Software compositing keeps the embedded player visible.
        settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
    }
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
                    let permitted = allowed.lock().is_ok_and(|url| destination.as_str() == *url)
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
fn watch_linux_failures(
    webview: &WebView,
    wrapper_url: Arc<Mutex<String>>,
    failure: Arc<Mutex<Option<String>>>,
) {
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
    let load_failure = Arc::clone(&failure);
    view.connect_load_failed(move |_, _, destination, _| {
        #[cfg(test)]
        eprintln!(
            "Stream GTK load failed: {}",
            diagnostic_destination(destination)
        );
        if wrapper_url.lock().is_ok_and(|url| destination == *url) {
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
    fn a_near_poll_deadline_does_not_request_immediate_egui_frames() {
        let ctx = egui::Context::default();
        for _ in 0..3 {
            let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        ctx.set_request_repaint_callback(move |info| recorded.lock().unwrap().push(info.delay));
        let wait = Duration::from_millis(1);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            schedule_state_poll(ui.ctx(), wait)
        });
        let requests = requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests.iter().all(|delay| *delay >= wait),
            "A near deadline must not spin until the SDK is due: {requests:?}"
        );
    }

    #[test]
    fn visible_state_reads_have_a_bounded_rate_even_while_paused() {
        let now = Instant::now();
        for (visible, expected) in [(true, 10), (false, 2)] {
            let mut last = None;
            let pending = AtomicBool::new(false);
            let mut requests = 0;
            // Much faster UI redraws must not turn into frame-rate SDK reads.
            for elapsed in 0..1000 {
                if begin_state_poll(
                    &mut last,
                    &pending,
                    now + Duration::from_millis(elapsed),
                    visible,
                ) == StatePoll::Started
                {
                    requests += 1;
                    pending.store(false, Ordering::Relaxed);
                }
            }
            assert_eq!(requests, expected);
        }
        // No playing-state condition: a visible paused player's own Play
        // gesture needs the same prompt observation as an advancing video.
        assert_eq!(state_poll_interval(true), Duration::from_millis(100));
    }

    #[test]
    fn a_stalled_sdk_callback_never_allows_overlapping_requests() {
        let now = Instant::now();
        let mut last = None;
        let pending = AtomicBool::new(false);
        assert_eq!(
            begin_state_poll(&mut last, &pending, now, true),
            StatePoll::Started
        );
        for seconds in [1, 2, 5, 30] {
            assert_eq!(
                begin_state_poll(
                    &mut last,
                    &pending,
                    now + Duration::from_secs(seconds),
                    true
                ),
                StatePoll::Pending
            );
            assert_eq!(last, Some(now));
        }
        // Callback or submission error releases the only outstanding request.
        pending.store(false, Ordering::Relaxed);
        let resumed = now + Duration::from_secs(31);
        assert_eq!(
            begin_state_poll(&mut last, &pending, resumed, true),
            StatePoll::Started
        );
        assert_eq!(last, Some(resumed));
    }

    #[test]
    fn visible_poll_deadline_is_not_delayed_by_hidden_cadence() {
        let now = Instant::now();
        let mut last = Some(now);
        let pending = AtomicBool::new(false);
        assert_eq!(
            begin_state_poll(&mut last, &pending, now + Duration::from_millis(40), true),
            StatePoll::Waiting(Duration::from_millis(60))
        );
        assert_eq!(
            begin_state_poll(&mut last, &pending, now + Duration::from_millis(150), false),
            StatePoll::Waiting(Duration::from_millis(350))
        );
        assert_eq!(
            begin_state_poll(&mut last, &pending, now + Duration::from_millis(150), true),
            StatePoll::Started
        );
    }

    #[test]
    fn a_new_provider_pause_cancels_only_an_older_native_play() {
        let now = Instant::now();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.pause_intent = true;
        state.buffering = true; // Intent can arrive before cached paused state.
        for at in [now - Duration::from_millis(1), now] {
            state.mark_polled_at(at);
            assert!(!newer_provider_pause(&state, Some((true, now)), false));
        }
        let requested = now - Duration::from_millis(1);
        state.mark_polled_at(now);
        assert!(newer_provider_pause(&state, Some((true, requested)), false));
        assert!(
            !playback_acknowledged(false, requested, &state),
            "Cancellation is not a fabricated paused acknowledgement"
        );
        assert!(!newer_provider_pause(
            &state,
            Some((false, requested)),
            false
        ));
        assert!(!newer_provider_pause(&state, Some((true, requested)), true));
        assert!(!newer_provider_pause(&state, None, false));
        state.pause_intent = false;
        assert!(!newer_provider_pause(
            &state,
            Some((true, requested)),
            false
        ));
        state.pause_intent = true;
        state.playing = true;
        assert!(!newer_provider_pause(
            &state,
            Some((true, requested)),
            false
        ));
    }

    #[test]
    fn a_provider_pause_hint_is_optional_and_cannot_acknowledge_playback() {
        let mut state: PlaybackState = serde_json::from_str(
            r#"{"ready":true,"seconds":72.125,"playing":false,"buffering":true,"pause_intent":true}"#,
        ).unwrap();
        assert!(state.pause_intent);
        let requested = Instant::now();
        state.mark_polled_at(requested);
        assert!(!playback_acknowledged(false, requested, &state));
        assert!(!seek_acknowledged(72.125, false, requested, &state));
        let legacy: PlaybackState = serde_json::from_str(r#"{"ready":true}"#).unwrap();
        assert!(!legacy.pause_intent);
        assert!(!legacy.play_intent);
        let mut play: PlaybackState = serde_json::from_str(
            r#"{"ready":true,"seconds":72.125,"playing":false,"buffering":true,"play_intent":true}"#,
        ).unwrap();
        play.mark_polled_at(requested);
        assert!(play.play_intent);
        assert!(!playback_acknowledged(true, requested, &play));
        assert!(!seek_acknowledged(72.125, true, requested, &play));
    }

    #[test]
    fn a_late_paused_callback_cannot_replace_newer_play_acknowledgment() {
        let now = Instant::now();
        let mut paused = PlaybackState::default();
        paused.ready = true;
        paused.seconds = 72.125;
        paused.mark_polled_at(now - Duration::from_millis(80));
        let requested = now - Duration::from_millis(40);
        assert!(!playback_acknowledged(false, requested, &paused));
        let mut playing = paused.clone();
        playing.playing = true;
        playing.seconds = 72.375;
        playing.mark_polled_at(now);
        let state = Mutex::new(PlaybackState::default());
        assert!(publish_playback_state(&state, playing));
        assert!(!publish_playback_state(&state, paused));
        let state = state.lock().unwrap();
        assert_eq!(state.seconds, 72.375);
        assert!(playback_acknowledged(true, requested, &state));
        assert!(!playback_acknowledged(false, requested, &state));
    }

    #[test]
    fn playback_intent_waits_for_a_fresh_matching_provider_sample() {
        let requested = Instant::now();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = false;
        state.polled_at = Some(requested - Duration::from_millis(1));
        assert!(!playback_acknowledged(false, requested, &state));
        state.polled_at = Some(requested);
        state.buffering = true;
        assert!(!playback_acknowledged(false, requested, &state));
        state.buffering = false;
        assert!(playback_acknowledged(false, requested, &state));
        assert!(!playback_acknowledged(true, requested, &state));
        state.playing = true;
        assert!(playback_acknowledged(true, requested, &state));
        state.ready = false;
        assert!(!playback_acknowledged(true, requested, &state));
    }

    #[test]
    fn seek_waits_for_real_playback_at_the_requested_moment() {
        let mut state = PlaybackState {
            ready: true,
            seconds: 120.0,
            playing: false,
            buffering: true,
            ..Default::default()
        };
        assert!(!seek_landed(120.0, true, Duration::ZERO, &state));
        state.playing = true;
        assert!(!seek_landed(120.0, true, Duration::ZERO, &state));
        state.buffering = false;
        assert!(seek_landed(120.0, true, Duration::ZERO, &state));
        state.seconds = 123.0;
        assert!(seek_landed(120.0, true, Duration::from_secs(4), &state));
        state.seconds = 120.0;
        assert!(!seek_landed(120.0, false, Duration::ZERO, &state));
        state.playing = false;
        state.seconds = 120.875;
        assert!(seek_landed(120.875, false, Duration::ZERO, &state));
        assert!(!seek_landed(120.0, false, Duration::ZERO, &state));
        state.seconds = 60.0;
        assert!(!seek_landed(120.0, false, Duration::ZERO, &state));
        state.seconds = 120.0;
        state.ready = false;
        assert!(!seek_landed(120.0, false, Duration::ZERO, &state));
    }

    #[test]
    fn a_delayed_post_command_sample_cannot_acknowledge_seek_play_or_pause() {
        let now = Instant::now();
        let requested = now - Duration::from_secs(5);
        for resume in [false, true] {
            let mut state = PlaybackState {
                ready: true,
                seconds: 120.0,
                playing: resume,
                polled_at: Some(requested + Duration::from_millis(100)),
                ..Default::default()
            };
            // The request began after the command and its position matches,
            // but its callback is arriving several seconds late.
            assert!(!seek_acknowledged(120.0, resume, requested, &state));
            assert!(!playback_acknowledged(resume, requested, &state));
            state.polled_at = Some(now);
            assert!(seek_acknowledged(120.0, resume, requested, &state));
            assert!(playback_acknowledged(resume, requested, &state));
            // Freshness alone must not acknowledge data preceding a new action.
            let newer_request = now + Duration::from_millis(1);
            assert!(!seek_acknowledged(120.0, resume, newer_request, &state));
            assert!(!playback_acknowledged(resume, newer_request, &state));
        }
    }

    #[test]
    fn playback_commands_keep_milliseconds_and_validate_before_queueing() {
        assert_eq!(
            playback_call(PlaybackCommand::Seek(120.875)).unwrap(),
            "seek(120.875,true)"
        );
        assert_eq!(
            playback_call(PlaybackCommand::SeekPaused(120.125)).unwrap(),
            "seek(120.125,false)"
        );
        for seconds in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.001,
            604800.001,
        ] {
            assert!(playback_call(PlaybackCommand::Seek(seconds)).is_err());
            assert!(playback_call(PlaybackCommand::SeekPaused(seconds)).is_err());
        }
    }

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
                        format!(
                            "localStorage.setItem('content-classification-labels-acknowledged',{});",
                            serde_json::to_string(&expected).unwrap()
                        )
                    } else {
                        String::new()
                    };
                    format!(
                        "<!doctype html><script>{acknowledgement}parent.postMessage({{kind:'fixture-report',value:localStorage.getItem('content-classification-labels-acknowledged')}},'*');</script>"
                    )
                } else {
                    format!(
                        "<!doctype html><script>addEventListener('message',e=>{{if(e.data.kind==='fixture-report')document.title=e.data.value||'missing'}})</script><iframe src='{provider_url}'></iframe>"
                    )
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
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
                allowed_url: Arc::new(Mutex::new(wrapper.clone())),
                bounds: [0; 4],
                visible: Cell::new(true),
                loaded: Arc::new(AtomicBool::new(true)),
                created: Instant::now(),
                failure: Arc::new(Mutex::new(None)),
                playback_state: Arc::new(Mutex::new(PlaybackState::default())),
                last_state_poll: None,
                state_pending: Arc::new(AtomicBool::new(false)),
                queued_command: Some(PlaybackCommand::Play),
                ready_since: None,
                pending_seek: None,
                pending_playback: None,
                command_retried: false,
                capture: capture::Controller::default(),
                fullscreen: fullscreen::Controller::new(&egui::Context::default()),
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
            eprintln!(
                "Native preference cycle {cycle}: original expiry restored; widget/context and all {} owned WebKit processes released",
                children.len()
            );
        }
        window.close();
        stopped.store(true, Ordering::Relaxed);
        server.join().unwrap();
    }

    #[test]
    fn load_timeout_does_not_interrupt_a_loaded_player() {
        let mut player = StreamPlayer {
            webview: None,
            allowed_url: Arc::new(Mutex::new(String::new())),
            preferences: None,
            playback_state: Arc::new(Mutex::new(PlaybackState::default())),
            last_state_poll: None,
            state_pending: Arc::new(AtomicBool::new(false)),
            queued_command: Some(PlaybackCommand::Play),
            ready_since: None,
            pending_seek: None,
            pending_playback: None,
            command_retried: false,
            capture: capture::Controller::default(),
            fullscreen: fullscreen::Controller::new(&egui::Context::default()),
            #[cfg(target_os = "linux")]
            preference_handler: None,
            bounds: [0; 4],
            visible: Cell::new(true),
            loaded: Arc::new(AtomicBool::new(false)),
            created: Instant::now() - WRAPPER_LOAD_TIMEOUT,
            failure: Arc::new(Mutex::new(None)),
        };
        assert!(player.failure().unwrap().contains("too long"));
        player.loaded.store(true, Ordering::Relaxed);
        assert!(player.failure().is_none());
        {
            let mut state = player.playback_state.lock().unwrap();
            state.ready = true;
            state.seconds = 123.5;
            state.mark_polled_now();
        }
        let requested = Instant::now() - COMMAND_RETRY_AFTER;
        player.pending_seek = Some((300.0, false, requested));
        player.pending_playback = Some((false, requested));
        player.visible.set(false);
        assert!(player.recover_pending_command().is_none());
        player.visible.set(true);
        player.playback_state.lock().unwrap().buffering = true;
        assert!(player.recover_pending_command().is_none());
        player.playback_state.lock().unwrap().buffering = false;
        assert!(matches!(
            player.recover_pending_command(),
            Some(PlaybackCommand::SeekPaused(300.0))
        ));
        assert!(
            player.recover_pending_command().is_none(),
            "Only one automatic retry"
        );
        let expired = Instant::now() - COMMAND_TIMEOUT;
        player.pending_seek = Some((300.0, false, expired));
        player.pending_playback = Some((false, expired));
        assert!(
            player.failure().is_none(),
            "A loaded player must not be discarded"
        );
        assert!(player.recover_pending_command().is_none());
        assert!(player.pending_seek.is_none());
        assert!(player.pending_playback.is_none());
        assert_eq!(
            player.playback_state().seconds,
            123.5,
            "Never fabricate a successful seek"
        );
        player.state_pending.store(true, Ordering::Relaxed);
        player.last_state_poll = Some(Instant::now() - Duration::from_secs(3));
        assert!(
            player.failure().is_none(),
            "A slow callback is still allowed to finish"
        );
        player.last_state_poll = Some(Instant::now() - STATE_POLL_TIMEOUT);
        assert!(player.failure().unwrap().contains("stopped responding"));
        assert!(
            player.state_pending.load(Ordering::Relaxed),
            "Failure must not queue an overlapping request"
        );
        player.state_pending.store(false, Ordering::Relaxed);
        assert!(
            player.failure().is_none(),
            "A completed callback releases the watchdog"
        );
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
    fn replay_navigation_only_accepts_bounded_position_and_broadcast() {
        let base = "https://brick.example";
        let path = "https://brick.example/v1/streams/player/12345/youtube";
        for query in [
            "at=0&broadcast=abcDEF_12-3",
            "broadcast=abcDEF_12-3&at=18000",
            "at=18000.875&broadcast=abcDEF_12-3&paused=1",
            "paused=1&broadcast=abcDEF_12-3&at=0.001",
            "at=604800.000&broadcast=abcDEF_12-3",
        ] {
            assert!(validate_player_address(&format!("{path}?{query}"), base).is_ok());
        }
        for query in [
            "at=0",
            "broadcast=abcDEF_12-3",
            "at=0&at=1",
            "at=1&broadcast=x&broadcast=y",
            "at=0&broadcast=x&token=secret",
            "at=-1&broadcast=x",
            "at=604801&broadcast=x",
            "at=18446744073709551616&broadcast=x",
            "at=1&broadcast=",
            "at=1&broadcast=../x",
            "paused=1",
            "at=1&paused=1",
            "at=1&broadcast=x&paused=0",
            "at=1&broadcast=x&paused=true",
            "at=1&broadcast=x&paused=1&paused=1",
            "at=1.&broadcast=x",
            "at=.5&broadcast=x",
            "at=1.0001&broadcast=x",
            "at=1e2&broadcast=x",
            "at=NaN&broadcast=x",
            "at=604800.001&broadcast=x",
        ] {
            assert!(
                validate_player_address(&format!("{path}?{query}"), base).is_err(),
                "{query}"
            );
        }
    }

    #[test]
    fn saved_recording_navigation_rejects_duplicate_foreign_and_incomplete_queries() {
        let base = "https://brick.example";
        for (provider, recording) in [("twitch", "123456789"), ("youtube", "abcDEF_12-3")] {
            let path = format!("{base}/v1/streams/player/12345/{provider}");
            for query in [
                format!("recording={recording}"),
                format!("recording={recording}&at=32.875&broadcast=example&paused=1"),
            ] {
                assert!(validate_player_address(&format!("{path}?{query}"), base).is_ok());
            }
            for query in [
                format!("recording={recording}&recording={recording}"),
                format!("recording={recording}&at=0"),
                format!("recording={recording}&broadcast=example"),
                format!("recording={recording}&paused=1"),
                format!("recording={recording}&at=0&broadcast=example&token=secret"),
                "recording=https%3A%2F%2Fyoutube.com".into(),
                "recording=..%2Fsecret".into(),
                "recording=".into(),
            ] {
                assert!(
                    validate_player_address(&format!("{path}?{query}"), base).is_err(),
                    "{query}"
                );
            }
        }
        assert!(validate_player_address(
            &format!("{base}/v1/streams/player/12345/twitch?recording=abcDEF_12-3"),
            base
        )
        .is_err());
        assert!(validate_player_address(
            &format!("{base}/v1/streams/player/12345/youtube?recording=123"),
            base
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
