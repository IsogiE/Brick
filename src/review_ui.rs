use crate::{
    defensives::{self, DefensiveGroup},
    discord_auth,
    stream_player::{PlaybackCommand, PlaybackState},
    streams::{Status, Stream},
    warcraftlogs::{while_current, Client, EventKind, Pull, RaidEvent, Review},
};
use eframe::egui::{self, Color32, RichText};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(244, 100, 56);
const MUTED: Color32 = Color32::from_rgb(145, 155, 173);
const DEATH: Color32 = Color32::from_rgb(237, 82, 93);

static PEER_METADATA_RUNNING: AtomicBool = AtomicBool::new(false);
struct PeerMetadataPermit;
impl PeerMetadataPermit {
    fn acquire() -> Option<Self> {
        PEER_METADATA_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}
impl Drop for PeerMetadataPermit {
    fn drop(&mut self) {
        PEER_METADATA_RUNNING.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct Playback {
    pub seconds: f64,
    pub autoplay: bool,
    pub broadcast_id: String,
    pub public_url: String,
}

#[derive(Clone)]
enum Action {
    Refresh,
    Connect,
    Disconnect,
    Events(Pull, EventKind),
    SaveCooldowns(defensives::Preferences),
}
enum Data {
    Review(Review, defensives::Preferences),
    Authentication,
    Events(String, EventKind, Vec<RaidEvent>, defensives::Preferences),
    Cooldowns(defensives::Preferences),
}
type Outcome = (u64, String, Result<Data, String>, bool);

struct EventFailure {
    kind: EventKind,
    message: String,
    attempts: u8,
    retry_at: Instant,
}

#[derive(Default)]
pub struct WorkspaceAction {
    pub compare: bool,
    pub close_comparison: bool,
    pub rect: Option<egui::Rect>,
    pub reload: bool,
    pub command: Option<PlaybackCommand>,
    pub stream: Option<Stream>,
}

struct ObservedSpellName {
    display: String,
    search: std::collections::BTreeSet<String>,
}

struct CooldownEditor {
    draft: defensives::Preferences,
    observed_names: std::collections::BTreeMap<u64, ObservedSpellName>,
    search: String,
    group: DefensiveGroup,
    advanced: bool,
    remembered_groups: std::collections::BTreeMap<u64, DefensiveGroup>,
    new_id: String,
    error: Option<String>,
}
impl CooldownEditor {
    fn new(draft: defensives::Preferences, events: &[RaidEvent], group: DefensiveGroup) -> Self {
        let mut editor = Self {
            draft,
            observed_names: Default::default(),
            search: String::new(),
            group,
            advanced: false,
            remembered_groups: Default::default(),
            new_id: String::new(),
            error: None,
        };
        editor.refresh_names(events);
        editor
    }

    fn category(&self, id: u64) -> DefensiveGroup {
        self.draft
            .classify(id)
            .or_else(|| self.remembered_groups.get(&id).copied())
            .or_else(|| self.draft.catalog.spell(id).map(|spell| spell.group))
            .unwrap_or(DefensiveGroup::Healing)
    }

    fn name(&self, id: u64) -> &str {
        self.observed_names
            .get(&id)
            .map(|names| names.display.as_str())
            .or_else(|| self.draft.spell_name(id))
            .unwrap_or("Custom spell")
    }

    fn apply_rule(&mut self, id: u64, rule: defensives::Rule) {
        let default = self.draft.catalog.rule(id);
        if default == Some(rule) {
            self.draft.overrides.remove(&id);
        } else if self.draft.overrides.contains_key(&id)
            || self.draft.overrides.len() < defensives::MAX_OVERRIDES
        {
            self.draft.overrides.insert(id, rule);
            if let Some(group) = rule.group {
                self.remembered_groups.insert(id, group);
            }
        } else {
            self.error = Some(
                "Up to 128 spell changes can be saved. Reset a changed spell to make room.".into(),
            );
        }
    }

    // WCL events and labels are already bounded. Index names only when opening
    // the editor or receiving event data, never once per tracked ID per frame.
    fn refresh_names(&mut self, events: &[RaidEvent]) {
        self.observed_names.clear();
        for event in events {
            let names = self
                .observed_names
                .entry(event.ability_id)
                .or_insert_with(|| ObservedSpellName {
                    display: event.ability.clone(),
                    search: Default::default(),
                });
            names.search.insert(event.ability.to_lowercase());
        }
    }
}

pub struct ReviewUi {
    client: Arc<Mutex<Option<Client>>>,
    marker_cache: Arc<Mutex<crate::replay_sync::Cache>>,
    marker_sync: crate::replay_sync::Sync,
    metadata_only: bool,
    work: Option<mpsc::Receiver<Outcome>>,
    work_action: Option<Action>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    key: String,
    last_attempt: Option<Instant>,
    refresh_period: Duration,
    review: Option<Review>,
    replay_coverage: Option<(i64, i64)>,
    notice: Option<String>,
    connected: bool,
    connection_checked: bool,
    active: bool,
    comparing: bool,
    open_first_pull: bool,
    popup_open: bool,
    signing_in: bool,
    playback: Option<Playback>,
    pull: Option<Pull>,
    kind: EventKind,
    events: Vec<RaidEvent>,
    requested_events: Vec<EventKind>,
    loaded_events: Vec<EventKind>,
    show_all_deaths: bool,
    cooldowns: defensives::Preferences,
    cooldown_editor: Option<CooldownEditor>,
    cooldown_save: Option<defensives::Preferences>,
    cooldown_notice: Option<String>,
    cooldown_filter: DefensiveGroup,
    selected_event: Option<(i64, String)>,
    scroll_to_event: bool,
    aligning: bool,
    event_failures: Vec<EventFailure>,
    search: String,
    scrub: Option<f64>,
    timeline_position: Option<f64>,
    pending_focus: Option<(Pull, i64, bool)>,
    pull_menu_cursor: Option<usize>,
    pov_menu: PovMenuState,
    range_epoch: Instant,
    range_pause_sent: bool,
    provider_observation: Option<(Instant, f64)>,
    provider_seek_generation: Option<u64>,
    provider_seek_pending: bool,
}

impl Default for ReviewUi {
    fn default() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            marker_cache: Arc::new(Mutex::new(crate::replay_sync::Cache::default())),
            marker_sync: crate::replay_sync::Sync::default(),
            metadata_only: false,
            work: None,
            work_action: None,
            cancel: Arc::new(AtomicBool::new(false)),
            generation: 0,
            key: String::new(),
            last_attempt: None,
            refresh_period: Duration::from_secs(60),
            review: None,
            replay_coverage: None,
            notice: None,
            connected: false,
            connection_checked: false,
            active: false,
            comparing: false,
            open_first_pull: false,
            popup_open: false,
            signing_in: false,
            playback: None,
            pull: None,
            kind: EventKind::Deaths,
            events: Vec::new(),
            requested_events: Vec::new(),
            loaded_events: Vec::new(),
            show_all_deaths: false,
            cooldowns: Default::default(),
            cooldown_editor: None,
            cooldown_save: None,
            cooldown_notice: None,
            cooldown_filter: DefensiveGroup::Healing,
            selected_event: None,
            scroll_to_event: false,
            aligning: false,
            event_failures: Vec::new(),
            search: String::new(),
            scrub: None,
            timeline_position: None,
            pending_focus: None,
            pull_menu_cursor: None,
            pov_menu: PovMenuState::default(),
            range_epoch: Instant::now(),
            range_pause_sent: false,
            provider_observation: None,
            provider_seek_generation: None,
            provider_seek_pending: false,
        }
    }
}
impl Drop for ReviewUi {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl ReviewUi {
    pub fn active(&self) -> bool {
        self.active
    }
    pub fn playback(&self) -> Option<&Playback> {
        self.playback.as_ref()
    }

    pub(crate) fn metadata_peer(&self) -> Self {
        let mut peer = Self::default();
        peer.client = self.client.clone();
        peer.marker_cache = self.marker_cache.clone();
        peer.metadata_only = true;
        peer
    }

    pub(crate) fn comparison_notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(crate) fn comparison_metadata(&self) -> Option<&Review> {
        self.review.as_ref()
    }

    pub(crate) fn comparison_context(&self) -> Option<(&Review, &Pull)> {
        if !self.active || self.pending_focus.is_some() {
            return None;
        }
        let review = self.review.as_ref()?;
        let playback = self.playback.as_ref()?;
        (playback.broadcast_id == review.replay.broadcast_id)
            .then_some((review, self.pull.as_ref()?))
    }

    /// The comparison has mapped a provider gesture into this recording. Keep
    /// its native child and use the existing event worker for a newly found pull.
    pub(crate) fn follow_comparison_position(
        &mut self,
        selected: Option<Pull>,
        seconds: f64,
        playing: bool,
    ) {
        if !self.active || !self.comparing || !seconds.is_finite() {
            return;
        }
        if self.pull.as_ref().map(pull_key) != selected.as_ref().map(pull_key) {
            self.cancel_read();
            self.marker_sync.reset(None);
            self.pull = selected;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.event_failures.clear();
            self.scroll_to_event = false;
        }
        self.selected_event = None;
        self.scrub = None;
        self.range_pause_sent = false;
        self.timeline_position = Some(seconds);
        if let Some((review, playback)) = self.review.as_ref().zip(self.playback.as_mut()) {
            playback.seconds = seconds;
            playback.autoplay = playing;
            playback.public_url = review.replay.public_url(seconds as u64);
        }
    }

    pub(crate) fn comparison_position(&self, state: &PlaybackState) -> Option<(i64, bool)> {
        let (review, pull) = self.comparison_context()?;
        if let Some((elapsed, playing)) = self.marker_sync.intent() {
            return Some((pull.start_ms + (elapsed * 1000.0).round() as i64, playing));
        }
        let seconds = self
            .confirmed_video_position(state)
            .or_else(|| {
                state
                    .is_fresh_since(self.range_epoch)
                    .then_some(state.seeking)
                    .flatten()
            })
            .or_else(|| self.playback.as_ref().map(|p| p.seconds))?;
        Some((
            encounter_moment(review, pull, seconds),
            playback_intent(state, self.playback.as_ref()),
        ))
    }

    pub(crate) fn pov_covers_moment(&self, candidate: &Stream, pull: &Pull, at_ms: i64) -> bool {
        let video_id = candidate.recording_id.as_deref().or_else(|| {
            (candidate.provider == crate::streams::Provider::Youtube)
                .then_some(candidate.channel_id.as_str())
        });
        let known = self.review.as_ref().filter(|review| {
            review.replay.provider == candidate.provider
                && (pov_key(candidate) == self.key
                    || video_id == Some(review.replay.video_id.as_str()))
        });
        if let Some(review) = known.filter(|r| r.marker_alignment(pull).is_some()) {
            let seconds = review.pull_video_start(pull) + (at_ms - pull.start_ms) as f64 / 1000.0;
            return seconds >= 0.0 && seconds < review.replay.available_seconds as f64;
        }
        // RFC3339 decoding happens once when metadata arrives, never while
        // painting or filtering the candidate list.
        let range = known
            .and(self.replay_coverage)
            .or_else(|| candidate.replay_range());
        let Some(range) = range else {
            return false;
        };
        recording_covers_moment(range, pull.start_ms, at_ms)
    }

    pub(crate) fn pov_selection_context(&self, state: &PlaybackState) -> Option<(&Pull, i64)> {
        if let Some((pull, at_ms, _)) = &self.pending_focus {
            return Some((pull, *at_ms));
        }
        Some((self.pull.as_ref()?, self.comparison_position(state)?.0))
    }

    pub(crate) fn set_recording_labels(&mut self, labels: RecordingLabels) {
        self.pov_menu.labels = labels;
    }

    pub(crate) fn set_comparing(&mut self, comparing: bool) {
        self.comparing = comparing;
    }

    pub(crate) fn open_recording(&mut self) {
        self.cancel_read();
        self.review = None;
        self.replay_coverage = None;
        self.pull = None;
        self.playback = None;
        self.pending_focus = None;
        self.events.clear();
        self.requested_events.clear();
        self.loaded_events.clear();
        self.event_failures.clear();
        self.selected_event = None;
        self.notice = None;
        self.last_attempt = None;
        self.popup_open = false;
        self.aligning = false;
        self.active = true;
        self.open_first_pull = true;
    }

    pub(crate) fn report_span(&self) -> Option<(i64, i64)> {
        let Some(review) = self.review.as_ref() else {
            return self
                .pending_focus
                .as_ref()
                .map(|(pull, _, _)| (pull.start_ms, pull.end_ms));
        };
        let report = &self.pull.as_ref().or(review.pulls.first())?.report;
        let mut pulls = review.pulls.iter().filter(|pull| &pull.report == report);
        let first = pulls.next()?;
        Some(
            pulls.fold((first.start_ms, first.end_ms), |(start, end), pull| {
                (start.min(pull.start_ms), end.max(pull.end_ms))
            }),
        )
    }

    pub(crate) fn syncing_marker(&self) -> bool {
        self.marker_sync.busy()
    }

    pub(crate) fn cancel_marker(&mut self, player: Option<&crate::stream_player::StreamPlayer>) {
        self.marker_sync.cancel(player);
    }

    pub(crate) fn sync_marker(
        &mut self,
        ctx: &egui::Context,
        player: &crate::stream_player::StreamPlayer,
        selected: Option<&Pull>,
        desired: Option<(i64, bool)>,
    ) -> Option<PlaybackCommand> {
        if self.popup_open || self.pending_focus.is_some() || (!self.metadata_only && !self.active)
        {
            return None;
        }
        let pull = selected.or(self.pull.as_ref())?;
        let review = self.review.as_mut()?;
        let estimate = review.pull_video_start(pull);
        let (elapsed, autoplay) = desired.map_or_else(
            || {
                self.playback
                    .as_ref()
                    .map_or((0.0, true), |p| (p.seconds - estimate, p.autoplay))
            },
            |(at_ms, playing)| ((at_ms - pull.start_ms) as f64 / 1000.0, playing),
        );
        let command = self.marker_sync.tick(
            ctx,
            player,
            &review.replay,
            pull,
            estimate,
            elapsed,
            autoplay,
            review.marker_alignment(pull).is_some(),
            true,
        );
        if let Some(alignment) = self.marker_sync.take_alignment() {
            crate::replay_library::submit(&review.replay, pull, alignment);
            if let Ok(mut cache) = self.marker_cache.lock() {
                cache.insert(
                    crate::replay_sync::Key::new(&review.replay, pull),
                    alignment,
                );
            }
            // Cache for the next explicit navigation. Changing this pull's
            // clock while it is playing would move its timeline or comparison.
        }
        if let Some(PlaybackCommand::Seek(seconds) | PlaybackCommand::SeekPaused(seconds)) = command
        {
            // Calibration probes are temporary. Only the final restored moment
            // becomes the workspace's playback intent.
            if !self.marker_sync.busy() {
                if let Some(playback) = &mut self.playback {
                    playback.seconds = seconds;
                    playback.public_url = review.replay.public_url(seconds as u64);
                }
                self.reset_playback_range();
            }
        }
        command
    }

    pub fn tick(&mut self, ctx: &egui::Context, stream: Option<&Stream>) -> bool {
        self.refresh_period = if !self.metadata_only
            && stream.is_some_and(|stream| {
                stream.status == Status::Live
                    || stream.replay_range().is_some_and(|(_, end)| {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
                        end >= now.saturating_sub(24 * 60 * 60 * 1000)
                    })
            }) {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(60)
        };
        let key = stream.map(pov_key).unwrap_or_default();
        let mut changed = false;
        if self.key != key {
            self.marker_sync.reset(None);
            self.cancel_read();
            self.key = key;
            self.review = None;
            self.replay_coverage = None;
            self.notice = None;
            self.last_attempt = None;
            self.playback = None;
            self.pull = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.event_failures.clear();
            self.selected_event = None;
            self.popup_open = false;
            if stream.is_none() {
                self.active = false;
                self.open_first_pull = false;
                self.pending_focus = None;
            }
            changed = true;
        }
        if !self.active && matches!(self.work_action, Some(Action::Events(..))) {
            self.cancel_read();
        }
        if let Some(rx) = &self.work {
            match rx.try_recv() {
                Ok((generation, key, result, connected)) => {
                    let action = self.work_action.take();
                    self.work = None;
                    self.signing_in = false;
                    if generation == self.generation
                        && key == self.key
                        && !self.cancel.load(Ordering::Relaxed)
                    {
                        if matches!(action, Some(Action::Refresh)) {
                            self.last_attempt = Some(Instant::now());
                        }
                        self.connected = connected;
                        self.connection_checked = true;
                        match result {
                            Ok(Data::Review(review, preferences)) => {
                                self.accept_cooldown_preferences(preferences);
                                changed |= self.accept_review(review);
                                changed |= self.restore_pov_position();
                            }
                            Ok(Data::Authentication) => {
                                self.marker_sync.reset(None);
                                if let Ok(mut cache) = self.marker_cache.lock() {
                                    *cache = Default::default();
                                }
                                self.review = None;
                                self.replay_coverage = None;
                                self.pull = None;
                                self.events.clear();
                                self.requested_events.clear();
                                self.loaded_events.clear();
                                self.event_failures.clear();
                                self.selected_event = None;
                                self.notice = None;
                                self.last_attempt = None;
                            }
                            Ok(Data::Cooldowns(preferences)) => {
                                self.accept_cooldown_preferences(preferences);
                                self.cooldown_editor = None;
                                self.cooldown_notice = None;
                            }
                            Ok(Data::Events(key, kind, events, preferences)) => {
                                self.accept_cooldown_preferences(preferences);
                                if self.pull.as_ref().is_some_and(|p| pull_key(p) == key) {
                                    self.events.retain(|event| event.kind != kind);
                                    self.events.extend(events);
                                    self.events.sort_by_key(|event| event.at_ms);
                                    if let Some(editor) = &mut self.cooldown_editor {
                                        editor.refresh_names(&self.events);
                                    }
                                    self.event_failures.retain(|failure| failure.kind != kind);
                                    if !self.loaded_events.contains(&kind) {
                                        self.loaded_events.push(kind);
                                    }
                                }
                            }
                            Err(error) => {
                                if let Some(Action::Events(pull, kind)) = action {
                                    if self
                                        .pull
                                        .as_ref()
                                        .is_some_and(|p| pull_key(p) == pull_key(&pull))
                                    {
                                        self.record_event_failure(kind, error);
                                    }
                                } else if matches!(action, Some(Action::SaveCooldowns(_))) {
                                    self.cooldown_notice = Some(error);
                                } else {
                                    self.notice = Some(error);
                                }
                            }
                        }
                        if !connected {
                            self.review = None;
                            self.replay_coverage = None;
                            self.pull = None;
                            self.events.clear();
                            self.requested_events.clear();
                            self.loaded_events.clear();
                            self.event_failures.clear();
                            self.selected_event = None;
                            self.playback = None;
                            // Keep the selected VOD open so connection recovery
                            // remains visible. Only navigation closes a review.
                            changed = true;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.work = None;
                    let action = self.work_action.take();
                    self.signing_in = false;
                    if !self.cancel.load(Ordering::Relaxed) {
                        self.connection_checked = true;
                        let message = "Warcraft Logs stopped loading. Brick will retry shortly.";
                        if let Some(Action::Events(pull, kind)) = action {
                            if self
                                .pull
                                .as_ref()
                                .is_some_and(|current| pull_key(current) == pull_key(&pull))
                            {
                                self.record_event_failure(kind, message.into());
                            }
                        } else {
                            self.notice = Some(message.into());
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        // Explicit local preference saves take priority over polling/retries and
        // still finish if the user has left this workspace while a read completed.
        if self.work.is_none() {
            if let Some(preferences) = self.cooldown_save.take() {
                self.start(ctx, stream, Action::SaveCooldowns(preferences));
            }
        }
        if let Some(action) = self.next_action() {
            if stream.is_some() {
                self.start(ctx, stream, action);
            }
        }
        if stream.is_some() && self.work.is_none() {
            // Do not depend on mouse input or a playing video for fresh pulls.
            // A busy shared metadata worker retries at a bounded cadence.
            let wait = self.last_attempt.map_or(Duration::from_secs(1), |at| {
                self.refresh_interval()
                    .saturating_sub(at.elapsed())
                    .max(Duration::from_millis(250))
            });
            let retry = self.event_retry_after();
            ctx.request_repaint_after(retry.map_or(wait, |retry| wait.min(retry)));
        }
        changed
    }

    fn record_event_failure(&mut self, kind: EventKind, message: String) {
        let attempts = self
            .event_failures
            .iter()
            .find(|failure| failure.kind == kind)
            .map_or(1, |failure| failure.attempts.saturating_add(1));
        let seconds = match attempts {
            1 => 5,
            2 => 15,
            3 => 30,
            _ => 60,
        };
        self.event_failures.retain(|failure| failure.kind != kind);
        self.event_failures.push(EventFailure {
            kind,
            message,
            attempts,
            retry_at: Instant::now() + Duration::from_secs(seconds),
        });
    }

    fn event_retry_after(&self) -> Option<Duration> {
        if !self.active || self.pull.is_none() || self.work.is_some() {
            return None;
        }
        self.event_failures
            .iter()
            .filter(|failure| !self.loaded_events.contains(&failure.kind))
            .map(|failure| {
                failure
                    .retry_at
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(250))
            })
            .min()
    }

    fn refresh_interval(&self) -> Duration {
        if self.notice.is_some() {
            Duration::from_secs(60)
        } else {
            self.refresh_period
        }
    }

    // Keep the worker until it finishes its bounded current request. Derive the
    // next read from the current UI selection instead of queuing obsolete pulls.
    fn cancel_read(&mut self) {
        if !matches!(self.work_action, Some(Action::Refresh | Action::Events(..)))
            || self.cancel.swap(true, Ordering::Relaxed)
        {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        match self.work_action.as_ref() {
            Some(Action::Refresh) => self.last_attempt = None,
            Some(Action::Events(pull, kind))
                if self
                    .pull
                    .as_ref()
                    .is_some_and(|p| pull_key(p) == pull_key(pull)) =>
            {
                self.requested_events.retain(|requested| requested != kind);
            }
            _ => (),
        }
    }

    fn next_action(&self) -> Option<Action> {
        if self.work.is_some() {
            return None;
        }
        let missing = [EventKind::Deaths, EventKind::Defensives]
            .into_iter()
            .find(|kind| {
                !self.loaded_events.contains(kind)
                    && (!self.requested_events.contains(kind)
                        || self.event_failures.iter().any(|failure| {
                            failure.kind == *kind && Instant::now() >= failure.retry_at
                        }))
            });
        if let Some((pull, kind)) = self.pull.clone().zip(missing).filter(|_| self.active) {
            Some(Action::Events(pull, kind))
        } else if self
            .last_attempt
            .is_none_or(|at| at.elapsed() >= self.refresh_interval())
        {
            Some(Action::Refresh)
        } else {
            None
        }
    }

    fn accept_review(&mut self, mut review: Review) -> bool {
        if let Ok(mut cache) = self.marker_cache.lock() {
            for pull in &review.pulls {
                if let Some(alignment) = review.marker_alignment(pull) {
                    cache.insert(
                        crate::replay_sync::Key::new(&review.replay, pull),
                        alignment,
                    );
                }
                if let Some(alignment) = cache.get(&review.replay, pull) {
                    review
                        .marker_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                }
            }
        }
        // A refresh may discover a better clock, but only explicit navigation
        // adopts it. Keep the currently watched pull's mapping unchanged.
        if let Some((old, selected)) = self.review.as_ref().zip(self.pull.as_ref()) {
            if old.replay.broadcast_id == review.replay.broadcast_id
                && old.replay.video_id == review.replay.video_id
                && review.pulls.iter().any(|p| {
                    p.report == selected.report
                        && p.id == selected.id
                        && p.start_ms == selected.start_ms
                        && p.end_ms == selected.end_ms
                })
            {
                let key = (selected.report.clone(), selected.id);
                match old.marker_alignment(selected) {
                    Some(alignment) => {
                        review.marker_timing.insert(key, alignment);
                    }
                    None => {
                        review.marker_timing.remove(&key);
                    }
                }
            }
        }
        self.replay_coverage = review.replay.start_ms().ok().and_then(|start| {
            start
                .checked_add(
                    i64::try_from(review.replay.available_seconds)
                        .ok()?
                        .checked_mul(1000)?,
                )
                .map(|end| (start, end))
        });
        let broadcast_changed = self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.broadcast_id != review.replay.broadcast_id);
        let current = self.pull.as_ref().and_then(|selected| {
            review
                .pulls
                .iter()
                .find(|pull| pull.report == selected.report && pull.id == selected.id)
        });
        let unavailable = self.pull.is_some() && current.is_none();
        if broadcast_changed || unavailable {
            self.pull = None;
            self.playback = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.event_failures.clear();
            self.selected_event = None;
            self.scroll_to_event = false;
            self.scrub = None;
            self.timeline_position = None;
            self.aligning = false;
        } else if let Some(current) = current {
            if self
                .pull
                .as_ref()
                .is_some_and(|old| old.start_ms != current.start_ms || old.end_ms != current.end_ms)
            {
                self.events.clear();
                self.requested_events.clear();
                self.loaded_events.clear();
                self.event_failures.clear();
                self.selected_event = None;
                self.scroll_to_event = false;
                self.range_epoch = Instant::now();
                self.range_pause_sent = false;
                self.timeline_position = None;
            }
            self.pull = Some(current.clone());
        }
        self.review = Some(review);
        self.notice = unavailable.then(|| "This pull is no longer available.".into());
        let first = self
            .open_first_pull
            .then(|| self.review.as_ref()?.pulls.first().cloned())
            .flatten();
        let opened = first.is_some();
        if let Some(pull) = first {
            self.select(pull);
            self.open_first_pull = false;
        }
        broadcast_changed || unavailable || opened
    }

    fn start(&mut self, ctx: &egui::Context, stream: Option<&Stream>, action: Action) {
        if self.work.is_some() {
            return;
        }
        let peer_permit = if self.metadata_only {
            let Some(permit) = PeerMetadataPermit::acquire() else {
                return;
            };
            Some(permit)
        } else {
            None
        };
        let client = self.client.clone();
        let stream = stream.cloned();
        let ctx = ctx.clone();
        let key = self.key.clone();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (tx, rx) = mpsc::channel();
        self.work = Some(rx);
        self.work_action = Some(action.clone());
        if matches!(action, Action::Refresh) {
            self.last_attempt = Some(Instant::now());
        }
        if let Action::Events(_, kind) = &action {
            if !self.requested_events.contains(kind) {
                self.requested_events.push(*kind);
            }
        }
        self.signing_in = matches!(action, Action::Connect);
        crate::guild::spawn(move || {
            let _peer_permit = peer_permit;
            let mut connected = false;
            let result = (|| {
                let token =
                    while_current(&cancel, discord_auth::current_or_refreshed_access_token)??
                        .ok_or("Sign in to Discord again.")?;
                let mut lock = client.lock().map_err(|_| "Warcraft Logs is unavailable.")?;
                // A metadata peer may have closed while waiting for the primary
                // request. No cancelled peer may initialize or use the client.
                while_current(&cancel, || ())?;
                if lock.is_none() {
                    *lock = Some(Client::new()?);
                }
                let client = lock.as_mut().unwrap();
                client.set_request_cancellation(cancel.clone());
                let result = match action {
                    Action::Connect => client
                        .login(&token, &ctx, &cancel)
                        .map(|_| Data::Authentication),
                    Action::Disconnect => client.disconnect(&token).map(|_| Data::Authentication),
                    Action::Refresh => client
                        .review(&token, stream.as_ref().ok_or("Choose a VOD first.")?)
                        .map(|review| Data::Review(review, client.cooldown_preferences())),
                    Action::Events(pull, kind) => {
                        client.events(&token, &pull, kind).map(|events| {
                            Data::Events(
                                pull_key(&pull),
                                kind,
                                events,
                                client.cooldown_preferences(),
                            )
                        })
                    }
                    Action::SaveCooldowns(preferences) => client
                        .save_cooldown_preferences(&token, preferences)
                        .map(Data::Cooldowns),
                };
                connected = client.connected();
                result
            })();
            let _ = tx.send((generation, key, result, connected));
            ctx.request_repaint();
        });
    }

    fn accept_cooldown_preferences(&mut self, mut preferences: defensives::Preferences) -> bool {
        // A local queued visibility toggle is newer than an in-flight read.
        // Adopt shared defaults without overwriting either saved or unsaved user choices.
        if let Some(pending) = self.cooldown_save.as_mut() {
            pending.catalog = preferences.catalog.clone();
            preferences.overrides = self.cooldowns.overrides.clone();
            preferences.hidden_groups = self.cooldowns.hidden_groups.clone();
        }
        let changed = self.cooldowns.overrides != preferences.overrides
            || self.cooldowns.catalog != preferences.catalog;
        if let Some(editor) = self.cooldown_editor.as_mut() {
            editor.draft.catalog = preferences.catalog.clone();
        }
        let needs_more_events = changed
            && preferences.ids().into_iter().any(|id| {
                let Some(rule) = preferences.rule(id).filter(|rule| rule.group.is_some()) else {
                    return false;
                };
                self.cooldowns.rule(id).is_none_or(|previous| {
                    previous.group.is_none() || previous.observation != rule.observation
                })
            });
        self.cooldowns = preferences;
        if changed {
            // Recategorization and removal are local operations. Keep still-relevant
            // rows while additional tracking loads; never blank the pull or restart media.
            self.events.retain_mut(|event| {
                if event.kind != EventKind::Defensives {
                    return true;
                }
                let observation = if event.observed_buff {
                    "applybuff"
                } else {
                    "cast"
                };
                let Some(rule) = self
                    .cooldowns
                    .rule(event.ability_id)
                    .filter(|rule| rule.group.is_some() && rule.observation.accepts(observation))
                else {
                    return false;
                };
                event.group = rule.group;
                true
            });
            if needs_more_events {
                self.loaded_events
                    .retain(|kind| *kind != EventKind::Defensives);
                self.requested_events
                    .retain(|kind| *kind != EventKind::Defensives);
                self.event_failures
                    .retain(|failure| failure.kind != EventKind::Defensives);
            }
        }
        changed
    }

    fn timeline_grid_height(&self) -> f32 {
        let lanes = 1 + DefensiveGroup::ALL
            .into_iter()
            .filter(|group| {
                self.cooldowns.visible(*group) && self.cooldowns.has_enabled_spells(*group)
            })
            .count();
        20.0 + lanes as f32 * 22.0
    }

    fn select(&mut self, pull: Pull) {
        self.marker_sync.reset(None);
        if let Some(review) = &mut self.review {
            if let Ok(cache) = self.marker_cache.lock() {
                if let Some(alignment) = cache.get(&review.replay, &pull) {
                    review
                        .marker_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                }
            }
        }
        let Some(review) = &self.review else {
            return;
        };
        let seconds = pull_video_start(review, &pull).max(0.0);
        let playback = Playback {
            seconds,
            autoplay: true,
            broadcast_id: review.replay.broadcast_id.clone(),
            public_url: review.replay.public_url(seconds as u64),
        };
        self.cancel_read();
        self.pending_focus = None;
        self.playback = Some(playback);
        self.aligning = false;
        self.pull = Some(pull);
        self.events.clear();
        self.requested_events.clear();
        self.loaded_events.clear();
        self.event_failures.clear();
        self.selected_event = None;
        self.scroll_to_event = false;
        self.scrub = None;
        self.reset_playback_range();
    }
    fn seek_absolute(&mut self, at_ms: i64) -> Option<PlaybackCommand> {
        self.seek_absolute_with_playback(at_ms, true)
    }
    fn watch_event(&mut self, event: RaidEvent) -> Option<PlaybackCommand> {
        // Both the event list and timeline promise the selected moment.
        // Context before an event is available through the ordinary seek bar.
        let command = self.seek_absolute(event.at_ms)?;
        self.selected_event = Some((event.at_ms, event.actor));
        Some(command)
    }
    fn seek_absolute_with_playback(
        &mut self,
        at_ms: i64,
        autoplay: bool,
    ) -> Option<PlaybackCommand> {
        if let Some((review, pull)) = self.review.as_mut().zip(self.pull.as_ref()) {
            if let Ok(cache) = self.marker_cache.lock() {
                if let Some(alignment) = cache.get(&review.replay, pull) {
                    review
                        .marker_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                }
            }
        }
        let review = self.review.as_ref()?;
        let pull = self.pull.as_ref()?;
        let seconds = review.pull_video_start(pull) + (at_ms - pull.start_ms) as f64 / 1000.0;
        if !seconds.is_finite()
            || seconds < 0.0
            || seconds >= review.replay.available_seconds as f64
        {
            return None;
        }
        let playback = self.playback.as_mut()?;
        playback.seconds = seconds;
        playback.autoplay = autoplay;
        playback.public_url = review.replay.public_url(seconds as u64);
        self.reset_playback_range();
        Some(if autoplay {
            PlaybackCommand::Seek(seconds)
        } else {
            PlaybackCommand::SeekPaused(seconds)
        })
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, stream: &Stream) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            if self.connected {
                let count = self.review.as_ref().map(|r| r.pulls.len()).unwrap_or(0);
                if ui
                    .add_enabled(
                        count > 0,
                        egui::Button::new(format!("Review raid · {count} pulls")),
                    )
                    .clicked()
                {
                    let pull = self.review.as_ref().unwrap().pulls[0].clone();
                    self.select(pull);
                    self.active = true;
                    changed = true;
                }
                if self.work.is_some() {
                    ui.small("Finding raid pulls…");
                }
                let menu = ui.menu_button("…", |ui| {
                    if ui
                        .add_enabled(
                            self.work.is_none(),
                            egui::Button::new("Disconnect Warcraft Logs"),
                        )
                        .clicked()
                    {
                        self.start(ui.ctx(), Some(stream), Action::Disconnect);
                        ui.close();
                    }
                });
                self.popup_open = menu.inner.is_some();
            } else {
                self.draw_connection_control(ui, stream);
            }
        });
        if let Some(notice) = &self.notice {
            ui.add(egui::Label::new(RichText::new(notice).small().color(MUTED)).truncate())
                .on_hover_text(notice);
        }
        changed
    }

    fn draw_connection_control(&mut self, ui: &mut egui::Ui, stream: &Stream) {
        if self.signing_in {
            ui.small("Finish signing in in your browser…");
        } else if !self.connection_checked {
            ui.small("Loading raid review…");
        } else if ui
            .add_enabled(
                self.work.is_none(),
                egui::Button::new("Connect Warcraft Logs"),
            )
            .clicked()
        {
            self.notice = None;
            self.start(ui.ctx(), Some(stream), Action::Connect);
        }
    }

    fn close_review(&mut self) {
        self.cancel_read();
        self.active = false;
        self.open_first_pull = false;
        self.playback = None;
        self.pull = None;
        self.events.clear();
        self.requested_events.clear();
        self.loaded_events.clear();
        self.event_failures.clear();
        self.selected_event = None;
        self.pending_focus = None;
        self.popup_open = false;
    }

    /// Fullscreen uses the same pull selection and native seek as the workspace.
    /// This returns a command without closing or rebuilding either video child.
    pub(crate) fn draw_fullscreen_navigation(
        &mut self,
        ui: &mut egui::Ui,
    ) -> Option<PlaybackCommand> {
        let pulls = self
            .review
            .as_ref()
            .map(|review| review.pulls.as_slice())
            .unwrap_or_default();
        let current = self.pull.as_ref().and_then(|selected| {
            pulls
                .iter()
                .position(|pull| pull.report == selected.report && pull.id == selected.id)
        });
        let mut chosen = None;
        let button_width =
            (ui.available_width() - 56.0 - 2.0 * ui.spacing().item_spacing.x).max(80.0);
        if ui
            .add_enabled_ui(current.is_some_and(|index| index > 0), |ui| {
                ui.add_sized(egui::vec2(28.0, 32.0), egui::Button::new("‹"))
            })
            .inner
            .on_hover_text("Previous pull")
            .clicked()
        {
            chosen = current.map(|index| index - 1);
        }
        chosen = draw_pull_selector_sized(
            ui,
            pulls,
            current,
            &mut self.pull_menu_cursor,
            None,
            button_width,
        )
        .or(chosen);
        if ui
            .add_enabled_ui(current.is_some_and(|index| index + 1 < pulls.len()), |ui| {
                ui.add_sized(egui::vec2(28.0, 32.0), egui::Button::new("›"))
            })
            .inner
            .on_hover_text("Next pull")
            .clicked()
        {
            chosen = current.map(|index| index + 1);
        }
        self.popup_open = egui::Popup::is_any_open(ui.ctx());
        let selected = chosen
            .filter(|index| Some(*index) != current)
            .and_then(|index| pulls.get(index))
            .cloned()?;
        self.navigate_pull(selected)
    }

    fn navigate_pull(&mut self, pull: Pull) -> Option<PlaybackCommand> {
        self.select(pull);
        self.playback
            .as_ref()
            .map(|playback| PlaybackCommand::Seek(playback.seconds))
    }

    pub fn draw_workspace(
        &mut self,
        ui: &mut egui::Ui,
        stream: &Stream,
        povs: &[Stream],
        state: &PlaybackState,
        native_player_present: bool,
        player_error: Option<&str>,
        mut comparison: Option<&mut crate::review_compare_ui::Comparison>,
    ) -> WorkspaceAction {
        let mut action = WorkspaceAction::default();
        ui.visuals_mut().selection.bg_fill = Color32::from_rgb(113, 52, 33);
        ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
        ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 34, 42);
        self.observe_provider_playback(state);
        ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_rgb(30, 34, 42);

        if !self.connected {
            // A saved recording can be the first thing someone opens. An unset
            // connection is still unknown until its existing background read finishes.
            // Keep this state static and leave navigation available.
            self.popup_open = false;
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 32.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui
                        .button(if stream.recording_id.is_some() {
                            "Back to VODs"
                        } else {
                            "Back to streams"
                        })
                        .clicked()
                    {
                        self.close_review();
                        action.reload = true;
                    }
                },
            );
            ui.add_space((ui.available_height() * 0.2).min(100.0));
            ui.vertical_centered(|ui| {
                if self.connection_checked {
                    ui.heading("Connect your Warcraft Logs account");
                    ui.add_space(8.0);
                    ui.label("Sign in to load this VOD's raid pulls and timeline.");
                    ui.add_space(16.0);
                    self.draw_connection_control(ui, stream);
                } else {
                    ui.heading("Loading raid review…");
                }
                if let Some(notice) = &self.notice {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(notice).small().color(MUTED)).wrap());
                }
            });
            return action;
        }

        let pulls = self
            .review
            .as_ref()
            .map(|r| r.pulls.as_slice())
            .unwrap_or_default();
        let index = self.pull.as_ref().and_then(|p| {
            pulls
                .iter()
                .position(|item| item.report == p.report && item.id == p.id)
        });
        let mut selected = None;
        let mut leave = false;
        let mut disconnect = false;
        self.popup_open = false;
        let pending_label = self.pending_focus.as_ref().map(|(pull, _, _)| {
            format!(
                "{} · {}",
                pull.name,
                if self.review.is_some() {
                    "Not in this POV"
                } else {
                    "Opening POV…"
                }
            )
        });
        ui.scope(|ui| {
            // Establish the row height before horizontal layout is created. Nested
            // dropdown scopes otherwise start at the smaller default interaction
            // height and sit below their neighboring buttons with Brick's padding.
            ui.spacing_mut().interact_size.y = 32.0;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(index.is_some_and(|i| i > 0), egui::Button::new("‹"))
                    .clicked()
                {
                    selected = index.map(|i| i - 1);
                }
                selected = draw_pull_selector(
                    ui,
                    pulls,
                    index,
                    &mut self.pull_menu_cursor,
                    pending_label.as_deref(),
                )
                .or(selected);
                if ui
                    .add_enabled(
                        index.is_some_and(|i| i + 1 < pulls.len()),
                        egui::Button::new("›"),
                    )
                    .clicked()
                {
                    selected = index.map(|i| i + 1);
                }
                if let Some(pull) = &self.pull {
                    if !pull.kill {
                        if let Some(remaining) = pull.remaining {
                            ui.label(
                                RichText::new(format!("{remaining:.1}% remaining")).color(DEATH),
                            );
                        }
                    }
                    ui.label(
                        RichText::new(clock((pull.end_ms - pull.start_ms) as f64 / 1000.0))
                            .color(MUTED),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let menu = ui.menu_button("…", |ui| {
                        if let Some(pull) = &self.pull {
                            ui.hyperlink_to("Open this pull in Warcraft Logs", pull.log_url());
                        }
                        if let Some(playback) = &self.playback {
                            ui.hyperlink_to("Open video in browser", &playback.public_url);
                        }
                        if ui
                            .add_enabled(
                                self.work.is_none(),
                                egui::Button::new("Disconnect Warcraft Logs"),
                            )
                            .clicked()
                        {
                            disconnect = true;
                            ui.close();
                        }
                    });
                    self.popup_open |= menu.inner.is_some();
                    if ui
                        .button(if stream.recording_id.is_some() {
                            "Back to VODs"
                        } else {
                            "Back to streams"
                        })
                        .clicked()
                    {
                        leave = true;
                    }
                });
            });
        });
        // Query actual popup memory: an InnerResponse also exists on the frame
        // a popup closes; use its actual state for marker synchronization.
        self.popup_open = egui::Popup::is_any_open(ui.ctx());
        if let Some(i) = selected.filter(|i| Some(*i) != index) {
            action.command = self.navigate_pull(pulls[i].clone());
        }
        let coverage_context = self.pov_selection_context(state).map(|(pull, at_ms)| {
            let at_ms = if action.command.is_some() {
                at_ms
            } else {
                comparison
                    .as_deref()
                    .map_or(at_ms, |peer| peer.position().0)
            };
            (pull.clone(), at_ms)
        });
        let covered: Vec<bool> = povs
            .iter()
            .map(|candidate| {
                coverage_context
                    .as_ref()
                    .is_some_and(|(pull, at_ms)| self.pov_covers_moment(candidate, pull, *at_ms))
            })
            .collect();
        let available: Vec<usize> = covered
            .iter()
            .enumerate()
            .filter_map(|(index, covered)| covered.then_some(index))
            .collect();
        self.pov_menu.covered = Some(covered);
        ui.add_space(5.0);
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new("POV").small().color(MUTED));
                if let Some(index) = draw_pov_selector(ui, povs, stream, &mut self.pov_menu) {
                    self.capture_pov_position(state);
                    action.stream = Some(povs[index].clone());
                }

                if let Some(comparison) = comparison.as_deref_mut() {
                    action.close_comparison =
                        comparison.draw_inline(ui, povs, stream, &self.pov_menu.labels, &available);
                }
                if !self.comparing
                    && ui
                        .add_enabled(
                            self.comparison_context().is_some()
                                && available.iter().any(|index| {
                                    let candidate = &povs[*index];
                                    pov_key(candidate) != pov_key(stream)
                                        && (candidate.recording_id.is_some()
                                            || candidate.status == Status::Live)
                                }),
                            egui::Button::new("Compare POVs"),
                        )
                        .clicked()
                {
                    action.compare = true;
                }
            });
        });
        self.popup_open = egui::Popup::is_any_open(ui.ctx());
        ui.add_space(7.0);
        if let Some(notice) = &self.notice {
            ui.add(egui::Label::new(RichText::new(notice).small().color(MUTED)).truncate());
        }
        // Allocate the workspace once. A long player/ability name must never push
        // the media child or controls beyond the native window's bounds.
        let (workspace, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let rail_width = if self.comparing {
            0.0
        } else if workspace.width() >= 1100.0 {
            270.0
        } else {
            235.0
        };
        let gap = if self.comparing { 0.0 } else { 14.0 };
        let left = egui::Rect::from_min_max(
            workspace.min,
            egui::pos2(workspace.right() - rail_width - gap, workspace.bottom()),
        );
        let timeline_height = 30.0 + ui.spacing().item_spacing.y + self.timeline_grid_height();
        let video = egui::Rect::from_min_max(
            left.min,
            egui::pos2(
                left.right(),
                (left.bottom() - timeline_height - 8.0).max(left.top() + 100.0),
            ),
        );
        ui.painter()
            .rect_filled(video, 5.0, Color32::from_rgb(10, 12, 16));
        if !native_player_present {
            if let Some(error) = player_error {
                ui.scope_builder(egui::UiBuilder::new().max_rect(video.shrink(24.0)), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((video.height() / 2.0 - 55.0).max(0.0));
                        ui.label(error);
                        if let Some(playback) = &self.playback {
                            ui.hyperlink_to("Open video in browser", &playback.public_url);
                        }
                    });
                });
            } else {
                ui.painter().text(
                    video.center(),
                    egui::Align2::CENTER_CENTER,
                    if self.playback.is_some() {
                        "Opening replay…"
                    } else if self.pending_focus.is_some() && self.review.is_some() {
                        "This POV does not contain the selected moment"
                    } else if self.pending_focus.is_some() {
                        "Finding this view…"
                    } else if self
                        .review
                        .as_ref()
                        .is_some_and(|review| review.pulls.is_empty())
                    {
                        "No raid pulls are available for this view"
                    } else if self.work.is_some() {
                        "Finding raid pulls…"
                    } else {
                        "Choose a pull to watch"
                    },
                    egui::FontId::proportional(15.0),
                    MUTED,
                );
            }
        }
        action.rect = Some(video);
        let timeline =
            egui::Rect::from_min_max(egui::pos2(left.left(), video.bottom() + 8.0), left.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(timeline), |ui| {
            ui.set_clip_rect(timeline.intersect(ui.clip_rect()));
            if let Some(command) = self.draw_timeline(ui, state) {
                action.command = Some(command);
            }
        });
        let rail = egui::Rect::from_min_max(
            egui::pos2(left.right() + gap, workspace.top()),
            workspace.max,
        );
        if !self.comparing {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rail), |ui| {
                ui.set_clip_rect(rail.intersect(ui.clip_rect()));
                if let Some(command) = self.draw_events(ui, rail.height()) {
                    action.command = Some(command);
                }
            });
        }
        self.draw_cooldown_editor(ui.ctx());
        if self.work.is_none() {
            if let Some(preferences) = self.cooldown_save.take() {
                self.start(ui.ctx(), Some(stream), Action::SaveCooldowns(preferences));
            }
        }
        if action.stream.is_some() {
            self.open_first_pull = false;
            self.cancel_read();
            self.playback = None;
            self.review = None;
            self.replay_coverage = None;
            self.pull = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.event_failures.clear();
            self.selected_event = None;
        }
        if leave || disconnect {
            self.close_review();
            action.reload = true;
            if disconnect {
                self.start(ui.ctx(), Some(stream), Action::Disconnect);
            }
        }
        if action.command.is_some() {
            self.marker_sync.cancel(None);
        }
        if let Some(playback) = &mut self.playback {
            match action.command {
                Some(PlaybackCommand::Play | PlaybackCommand::Seek(_)) => playback.autoplay = true,
                Some(PlaybackCommand::Pause | PlaybackCommand::SeekPaused(_)) => {
                    playback.autoplay = false
                }
                None => (),
            }
        }
        action
    }

    fn capture_pov_position(&mut self, state: &PlaybackState) {
        // While a POV is loading or cannot show the requested moment, retain
        // the existing intent for the next switch instead of inventing a time.
        if self.playback.is_none() {
            return;
        }
        if let Some((elapsed, autoplay)) = self.marker_sync.intent() {
            if let Some(pull) = self.pull.clone() {
                let at_ms = pull.start_ms + (elapsed * 1000.0).round() as i64;
                self.pending_focus = Some((pull, at_ms, autoplay));
            }
            return;
        }
        if let (Some(pull), Some(review)) = (&self.pull, &self.review) {
            // Opening review establishes a requested moment before the provider
            // supplies its first current sample. An old/live sample must not
            // replace that moment during an immediate POV switch.
            let current = state.ready && state.is_fresh_since(self.range_epoch);
            let seconds = if current {
                state.seeking.unwrap_or(state.seconds)
            } else {
                self.playback
                    .as_ref()
                    .map(|playback| playback.seconds)
                    .unwrap_or(pull_video_start(review, pull))
            };
            self.pending_focus = Some((
                pull.clone(),
                encounter_moment(review, pull, seconds),
                state.playback_intent.unwrap_or_else(|| {
                    if current {
                        playback_intent(state, self.playback.as_ref())
                    } else {
                        self.playback
                            .as_ref()
                            .is_none_or(|playback| playback.autoplay)
                    }
                }),
            ));
        }
    }

    fn restore_pov_position(&mut self) -> bool {
        let Some((wanted, at_ms, autoplay)) = self.pending_focus.clone() else {
            return false;
        };
        let found = self.review.as_ref().and_then(|review| {
            review
                .pulls
                .iter()
                .find(|pull| {
                    pull.encounter == wanted.encounter
                        && pull.difficulty == wanted.difficulty
                        && (pull.start_ms - wanted.start_ms).abs() < 3000
                })
                .cloned()
        });
        if let Some(pull) = found {
            self.select(pull);
            if self.seek_absolute_with_playback(at_ms, autoplay).is_some() {
                self.notice = None;
                return true;
            }
            self.playback = None;
            self.notice = Some("This POV does not contain the selected moment.".into());
        } else {
            self.notice = Some("This POV does not contain the selected pull.".into());
        }
        // A missing recording does not clear the user's pull/time. Another
        // available POV must restore it even after several immediate switches.
        self.pending_focus = Some((wanted, at_ms, autoplay));
        true
    }

    fn draw_timeline(
        &mut self,
        ui: &mut egui::Ui,
        state: &PlaybackState,
    ) -> Option<PlaybackCommand> {
        let Some(pull) = self.pull.clone() else {
            if self.playback.is_some() {
                ui.label(RichText::new("Between raid pulls").color(MUTED));
            }
            return None;
        };
        let duration = (pull.end_ms - pull.start_ms) as f64 / 1000.0;
        let video_start = pull_video_start(self.review.as_ref()?, &pull);
        let confirmed_position = self.confirmed_video_position(state);
        if let Some(seconds) = confirmed_position {
            self.timeline_position = Some(seconds);
        }
        // A seek target is useful feedback even before the decoder reaches it.
        // Keep it visible, then hold the last observed position during buffering;
        // neither value acknowledges a seek or advances the playback clock.
        let display_position = state
            .seeking
            .filter(|seconds| seconds.is_finite() && state.is_fresh_since(self.range_epoch))
            .or(confirmed_position)
            .or(self.timeline_position)
            .or_else(|| self.playback.as_ref().map(|playback| playback.seconds))
            .filter(|seconds| seconds.is_finite());
        let elapsed = display_position
            .map(|seconds| seconds - video_start)
            .unwrap_or(0.0);
        let current = elapsed.clamp(0.0, duration);
        let mut command = None;
        let playing = !state.blocked && playback_intent(state, self.playback.as_ref());
        let replay = confirmed_position.is_some() && elapsed >= duration && !self.aligning;
        let mut seek_range = None;
        let mut cursor_position = self.scrub.unwrap_or(current);
        let label_gutter = ui.next_widget_position().x + 176.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(8.0, 6.0);
            if ui
                .add_enabled_ui(state.ready, |ui| {
                    ui.add_sized(
                        egui::vec2(64.0, 30.0),
                        egui::Button::new(if replay {
                            "Replay"
                        } else if playing {
                            "Pause"
                        } else {
                            "Play"
                        }),
                    )
                })
                .inner
                .on_hover_text(if replay {
                    "Watch this pull from its start"
                } else if playing {
                    "Pause"
                } else {
                    "Play"
                })
                .clicked()
            {
                command = if replay {
                    self.seek_absolute(pull.start_ms)
                } else {
                    Some(if playing {
                        PlaybackCommand::Pause
                    } else {
                        PlaybackCommand::Play
                    })
                };
            }
            ui.add_sized(
                egui::vec2(92.0, 30.0),
                egui::Label::new(
                    RichText::new(format!(
                        "{} / {}",
                        if self.scrub.is_some() || display_position.is_some() {
                            relative_clock(self.scrub.unwrap_or(elapsed))
                        } else {
                            "–:––".into()
                        },
                        clock(duration)
                    ))
                    .size(12.0)
                    .color(MUTED),
                )
                .truncate(),
            );
            let mut position = self.scrub.unwrap_or(current);
            ui.add_space((label_gutter - ui.next_widget_position().x).max(0.0));
            ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(60.0);
            let response = ui.add_enabled(
                state.ready,
                egui::Slider::new(&mut position, 0.0..=duration)
                    .show_value(false)
                    .handle_shape(egui::style::HandleShape::Circle)
                    .smart_aim(false),
            );
            // egui places a circular slider handle inside its allocation by
            // height / 2.5. Use that same travel range for the event timeline,
            // so its cursor and every clickable time line up with the thumb.
            seek_range = Some(response.rect.x_range().shrink(response.rect.height() / 2.5));
            response.widget_info(|| egui::WidgetInfo::slider(state.ready, position, "Pull time"));
            response.clone().on_hover_text("Seek within this pull");
            if response.dragged() {
                self.scrub = Some(position);
            }
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                self.scrub = None;
                command = self.seek_absolute_with_playback(
                    pull.start_ms + (position * 1000.0).round() as i64,
                    playback_intent(state, self.playback.as_ref()),
                );
            }
            cursor_position = position;
        });

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), self.timeline_grid_height()),
            egui::Sense::click(),
        );
        let seek_range = seek_range?;
        let grid = egui::Rect::from_min_max(
            egui::pos2(seek_range.min, rect.top() + 20.0),
            egui::pos2(seek_range.max, rect.bottom()),
        );
        let painter = ui.painter();
        for i in 0..=4 {
            let x = egui::lerp(grid.x_range(), i as f32 / 4.0);
            painter.text(
                egui::pos2(x, rect.top() + 3.0),
                if i == 4 {
                    egui::Align2::RIGHT_TOP
                } else {
                    egui::Align2::LEFT_TOP
                },
                clock(duration * i as f64 / 4.0),
                egui::FontId::proportional(10.0),
                MUTED,
            );
            painter.line_segment(
                [egui::pos2(x, grid.top()), egui::pos2(x, grid.bottom())],
                egui::Stroke::new(1.0_f32, Color32::from_rgb(39, 43, 51)),
            );
        }
        let lanes = [
            ("Deaths", None, DEATH),
            (
                "Personal defensives",
                Some(DefensiveGroup::Personal),
                Color32::from_rgb(109, 167, 233),
            ),
            (
                "Externals",
                Some(DefensiveGroup::External),
                Color32::from_rgb(185, 145, 222),
            ),
            (
                "Healing CDs",
                Some(DefensiveGroup::Healing),
                Color32::from_rgb(99, 193, 159),
            ),
            (
                "Damage reduction",
                Some(DefensiveGroup::DamageReduction),
                Color32::from_rgb(230, 184, 96),
            ),
            (
                "Buffs / utility",
                Some(DefensiveGroup::Utility),
                Color32::from_rgb(112, 196, 210),
            ),
        ];
        let mut chosen = None;
        for (lane, (label, group, color)) in lanes
            .iter()
            .filter(|(_, group, _)| {
                group.is_none_or(|group| {
                    self.cooldowns.visible(group) && self.cooldowns.has_enabled_spells(group)
                })
            })
            .enumerate()
        {
            let y = grid.top() + lane as f32 * 22.0 + 11.0;
            painter.text(
                egui::pos2(rect.left(), y),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(11.0),
                *color,
            );
            let lane_rect = egui::Rect::from_min_max(
                egui::pos2(grid.left(), y - 11.0),
                egui::pos2(grid.right(), y + 11.0),
            );
            painter.rect_filled(
                lane_rect,
                3.0,
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 10),
            );
            let events: Vec<_> = self
                .events
                .iter()
                .filter(|event| {
                    event.group == *group && (group.is_some() || event.kind == EventKind::Deaths)
                })
                .collect();
            if events.is_empty()
                && self.loaded_events.contains(&if group.is_none() {
                    EventKind::Deaths
                } else {
                    EventKind::Defensives
                })
            {
                painter.text(
                    egui::pos2(grid.left() + 7.0, y),
                    egui::Align2::LEFT_CENTER,
                    "None recorded",
                    egui::FontId::proportional(10.0),
                    MUTED,
                );
            }
            // Nearby markers share a readable count and a larger hit target.
            let mut clusters: Vec<(f32, Vec<&RaidEvent>)> = Vec::new();
            for event in events {
                let x = grid.left()
                    + ((event.at_ms - pull.start_ms) as f64 / 1000.0 / duration) as f32
                        * grid.width();
                if let Some((last_x, cluster)) =
                    clusters.last_mut().filter(|(last_x, _)| x - *last_x < 17.0)
                {
                    let _ = last_x;
                    cluster.push(event);
                } else {
                    clusters.push((x, vec![event]));
                }
            }
            for (n, (x, events)) in clusters.iter().enumerate() {
                let center = egui::pos2(*x, y);
                let marker = ui.interact(
                    egui::Rect::from_center_size(center, egui::vec2(20.0, 24.0)),
                    ui.id().with(("event", lane, n, events[0].ability_id)),
                    egui::Sense::click(),
                );
                let selected = events.iter().any(|e| {
                    self.selected_event
                        .as_ref()
                        .is_some_and(|(at, actor)| *at == e.at_ms && actor == &e.actor)
                });
                painter.circle_filled(
                    center,
                    if selected || marker.hovered() || marker.has_focus() {
                        8.0
                    } else {
                        6.0
                    },
                    *color,
                );
                if events.len() > 1 {
                    painter.text(
                        center,
                        egui::Align2::CENTER_CENTER,
                        events.len(),
                        egui::FontId::proportional(9.0),
                        Color32::from_rgb(15, 18, 24),
                    );
                } else if group.is_none() {
                    let stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(15, 18, 24));
                    for delta in [egui::vec2(2.0, 2.0), egui::vec2(2.0, -2.0)] {
                        painter.line_segment([center - delta, center + delta], stroke);
                    }
                }
                marker.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        format!(
                            "{} at {}. {} events. Watch this moment",
                            event_description(events[0]),
                            clock((events[0].at_ms - pull.start_ms) as f64 / 1000.0),
                            events.len()
                        ),
                    )
                });
                if marker.clicked() {
                    chosen = Some((*events[0]).clone());
                }
                marker.on_hover_ui(|ui| {
                    for event in events.iter().take(12) {
                        ui.label(format!(
                            "{}  {}",
                            clock((event.at_ms - pull.start_ms) as f64 / 1000.0),
                            event_description(event)
                        ));
                    }
                    if events.len() > 12 {
                        ui.small(format!("{} more events", events.len() - 12));
                    }
                    ui.small("Click to watch this moment");
                });
            }
        }
        // Keep the cursor at the requested moment while seeking. Actual footage
        // outside the pull still has its true elapsed label and no clamped cursor.
        if self.scrub.is_some()
            || (display_position.is_some() && (0.0..=duration).contains(&elapsed))
        {
            let x = grid.left() + cursor_position as f32 / duration as f32 * grid.width();
            painter.line_segment(
                [egui::pos2(x, grid.top()), egui::pos2(x, grid.bottom())],
                egui::Stroke::new(1.5_f32, Color32::WHITE),
            );
        }
        if let Some(event) = chosen {
            self.kind = event.kind;
            self.search.clear();
            self.scroll_to_event = true;
            self.show_all_deaths = true;
            command = self.watch_event(event);
        } else if response.clicked() {
            if let Some(pos) = response
                .interact_pointer_pos()
                .filter(|p| grid.contains(*p))
            {
                command = self.seek_absolute(
                    pull.start_ms
                        + (((pos.x - grid.left()) / grid.width()).clamp(0.0, 1.0) as f64
                            * duration
                            * 1000.0) as i64,
                );
            }
        }
        command.or_else(|| self.pause_at_pull_end(state))
    }

    fn reset_playback_range(&mut self) {
        self.range_epoch = Instant::now();
        self.range_pause_sent = false;
        self.timeline_position = None;
        self.provider_observation = None;
        self.provider_seek_generation = None;
        self.provider_seek_pending = false;
    }

    /// Follow provider controls without issuing playback commands. This runs in
    /// the ordinary player tick, including while the review UI is fullscreen.
    pub(crate) fn observe_provider_playback(&mut self, state: &PlaybackState) {
        if !self.active
            || self.comparing
            || self.aligning
            || self.marker_sync.busy()
            || self.pending_focus.is_some()
            || !state.is_fresh_since(self.range_epoch)
            || !state.seconds.is_finite()
            || !(0.0..=604800.0).contains(&state.seconds)
            || self
                .review
                .as_ref()
                .zip(self.playback.as_ref())
                .is_none_or(|(review, playback)| {
                    review.replay.broadcast_id != playback.broadcast_id
                })
        {
            return;
        }
        let Some([observed_at, _]) = state.observation_window() else {
            return;
        };
        if self
            .provider_observation
            .is_some_and(|(previous, _)| observed_at <= previous)
        {
            return;
        }
        // Initial navigation and Brick's own commands establish a new baseline.
        // A provider media seek caused by those commands is not a user gesture.
        if state.seeking.is_some() || state.playback_intent.is_some() || !state.ready {
            self.provider_observation = None;
            self.provider_seek_generation = state.provider_seek_generation;
            self.provider_seek_pending = false;
            return;
        }
        let generation_changed = self
            .provider_seek_generation
            .zip(state.provider_seek_generation)
            .is_some_and(|(previous, current)| previous != current);
        let jumped = self
            .provider_observation
            .is_some_and(|(previous, seconds)| {
                let elapsed = observed_at
                    .saturating_duration_since(previous)
                    .as_secs_f64();
                // Backwards playback or advancement beyond the observed wall time
                // covers older wrappers and a gesture whose media event was missed.
                state.seconds < seconds - 1.0 || state.seconds > seconds + elapsed * 2.0 + 1.0
            });
        self.provider_seek_pending |= self.provider_observation.is_some()
            && (generation_changed || jumped || state.diagnostics.media_seeking == Some(true));
        self.provider_seek_generation = state.provider_seek_generation;
        self.provider_observation = Some((observed_at, state.seconds));
        if state.blocked || state.buffering || state.diagnostics.media_seeking == Some(true) {
            return;
        }
        self.timeline_position = Some(state.seconds);
        if let Some(playback) = &mut self.playback {
            playback.seconds = state.seconds;
            playback.autoplay = state.playing;
        }
        if !self.provider_seek_pending && self.pull.is_some() {
            return;
        }
        self.provider_seek_pending = false;
        let review = self.review.as_ref().unwrap();
        let contains = |pull: &Pull| {
            let start = pull_video_start(review, pull);
            state.seconds >= start
                && state.seconds < start + (pull.end_ms - pull.start_ms) as f64 / 1000.0
        };
        // Prefer the current pull when metadata ranges overlap. Scan only on a
        // seek or while between pulls; ordinary playback does no catalogue scan.
        let selected = self
            .pull
            .as_ref()
            .filter(|pull| contains(pull))
            .or_else(|| review.pulls.iter().find(|pull| contains(pull)))
            .cloned();
        let changed = self.pull.as_ref().map(pull_key) != selected.as_ref().map(pull_key);
        if changed {
            self.cancel_read();
            self.marker_sync.reset(None);
            self.pull = selected;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.event_failures.clear();
            self.scroll_to_event = false;
        }
        self.selected_event = None;
        self.scrub = None;
        self.range_pause_sent = false;
        // Keep the observation's epoch, position and play intent. Calling
        // select() here would turn the user's seek into a seek to pull start.
        if let Some((review, playback)) = self.review.as_ref().zip(self.playback.as_mut()) {
            playback.public_url = review.replay.public_url(state.seconds as u64);
        }
    }

    fn confirmed_video_position(&self, state: &PlaybackState) -> Option<f64> {
        (state.ready
            && state.seconds.is_finite()
            && state.is_fresh_since(self.range_epoch)
            && state.seeking.is_none()
            && !state.blocked
            && !state.buffering)
            .then_some(state.seconds)
    }

    pub(crate) fn pause_at_pull_end(&mut self, state: &PlaybackState) -> Option<PlaybackCommand> {
        if self.comparing || self.marker_sync.busy() || self.provider_seek_pending {
            return None;
        }
        // A prior POV's sample, pending seek or stalled frame must never pause
        // the newly selected replay. Timestamp freshness uses SDK request start.
        if !self.active
            || self.aligning
            || self.playback.is_none()
            || !state.ready
            || !state.seconds.is_finite()
            || !state.is_fresh_since(self.range_epoch)
            || state.seeking.is_some()
            || state.blocked
            || state.buffering
            || state.playback_intent.is_some()
        {
            return None;
        }
        let pull = self.pull.as_ref()?;
        let end = pull_video_start(self.review.as_ref()?, pull)
            + (pull.end_ms - pull.start_ms) as f64 / 1000.0;
        if state.seconds < end - 1.0 {
            // Re-arm on a real return into the pull, avoiding jitter at its end.
            self.range_pause_sent = false;
        }
        if state.playing && state.seconds >= end && !self.range_pause_sent {
            self.range_pause_sent = true;
            Some(PlaybackCommand::Pause)
        } else {
            None
        }
    }

    fn draw_cooldown_editor(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.cooldown_editor.as_mut() else {
            return;
        };
        self.popup_open = true;
        let busy = self.cooldown_save.is_some()
            || matches!(self.work_action, Some(Action::SaveCooldowns(_)));
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let screen = ctx.content_rect();
        // A separate window leaves the player's allocation and playback intact.
        let size = egui::vec2(
            (screen.width() - 56.0).clamp(280.0, 640.0),
            (screen.height() - 96.0).clamp(240.0, 540.0),
        );
        egui::Window::new(RichText::new(format!("Edit {}", editor.group.label())).strong().color(Color32::from_rgb(226, 230, 237)))
            .id(egui::Id::new("cooldown-editor"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .fixed_size(size)
            .default_pos(screen.center() - size * 0.5)
            .constrain_to(screen.shrink(12.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
                ui.spacing_mut().interact_size.y = 26.0;
                ui.label("Check Track to include a spell in this category.");
                if let Some(error) = self.cooldown_notice.as_ref().or(editor.error.as_ref()) {
                    ui.colored_label(DEATH, error);
                }
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut editor.search)
                            .hint_text("Search spells…").char_limit(80)
                            .desired_width((ui.available_width() - 105.0).max(80.0)));
                        let advanced = ui.scope(|ui| {
                            // Egui's default hover expansion changes the painted frame.
                            // Keep this compact search-row control the same size in every state.
                            let widgets = &mut ui.visuals_mut().widgets;
                            widgets.inactive.expansion = 0.0;
                            widgets.hovered.expansion = 0.0;
                            widgets.active.expansion = 0.0;
                            ui.add_sized([80.0, 28.0], egui::Button::new("Advanced")
                                .fill(if editor.advanced { Color32::from_rgb(113, 52, 33) } else { Color32::from_rgb(43, 48, 58) })
                                .stroke(egui::Stroke::new(1.0_f32, if editor.advanced { ACCENT } else { Color32::from_rgb(85, 94, 111) })))
                        }).inner;
                        if advanced.clicked() { editor.advanced = !editor.advanced; }
                    });
                    if editor.advanced {
                        ui.small("Advanced: add a spell ID, change its category or include buff applications.");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut editor.new_id)
                                .hint_text("Warcraft Logs spell ID").char_limit(7).desired_width(190.0));
                            if ui.button("Add spell").clicked() {
                                match defensives::Preferences::parse_id(&editor.new_id) {
                                    Ok(id) => {
                                        editor.error = None;
                                        editor.apply_rule(id, defensives::Rule {
                                            group: Some(editor.group), observation: defensives::Observation::Cast,
                                        });
                                        if editor.error.is_none() {
                                            editor.search = id.to_string();
                                            editor.new_id.clear();
                                        }
                                    }
                                    Err(error) => editor.error = Some(error),
                                }
                            }
                            if ui.button("Reset category").on_hover_text("Restore this category's default spells and cast tracking.").clicked() {
                                let ids: Vec<_> = editor.draft.ids().into_iter()
                                    .filter(|id| editor.category(*id) == editor.group
                                        || editor.draft.catalog.spell(*id).is_some_and(|spell| spell.group == editor.group)).collect();
                                for id in ids { editor.draft.overrides.remove(&id); }
                                editor.remembered_groups.clear();
                                editor.search.clear();
                                editor.error = None;
                            }
                        });
                    }
                    let search = editor.search.to_lowercase();
                    let mut ids: Vec<_> = editor.draft.ids().into_iter().filter(|id| {
                        editor.category(*id) == editor.group && (search.is_empty()
                            || id.to_string().contains(&search)
                            || editor.draft.spell_name(*id).is_some_and(|name| name.to_lowercase().contains(&search))
                            || editor.observed_names.get(id).is_some_and(|names| {
                                names.search.iter().any(|name| name.contains(&search))
                            }))
                    }).collect();
                    ids.sort_by(|a, b| editor.name(*a).cmp(editor.name(*b)).then(a.cmp(b)));
                    let tracked = ids.iter().filter(|id| editor.draft.classify(**id).is_some()).count();
                    ui.label(RichText::new(format!("{tracked} of {} spells tracked", ids.len())).color(MUTED));
                    ui.separator();
                    let row_height = if editor.advanced { 72.0 } else { 30.0 };
                    let list_height = (ui.available_height() - 60.0).max(0.0);
                    egui::ScrollArea::vertical().id_salt("cooldown-rules")
                        .auto_shrink([false, false]).max_height(list_height)
                        .show_rows(ui, row_height, ids.len(), |ui, range| {
                            for index in range {
                                let id = ids[index];
                                let Some(mut rule) = editor.draft.rule(id) else { continue; };
                                let before = rule;
                                let category = editor.category(id);
                                ui.push_id(id, |ui| {
                                    ui.allocate_ui(egui::vec2(ui.available_width(), row_height), |ui| {
                                        ui.horizontal(|ui| {
                                            let mut tracked = rule.group.is_some();
                                            if ui.checkbox(&mut tracked, "Track").changed() {
                                                editor.remembered_groups.insert(id, category);
                                                rule.group = tracked.then_some(category);
                                            }
                                            let name = editor.name(id);
                                            ui.add(egui::Label::new(RichText::new(name).size(15.0).color(Color32::from_rgb(226, 230, 237))).truncate()).on_hover_text(name);
                                        });
                                        if editor.advanced {
                                            ui.horizontal(|ui| {
                                                ui.label(RichText::new(format!("ID {id}")).small().color(MUTED));
                                                let mut group = category;
                                                ui.add_enabled_ui(rule.group.is_some(), |ui| {
                                                egui::ComboBox::from_id_salt("category").selected_text(group.label()).width(155.0)
                                                    .show_ui(ui, |ui| {
                                                        for category in DefensiveGroup::ALL {
                                                            ui.selectable_value(&mut group, category, category.label());
                                                        }
                                                    });
                                                });
                                                if group != category {
                                                    editor.remembered_groups.insert(id, group);
                                                    if rule.group.is_some() { rule.group = Some(group); }
                                                }
                                                egui::ComboBox::from_id_salt("evidence").selected_text(rule.observation.label()).width(168.0)
                                                    .show_ui(ui, |ui| {
                                                        for observation in defensives::Observation::ALL {
                                                            ui.selectable_value(&mut rule.observation, observation, observation.label());
                                                        }
                                                    });
                                                if editor.draft.overrides.contains_key(&id)
                                                    && ui.small_button("Reset").clicked() {
                                                    editor.draft.overrides.remove(&id);
                                                    editor.remembered_groups.remove(&id);
                                                }
                                            });
                                        }
                                    });
                                });
                                if before != rule { editor.apply_rule(id, rule); }
                            }
                        });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    save = ui.add_enabled(!busy, egui::Button::new(RichText::new("Save changes").color(Color32::WHITE)).fill(ACCENT)).clicked();
                    cancel = ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked();
                    if busy { ui.spinner(); ui.label("Saving…"); }
                });
            });
        if save {
            match editor.draft.validate() {
                Ok(()) => {
                    self.cooldown_save = Some(editor.draft.clone());
                    self.cooldown_notice = None;
                }
                Err(error) => editor.error = Some(error),
            }
        }
        if (!open || cancel) && !busy {
            self.cooldown_editor = None;
            self.cooldown_notice = None;
        }
    }

    fn draw_events(&mut self, ui: &mut egui::Ui, _height: f32) -> Option<PlaybackCommand> {
        if self.pull.is_none() && self.playback.is_some() {
            ui.label(
                RichText::new("No raid events at this video position.")
                    .small()
                    .color(MUTED),
            );
            return None;
        }
        ui.horizontal(|ui| {
            for kind in [EventKind::Deaths, EventKind::Defensives] {
                let count = self
                    .events
                    .iter()
                    .filter(|event| {
                        event.kind == kind
                            && (kind == EventKind::Deaths
                                || event.group == Some(self.cooldown_filter))
                    })
                    .count();
                if ui
                    .selectable_label(self.kind == kind, format!("{} ({count})", kind.label()))
                    .clicked()
                {
                    self.kind = kind;
                }
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Find a player or spell")
                .desired_width(f32::INFINITY),
        );
        if self.kind == EventKind::Deaths {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if self.show_all_deaths {
                        "All deaths"
                    } else {
                        "First three deaths"
                    })
                    .small()
                    .color(MUTED),
                );
                if ui
                    .small_button(if self.show_all_deaths {
                        "First three"
                    } else {
                        "Show all"
                    })
                    .clicked()
                {
                    self.show_all_deaths = !self.show_all_deaths;
                }
            });
        } else {
            let selector = egui::ComboBox::from_id_salt("cooldown-category")
                .selected_text(self.cooldown_filter.label())
                .width(ui.available_width() - 16.0)
                .show_ui(ui, |ui| {
                    for group in DefensiveGroup::ALL {
                        ui.selectable_value(&mut self.cooldown_filter, group, group.label());
                    }
                });
            self.popup_open |= selector.inner.is_some();
            let control_height = ui.spacing().interact_size.y.max(
                ui.text_style_height(&egui::TextStyle::Button)
                    + 2.0 * ui.spacing().button_padding.y,
            );
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), control_height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().interact_size.y = control_height;
                    let saving = matches!(self.work_action, Some(Action::SaveCooldowns(_)));
                    let mut visible = self.cooldowns.visible(self.cooldown_filter);
                    if ui
                        .add_enabled(
                            !saving && self.cooldown_editor.is_none(),
                            egui::Checkbox::new(&mut visible, "Show in timeline"),
                        )
                        .changed()
                    {
                        self.cooldowns
                            .hidden_groups
                            .retain(|group| *group != self.cooldown_filter);
                        if !visible {
                            self.cooldowns.hidden_groups.push(self.cooldown_filter);
                        }
                        self.cooldown_save = Some(self.cooldowns.clone());
                        self.cooldown_notice = None;
                    }
                    if ui
                        .add_enabled(
                            !self.loaded_events.is_empty() && !saving,
                            egui::Button::new("Edit"),
                        )
                        .clicked()
                    {
                        self.cooldown_editor = Some(CooldownEditor::new(
                            self.cooldowns.clone(),
                            &self.events,
                            self.cooldown_filter,
                        ));
                        self.cooldown_notice = None;
                    }
                },
            );
            if self.cooldown_editor.is_none() {
                if let Some(notice) = &self.cooldown_notice {
                    ui.label(RichText::new(notice).small().color(DEATH));
                    if ui.small_button("Retry save").clicked() {
                        self.cooldown_save = Some(self.cooldowns.clone());
                        self.cooldown_notice = None;
                    }
                }
            }
        }
        ui.separator();
        if let Some(failure) = self
            .event_failures
            .iter()
            .find(|failure| failure.kind == self.kind)
        {
            ui.label(RichText::new(&failure.message).small().color(MUTED));
            if ui
                .add_enabled(self.work.is_none(), egui::Button::new("Retry"))
                .clicked()
            {
                self.requested_events.retain(|kind| *kind != self.kind);
                self.event_failures
                    .retain(|failure| failure.kind != self.kind);
            }
        } else if !self.loaded_events.contains(&self.kind) {
            ui.spinner();
            ui.small("Loading this pull's events…");
        }
        let search = self.search.to_lowercase();
        let limit = if self.kind == EventKind::Deaths && !self.show_all_deaths && search.is_empty()
        {
            3
        } else {
            usize::MAX
        };
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|e| {
                e.kind == self.kind
                    && (e.kind == EventKind::Deaths || e.group == Some(self.cooldown_filter))
                    && (search.is_empty()
                        || e.actor.to_lowercase().contains(&search)
                        || e.ability.to_lowercase().contains(&search)
                        || e.target
                            .as_ref()
                            .is_some_and(|s| s.to_lowercase().contains(&search)))
            })
            .take(limit)
            .collect();
        if events.is_empty() && self.loaded_events.contains(&self.kind) {
            ui.small("No matching events in this pull.");
        }
        let width = ui.available_width();
        let mut chosen = None;
        let mut scroll = egui::ScrollArea::vertical().id_salt((
            "review-events",
            self.pull.as_ref().map(pull_key),
            self.kind,
        ));
        const ROW_HEIGHT: f32 = 43.0;
        if self.scroll_to_event {
            if let Some(index) = events.iter().position(|event| {
                self.selected_event
                    .as_ref()
                    .is_some_and(|(at, actor)| *at == event.at_ms && actor == &event.actor)
            }) {
                scroll = scroll.vertical_scroll_offset(
                    index as f32 * (ROW_HEIGHT + ui.spacing().item_spacing.y),
                );
            }
            self.scroll_to_event = false;
        }
        scroll.max_height(ui.available_height().max(0.0)).show_rows(
            ui,
            ROW_HEIGHT,
            events.len(),
            |ui, range| {
                for i in range {
                    let event = events[i];
                    let offset = self
                        .pull
                        .as_ref()
                        .map(|p| (event.at_ms - p.start_ms) as f64 / 1000.0)
                        .unwrap_or(0.0);
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());
                    let selected = self
                        .selected_event
                        .as_ref()
                        .is_some_and(|(at, actor)| *at == event.at_ms && actor == &event.actor);
                    if selected || response.hovered() || response.has_focus() {
                        ui.painter()
                            .rect_filled(rect, 4.0, Color32::from_rgb(37, 42, 51));
                    }
                    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                    painter.text(
                        egui::pos2(rect.right() - 5.0, rect.top() + 7.0),
                        egui::Align2::RIGHT_TOP,
                        clock(offset),
                        egui::FontId::proportional(11.0),
                        MUTED,
                    );
                    let text_rect = egui::Rect::from_min_max(
                        rect.min + egui::vec2(7.0, 0.0),
                        egui::pos2(rect.right() - 49.0, rect.bottom()),
                    );
                    let text_painter = painter.with_clip_rect(text_rect.intersect(ui.clip_rect()));
                    text_painter.text(
                        text_rect.min + egui::vec2(0.0, 6.0),
                        egui::Align2::LEFT_TOP,
                        &event.actor,
                        egui::FontId::proportional(12.0),
                        class_color(&event.class),
                    );
                    let mut detail = if event.kind == EventKind::Deaths {
                        "Died".into()
                    } else if let Some(target) = &event.target {
                        format!("{} on {}", event.ability, target)
                    } else {
                        event.ability.clone()
                    };
                    if event.observed_buff {
                        detail.push_str(" · buff applied");
                    }
                    text_painter.text(
                        text_rect.min + egui::vec2(0.0, 24.0),
                        egui::Align2::LEFT_TOP,
                        detail,
                        egui::FontId::proportional(11.0),
                        MUTED,
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!(
                                "{} at {}. Watch this moment",
                                event_description(event),
                                clock(offset)
                            ),
                        )
                    });
                    if response.clicked() {
                        chosen = Some((*event).clone());
                    }
                    response.on_hover_text(format!(
                        "{} · {}
Click to watch this moment",
                        clock(offset),
                        event_description(event)
                    ));
                }
            },
        );
        chosen.and_then(|event| self.watch_event(event))
    }
}
#[derive(Clone)]
pub(crate) struct RecordingLabel {
    pub when: String,
    pub title: String,
}
pub(crate) type RecordingLabels = std::collections::HashMap<String, RecordingLabel>;
pub(crate) fn recording_label<'a>(
    labels: &'a RecordingLabels,
    stream: &Stream,
) -> Option<&'a RecordingLabel> {
    let id = stream.recording_id.as_deref().or_else(|| {
        (stream.provider == crate::streams::Provider::Youtube).then_some(stream.channel_id.as_str())
    })?;
    labels.get(&format!("{}:{id}", stream.provider.key()))
}
pub(crate) fn pov_display_label(labels: &RecordingLabels, stream: &Stream) -> String {
    let base = format!("{} · {}", stream.name, stream.provider.label());
    recording_label(labels, stream).map_or(base.clone(), |recording| {
        format!("{base} · {}", recording.when)
    })
}

#[derive(Default)]
struct PovMenuState {
    search: String,
    cursor: Option<String>,
    labels: RecordingLabels,
    covered: Option<Vec<bool>>,
}

fn pov_key(stream: &Stream) -> String {
    format!(
        "{}:{}:{}:{}",
        stream.user_id,
        stream.provider.key(),
        stream.channel_id,
        stream.recording_id.as_deref().unwrap_or("")
    )
}

/// Match the protected replay mapper's pull-start rule, then check the actual
/// requested video instant using the API estimate. Ends are exclusive.
pub(crate) fn recording_covers_moment(range: (i64, i64), pull_start_ms: i64, at_ms: i64) -> bool {
    let (start, end) = range;
    if start < 0
        || end <= start
        || end
            .checked_sub(start)
            .is_none_or(|duration| duration > 7 * 86_400_000)
        || !(start..end).contains(&pull_start_ms)
    {
        return false;
    }
    (start..end).contains(&at_ms)
}

fn pov_unavailable(stream: &Stream, current: &Stream) -> Option<&'static str> {
    if pov_key(stream) == pov_key(current) {
        return None;
    }
    if stream.recording_id.is_some() {
        return None;
    }
    match stream.status {
        Status::Live => None,
        Status::Checking => Some("Brick is checking whether this stream is live."),
        Status::Unknown => Some("This stream's status is unavailable. Brick will check again."),
        Status::Offline => Some(match stream.broadcast_state.as_deref() {
            Some("upcoming") => "This broadcast hasn't started yet.",
            Some("ended") => "This broadcast has ended. Its replay isn't available in this view.",
            _ => "This player is offline.",
        }),
    }
}

fn matching_povs<'a>(povs: &'a [Stream], search: &str) -> Vec<(usize, &'a Stream)> {
    let search = search.trim().to_lowercase();
    let mut rows: Vec<_> = povs
        .iter()
        .enumerate()
        .filter(|(_, pov)| {
            let haystack = format!("{} {}", pov.name, pov.provider.label()).to_lowercase();
            search
                .split_whitespace()
                .all(|word| haystack.contains(word))
        })
        .collect();
    rows.sort_by_cached_key(|(_, pov)| {
        (
            crate::profile::role_order(pov.raid_role),
            pov.name.to_lowercase(),
            pov.provider.key(),
            pov.user_id.clone(),
        )
    });
    rows
}

fn draw_pov_selector(
    ui: &mut egui::Ui,
    povs: &[Stream],
    current: &Stream,
    menu: &mut PovMenuState,
) -> Option<usize> {
    let popup_id = ui.make_persistent_id("review-pov-menu");
    let was_open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    if !was_open {
        menu.search.clear();
        menu.cursor = Some(pov_key(current));
    }
    if povs.is_empty() && was_open {
        egui::Popup::close_id(ui.ctx(), popup_id);
    }
    let label = pov_display_label(&menu.labels, current);
    let button = ui
        .add_enabled_ui(!povs.is_empty(), |ui| {
            ui.add_sized(
                egui::vec2(320.0, ui.spacing().interact_size.y),
                egui::Button::new(&label)
                    .right_text("   ")
                    .selected(true)
                    .truncate(),
            )
        })
        .inner;
    dropdown_indicator(ui, &button);
    button.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::ComboBox,
            !povs.is_empty(),
            format!("Switch POV. Watching {label}"),
        )
    });
    let bounds = ui.ctx().content_rect();
    let menu_width = 400.0_f32.min(bounds.width() - 24.0).max(160.0);
    let menu_height = 420.0_f32.min((bounds.bottom() - button.rect.bottom() - 16.0).max(80.0));
    let mut chosen = None;
    egui::Popup::menu(&button)
        .id(popup_id)
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[])
        .width(menu_width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(menu_width);
            ui.set_height(menu_height);
            let opening = !was_open;
            let search_id = ui.make_persistent_id("pov-search");
            let mut direction = 0_i32;
            let mut confirm = false;
            let mut boundary = None;
            // Keep typing in the search field while Up/Down chooses a result.
            // Home/End retain their normal text-editing meaning in that field.
            if !opening {
                let searching = ui.memory(|memory| memory.has_focus(search_id));
                ui.input_mut(|input| {
                    let modifiers = egui::Modifiers::NONE;
                    if input.consume_key(modifiers, egui::Key::ArrowDown) {
                        direction = 1;
                    }
                    if input.consume_key(modifiers, egui::Key::ArrowUp) {
                        direction = -1;
                    }
                    confirm = input.consume_key(modifiers, egui::Key::Enter);
                    if !searching && input.consume_key(modifiers, egui::Key::Home) {
                        boundary = Some(false);
                    }
                    if !searching && input.consume_key(modifiers, egui::Key::End) {
                        boundary = Some(true);
                    }
                });
                if direction != 0 || boundary.is_some() {
                    ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
                }
            }
            let search = ui.add(
                egui::TextEdit::singleline(&mut menu.search)
                    .id(search_id)
                    .hint_text("Find a player or platform")
                    .char_limit(80)
                    .desired_width(f32::INFINITY),
            );
            if opening {
                search.request_focus();
            }
            let mut rows = matching_povs(povs, &menu.search);
            rows.retain(|(index, _)| {
                menu.covered
                    .as_ref()
                    .is_none_or(|covered| covered.get(*index) == Some(&true))
            });
            let available: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(_, (_, pov))| pov_unavailable(pov, current).is_none())
                .map(|(index, _)| index)
                .collect();
            if search.changed()
                || !rows.iter().any(|(_, pov)| {
                    menu.cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor == &pov_key(pov))
                })
            {
                menu.cursor = available.first().map(|index| pov_key(rows[*index].1));
            }
            if direction != 0 || boundary.is_some() {
                let previous = available.iter().position(|index| {
                    menu.cursor
                        .as_ref()
                        .is_some_and(|cursor| cursor == &pov_key(rows[*index].1))
                });
                let next = if let Some(last) = boundary {
                    if last {
                        available.len().saturating_sub(1)
                    } else {
                        0
                    }
                } else {
                    match previous {
                        Some(index) => (index as i32 + direction)
                            .clamp(0, available.len().saturating_sub(1) as i32)
                            as usize,
                        None => 0,
                    }
                };
                menu.cursor = available.get(next).map(|index| pov_key(rows[*index].1));
            }
            ui.label(
                RichText::new("Switch POV at the same pull time")
                    .small()
                    .color(MUTED),
            );
            ui.separator();
            if rows.is_empty() {
                ui.label(if menu.search.trim().is_empty() && menu.covered.is_some() {
                    "No other VODs cover this moment."
                } else {
                    "No matching players."
                });
            }
            let row_height = if rows
                .iter()
                .any(|(_, pov)| recording_label(&menu.labels, pov).is_some())
            {
                62.0
            } else {
                44.0
            };
            let stride = row_height + ui.spacing().item_spacing.y;
            let height = ui.available_height().max(row_height);
            let mut scroll = egui::ScrollArea::vertical()
                .id_salt("pov-list")
                .max_height(height)
                .auto_shrink([false, false]);
            if opening || search.changed() || direction != 0 || boundary.is_some() {
                let index = rows
                    .iter()
                    .position(|(_, pov)| {
                        menu.cursor
                            .as_ref()
                            .is_some_and(|cursor| cursor == &pov_key(pov))
                    })
                    .unwrap_or(0);
                scroll = scroll.vertical_scroll_offset(
                    (index as f32 * stride - (height - stride) / 2.0).max(0.0),
                );
            }
            scroll.show_rows(ui, row_height, rows.len(), |ui, range| {
                for index in range {
                    let (source_index, pov) = rows[index];
                    let watching = pov_key(pov) == pov_key(current);
                    let unavailable = pov_unavailable(pov, current);
                    let enabled = unavailable.is_none();
                    let row = ui
                        .add_enabled_ui(enabled, |ui| {
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), row_height),
                                egui::Sense::click(),
                            );
                            let highlighted = menu
                                .cursor
                                .as_ref()
                                .is_some_and(|cursor| cursor == &pov_key(pov));
                            if watching || response.hovered() || response.has_focus() || highlighted
                            {
                                ui.painter().rect_filled(
                                    rect,
                                    4.0,
                                    if watching {
                                        Color32::from_rgb(75, 40, 31)
                                    } else {
                                        Color32::from_rgb(37, 42, 51)
                                    },
                                );
                            }
                            let painter = ui.painter().with_clip_rect(
                                rect.shrink2(egui::vec2(8.0, 0.0)).intersect(ui.clip_rect()),
                            );
                            let text_color = if enabled {
                                ui.visuals().text_color()
                            } else {
                                MUTED
                            };
                            painter.text(
                                rect.min + egui::vec2(8.0, 6.0),
                                egui::Align2::LEFT_TOP,
                                &pov.name,
                                egui::FontId::proportional(13.0),
                                text_color,
                            );
                            let status = if watching {
                                "Watching"
                            } else if pov.recording_id.is_some() {
                                "VOD"
                            } else {
                                match pov.status {
                                    Status::Live => "Live",
                                    Status::Checking => "Checking…",
                                    Status::Unknown => "Status unavailable",
                                    Status::Offline => match pov.broadcast_state.as_deref() {
                                        Some("upcoming") => "Scheduled",
                                        Some("ended") => "Ended",
                                        _ => "Offline",
                                    },
                                }
                            };
                            painter.text(
                                rect.min + egui::vec2(8.0, 25.0),
                                egui::Align2::LEFT_TOP,
                                recording_label(&menu.labels, pov).map_or_else(
                                    || format!("{} · {status}", pov.provider.label()),
                                    |recording| {
                                        format!(
                                            "{} · {status} · {}",
                                            pov.provider.label(),
                                            recording.when
                                        )
                                    },
                                ),
                                egui::FontId::proportional(11.0),
                                if watching { ACCENT } else { MUTED },
                            );
                            if let Some(recording) = recording_label(&menu.labels, pov) {
                                painter.text(
                                    rect.min + egui::vec2(8.0, 43.0),
                                    egui::Align2::LEFT_TOP,
                                    &recording.title,
                                    egui::FontId::proportional(11.0),
                                    MUTED,
                                );
                            }
                            response
                        })
                        .inner;
                    row.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            enabled,
                            format!(
                                "{} · {}. {}",
                                pov.name,
                                pov.provider.label(),
                                unavailable.unwrap_or(if watching {
                                    "Watching this POV"
                                } else {
                                    "Switch to this POV at the same pull time"
                                })
                            ),
                        )
                    });
                    if row.has_focus() && direction == 0 {
                        menu.cursor = Some(pov_key(pov));
                    }
                    if row.clicked()
                        || (confirm
                            && enabled
                            && menu
                                .cursor
                                .as_ref()
                                .is_some_and(|cursor| cursor == &pov_key(pov)))
                    {
                        if !watching {
                            chosen = Some(source_index);
                        }
                        ui.close();
                        button.request_focus();
                    }
                    if let Some(reason) = unavailable {
                        row.on_disabled_hover_text(reason);
                    } else {
                        row.on_hover_text(recording_label(&menu.labels, pov).map_or_else(
                            || pov_display_label(&menu.labels, pov),
                            |recording| {
                                format!(
                                    "{}\n{}",
                                    pov_display_label(&menu.labels, pov),
                                    recording.title
                                )
                            },
                        ));
                    }
                }
            });
        });
    chosen
}

fn dropdown_indicator(ui: &egui::Ui, button: &egui::Response) {
    let center = egui::pos2(button.rect.right() - 14.0, button.rect.center().y);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            center + egui::vec2(-4.0, -2.0),
            center + egui::vec2(4.0, -2.0),
            center + egui::vec2(0.0, 3.0),
        ],
        ui.visuals().text_color(),
        egui::Stroke::NONE,
    ));
}

fn pull_outcome(kill: bool, last_phase: Option<u32>, intermission: bool) -> String {
    if kill {
        "Kill".into()
    } else if let Some(phase) = last_phase {
        format!(
            "Wipe · {} {phase}",
            if intermission {
                "Intermission"
            } else {
                "Phase"
            }
        )
    } else {
        "Wipe".into()
    }
}

/// Give the native menu a stable viewport instead of reusing its last measured
/// height. Opening a late pull also puts that pull in view immediately.
fn draw_pull_selector(
    ui: &mut egui::Ui,
    pulls: &[Pull],
    current: Option<usize>,
    cursor: &mut Option<usize>,
    pending_label: Option<&str>,
) -> Option<usize> {
    draw_pull_selector_sized(ui, pulls, current, cursor, pending_label, 352.0)
}

fn draw_pull_selector_sized(
    ui: &mut egui::Ui,
    pulls: &[Pull],
    current: Option<usize>,
    cursor: &mut Option<usize>,
    pending_label: Option<&str>,
    button_width: f32,
) -> Option<usize> {
    let popup_id = ui.make_persistent_id("review-pull-menu");
    let was_open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    if pulls.is_empty() && was_open {
        egui::Popup::close_id(ui.ctx(), popup_id);
    }
    let current = current.filter(|index| *index < pulls.len());
    if !was_open || cursor.is_none_or(|index| index >= pulls.len()) {
        *cursor = current.or((!pulls.is_empty()).then_some(0));
    }
    let mut moved = false;
    let mut chosen = None;
    if was_open {
        ui.input_mut(|input| {
            let modifiers = egui::Modifiers::NONE;
            if let Some(index) = cursor.as_mut() {
                if input.consume_key(modifiers, egui::Key::ArrowDown) {
                    *index = (*index + 1).min(pulls.len().saturating_sub(1));
                    moved = true;
                }
                if input.consume_key(modifiers, egui::Key::ArrowUp) {
                    *index = index.saturating_sub(1);
                    moved = true;
                }
                if input.consume_key(modifiers, egui::Key::Home) {
                    *index = 0;
                    moved = true;
                }
                if input.consume_key(modifiers, egui::Key::End) {
                    *index = pulls.len().saturating_sub(1);
                    moved = true;
                }
                if input.consume_key(modifiers, egui::Key::Enter) {
                    chosen = Some(*index);
                }
            }
        });
        if moved {
            // egui computes directional focus before widget input is consumed.
            // Cancel that second move after handling navigation in this list.
            ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
        }
    }
    let primary_label = current
        .map(|index| {
            format!(
                "{} · Pull {} of {}",
                pulls[index].name,
                index + 1,
                pulls.len()
            )
        })
        .unwrap_or_else(|| pending_label.unwrap_or("Choose a pull").into());
    let outcome = current.map(|index| {
        let pull = &pulls[index];
        pull_outcome(pull.kill, pull.last_phase, pull.last_phase_is_intermission)
    });
    let label = outcome.as_ref().map_or_else(
        || primary_label.clone(),
        |outcome| format!("{primary_label} · {outcome}"),
    );
    let button = ui
        .add_enabled_ui(!pulls.is_empty(), |ui| {
            ui.add_sized(
                egui::vec2(button_width, ui.spacing().interact_size.y),
                egui::Button::new(&primary_label)
                    .right_text(
                        outcome
                            .as_ref()
                            .map_or_else(|| "   ".into(), |outcome| format!("{outcome}   ")),
                    )
                    .truncate(),
            )
        })
        .inner
        .on_hover_text(&label);
    dropdown_indicator(ui, &button);
    button.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, !pulls.is_empty(), &label)
    });
    let bounds = ui.ctx().content_rect();
    let menu_width = 400.0_f32.min(bounds.width() - 24.0).max(160.0);
    let available_height = (bounds.bottom() - button.rect.bottom() - 16.0).max(40.0);
    let menu_height = 360.0_f32.min(available_height);
    let popup = egui::Popup::menu(&button)
        .id(popup_id)
        .align(egui::RectAlign::BOTTOM_START)
        .align_alternatives(&[])
        .width(menu_width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    let opening = !was_open;
    popup.show(|ui| {
        const ROW_HEIGHT: f32 = 30.0;
        ui.spacing_mut().interact_size.y = ROW_HEIGHT;
        // Reserve a real gutter: an overlay scrollbar otherwise covers the
        // phase and duration at the right edge of each pull row.
        ui.spacing_mut().scroll.floating = false;
        let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
        let height = menu_height.min(pulls.len() as f32 * stride);
        ui.set_width(menu_width);
        ui.set_height(height);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("pull-list")
            .max_height(height)
            .auto_shrink([false, false]);
        if opening || moved {
            let offset = cursor.unwrap_or(0) as f32 * stride - (height - stride) / 2.0;
            scroll = scroll.vertical_scroll_offset(offset.max(0.0));
        }
        scroll.show_rows(ui, ROW_HEIGHT, pulls.len(), |ui, range| {
            for index in range {
                let pull = &pulls[index];
                let name = format!("{} · {}", index + 1, pull.name);
                let details = format!(
                    "{} · {}",
                    pull_outcome(pull.kill, pull.last_phase, pull.last_phase_is_intermission),
                    clock((pull.end_ms - pull.start_ms) as f64 / 1000.0)
                );
                let label = format!("{name} · {details}");
                let row = ui.add_sized(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    egui::Button::new(&name)
                        .right_text(&details)
                        .selected(current == Some(index))
                        .truncate(),
                );
                if (opening || moved) && *cursor == Some(index) {
                    row.request_focus();
                } else if row.has_focus() && !moved {
                    *cursor = Some(index);
                }
                if row.clicked() {
                    chosen = Some(index);
                }
                row.on_hover_text(label);
            }
        });
        if chosen.is_some() {
            ui.close();
            button.request_focus();
        }
    });
    chosen
}

fn pull_key(pull: &Pull) -> String {
    format!("{}:{}", pull.report, pull.id)
}
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
fn relative_clock(seconds: f64) -> String {
    if seconds < 0.0 {
        format!("-{}", clock(-seconds))
    } else {
        clock(seconds)
    }
}
fn class_color(class: &str) -> Color32 {
    match class {
        "DeathKnight" => Color32::from_rgb(196, 30, 58),
        "DemonHunter" => Color32::from_rgb(163, 48, 201),
        "Druid" => Color32::from_rgb(255, 124, 10),
        "Evoker" => Color32::from_rgb(51, 147, 127),
        "Hunter" => Color32::from_rgb(170, 211, 114),
        "Mage" => Color32::from_rgb(63, 199, 235),
        "Monk" => Color32::from_rgb(0, 255, 152),
        "Paladin" => Color32::from_rgb(244, 140, 186),
        "Priest" => Color32::WHITE,
        "Rogue" => Color32::from_rgb(255, 244, 104),
        "Shaman" => Color32::from_rgb(64, 133, 240),
        "Warlock" => Color32::from_rgb(135, 135, 237),
        "Warrior" => Color32::from_rgb(198, 155, 109),
        _ => Color32::from_rgb(215, 220, 230),
    }
}

fn event_description(event: &RaidEvent) -> String {
    if event.kind == EventKind::Deaths {
        format!("{} died", event.actor)
    } else {
        let evidence = if event.observed_buff {
            " · buff applied"
        } else {
            ""
        };
        if let Some(target) = &event.target {
            format!(
                "{} · {} on {}{}",
                event.actor, event.ability, target, evidence
            )
        } else {
            format!("{} · {}{}", event.actor, event.ability, evidence)
        }
    }
}

fn pull_video_start(review: &Review, pull: &Pull) -> f64 {
    review.pull_video_start(pull)
}

fn encounter_moment(review: &Review, pull: &Pull, video_seconds: f64) -> i64 {
    pull.start_ms + ((video_seconds - pull_video_start(review, pull)) * 1000.0).round() as i64
}

fn playback_intent(state: &PlaybackState, playback: Option<&Playback>) -> bool {
    if let Some(intent) = state.playback_intent {
        intent
    } else if !state.ready || state.seeking.is_some() || state.blocked || state.buffering {
        playback.is_none_or(|p| p.autoplay)
    } else {
        state.playing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{Provider, Status};
    use crate::warcraftlogs::Replay;
    use std::collections::HashMap;

    fn fixture() -> (Review, Pull, Stream) {
        let replay = Replay {
            provider: Provider::Youtube,
            video_id: "abcDEF_12-3".into(),
            broadcast_id: "abcDEF_12-3".into(),
            started_at: "2026-09-01T12:00:00Z".into(),
            available_seconds: 25_000,
        };
        let start = replay.start_ms().unwrap() + 19_800_375;
        let pull = Pull {
            report: "abcdefghABCDEFGH".into(),
            id: 1,
            encounter: 1,
            difficulty: 5,
            report_start_ms: start - 50_000,
            remaining: Some(75.8),
            last_phase: Some(2),
            last_phase_is_intermission: false,
            name: "A very long encounter name".into(),
            kill: false,
            start_ms: start,
            end_ms: start + 210_000,
            seconds: 19_800,
        };
        let stream = Stream {
            replay_start_ms: replay.start_ms().ok(),
            replay_end_ms: replay
                .start_ms()
                .ok()
                .map(|start| start + replay.available_seconds as i64 * 1000),
            recording_id: None,
            user_id: "101".into(),
            raid_role: None,
            name: "A guildmate with a long character name".into(),
            provider: Provider::Youtube,
            channel_id: replay.video_id.clone(),
            url: replay.public_url(0),
            status: Status::Live,
            broadcast_state: None,
        };
        (
            Review {
                marker_timing: HashMap::new(),
                replay,
                pulls: vec![pull.clone()],
            },
            pull,
            stream,
        )
    }

    #[test]
    fn current_pulls_schedule_a_wakeup_without_pointer_or_video_activity() {
        let (_, _, stream) = fixture();
        let ctx = egui::Context::default();
        let mut review = ReviewUi::default();
        review.key = pov_key(&stream);
        review.last_attempt = Some(Instant::now());
        let mut delay = Duration::ZERO;
        for _ in 0..3 {
            let output = ctx.run_ui(egui::RawInput::default(), |_| {
                review.tick(&ctx, Some(&stream));
            });
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        assert!(delay > Duration::from_secs(1) && delay <= Duration::from_secs(15));
        assert!(review.work.is_none());
        review.last_attempt = Some(Instant::now() - Duration::from_secs(16));
        assert!(matches!(review.next_action(), Some(Action::Refresh)));
        review.notice = Some("Rate limited".into());
        assert!(review.next_action().is_none(), "Errors must back off");
        review.notice = None;
        let mut archive = stream;
        archive.status = Status::Offline;
        review.last_attempt = Some(Instant::now());
        review.tick(&ctx, Some(&archive));
        assert_eq!(review.refresh_interval(), Duration::from_secs(60));
        review.last_attempt = Some(Instant::now() - Duration::from_secs(16));
        assert!(review.next_action().is_none());
    }

    #[test]
    fn recording_coverage_excludes_later_povs_from_earlier_pulls_and_checks_current_moment() {
        let (review, pull, _) = fixture();
        let start = review.replay.start_ms().unwrap();
        let end = start + 25_000_000;
        assert!(recording_covers_moment(
            (start, end),
            pull.start_ms,
            pull.start_ms + 121_867,
        ));
        // Overlapping the same raid report is insufficient when this recording
        // starts several pulls after the selected fight.
        let later = pull.end_ms + 5 * 240_000;
        assert!(!recording_covers_moment(
            (later, end),
            pull.start_ms,
            pull.start_ms + 121_867,
        ));
        assert!(recording_covers_moment(
            (later, end),
            later + 60_000,
            later + 121_867,
        ));
        let short_end = pull.start_ms + 120_000;
        assert!(recording_covers_moment(
            (start, short_end),
            pull.start_ms,
            pull.start_ms + 100_000,
        ));
        assert!(!recording_covers_moment(
            (start, short_end),
            pull.start_ms,
            pull.start_ms + 121_867,
        ));
    }

    #[test]
    fn recording_coverage_preserves_milliseconds_without_an_offset() {
        let start = 1_700_000_000_000;
        let end = start + 240_000;
        assert!(recording_covers_moment((start, end), start, start));
        assert!(recording_covers_moment((start, end), start, end - 1));
        assert!(!recording_covers_moment((start, end), start, start - 1));
        assert!(!recording_covers_moment((start, end), start, end));
        for range in [(start, start), (end, start), (-1, end), (0, i64::MAX)] {
            assert!(!recording_covers_moment(range, start, start));
        }
    }

    #[test]
    fn pov_coverage_uses_loaded_current_metadata_and_never_assumes_unknown_candidates() {
        let (review, pull, mut current) = fixture();
        let mut ui = ReviewUi::default();
        ui.key = pov_key(&current);
        ui.accept_review(review.clone());
        current.replay_start_ms = None;
        current.replay_end_ms = None;
        assert!(ui.pov_covers_moment(&current, &pull, pull.start_ms + 121_867));
        let mut other = current.clone();
        other.channel_id = "otherVID123".into();
        other.user_id = "102".into();
        assert!(!ui.pov_covers_moment(&other, &pull, pull.start_ms + 121_867));
        other.replay_start_ms = Some(pull.end_ms + 5 * 240_000);
        other.replay_end_ms = Some(pull.end_ms + 20 * 240_000);
        assert!(!ui.pov_covers_moment(&other, &pull, pull.start_ms + 121_867));
        other.replay_start_ms = review.replay.start_ms().ok();
        other.replay_end_ms = Some(pull.end_ms);
        assert!(ui.pov_covers_moment(&other, &pull, pull.end_ms - 1));
        assert!(!ui.pov_covers_moment(&other, &pull, pull.end_ms));
    }

    #[test]
    fn raid_marker_alignment_is_per_pull_and_preserves_event_milliseconds() {
        let (mut review, pull, _) = fixture();
        review.marker_timing.insert(
            (pull.report.clone(), pull.id),
            crate::replay_sync::Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: 123.456,
                uncertainty_seconds: 0.10,
            },
        );
        assert_eq!(review.pull_video_start(&pull), 123.456);
        let mut other = pull.clone();
        other.id += 1;
        assert_ne!(review.pull_video_start(&other), 123.456);
        let at_ms = pull.start_ms + 137_625;
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.select(pull.clone());
        let command = ui.seek_absolute_with_playback(at_ms, false).unwrap();
        assert!(
            matches!(command, PlaybackCommand::SeekPaused(seconds) if (seconds - 261.081).abs() < 0.00001)
        );
        assert_eq!(
            encounter_moment(ui.review.as_ref().unwrap(), &pull, 261.081),
            at_ms
        );
        assert!(!ui.playback.as_ref().unwrap().autoplay);
    }

    #[test]
    fn discovering_a_timestamp_does_not_move_the_watched_pull_until_navigation() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.accept_review(review.clone());
        ui.select(pull.clone());
        let original = ui.review.as_ref().unwrap().pull_video_start(&pull);
        ui.marker_cache.lock().unwrap().insert(
            crate::replay_sync::Key::new(&review.replay, &pull),
            crate::replay_sync::Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: original + 3.5,
                uncertainty_seconds: 0.1,
            },
        );
        ui.accept_review(review);
        assert_eq!(
            ui.review.as_ref().unwrap().pull_video_start(&pull),
            original
        );
        assert_eq!(ui.playback.as_ref().unwrap().seconds, original);
        ui.select(pull.clone());
        assert_eq!(
            ui.review.as_ref().unwrap().pull_video_start(&pull),
            original + 3.5
        );
        assert_eq!(ui.playback.as_ref().unwrap().seconds, original + 3.5);
    }

    #[test]
    fn refreshed_metadata_reuses_only_calibration_for_identical_pull_bounds() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.marker_cache.lock().unwrap().insert(
            crate::replay_sync::Key::new(&review.replay, &pull),
            crate::replay_sync::Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: 123.456,
                uncertainty_seconds: 0.10,
            },
        );
        ui.accept_review(review.clone());
        assert_eq!(ui.review.as_ref().unwrap().pull_video_start(&pull), 123.456);
        let mut changed = review;
        changed.pulls[0].start_ms += 1;
        let changed_pull = changed.pulls[0].clone();
        ui.accept_review(changed);
        assert!(ui
            .review
            .as_ref()
            .unwrap()
            .marker_alignment(&changed_pull)
            .is_none());
    }

    #[test]
    fn event_click_preserves_the_log_millisecond_without_a_lead_in() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review.clone());
        ui.select(pull.clone());
        for offset in [375, 121_867] {
            let at_ms = pull.start_ms + offset;
            let event = RaidEvent {
                actor_id: 1,
                observed_buff: false,
                target_actor_id: None,
                at_ms,
                actor: "Player".into(),
                class: "Mage".into(),
                ability: String::new(),
                ability_id: 0,
                target: None,
                kind: EventKind::Deaths,
                group: None,
            };
            let Some(PlaybackCommand::Seek(seconds)) = ui.watch_event(event) else {
                panic!("An event click must seek and play");
            };
            assert_eq!(encounter_moment(&review, &pull, seconds), at_ms);
            assert_eq!(ui.selected_event, Some((at_ms, "Player".into())));
        }
    }

    #[test]
    fn timeline_position_waits_for_current_settled_video_after_a_seek() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.select(pull.clone());
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = ui.playback().unwrap().seconds;
        state.mark_polled_now();
        assert_eq!(ui.confirmed_video_position(&state), Some(state.seconds));

        ui.seek_absolute(pull.start_ms + 121_867).unwrap();
        // The old frame belongs to the same POV, but predates the click.
        assert_eq!(ui.confirmed_video_position(&state), None);
        state.mark_polled_now();
        state.seconds = ui.playback().unwrap().seconds;
        state.seeking = Some(state.seconds);
        assert_eq!(ui.confirmed_video_position(&state), None);
        state.seeking = None;
        state.buffering = true;
        assert_eq!(ui.confirmed_video_position(&state), None);
        state.buffering = false;
        assert_eq!(ui.confirmed_video_position(&state), Some(state.seconds));
    }

    #[test]
    fn pov_switch_preserves_encounter_millisecond_across_different_starts_and_alignment() {
        let (source, pull, _) = fixture();
        let moment = encounter_moment(&source, &pull, pull_video_start(&source, &pull) + 137.625);
        assert_eq!(moment, pull.start_ms + 137_625);
        let mut other = source.clone();
        other.replay.started_at = "2026-09-01T17:00:00Z".into();
        other.replay.video_id = "xyzDEF_12-3".into();
        other.marker_timing.insert(
            (pull.report.clone(), pull.id),
            crate::replay_sync::Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: other.pull_video_start(&pull) - 3.0,
                uncertainty_seconds: 0.1,
            },
        );
        let mut ui = ReviewUi::default();
        ui.review = Some(other.clone());
        ui.select(pull.clone());
        let command = ui.seek_absolute_with_playback(moment, false).unwrap();
        let seconds = match command {
            PlaybackCommand::SeekPaused(seconds) => seconds,
            _ => panic!("A paused POV switch must stay paused"),
        };
        assert!((seconds - 1935.0).abs() < 0.001);
        assert_eq!(encounter_moment(&other, &pull, seconds), moment);
        assert!(!ui.playback().unwrap().autoplay);
        // Repeated swaps cannot accumulate whole-second rounding errors.
        ui.review = Some(source.clone());
        ui.select(pull.clone());
        ui.seek_absolute_with_playback(moment, true).unwrap();
        assert_eq!(
            encounter_moment(&source, &pull, ui.playback().unwrap().seconds),
            moment
        );
    }

    #[test]
    fn recording_review_opens_first_pull_once_and_uses_a_distinct_request_identity() {
        let (review, pull, live) = fixture();
        let mut saved = live.clone();
        saved.status = Status::Offline;
        saved.recording_id = Some(saved.channel_id.clone());
        assert_ne!(pov_key(&live), pov_key(&saved));
        assert!(pov_unavailable(&saved, &live).is_none());
        let mut ui = ReviewUi::default();
        ui.open_recording();
        assert!(ui.active());
        assert!(ui.playback().is_none());
        assert!(ui.accept_review(review.clone()));
        assert_eq!(ui.pull.as_ref().unwrap().id, pull.id);
        ui.seek_absolute_with_playback(pull.start_ms + 120_000, false)
            .unwrap();
        let seconds = ui.playback().unwrap().seconds;
        ui.accept_review(review);
        assert_eq!(ui.playback().unwrap().seconds, seconds);
        assert!(!ui.playback().unwrap().autoplay);
        assert_eq!(ui.report_span(), Some((pull.start_ms, pull.end_ms)));
    }

    #[test]
    fn initial_saved_connection_check_shows_static_loading_then_the_review() {
        let (mut review, _, mut stream) = fixture();
        stream.status = Status::Offline;
        stream.recording_id = Some(review.replay.video_id.clone());
        // No pulls avoids starting an unrelated event request in this UI test.
        review.pulls.clear();
        let ctx = egui::Context::default();
        let mut review_ui = ReviewUi::default();
        review_ui.key = pov_key(&stream);
        review_ui.open_recording();
        let (tx, rx) = mpsc::channel();
        review_ui.work = Some(rx);
        review_ui.work_action = Some(Action::Refresh);
        let has_label = |output: &egui::FullOutput, label: &str| {
            output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == label)
        })
        };
        for frame in 0..3 {
            review_ui.tick(&ctx, Some(&stream));
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 720.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    let action = review_ui.draw_workspace(
                        ui,
                        &stream,
                        &[],
                        &PlaybackState::default(),
                        false,
                        None,
                        None,
                    );
                    assert!(action.rect.is_none() && !action.reload && action.command.is_none());
                },
            );
            assert!(has_label(&output, "Loading raid review…"));
            assert!(has_label(&output, "Back to VODs"));
            assert!(!has_label(&output, "Connect your Warcraft Logs account"));
            assert!(!has_label(&output, "Connect Warcraft Logs"));
            assert!(!review_ui.connection_checked);
            assert!(review_ui.work.is_some());
            if frame == 2 {
                assert!(
                    output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                        > Duration::from_secs(1),
                    "Waiting for saved sign-in must not request animation frames"
                );
            }
        }
        tx.send((
            review_ui.generation,
            review_ui.key.clone(),
            Ok(Data::Review(review, Default::default())),
            true,
        ))
        .unwrap();
        review_ui.tick(&ctx, Some(&stream));
        assert!(review_ui.connected && review_ui.connection_checked && review_ui.active());
        assert!(review_ui.work.is_none());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(980.0, 720.0),
                )),
                ..Default::default()
            },
            |ui| {
                let action = review_ui.draw_workspace(
                    ui,
                    &stream,
                    &[],
                    &PlaybackState::default(),
                    false,
                    None,
                    None,
                );
                assert!(action.rect.is_some());
            },
        );
        assert!(!has_label(&output, "Loading raid review…"));
        assert!(!has_label(&output, "Connect Warcraft Logs"));
    }

    #[test]
    fn obsolete_connection_result_does_not_expose_sign_in_during_the_initial_check() {
        let (_, _, stream) = fixture();
        let ctx = egui::Context::default();
        let mut review_ui = ReviewUi::default();
        review_ui.key = pov_key(&stream);
        review_ui.generation = 2;
        review_ui.last_attempt = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        review_ui.work = Some(rx);
        review_ui.work_action = Some(Action::Refresh);
        tx.send((1, review_ui.key.clone(), Ok(Data::Authentication), false))
            .unwrap();
        review_ui.tick(&ctx, Some(&stream));
        assert!(!review_ui.connected && !review_ui.connection_checked);
        assert!(review_ui.work.is_none());
    }

    #[test]
    fn recording_review_keeps_connection_recovery_visible_after_loading_fails() {
        for previously_connected in [false, true] {
            let (review, _, mut stream) = fixture();
            stream.status = Status::Offline;
            stream.recording_id = Some(review.replay.video_id.clone());
            let ctx = egui::Context::default();
            let mut ui = ReviewUi::default();
            ui.key = pov_key(&stream);
            ui.connected = previously_connected;
            ui.open_recording();

            // A new account or an expired session completes the metadata request
            // without a Logs connection. The host returns to the VOD list as soon
            // as active becomes false, hiding the connection button and notice.
            let (tx, rx) = mpsc::channel();
            ui.work = Some(rx);
            ui.work_action = Some(Action::Refresh);
            tx.send((
                ui.generation,
                ui.key.clone(),
                Err("Connect Warcraft Logs to see this raid's pulls.".into()),
                false,
            ))
            .unwrap();
            ui.tick(&ctx, Some(&stream));
            assert!(ui.active(), "A failed read must not close the selected VOD");
            assert!(!ui.connected);
            assert!(ui.connection_checked);
            assert!(ui.open_first_pull);
            assert!(ui.playback().is_none());

            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 720.0),
                    )),
                    ..Default::default()
                },
                |root| {
                    let action = ui.draw_workspace(
                        root,
                        &stream,
                        &[],
                        &PlaybackState::default(),
                        false,
                        None,
                        None,
                    );
                    assert!(action.rect.is_none());
                    assert!(!action.reload);
                },
            );
            for label in ["Connect Warcraft Logs", "Back to VODs"] {
                assert!(
                    output.shapes.iter().any(|shape| matches!(&shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == label)),
                    "Missing recovery control: {label}"
                );
            }
            assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == "Opening replay…")));
            // Remaining in the workspace must not turn a failed request into a
            // retry on every UI frame or start a player without a selected pull.
            for _ in 0..5 {
                ui.tick(&ctx, Some(&stream));
                assert!(ui.work.is_none());
                assert!(ui.playback().is_none());
                assert!(ui.active());
            }
            assert!(ui.accept_review(review));
            assert!(ui.playback().is_some());
            assert!(!ui.open_first_pull);
        }
    }

    #[test]
    fn unavailable_pov_time_does_not_seek_to_an_unrelated_moment() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.select(pull.clone());
        let before = ui.playback().unwrap().seconds;
        assert!(ui.seek_absolute(pull.start_ms - 30_000_000).is_none());
        assert!(ui.seek_absolute(pull.start_ms + 30_000_000).is_none());
        assert_eq!(ui.playback().unwrap().seconds, before);
    }

    #[test]
    fn selecting_a_pull_uses_unix_or_raw_metadata_without_an_offset() {
        for alignment in [None, Some(123.456)] {
            let (mut review, pull, _) = fixture();
            if let Some(video_seconds) = alignment {
                review.marker_timing.insert(
                    (pull.report.clone(), pull.id),
                    crate::replay_sync::Alignment {
                        unix_seconds: pull.start_ms / 1000,
                        video_seconds,
                        uncertainty_seconds: 0.10,
                    },
                );
            }
            let expected = alignment
                .unwrap_or((pull.start_ms - review.replay.start_ms().unwrap()) as f64 / 1000.0);
            let mut ui = ReviewUi::default();
            ui.review = Some(review.clone());
            ui.active = true;
            ui.select(pull.clone());
            let playback = ui.playback().unwrap();
            assert!((playback.seconds - expected).abs() < 0.000_001);
            assert_eq!(
                encounter_moment(&review, &pull, playback.seconds),
                pull.start_ms
            );
            assert!(playback.autoplay);
            ui.capture_pov_position(&PlaybackState::default());
            assert_eq!(ui.pending_focus.as_ref().unwrap().1, pull.start_ms);
        }
    }

    #[test]
    fn immediate_pov_switch_keeps_initial_pull_before_first_current_sample() {
        let (review, pull, _) = fixture();
        let mut old_live_state = PlaybackState::default();
        old_live_state.ready = true;
        old_live_state.seconds = 0.0;
        old_live_state.mark_polled_now();
        let mut ui = ReviewUi::default();
        ui.active = true;
        ui.review = Some(review);
        ui.select(pull.clone());
        for state in [PlaybackState::default(), old_live_state] {
            ui.capture_pov_position(&state);
            let (wanted, at_ms, autoplay) = ui.pending_focus.as_ref().unwrap();
            assert_eq!(pull_key(wanted), pull_key(&pull));
            assert_eq!(*at_ms, pull.start_ms);
            assert!(*autoplay);
        }
    }

    #[test]
    fn missing_or_loading_pov_preserves_moment_for_next_available_view() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.active = true;
        ui.review = Some(review.clone());
        ui.select(pull.clone());
        let moment = pull.start_ms + 137_625;
        ui.seek_absolute_with_playback(moment, false).unwrap();
        ui.capture_pov_position(&PlaybackState::default());

        // A rapid second switch can precede the first view's metadata/player.
        ui.playback = None;
        ui.pull = None;
        ui.review = None;
        ui.capture_pov_position(&PlaybackState::default());
        assert_eq!(ui.pending_focus.as_ref().unwrap().1, moment);

        let mut missing = review.clone();
        missing.pulls.clear();
        ui.accept_review(missing);
        assert!(ui.restore_pov_position());
        assert!(ui.playback.is_none());
        assert_eq!(ui.pending_focus.as_ref().unwrap().1, moment);

        ui.capture_pov_position(&PlaybackState::default());
        ui.accept_review(review.clone());
        assert!(ui.restore_pov_position());
        assert!(ui.pending_focus.is_none());
        assert_eq!(pull_key(ui.pull.as_ref().unwrap()), pull_key(&pull));
        let playback = ui.playback.as_ref().unwrap();
        assert!(!playback.autoplay);
        assert_eq!(encounter_moment(&review, &pull, playback.seconds), moment);
        assert!(ui.notice.is_none());
    }

    #[test]
    fn explicit_pull_selection_replaces_an_unavailable_pov_moment() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.pending_focus = Some((pull.clone(), pull.start_ms + 10_000, false));
        ui.select(pull);
        assert!(ui.pending_focus.is_none());
        assert!(ui.playback.is_some());
    }

    #[test]
    fn refreshed_reports_remove_unavailable_pull_and_refresh_existing_metadata() {
        let (mut review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review.clone());
        ui.select(pull.clone());
        ui.seek_absolute_with_playback(pull.start_ms + 137_625, false)
            .unwrap();
        let seconds = ui.playback().unwrap().seconds;
        review.pulls[0].remaining = Some(65.0);
        assert!(!ui.accept_review(review.clone()));
        assert_eq!(ui.pull.as_ref().unwrap().remaining, Some(65.0));
        assert_eq!(ui.playback().unwrap().seconds, seconds);
        assert!(!ui.playback().unwrap().autoplay);
        ui.events.push(RaidEvent {
            actor_id: 1,
            observed_buff: false,
            target_actor_id: None,
            at_ms: pull.start_ms + 5_000,
            actor: "Guildmate".into(),
            class: "Priest".into(),
            ability: "Died".into(),
            ability_id: 0,
            target: None,
            kind: EventKind::Deaths,
            group: None,
        });
        ui.loaded_events.push(EventKind::Deaths);
        review.pulls.clear();
        assert!(ui.accept_review(review));
        assert!(ui.pull.is_none());
        assert!(ui.playback.is_none());
        assert!(ui.events.is_empty());
        assert!(ui.loaded_events.is_empty());
    }

    #[test]
    fn event_failures_retry_with_backoff_without_blocking_the_other_row() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull.clone());
        ui.last_attempt = Some(Instant::now());
        ui.requested_events.push(EventKind::Deaths);
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.work_action = Some(Action::Events(pull, EventKind::Deaths));
        tx.send((
            ui.generation,
            String::new(),
            Err("Temporary failure".into()),
            true,
        ))
        .unwrap();
        assert!(!ui.tick(&egui::Context::default(), None));
        assert!(matches!(
            ui.next_action(),
            Some(Action::Events(_, EventKind::Defensives))
        ));
        assert!(ui.event_retry_after().unwrap() <= Duration::from_secs(5));
        ui.requested_events.push(EventKind::Defensives);
        ui.loaded_events.push(EventKind::Defensives);
        assert!(ui.next_action().is_none());
        ui.event_failures[0].retry_at = Instant::now() - Duration::from_millis(1);
        assert!(matches!(
            ui.next_action(),
            Some(Action::Events(_, EventKind::Deaths))
        ));
        let (_tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        assert!(ui.next_action().is_none());
        assert!(ui.event_retry_after().is_none());
        ui.work = None;
        for _ in 0..100 {
            ui.record_event_failure(EventKind::Deaths, "Still offline".into());
        }
        assert_eq!(ui.event_failures.len(), 1);
        assert_eq!(ui.requested_events.len(), 2);
        let retry = ui.event_retry_after().unwrap();
        assert!(retry > Duration::from_secs(59) && retry <= Duration::from_secs(60));
        ui.select(ui.pull.clone().unwrap());
        assert!(ui.event_failures.is_empty());
    }

    #[test]
    fn cooldown_saves_do_not_restart_media_or_clear_existing_timeline_rows() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull.clone());
        ui.last_attempt = Some(Instant::now());
        ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
        ui.requested_events = ui.loaded_events.clone();
        ui.events = vec![RaidEvent {
            at_ms: pull.start_ms + 1000,
            actor_id: 1,
            observed_buff: false,
            actor: "Priest".into(),
            class: "Priest".into(),
            ability: "Apotheosis".into(),
            ability_id: 200183,
            target: None,
            target_actor_id: None,
            kind: EventKind::Defensives,
            group: Some(DefensiveGroup::Healing),
        }];
        let position = ui.playback.as_ref().unwrap().seconds;
        for (rule, expect_fetch, count) in [
            (
                defensives::Rule {
                    group: Some(DefensiveGroup::Utility),
                    observation: defensives::Observation::Cast,
                },
                false,
                1,
            ),
            (
                defensives::Rule {
                    group: None,
                    observation: defensives::Observation::Cast,
                },
                false,
                0,
            ),
            (
                defensives::Rule {
                    group: Some(DefensiveGroup::Healing),
                    observation: defensives::Observation::Cast,
                },
                true,
                0,
            ),
        ] {
            let mut preferences = ui.cooldowns.clone();
            preferences.overrides.insert(200183, rule);
            let (tx, rx) = mpsc::channel();
            ui.work = Some(rx);
            ui.work_action = Some(Action::SaveCooldowns(preferences.clone()));
            tx.send((
                ui.generation,
                String::new(),
                Ok(Data::Cooldowns(preferences)),
                true,
            ))
            .unwrap();
            assert!(
                !ui.tick(&egui::Context::default(), None),
                "Preference saves must not request a player reload"
            );
            assert_eq!(ui.playback.as_ref().unwrap().seconds, position);
            assert!(ui.active && ui.pull.is_some() && ui.review.is_some());
            assert_eq!(ui.events.len(), count);
            if count > 0 {
                assert_eq!(ui.events[0].group, rule.group);
            }
            assert_eq!(
                matches!(
                    ui.next_action(),
                    Some(Action::Events(_, EventKind::Defensives))
                ),
                expect_fetch
            );
            assert!(ui.loaded_events.contains(&EventKind::Deaths));
        }
    }

    #[test]
    fn expanding_tracking_keeps_previous_events_and_row_visibility_needs_no_fetch() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull.clone());
        ui.last_attempt = Some(Instant::now());
        ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
        ui.requested_events = ui.loaded_events.clone();
        ui.events.push(RaidEvent {
            at_ms: pull.start_ms + 1000,
            actor_id: 1,
            observed_buff: false,
            actor: "Priest".into(),
            class: "Priest".into(),
            ability: "Apotheosis".into(),
            ability_id: 200183,
            target: None,
            target_actor_id: None,
            kind: EventKind::Defensives,
            group: Some(DefensiveGroup::Healing),
        });
        let mut preferences = ui.cooldowns.clone();
        preferences.hidden_groups.push(DefensiveGroup::Healing);
        assert!(!ui.accept_cooldown_preferences(preferences.clone()));
        assert!(ui.next_action().is_none());
        assert_eq!(ui.events.len(), 1);
        preferences.overrides.insert(
            1234567,
            defensives::Rule {
                group: Some(DefensiveGroup::Utility),
                observation: defensives::Observation::Cast,
            },
        );
        assert!(ui.accept_cooldown_preferences(preferences));
        assert_eq!(ui.events.len(), 1);
        assert_eq!(ui.events[0].ability_id, 200183);
        assert!(matches!(
            ui.next_action(),
            Some(Action::Events(_, EventKind::Defensives))
        ));
        ui.record_event_failure(EventKind::Defensives, "Slow provider".into());
        assert_eq!(ui.events.len(), 1);
    }

    #[test]
    fn advanced_hover_keeps_its_background_and_editor_geometry_fixed() {
        let mut review = ReviewUi::default();
        review.cooldown_editor = Some(CooldownEditor::new(
            Default::default(),
            &[],
            DefensiveGroup::Healing,
        ));
        let ctx = egui::Context::default();
        let mut style = (*ctx.global_style()).clone();
        style.animation_time = 0.0;
        style.visuals.widgets.hovered.expansion = 3.0;
        ctx.set_global_style(style);
        let mut frame = |events| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 720.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| review.draw_cooldown_editor(ui.ctx()),
            )
        };
        frame(vec![]);
        frame(vec![]);
        let geometry = |output: &egui::FullOutput| {
            let rect = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect) if rect.fill == Color32::from_rgb(43, 48, 58) => {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .unwrap();
            let save = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "Save changes" => {
                        Some(text.pos)
                    }
                    _ => None,
                })
                .unwrap();
            (rect, save)
        };
        let before = geometry(&frame(vec![]));
        let after = geometry(&frame(vec![egui::Event::PointerMoved(before.0.center())]));
        assert_eq!(before, after);
        assert_eq!(geometry(&frame(vec![egui::Event::PointerGone])), before);
    }

    #[test]
    fn late_events_from_previous_pull_do_not_replace_current_selection() {
        let (mut review, pull, _) = fixture();
        let mut next = pull.clone();
        next.id = 2;
        next.start_ms += 300_000;
        next.end_ms += 300_000;
        review.pulls.push(next.clone());
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.select(pull.clone());
        ui.select(next.clone());
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        tx.send((
            ui.generation,
            String::new(),
            Ok(Data::Events(
                pull_key(&pull),
                EventKind::Deaths,
                vec![RaidEvent {
                    actor_id: 1,
                    observed_buff: false,
                    target_actor_id: None,
                    at_ms: pull.start_ms + 5_000,
                    actor: "Previous pull".into(),
                    class: "Priest".into(),
                    ability: "Died".into(),
                    ability_id: 0,
                    target: None,
                    kind: EventKind::Deaths,
                    group: None,
                }],
                defensives::Preferences::default(),
            )),
            true,
        ))
        .unwrap();
        ui.tick(&egui::Context::default(), None);
        assert_eq!(ui.pull.as_ref().unwrap().id, next.id);
        assert!(ui.events.is_empty());
        assert!(ui.loaded_events.is_empty());
    }

    #[test]
    fn rapid_pull_changes_keep_one_worker_and_schedule_only_the_latest_pull() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull.clone());
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.work_action = Some(Action::Events(pull.clone(), EventKind::Deaths));
        ui.requested_events.push(EventKind::Deaths);
        let generation = ui.generation;
        let cancel = ui.cancel.clone();
        for id in 2..=31 {
            let mut next = pull.clone();
            next.id = id;
            ui.select(next);
            assert!(ui.work.is_some());
            assert!(ui.next_action().is_none());
        }
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(ui.generation, generation + 1);
        tx.send((
            generation,
            String::new(),
            Err("Obsolete failure".into()),
            true,
        ))
        .unwrap();
        ui.tick(&egui::Context::default(), None);
        assert!(ui.notice.is_none());
        assert!(ui.event_failures.is_empty());
        assert!(
            matches!(ui.next_action(), Some(Action::Events(pull, EventKind::Deaths)) if pull.id == 31)
        );
    }

    #[test]
    fn returning_to_same_pov_does_not_accept_its_cancelled_generation() {
        let (review, _, stream) = fixture();
        let key = pov_key(&stream);
        let mut other = stream.clone();
        other.user_id = "102".into();
        let ctx = egui::Context::default();
        let mut ui = ReviewUi::default();
        ui.key = key.clone();
        ui.review = Some(review.clone());
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.work_action = Some(Action::Refresh);
        let generation = ui.generation;
        ui.tick(&ctx, Some(&other));
        ui.tick(&ctx, Some(&stream));
        // Keep this fake-transport test from launching an actual refresh.
        ui.last_attempt = Some(Instant::now());
        tx.send((
            generation,
            key,
            Ok(Data::Review(review, Default::default())),
            true,
        ))
        .unwrap();
        ui.tick(&ctx, Some(&stream));
        assert!(ui.review.is_none());
        assert!(ui.work.is_none());
        assert!(ui.notice.is_none());
    }

    #[test]
    fn navigation_keeps_explicit_account_actions_but_drop_cancels_login() {
        let mut ui = ReviewUi::default();
        let cancel = ui.cancel.clone();
        for action in [Action::Connect, Action::Disconnect] {
            ui.work_action = Some(action);
            ui.cancel_read();
            assert!(!cancel.load(Ordering::Relaxed));
        }
        ui.work_action = Some(Action::Connect);
        drop(ui);
        assert!(cancel.load(Ordering::Relaxed));
    }

    thread_local! {
        static PROVIDER_TEST_CLOCK: std::cell::Cell<Instant> = std::cell::Cell::new(
            Instant::now() - Duration::from_secs(1)
        );
    }

    fn provider_test_now() -> Instant {
        PROVIDER_TEST_CLOCK.with(|clock| {
            let next = clock.get() + Duration::from_millis(1);
            clock.set(next);
            next
        })
    }

    fn provider_review_fixture() -> (ReviewUi, Stream, Pull, Pull) {
        // Keep test samples recent but strictly ordered even when Windows gives
        // several back-to-back Instant::now() calls the same clock tick.
        PROVIDER_TEST_CLOCK.with(|clock| clock.set(Instant::now() - Duration::from_secs(1)));
        let (mut review, first, stream) = fixture();
        let mut second = first.clone();
        second.id = 2;
        second.start_ms += 300_000;
        second.end_ms += 300_000;
        second.seconds += 300;
        review.pulls.push(second.clone());
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.connected = true;
        ui.active = true;
        ui.last_attempt = Some(Instant::now());
        ui.select(first.clone());
        ui.range_epoch = provider_test_now();
        ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
        (ui, stream, first, second)
    }

    fn provider_sample(seconds: f64, playing: bool, generation: u64) -> PlaybackState {
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = seconds;
        state.playing = playing;
        state.provider_seek_generation = Some(generation);
        state.mark_polled_at(provider_test_now());
        state
    }

    #[test]
    fn direct_provider_seek_inside_pull_preserves_events_and_play_intent() {
        for playing in [false, true] {
            let (mut ui, _, first, _) = provider_review_fixture();
            let start = pull_video_start(ui.review.as_ref().unwrap(), &first);
            ui.observe_provider_playback(&provider_sample(start + 10.0, playing, 0));
            ui.selected_event = Some((first.start_ms, "Actor".into()));
            let state = provider_sample(start + 127.625, playing, 1);
            ui.observe_provider_playback(&state);
            assert_eq!(ui.pull.as_ref().map(pull_key), Some(pull_key(&first)));
            assert_eq!(ui.timeline_position, Some(state.seconds));
            assert_eq!(ui.playback.as_ref().unwrap().seconds, state.seconds);
            assert_eq!(ui.playback.as_ref().unwrap().autoplay, playing);
            assert!(ui.selected_event.is_none());
            assert_eq!(ui.loaded_events.len(), 2);
            assert!(
                ui.next_action().is_none(),
                "Within-pull seeking uses loaded events"
            );
            assert!(ui.pause_at_pull_end(&state).is_none());
        }
    }

    #[test]
    fn provider_seek_across_pulls_waits_for_settled_video_and_never_pauses_old_pull() {
        for playing in [false, true] {
            let (mut ui, _, first, second) = provider_review_fixture();
            let start = pull_video_start(ui.review.as_ref().unwrap(), &first);
            let target = pull_video_start(ui.review.as_ref().unwrap(), &second) + 17.375;
            ui.observe_provider_playback(&provider_sample(start + 209.0, playing, 0));
            let mut moving = provider_sample(target, playing, 1);
            moving.buffering = true;
            moving.diagnostics.media_seeking = Some(true);
            ui.observe_provider_playback(&moving);
            assert_eq!(ui.pull.as_ref().map(pull_key), Some(pull_key(&first)));
            assert!(ui.provider_seek_pending);
            assert!(ui.pause_at_pull_end(&moving).is_none());
            let settled = provider_sample(target, playing, 1);
            ui.observe_provider_playback(&settled);
            assert_eq!(ui.pull.as_ref().map(pull_key), Some(pull_key(&second)));
            assert_eq!(ui.playback.as_ref().unwrap().seconds, target);
            assert_eq!(ui.playback.as_ref().unwrap().autoplay, playing);
            assert!(ui.pause_at_pull_end(&settled).is_none());
            assert!(
                matches!(ui.next_action(), Some(Action::Events(p, EventKind::Deaths)) if p.id == second.id)
            );
            ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
            ui.observe_provider_playback(&settled);
            assert_eq!(
                ui.loaded_events.len(),
                2,
                "The same sample cannot invalidate events twice"
            );
            let backwards = provider_sample(start + 75.5, playing, 2);
            ui.observe_provider_playback(&backwards);
            assert_eq!(ui.pull.as_ref().unwrap().id, first.id);
            assert_eq!(ui.timeline_position, Some(backwards.seconds));
        }
    }

    #[test]
    fn seeking_between_pulls_keeps_video_and_resumes_timeline_at_the_next_pull() {
        let (mut ui, stream, first, second) = provider_review_fixture();
        let start = pull_video_start(ui.review.as_ref().unwrap(), &first);
        ui.observe_provider_playback(&provider_sample(start + 100.0, true, 0));
        let state = provider_sample(start + 299.75, true, 1);
        ui.observe_provider_playback(&state);
        assert!(ui.pull.is_none() && ui.playback.is_some());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, state.seconds);
        assert!(ui.pause_at_pull_end(&state).is_none());
        assert!(
            ui.next_action().is_none(),
            "Gaps must not request unrelated pull events"
        );
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |egui_ui| {
            let action = ui.draw_workspace(egui_ui, &stream, &[], &state, true, None, None);
            assert!(action.rect.is_some() && !action.reload && action.command.is_none());
        });
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Between raid pulls"));
        assert!(!labels.contains(&"Loading this pull's events…"));
        let entering = provider_sample(start + 300.0, true, 1);
        ui.observe_provider_playback(&entering);
        assert_eq!(ui.pull.as_ref().unwrap().id, second.id);
        assert_eq!(ui.playback.as_ref().unwrap().seconds, entering.seconds);
        assert!(ui.pause_at_pull_end(&entering).is_none());
    }

    #[test]
    fn workspace_after_fullscreen_uses_current_provider_position_without_navigation() {
        let (mut ui, stream, first, second) = provider_review_fixture();
        let start = pull_video_start(ui.review.as_ref().unwrap(), &first);
        ui.observe_provider_playback(&provider_sample(start + 60.0, true, 0));
        // No workspace is drawn while the native provider is fullscreen. Its
        // latest sample must win immediately when the review is painted again.
        let state = provider_sample(start + 342.25, false, 1);
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |egui_ui| {
            let action = ui.draw_workspace(egui_ui, &stream, &[], &state, true, None, None);
            assert!(action.rect.is_some() && !action.reload && action.command.is_none());
        });
        assert_eq!(ui.pull.as_ref().unwrap().id, second.id);
        assert_eq!(ui.timeline_position, Some(state.seconds));
        assert!(!ui.playback.as_ref().unwrap().autoplay);
        assert_eq!(
            ui.comparison_position(&state).unwrap(),
            (second.start_ms + 42_250, false)
        );
    }

    #[test]
    fn ordinary_playback_still_pauses_at_selected_pull_end_with_provider_observation() {
        let (mut ui, _, first, _) = provider_review_fixture();
        let end = pull_video_start(ui.review.as_ref().unwrap(), &first) + 210.0;
        ui.observe_provider_playback(&provider_sample(end - 0.001, true, 0));
        let state = provider_sample(end, true, 0);
        ui.observe_provider_playback(&state);
        assert_eq!(ui.pull.as_ref().unwrap().id, first.id);
        assert!(matches!(
            ui.pause_at_pull_end(&state),
            Some(PlaybackCommand::Pause)
        ));
        assert!(ui.pause_at_pull_end(&state).is_none());
    }

    #[test]
    fn brief_provider_seek_past_boundary_uses_durable_seek_generation() {
        let (mut ui, _, first, _) = provider_review_fixture();
        let end = pull_video_start(ui.review.as_ref().unwrap(), &first) + 210.0;
        ui.observe_provider_playback(&provider_sample(end - 0.1, true, 10));
        // This tiny seek completed between SDK polls; position discontinuity
        // alone cannot distinguish it from reaching the end naturally.
        let state = provider_sample(end + 0.1, true, 11);
        ui.observe_provider_playback(&state);
        assert!(ui.pull.is_none());
        assert!(ui.playback.as_ref().unwrap().autoplay);
        assert!(ui.pause_at_pull_end(&state).is_none());
    }

    #[test]
    fn native_commands_and_stale_provider_samples_cannot_change_selected_pull() {
        let (mut ui, _, first, second) = provider_review_fixture();
        let start = pull_video_start(ui.review.as_ref().unwrap(), &first);
        ui.observe_provider_playback(&provider_sample(start + 60.0, true, 0));
        let stale = provider_sample(start + 340.0, true, 1);
        ui.seek_absolute(first.start_ms + 80_000).unwrap();
        ui.range_epoch = provider_test_now();
        ui.observe_provider_playback(&stale);
        assert_eq!(ui.pull.as_ref().unwrap().id, first.id);
        assert_eq!(ui.playback.as_ref().unwrap().seconds, start + 80.0);
        let mut pending = provider_sample(start + 340.0, true, 1);
        pending.seeking = Some(start + 80.0);
        pending.playback_intent = Some(true);
        ui.observe_provider_playback(&pending);
        assert_eq!(ui.pull.as_ref().unwrap().id, first.id);
        ui.observe_provider_playback(&provider_sample(start + 80.0, true, 2));
        assert!(!ui.provider_seek_pending);
        ui.observe_provider_playback(&provider_sample(start + 340.0, true, 3));
        assert_eq!(ui.pull.as_ref().unwrap().id, second.id);
    }

    fn provider_comparison_fixture() -> (ReviewUi, crate::review_compare_ui::Comparison, Pull, Pull)
    {
        let (mut primary, mut stream, first, second) = provider_review_fixture();
        primary.set_comparing(true);
        let mut secondary = primary.review.clone().unwrap();
        secondary.replay.video_id = "different12".into();
        secondary.replay.broadcast_id = "different12".into();
        secondary.replay.provider = Provider::Twitch;
        for pull in [&first, &second] {
            secondary.marker_timing.insert(
                (pull.report.clone(), pull.id),
                crate::replay_sync::Alignment {
                    unix_seconds: pull.start_ms / 1000,
                    video_seconds: primary.review.as_ref().unwrap().pull_video_start(pull)
                        + 600.625,
                    uncertainty_seconds: 0.1,
                },
            );
        }
        stream.provider = Provider::Twitch;
        stream.recording_id = Some(secondary.replay.video_id.clone());
        let mut comparison = crate::review_compare_ui::Comparison::new(
            &primary,
            stream,
            first.start_ms + 10_000,
            true,
        );
        comparison.set_provider_epoch_for_test(provider_test_now());
        comparison.metadata_for_test().review = Some(secondary);
        (primary, comparison, first, second)
    }

    #[test]
    fn either_fullscreen_comparison_pov_can_seek_across_pulls_without_reseeking_itself() {
        for leader in 0..2 {
            for playing in [false, true] {
                let (mut primary, mut comparison, first, second) = provider_comparison_fixture();
                let starts = [
                    primary.review.as_ref().unwrap().pull_video_start(&first),
                    comparison
                        .metadata_for_test()
                        .review
                        .as_ref()
                        .unwrap()
                        .pull_video_start(&first),
                ];
                let baseline = starts.map(|start| provider_sample(start + 10.0, playing, 0));
                assert!(comparison
                    .follow_provider_controls(
                        &mut primary,
                        [&baseline[0], &baseline[1]],
                        provider_test_now()
                    )
                    .is_none());
                let mut seeking = baseline.clone();
                seeking[leader] = provider_sample(starts[leader] + 342.375, playing, 1);
                let commands = comparison
                    .follow_provider_controls(
                        &mut primary,
                        [&seeking[0], &seeking[1]],
                        provider_test_now(),
                    )
                    .unwrap();
                let commands = [commands.primary, commands.secondary];
                assert!(commands[leader].is_none());
                let peer_seconds = match commands[1 - leader] {
                    Some(PlaybackCommand::Seek(seconds)) if playing => seconds,
                    Some(PlaybackCommand::SeekPaused(seconds)) if !playing => seconds,
                    _ => panic!("Only the other POV should follow the seek"),
                };
                assert_eq!(peer_seconds, starts[1 - leader] + 342.375);
                assert_eq!(primary.pull.as_ref().unwrap().id, second.id);
                assert_eq!(
                    primary.playback.as_ref().unwrap().seconds,
                    starts[0] + 342.375
                );
                assert_eq!(comparison.position(), (second.start_ms + 42_375, playing));
                assert!(
                    primary.pause_at_pull_end(&seeking[0]).is_none(),
                    "Comparison owns its boundary"
                );
                if leader == 1 {
                    let controls = comparison.state_for_controls(seeking[0].clone());
                    assert_eq!(
                        controls.seconds, seeking[0].seconds,
                        "Keep real primary evidence"
                    );
                    assert_eq!(controls.seeking, Some(starts[0] + 342.375));
                    assert_eq!(
                        primary.comparison_position(&controls),
                        Some((second.start_ms + 42_375, playing))
                    );
                }
            }
        }
    }

    #[test]
    fn comparison_seek_inside_pull_reuses_loaded_timeline_events() {
        for leader in 0..2 {
            let (mut primary, mut comparison, first, _) = provider_comparison_fixture();
            let starts = [
                primary.review.as_ref().unwrap().pull_video_start(&first),
                comparison
                    .metadata_for_test()
                    .review
                    .as_ref()
                    .unwrap()
                    .pull_video_start(&first),
            ];
            let mut states = starts.map(|start| provider_sample(start + 10.0, true, 0));
            comparison.follow_provider_controls(
                &mut primary,
                [&states[0], &states[1]],
                provider_test_now(),
            );
            states[leader] = provider_sample(starts[leader] + 99.875, false, 1);
            let commands = comparison
                .follow_provider_controls(
                    &mut primary,
                    [&states[0], &states[1]],
                    provider_test_now(),
                )
                .unwrap();
            assert!([commands.primary, commands.secondary][leader].is_none());
            assert!(matches!(
                [commands.primary, commands.secondary][1 - leader],
                Some(PlaybackCommand::SeekPaused(_))
            ));
            assert_eq!(primary.pull.as_ref().unwrap().id, first.id);
            assert_eq!(primary.loaded_events.len(), 2);
            assert!(
                primary.next_action().is_none(),
                "A same-pull seek keeps its cached event data"
            );
            assert_eq!(comparison.position(), (first.start_ms + 99_875, false));
        }
    }

    #[test]
    fn comparison_gap_holds_only_peer_once_and_follows_next_shared_pull() {
        for leader in 0..2 {
            let (mut primary, mut comparison, first, second) = provider_comparison_fixture();
            let starts = [
                primary.review.as_ref().unwrap().pull_video_start(&first),
                comparison
                    .metadata_for_test()
                    .review
                    .as_ref()
                    .unwrap()
                    .pull_video_start(&first),
            ];
            let baseline = starts.map(|start| provider_sample(start + 10.0, true, 0));
            comparison.follow_provider_controls(
                &mut primary,
                [&baseline[0], &baseline[1]],
                provider_test_now(),
            );
            let mut gap = baseline.clone();
            gap[leader] = provider_sample(starts[leader] + 299.75, true, 1);
            for iteration in 0..3 {
                let commands = comparison
                    .follow_provider_controls(&mut primary, [&gap[0], &gap[1]], provider_test_now())
                    .unwrap();
                let commands = [commands.primary, commands.secondary];
                assert!(commands[leader].is_none());
                assert_eq!(
                    matches!(commands[1 - leader], Some(PlaybackCommand::Pause)),
                    iteration == 0
                );
            }
            assert!(primary.pull.is_none() && primary.playback.is_some());
            assert!(
                primary.next_action().is_none(),
                "Gap polling must not start WCL event requests"
            );
            assert!(!comparison.unavailable_for_review(&primary));
            gap[leader] = provider_sample(starts[leader] + 300.1, true, 1);
            let commands = comparison
                .follow_provider_controls(&mut primary, [&gap[0], &gap[1]], provider_test_now())
                .unwrap();
            assert!([commands.primary, commands.secondary][leader].is_none());
            assert!(matches!(
                [commands.primary, commands.secondary][1 - leader],
                Some(PlaybackCommand::Seek(_))
            ));
            assert_eq!(primary.pull.as_ref().unwrap().id, second.id);
            assert_eq!(comparison.position(), (second.start_ms + 100, true));
        }
    }

    #[test]
    fn unavailable_comparison_peer_cannot_rewind_or_stop_seeking_pov() {
        let (mut primary, mut comparison, first, _) = provider_comparison_fixture();
        comparison
            .metadata_for_test()
            .review
            .as_mut()
            .unwrap()
            .pulls
            .truncate(1);
        let start = primary.review.as_ref().unwrap().pull_video_start(&first);
        let baseline = [
            provider_sample(start + 10.0, true, 0),
            provider_sample(start + 610.625, true, 0),
        ];
        comparison.follow_provider_controls(
            &mut primary,
            [&baseline[0], &baseline[1]],
            provider_test_now(),
        );
        let target = provider_sample(start + 340.0, true, 1);
        let commands = comparison
            .follow_provider_controls(&mut primary, [&target, &baseline[1]], provider_test_now())
            .unwrap();
        assert!(commands.primary.is_none());
        assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
        assert!(
            !comparison.unavailable_for_review(&primary),
            "Keep the user's controlling VOD open"
        );
        assert!(primary.playback.as_ref().unwrap().autoplay);
    }

    struct FullscreenPullHarness {
        ctx: egui::Context,
        review: ReviewUi,
        width: f32,
        navigation: egui::Rect,
        popup: egui::Id,
    }
    impl FullscreenPullHarness {
        fn new(review: ReviewUi, width: f32) -> Self {
            Self {
                ctx: egui::Context::default(),
                review,
                width,
                navigation: egui::Rect::NOTHING,
                popup: egui::Id::NULL,
            }
        }
        fn frame(&mut self, events: Vec<egui::Event>) -> Option<PlaybackCommand> {
            let mut command = None;
            let _ = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(self.width, 560.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.spacing_mut().interact_size.y = 32.0;
                    ui.horizontal(|ui| {
                        ui.add_space(144.0);
                        let response = ui.allocate_ui_with_layout(
                            egui::vec2(420.0, 32.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                self.popup = ui.make_persistent_id("review-pull-menu");
                                command = self.review.draw_fullscreen_navigation(ui);
                            },
                        );
                        self.navigation = response.response.rect;
                    });
                    // The video is drawn after the toolbar; the pull popup must
                    // remain in a higher egui layer over its native-player region.
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(0.0, 44.0),
                            egui::pos2(self.width, 560.0),
                        ),
                        0.0,
                        Color32::BLACK,
                    );
                },
            );
            command
        }
        fn click(&mut self, pos: egui::Pos2) -> Option<PlaybackCommand> {
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }])
        }
        fn key(&mut self, key: egui::Key) -> Option<PlaybackCommand> {
            let command = self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            command
        }
    }

    #[test]
    fn fullscreen_pull_controls_navigate_without_reload_and_keep_popup_above_video() {
        for width in [720.0, 980.0, 1920.0] {
            let (review, _, first, second) = provider_review_fixture();
            let mut fullscreen = FullscreenPullHarness::new(review, width);
            fullscreen.frame(vec![]);
            let initial = fullscreen.navigation;
            assert!((initial.width() - 420.0).abs() < 1.0 && initial.height() <= 32.0);
            assert!(initial.right() <= width && initial.left() >= 144.0);
            let next = fullscreen.click(egui::pos2(initial.right() - 14.0, initial.center().y));
            let expected = fullscreen
                .review
                .review
                .as_ref()
                .unwrap()
                .pull_video_start(&second);
            assert!(matches!(next, Some(PlaybackCommand::Seek(seconds)) if seconds == expected));
            assert_eq!(fullscreen.review.pull.as_ref().unwrap().id, second.id);
            assert!(fullscreen.review.active() && fullscreen.review.playback().is_some());
            assert_eq!(fullscreen.navigation, initial);
            assert!(
                fullscreen
                    .click(egui::pos2(initial.right() - 14.0, initial.center().y))
                    .is_none(),
                "Last pull disables Next"
            );
            let previous = fullscreen.click(egui::pos2(initial.left() + 14.0, initial.center().y));
            assert!(matches!(previous, Some(PlaybackCommand::Seek(_))));
            assert_eq!(fullscreen.review.pull.as_ref().unwrap().id, first.id);
            fullscreen.click(initial.center());
            fullscreen.frame(vec![]);
            assert!(fullscreen.review.popup_open);
            let popup = fullscreen.ctx.read_response(fullscreen.popup).unwrap().rect;
            assert!(popup.top() >= initial.bottom() && popup.bottom() <= 560.0);
            assert!(popup.left() >= 0.0 && popup.right() <= width);
            assert_eq!(
                fullscreen.ctx.layer_id_at(popup.center()).unwrap().order,
                egui::Order::Foreground
            );
            assert_eq!(
                fullscreen.navigation, initial,
                "Opening the menu cannot resize the video toolbar"
            );
            fullscreen.key(egui::Key::End);
            let chosen = fullscreen.key(egui::Key::Enter);
            assert!(matches!(chosen, Some(PlaybackCommand::Seek(seconds)) if seconds == expected));
            assert_eq!(fullscreen.review.pull.as_ref().unwrap().id, second.id);
            assert!(!egui::Popup::is_id_open(&fullscreen.ctx, fullscreen.popup));
        }
    }

    #[test]
    fn fullscreen_pull_selection_uses_existing_comparison_clock_mapping() {
        let (review, mut comparison, first, second) = provider_comparison_fixture();
        let mut fullscreen = FullscreenPullHarness::new(review, 980.0);
        fullscreen.frame(vec![]);
        let bar = fullscreen.navigation;
        let command = fullscreen
            .click(egui::pos2(bar.right() - 14.0, bar.center().y))
            .unwrap();
        comparison.command(command, &fullscreen.review);
        assert_eq!(comparison.position(), (second.start_ms, true));
        let at_ms = comparison.position().0;
        let peer = comparison.metadata_for_test().review.as_ref().unwrap();
        assert_eq!(
            crate::review_compare_ui::recording_clock(peer, &second)
                .unwrap()
                .video_seconds(at_ms),
            Some(peer.pull_video_start(&second))
        );
        assert!(fullscreen.review.comparing && fullscreen.review.active());
        fullscreen.frame(vec![]); // Paint the newly enabled Previous button.
        let command = fullscreen
            .click(egui::pos2(bar.left() + 14.0, bar.center().y))
            .unwrap();
        comparison.command(command, &fullscreen.review);
        assert_eq!(comparison.position(), (first.start_ms, true));
        assert!(fullscreen.review.playback().is_some());
    }

    #[test]
    fn settled_playback_pauses_once_at_pull_end_and_rearms_on_replay() {
        let (review, pull, _) = fixture();
        let end = pull_video_start(&review, &pull) + 210.0;
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull.clone());
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = end - 0.001;
        state.mark_polled_now();
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.seconds = end;
        assert!(matches!(
            ui.pause_at_pull_end(&state),
            Some(PlaybackCommand::Pause)
        ));
        state.seconds = end + 2.0;
        assert!(
            ui.pause_at_pull_end(&state).is_none(),
            "No pause spam while acknowledgement is pending"
        );
        state.seconds = end - 0.1;
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.seconds = end;
        assert!(
            ui.pause_at_pull_end(&state).is_none(),
            "Small SDK jitter must not re-arm the boundary"
        );
        ui.seek_absolute(pull.start_ms).unwrap();
        state.seconds = end - 200.0;
        state.mark_polled_now();
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.seconds = end;
        assert!(matches!(
            ui.pause_at_pull_end(&state),
            Some(PlaybackCommand::Pause)
        ));
    }

    #[test]
    fn pull_boundary_ignores_old_view_buffering_and_unacknowledged_actions() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review.clone());
        ui.active = true;
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = pull_video_start(&review, &pull) + 240.0;
        state.mark_polled_now();
        ui.select(pull);
        assert!(
            ui.pause_at_pull_end(&state).is_none(),
            "Old view sample cannot pause new playback"
        );
        state.mark_polled_now();
        state.buffering = true;
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.buffering = false;
        state.seeking = Some(state.seconds - 180.0);
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.seeking = None;
        state.playback_intent = Some(true);
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.playback_intent = Some(false);
        assert!(ui.pause_at_pull_end(&state).is_none());
        state.playback_intent = None;
        ui.aligning = true;
        assert!(
            ui.pause_at_pull_end(&state).is_none(),
            "Calibration may inspect outside the pull"
        );
        ui.aligning = false;
        assert!(matches!(
            ui.pause_at_pull_end(&state),
            Some(PlaybackCommand::Pause)
        ));
    }

    #[test]
    fn pull_outcome_names_the_wipe_phase_without_guessing_missing_data() {
        assert_eq!(pull_outcome(false, Some(2), false), "Wipe · Phase 2");
        assert_eq!(pull_outcome(false, Some(1), true), "Wipe · Intermission 1");
        assert_eq!(pull_outcome(false, None, false), "Wipe");
        assert_eq!(pull_outcome(true, Some(3), false), "Kill");
    }

    struct SelectorHarness {
        ctx: egui::Context,
        pulls: Vec<Pull>,
        current: Option<usize>,
        cursor: Option<usize>,
        button: egui::Rect,
        popup: egui::Id,
        size: egui::Vec2,
        details_right: Option<f32>,
    }
    impl SelectorHarness {
        fn new() -> Self {
            let (_, pull, _) = fixture();
            let ctx = egui::Context::default();
            ctx.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.button_padding = egui::vec2(14.0, 8.0);
            });
            Self {
                ctx,
                pulls: (0..36)
                    .map(|index| {
                        let mut item = pull.clone();
                        item.id = index + 1;
                        item
                    })
                    .collect(),
                current: Some(20),
                cursor: None,
                button: egui::Rect::NOTHING,
                popup: egui::Id::NULL,
                size: egui::vec2(980.0, 720.0),
                details_right: None,
            }
        }
        fn frame(&mut self, events: Vec<egui::Event>) -> Option<usize> {
            let mut chosen = None;
            let output = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, self.size)),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.add_space(180.0);
                    let row = ui.horizontal(|ui| {
                        self.popup = ui.make_persistent_id("review-pull-menu");
                        chosen = draw_pull_selector(
                            ui,
                            &self.pulls,
                            self.current,
                            &mut self.cursor,
                            None,
                        );
                    });
                    self.button = row.response.rect;
                },
            );
            self.details_right = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text().starts_with("Wipe · Phase") => {
                        Some(text.pos.x + text.galley.rect.right())
                    }
                    _ => None,
                })
                .reduce(f32::max);
            if chosen.is_some() {
                self.current = chosen;
            }
            chosen
        }
        fn click(&mut self, pos: egui::Pos2) -> Option<usize> {
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }])
        }
        fn key(&mut self, key: egui::Key) -> Option<usize> {
            let chosen = self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            chosen
        }
        fn open(&mut self) -> egui::Rect {
            self.frame(vec![]);
            self.click(self.button.center());
            self.frame(vec![]);
            assert!(egui::Popup::is_id_open(&self.ctx, self.popup));
            self.ctx.read_response(self.popup).unwrap().rect
        }
    }

    #[test]
    fn pull_phase_and_duration_leave_space_for_the_scrollbar() {
        let mut menu = SelectorHarness::new();
        for size in [egui::vec2(980.0, 720.0), egui::vec2(1440.0, 900.0)] {
            menu.size = size;
            let popup = menu.open();
            let right = menu.details_right.expect("visible phase and duration text");
            assert!(
                right <= popup.right() - 10.0,
                "Pull details overlap scrollbar: {right} in {popup:?}"
            );
            menu.key(egui::Key::Escape);
        }
    }

    #[test]
    fn pull_menu_keeps_height_and_active_pull_when_reopened_and_resized() {
        let mut menu = SelectorHarness::new();
        for size in [
            egui::vec2(980.0, 720.0),
            egui::vec2(1440.0, 900.0),
            egui::vec2(980.0, 720.0),
        ] {
            menu.size = size;
            let rect = menu.open();
            assert!(rect.height() >= 340.0, "Pull list collapsed: {rect:?}");
            assert!(
                rect.width() < 430.0,
                "Pull names expanded the menu: {rect:?}"
            );
            assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect));
            assert_eq!(menu.cursor, Some(20));
            let focused = menu.ctx.memory(|memory| memory.focused()).unwrap();
            let active_row = menu.ctx.read_response(focused).unwrap().rect;
            assert!(
                rect.contains_rect(active_row),
                "The current pull is outside the menu viewport"
            );
            menu.key(egui::Key::Escape);
            assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
            assert_eq!(menu.current, Some(20));
        }
    }

    #[test]
    fn pull_menu_keyboard_selection_stays_in_bounds_and_closes_immediately() {
        let mut menu = SelectorHarness::new();
        menu.open();
        menu.key(egui::Key::ArrowDown);
        assert_eq!(menu.key(egui::Key::Enter), Some(21));
        assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
        menu.open();
        menu.key(egui::Key::End);
        menu.key(egui::Key::ArrowDown);
        assert_eq!(menu.key(egui::Key::Enter), Some(35));
        menu.open();
        menu.key(egui::Key::Home);
        menu.key(egui::Key::ArrowUp);
        assert_eq!(menu.key(egui::Key::Enter), Some(0));
    }

    #[test]
    fn pull_menu_handles_reports_changing_while_open() {
        let mut menu = SelectorHarness::new();
        menu.open();
        menu.pulls.truncate(2);
        menu.frame(vec![]);
        assert_eq!(menu.cursor, Some(0));
        menu.key(egui::Key::End);
        assert_eq!(menu.key(egui::Key::Enter), Some(1));
        assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
        menu.open();
        menu.pulls.clear();
        menu.frame(vec![]);
        assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
    }

    fn pov_fixture() -> Vec<Stream> {
        let (_, _, stream) = fixture();
        (0..30)
            .map(|index| {
                let mut pov = stream.clone();
                pov.user_id = (index + 100).to_string();
                pov.name = format!("Player {index:02}");
                pov.provider = if index % 2 == 0 {
                    Provider::Youtube
                } else {
                    Provider::Twitch
                };
                pov
            })
            .collect()
    }

    struct PovSelectorHarness {
        ctx: egui::Context,
        povs: Vec<Stream>,
        current: Stream,
        menu: PovMenuState,
        button: egui::Rect,
        popup: egui::Id,
    }
    impl PovSelectorHarness {
        fn new() -> Self {
            let povs = pov_fixture();
            let ctx = egui::Context::default();
            ctx.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.button_padding = egui::vec2(14.0, 8.0);
            });
            Self {
                ctx,
                current: povs[0].clone(),
                povs,
                menu: PovMenuState::default(),
                button: egui::Rect::NOTHING,
                popup: egui::Id::NULL,
            }
        }
        fn frame(&mut self, events: Vec<egui::Event>) -> Option<usize> {
            let mut chosen = None;
            let _ = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 720.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.add_space(240.0);
                    let row = ui.horizontal(|ui| {
                        self.popup = ui.make_persistent_id("review-pov-menu");
                        chosen = draw_pov_selector(ui, &self.povs, &self.current, &mut self.menu);
                    });
                    self.button = row.response.rect;
                },
            );
            if let Some(index) = chosen {
                self.current = self.povs[index].clone();
            }
            chosen
        }
        fn key(&mut self, key: egui::Key) -> Option<usize> {
            let chosen = self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            chosen
        }
        fn open(&mut self) -> egui::Rect {
            self.frame(vec![]);
            let pos = self.button.center();
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            self.frame(vec![]);
            assert!(egui::Popup::is_id_open(&self.ctx, self.popup));
            self.ctx.read_response(self.popup).unwrap().rect
        }
    }

    #[test]
    fn pov_menu_removes_uncovered_views_from_mouse_and_keyboard_choices() {
        let mut menu = PovSelectorHarness::new();
        let mut covered = vec![false; menu.povs.len()];
        covered[0] = true;
        covered[4] = true;
        menu.menu.covered = Some(covered);
        menu.open();
        menu.key(egui::Key::ArrowDown);
        assert_eq!(menu.key(egui::Key::Enter), Some(4));
        // An ended current recording keeps its selector label, but cannot
        // re-enable itself as a menu choice when the canonical moment advances.
        menu.menu.covered.as_mut().unwrap()[4] = false;
        menu.open();
        assert_eq!(menu.key(egui::Key::Enter), Some(0));
        menu.menu.covered = Some(vec![false; menu.povs.len()]);
        menu.open();
        assert_eq!(menu.key(egui::Key::Enter), None);
    }

    #[test]
    fn thirty_povs_fit_one_control_and_search_selects_the_matching_player() {
        let mut menu = PovSelectorHarness::new();
        let rect = menu.open();
        assert!(
            menu.button.width() <= 322.0,
            "Thirty POVs expanded the control"
        );
        assert!(
            rect.width() <= 430.0 && rect.height() >= 400.0,
            "POV menu lost its bounded viewport: {rect:?}"
        );
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(980.0, 720.0))
                .contains_rect(rect)
        );
        menu.frame(vec![egui::Event::Text("PLAYER 29 tWiTch".into())]);
        assert_eq!(menu.menu.search, "PLAYER 29 tWiTch");
        menu.frame(vec![]);
        assert_eq!(
            menu.ctx.read_response(menu.popup).unwrap().rect.height(),
            rect.height()
        );
        assert_eq!(menu.key(egui::Key::Enter), Some(29));
        assert_eq!(menu.current.name, "Player 29");
        assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
        menu.open();
        assert!(
            menu.menu.search.is_empty(),
            "Opening the menu should show all POVs again"
        );
        assert_eq!(
            menu.key(egui::Key::Enter),
            None,
            "Choosing the current POV should preserve the player"
        );
        assert!(!egui::Popup::is_id_open(&menu.ctx, menu.popup));
    }

    #[test]
    fn pov_search_keyboard_navigation_skips_unavailable_streams() {
        let mut menu = PovSelectorHarness::new();
        menu.povs[3].status = Status::Offline;
        menu.open();
        menu.frame(vec![egui::Event::Text("twitch".into())]);
        menu.key(egui::Key::ArrowDown);
        assert_eq!(menu.key(egui::Key::Enter), Some(5));
        menu.open();
        menu.frame(vec![egui::Event::Text("Player 03".into())]);
        assert_eq!(menu.key(egui::Key::Enter), None);
        assert!(egui::Popup::is_id_open(&menu.ctx, menu.popup));
        menu.povs[3].status = Status::Live;
        menu.frame(vec![]);
        assert_eq!(menu.key(egui::Key::Enter), Some(3));
    }

    #[test]
    fn pov_search_keeps_player_identity_when_streams_reorder() {
        let mut menu = PovSelectorHarness::new();
        menu.open();
        menu.frame(vec![egui::Event::Text("Player 29".into())]);
        menu.povs.reverse();
        menu.frame(vec![]);
        assert_eq!(menu.key(egui::Key::Enter), Some(0));
        assert_eq!(menu.current.name, "Player 29");
        menu.open();
        menu.frame(vec![egui::Event::Text("Nobody matches".into())]);
        assert_eq!(menu.key(egui::Key::Enter), None);
        assert!(egui::Popup::is_id_open(&menu.ctx, menu.popup));
    }

    #[test]
    fn pov_capture_uses_exact_pending_seek_and_preserves_pause() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review.clone());
        ui.select(pull.clone());
        let target = pull_video_start(&review, &pull) + 137.625;
        ui.seek_absolute_with_playback(pull.start_ms + 137_625, false)
            .unwrap();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = target - 20.0;
        state.seeking = Some(target);
        ui.capture_pov_position(&state);
        let (_, at, autoplay) = ui.pending_focus.as_ref().unwrap();
        assert_eq!(*at, pull.start_ms + 137_625);
        assert!(!autoplay);
    }

    #[test]
    fn pov_capture_preserves_pause_before_the_provider_acknowledges_it() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review.clone());
        ui.select(pull.clone());
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = pull_video_start(&review, &pull) + 137.625;
        state.playback_intent = Some(false);
        state.mark_polled_now();
        ui.capture_pov_position(&state);
        let (_, at, autoplay) = ui.pending_focus.as_ref().unwrap();
        assert_eq!(*at, pull.start_ms + 137_625);
        assert!(
            !autoplay,
            "A stale playing sample must not undo Pause during a POV switch"
        );
    }

    fn timeline_paint(output: &egui::FullOutput) -> (egui::Pos2, f32, egui::Rect) {
        fn visit(
            shape: &egui::Shape,
            circles: &mut Vec<egui::Pos2>,
            lines: &mut Vec<[egui::Pos2; 2]>,
            cursor: &mut Option<f32>,
        ) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        visit(shape, circles, lines, cursor);
                    }
                }
                egui::Shape::Circle(circle) => circles.push(circle.center),
                egui::Shape::LineSegment { points, stroke } if points[0].x == points[1].x => {
                    if stroke.color == Color32::WHITE && stroke.width == 1.5 {
                        *cursor = Some(points[0].x);
                    } else if stroke.color == Color32::from_rgb(39, 43, 51) && stroke.width == 1.0 {
                        lines.push(*points);
                    }
                }
                _ => (),
            }
        }
        let mut circles = Vec::new();
        let mut lines = Vec::new();
        let mut cursor = None;
        for shape in &output.shapes {
            visit(&shape.shape, &mut circles, &mut lines, &mut cursor);
        }
        let left = lines.iter().map(|line| line[0].x).reduce(f32::min).unwrap();
        let right = lines.iter().map(|line| line[0].x).reduce(f32::max).unwrap();
        let grid = egui::Rect::from_min_max(
            egui::pos2(left, lines[0][0].y),
            egui::pos2(right, lines[0][1].y),
        );
        let thumb = circles
            .into_iter()
            .find(|center| center.y < grid.top())
            .unwrap();
        (thumb, cursor.unwrap(), grid)
    }

    #[test]
    fn seek_thumb_and_event_cursor_share_the_rendered_range_at_every_size() {
        for (width, scale) in [
            (640.0, 1.0),
            (980.0, 1.0),
            (1440.0, 1.0),
            (980.0, 1.5),
            (980.0, 2.0),
        ] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            // Apply the queued DPI change before measuring the viewport.
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 300.0),
                    )),
                    ..Default::default()
                },
                |_| {},
            );
            for fraction in [0.0, 0.25, 0.5, 0.666, 1.0] {
                let (review, pull, _) = fixture();
                let start = pull_video_start(&review, &pull);
                let mut review_ui = ReviewUi::default();
                review_ui.review = Some(review);
                review_ui.select(pull);
                let mut state = PlaybackState::default();
                state.ready = true;
                state.seconds = start + 210.0 * fraction;
                state.mark_polled_now();
                for _ in 0..2 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width, 300.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            assert!(
                                review_ui.draw_timeline(ui, &state).is_none(),
                                "Painting a settled video must not seek it"
                            );
                        },
                    );
                    let (thumb, cursor, grid) = timeline_paint(&output);
                    assert!(
                        (thumb.x - cursor).abs() < 0.1,
                        "Thumb {thumb:?} and cursor {cursor} differ at {fraction}"
                    );
                    assert!((thumb.x - egui::lerp(grid.x_range(), fraction as f32)).abs() < 0.1);
                    assert!(
                        grid.left() >= 132.0 && grid.right() < width,
                        "Timeline {grid:?} escaped width={width}, scale={scale}, position={fraction}"
                    );
                }
            }
        }
    }

    #[test]
    fn playback_controls_and_timeline_do_not_move_when_playback_state_changes() {
        let (review, pull, _) = fixture();
        let start = pull_video_start(&review, &pull);
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.select(pull);
        let ctx = egui::Context::default();
        let mut style = (*ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 8.0);
        ctx.set_global_style(style);
        let mut expected = None;
        for mode in 0..6 {
            let mut state = PlaybackState::default();
            state.ready = mode != 4;
            state.playing = mode == 0;
            state.seconds = start + if mode == 5 { 210.0 } else { 72.0 };
            state.buffering = matches!(mode, 2 | 3);
            state.seeking = (mode == 2).then_some(start + 72.0);
            state.mark_polled_now();
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    assert!(review_ui.draw_timeline(ui, &state).is_none());
                },
            );
            fn collect(shape: &egui::Shape, buttons: &mut Vec<egui::Rect>, text: &mut String) {
                match shape {
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect(shape, buttons, text);
                        }
                    }
                    egui::Shape::Rect(rect)
                        if (rect.rect.width() - 64.0).abs() < 0.1
                            && (rect.rect.height() - 30.0).abs() < 0.1 =>
                    {
                        buttons.push(rect.rect)
                    }
                    egui::Shape::Text(label) => {
                        text.push_str(label.galley.text());
                        text.push('\n');
                    }
                    _ => (),
                }
            }
            let mut buttons = Vec::new();
            let mut text = String::new();
            for shape in &output.shapes {
                collect(&shape.shape, &mut buttons, &mut text);
            }
            let button = *buttons
                .first()
                .expect("Playback button must retain its allocation");
            let (_, _, grid) = timeline_paint(&output);
            if let Some((old_button, old_grid)) = expected {
                assert_eq!(button, old_button);
                assert_eq!(
                    grid, old_grid,
                    "Playback status must not move the seek target"
                );
            } else {
                expected = Some((button, grid));
            }
            for status in [
                "Seeking to",
                "Buffering",
                "Waiting for video",
                "Pull finished",
            ] {
                assert!(!text.contains(status));
            }
        }
    }

    #[test]
    fn dragging_seekbar_keeps_event_cursor_under_thumb_and_preserves_pause() {
        let (review, pull, _) = fixture();
        let start = pull_video_start(&review, &pull);
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.select(pull);
        let ctx = egui::Context::default();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = start;
        let mut frame = |events| {
            state.mark_polled_now();
            let mut command = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 300.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    command = review_ui.draw_timeline(ui, &state);
                },
            );
            (output, command)
        };
        frame(vec![]);
        let (output, _) = frame(vec![]);
        let (thumb, _, grid) = timeline_paint(&output);
        frame(vec![
            egui::Event::PointerMoved(thumb),
            egui::Event::PointerButton {
                pos: thumb,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let target = egui::pos2(egui::lerp(grid.x_range(), 0.687), thumb.y);
        let (output, _) = frame(vec![egui::Event::PointerMoved(target)]);
        let (thumb, cursor, _) = timeline_paint(&output);
        assert!((thumb.x - cursor).abs() < 0.1);
        assert!((thumb.x - target.x).abs() < 0.1);
        let (output, command) = frame(vec![egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let (thumb, cursor, _) = timeline_paint(&output);
        assert!(
            (thumb.x - cursor).abs() < 0.1,
            "Release must keep the cursor under the thumb"
        );
        let Some(PlaybackCommand::SeekPaused(seconds)) = command else {
            panic!("Paused drag must emit a paused seek");
        };
        assert!((seconds - start - 210.0 * 0.687).abs() < 0.002);
    }

    #[test]
    fn timeline_keeps_requested_cursor_until_seek_finishes_then_holds_during_buffering() {
        let (review, pull, _) = fixture();
        let start = pull_video_start(&review, &pull);
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.select(pull.clone());
        let ctx = egui::Context::default();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = start + 140.0;
        state.mark_polled_now();
        let draw = |review_ui: &mut ReviewUi, state: &PlaybackState, expected: f64| {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    assert!(review_ui.draw_timeline(ui, state).is_none());
                },
            );
            let (thumb, cursor, grid) = timeline_paint(&output);
            assert!((thumb.x - cursor).abs() < 0.1);
            assert!((cursor - egui::lerp(grid.x_range(), expected as f32 / 210.0)).abs() < 0.1);
            fn has_unknown(shape: &egui::Shape) -> bool {
                match shape {
                    egui::Shape::Vec(shapes) => shapes.iter().any(has_unknown),
                    egui::Shape::Text(text) => text.galley.text().contains("–:––"),
                    _ => false,
                }
            }
            assert!(!output.shapes.iter().any(|shape| has_unknown(&shape.shape)));
        };
        draw(&mut review_ui, &state, 140.0);
        let command = review_ui.seek_absolute_with_playback(pull.start_ms + 72_000, true);
        assert!(matches!(command, Some(PlaybackCommand::Seek(_))));
        // The first frame can still carry the sample from before the gesture.
        draw(&mut review_ui, &state, 72.0);
        state.seeking = Some(start + 72.0);
        state.buffering = true;
        state.playing = false;
        state.playback_intent = Some(true);
        state.mark_polled_now();
        draw(&mut review_ui, &state, 72.0);
        assert!(review_ui.confirmed_video_position(&state).is_none());
        assert!(review_ui.pause_at_pull_end(&state).is_none());
        // Settling replaces the target with actual media time.
        state.seeking = None;
        state.buffering = false;
        state.playing = true;
        state.playback_intent = None;
        state.seconds = start + 73.25;
        state.mark_polled_now();
        draw(&mut review_ui, &state, 73.25);
        state.buffering = true;
        draw(&mut review_ui, &state, 73.25);
        assert!(review_ui.confirmed_video_position(&state).is_none());
    }

    #[test]
    fn timeline_click_seeks_once_and_accepts_a_new_target_while_waiting() {
        let (review, pull, _) = fixture();
        let start = pull_video_start(&review, &pull);
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.select(pull);
        let ctx = egui::Context::default();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = start + 145.0;
        state.mark_polled_now();
        let mut frame = |events, state: &PlaybackState| {
            let mut command = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 300.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    command = review_ui.draw_timeline(ui, state);
                },
            );
            (output, command)
        };
        frame(vec![], &state);
        for target in [72.125, 110.375] {
            let (output, _) = frame(vec![], &state);
            let (_, _, grid) = timeline_paint(&output);
            let pos = egui::pos2(
                egui::lerp(grid.x_range(), target as f32 / 210.0),
                grid.bottom() - 1.0,
            );
            frame(
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &state,
            );
            let (_, command) = frame(
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &state,
            );
            let Some(PlaybackCommand::Seek(seconds)) = command else {
                panic!("Clicking timeline moment {target} must seek and resume playback");
            };
            assert!((seconds - start - target).abs() < 0.002);
            state.seeking = Some(seconds);
            state.playing = false;
            state.buffering = true;
            state.playback_intent = Some(true);
            state.mark_polled_now();
            for _ in 0..3 {
                assert!(
                    frame(vec![], &state).1.is_none(),
                    "Waiting must not resend a seek"
                );
            }
        }
    }

    #[test]
    #[ignore = "opt-in native workspace drawing CPU benchmark, without provider decoding"]
    fn review_workspace_drawing_benchmark() {
        assert_eq!(std::env::var("BRICK_REVIEW_UI_BENCH").as_deref(), Ok("1"));
        println!(
            "review_ui_benchmark pid={} events=8000 povs=30 frames_per_case=600 warmup=30",
            std::process::id()
        );
        for (width, height, comparing) in [
            (960.0, 640.0, false),
            (1440.0, 800.0, false),
            (1440.0, 800.0, true),
        ] {
            let (review, pull, stream) = fixture();
            let start = pull_video_start(&review, &pull);
            let mut review_ui = ReviewUi::default();
            review_ui.review = Some(review);
            review_ui.connected = true;
            review_ui.active = true;
            review_ui.select(pull.clone());
            review_ui.comparing = comparing;
            review_ui.kind = EventKind::Defensives;
            review_ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
            review_ui.events = (0..8000)
                .map(|index| {
                    let group = match index % 4 {
                        0 => None,
                        1 => Some(DefensiveGroup::Personal),
                        2 => Some(DefensiveGroup::External),
                        _ => Some(DefensiveGroup::Healing),
                    };
                    RaidEvent {
                        actor_id: 1,
                        observed_buff: false,
                        target_actor_id: None,
                        at_ms: pull.start_ms + index * 210_000 / 8000,
                        actor: format!("Player {}", index % 30),
                        class: "Priest".into(),
                        ability: "Major defensive cooldown".into(),
                        ability_id: 33206,
                        target: Some(format!("Player {}", (index + 1) % 30)),
                        kind: if group.is_some() {
                            EventKind::Defensives
                        } else {
                            EventKind::Deaths
                        },
                        group,
                    }
                })
                .collect();
            let povs = pov_fixture();
            let ctx = egui::Context::default();
            let mut state = PlaybackState::default();
            state.ready = true;
            state.seconds = start;
            let mut samples = Vec::with_capacity(600);
            let mut shape_count = 0;
            let mut painted: Option<(egui::Pos2, f32, egui::Rect)> = None;
            for frame in 0..630 {
                state.seconds = start + (frame % 180) as f64;
                state.mark_polled_now();
                let mut events = Vec::new();
                if (230..430).contains(&frame) {
                    events.push(egui::Event::PointerMoved(egui::pos2(
                        width - 100.0,
                        height / 2.0,
                    )));
                    events.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -32.0),
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                    });
                } else if frame >= 430 {
                    if let Some((thumb, _, grid)) = painted {
                        let fraction = ((frame - 430) % 90) as f32 / 90.0;
                        let pos = egui::pos2(egui::lerp(grid.x_range(), fraction), thumb.y);
                        events.push(egui::Event::PointerMoved(pos));
                        if frame == 430 || frame == 629 {
                            events.push(egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: frame == 430,
                                modifiers: egui::Modifiers::NONE,
                            });
                        }
                    }
                }
                let begin = Instant::now();
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, height),
                        )),
                        time: Some(frame as f64 / 60.0),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        std::hint::black_box(
                            review_ui.draw_workspace(ui, &stream, &povs, &state, false, None, None),
                        );
                    },
                );
                let elapsed = begin.elapsed().as_secs_f64() * 1000.0;
                if frame >= 30 {
                    samples.push(elapsed);
                }
                shape_count = shape_count.max(output.shapes.len());
                painted = Some(timeline_paint(&output));
                std::hint::black_box(&output);
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "review_ui_benchmark width={width} height={height} comparing={comparing} median_ms={:.3} p95_ms={:.3} worst_ms={:.3} max_shapes={shape_count}",
                samples[300], samples[569], samples[599]
            );
        }
    }

    #[test]
    fn comparison_selector_stays_in_the_pov_row_at_small_and_default_sizes() {
        for size in [egui::vec2(924.0, 548.0), egui::vec2(1384.0, 728.0)] {
            let (review, pull, stream) = fixture();
            let mut review_ui = ReviewUi::default();
            review_ui.review = Some(review);
            review_ui.active = true;
            review_ui.connected = true;
            review_ui.select(pull.clone());
            review_ui.comparing = true;
            let mut other = stream.clone();
            other.user_id = "102".into();
            other.name = "Another player with a very long name".into();
            let povs = [stream.clone(), other.clone()];
            let mut comparison =
                crate::review_compare_ui::Comparison::new(&review_ui, other, pull.start_ms, false);
            comparison.metadata_for_test().review = review_ui.review.clone();
            let ctx = egui::Context::default();
            ctx.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.button_padding = egui::vec2(14.0, 8.0);
            });
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ui| {
                        let available = ui.available_rect_before_wrap();
                        let action = review_ui.draw_workspace(
                            ui,
                            &stream,
                            &povs,
                            &PlaybackState::default(),
                            false,
                            None,
                            Some(&mut comparison),
                        );
                        assert!(available.contains_rect(action.rect.unwrap()));
                        assert!(
                            ui.min_rect().right() <= available.right() + 1.0,
                            "Comparison toolbar escaped the window"
                        );
                    },
                );
                let label = |word: &str| {
                    output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) if text.galley.text() == word => Some(text.pos),
                            _ => None,
                        })
                        .unwrap()
                };
                let first = label("POV");
                let second = label("Compare");
                assert!(second.x > first.x);
                assert!(
                    (first.y - second.y).abs() <= 1.0,
                    "Comparison is not beside the POV selector"
                );
            }
        }
    }

    #[test]
    fn utility_timeline_row_appears_only_after_a_spell_is_explicitly_enabled() {
        for enabled in [false, true] {
            let (review, pull, _) = fixture();
            let mut review_ui = ReviewUi::default();
            review_ui.review = Some(review);
            review_ui.select(pull);
            if enabled {
                review_ui.cooldowns.overrides.insert(
                    197908,
                    defensives::Rule {
                        group: Some(DefensiveGroup::Utility),
                        observation: defensives::Observation::Cast,
                    },
                );
            }
            let ctx = egui::Context::default();
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    review_ui.draw_timeline(ui, &PlaybackState::default());
                },
            );
            let utility_row = output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "Buffs / utility")
            });
            assert_eq!(utility_row, enabled);
        }
    }

    #[test]
    fn timeline_reclaims_space_and_restores_it_when_categories_are_toggled() {
        let (review, pull, stream) = fixture();
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.connected = true;
        review_ui.active = true;
        review_ui.select(pull);
        let ctx = egui::Context::default();
        let mut heights = Vec::new();
        let mut grids = Vec::new();
        for hidden in [
            vec![],
            vec![DefensiveGroup::Healing],
            DefensiveGroup::ALL.to_vec(),
            vec![],
        ] {
            review_ui.cooldowns.hidden_groups = hidden;
            for _ in 0..2 {
                let _ = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(980.0, 720.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        let action = review_ui.draw_workspace(
                            ui,
                            &stream,
                            &pov_fixture(),
                            &PlaybackState::default(),
                            true,
                            None,
                            None,
                        );
                        assert!(
                            action.command.is_none(),
                            "A display preference must not control playback"
                        );
                        heights.push(action.rect.unwrap().height());
                        grids.push(review_ui.timeline_grid_height());
                    },
                );
            }
        }
        assert_eq!(heights[2] - heights[0], 22.0);
        assert_eq!(heights[4] - heights[0], 88.0);
        assert_eq!(heights[6], heights[0]);
        assert_eq!(
            grids,
            vec![130.0, 130.0, 108.0, 108.0, 42.0, 42.0, 130.0, 130.0]
        );
    }

    #[test]
    fn server_catalogue_refresh_keeps_user_choices_and_invalidates_cooldown_events_once() {
        let mut review_ui = ReviewUi::default();
        review_ui.cooldowns.overrides.insert(
            200183,
            defensives::Rule {
                group: None,
                observation: defensives::Observation::Cast,
            },
        );
        review_ui
            .cooldowns
            .hidden_groups
            .push(DefensiveGroup::Healing);
        review_ui.cooldown_editor = Some(CooldownEditor::new(
            review_ui.cooldowns.clone(),
            &[],
            DefensiveGroup::Healing,
        ));
        review_ui
            .cooldown_editor
            .as_mut()
            .unwrap()
            .draft
            .overrides
            .insert(
                120517,
                defensives::Rule {
                    group: Some(DefensiveGroup::Healing),
                    observation: defensives::Observation::Buff,
                },
            );
        // A pending local timeline toggle must survive an older worker read.
        review_ui.cooldown_save = Some(review_ui.cooldowns.clone());
        let mut incoming = defensives::Preferences::default();
        incoming.catalog = Arc::new(defensives::Catalog::parse(br#"{"schemaVersion":1,"revision":"new-policy","spells":[{"id":200183,"name":"Apotheosis","category":"damageReduction","defaultEnabled":true},{"id":1234567,"name":"New raid cooldown","category":"healing","defaultEnabled":true}]}"#).unwrap());
        review_ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
        review_ui.requested_events = review_ui.loaded_events.clone();
        assert!(review_ui.accept_cooldown_preferences(incoming.clone()));
        assert_eq!(review_ui.cooldowns.classify(200183), None);
        assert_eq!(
            review_ui.cooldowns.classify(1234567),
            Some(DefensiveGroup::Healing)
        );
        assert!(!review_ui.cooldowns.visible(DefensiveGroup::Healing));
        assert_eq!(review_ui.loaded_events, vec![EventKind::Deaths]);
        assert_eq!(review_ui.requested_events, vec![EventKind::Deaths]);
        assert_eq!(
            review_ui
                .cooldown_editor
                .as_ref()
                .unwrap()
                .draft
                .classify(120517),
            Some(DefensiveGroup::Healing)
        );
        assert!(!review_ui.accept_cooldown_preferences(incoming));
        assert_eq!(
            review_ui.cooldown_save.as_ref().unwrap().classify(200183),
            None
        );
    }

    #[test]
    fn cooldown_editor_caches_observed_names_and_refreshes_only_on_new_data() {
        let event = RaidEvent {
            at_ms: 0,
            actor_id: 1,
            observed_buff: false,
            target_actor_id: None,
            actor: String::new(),
            class: String::new(),
            ability: "Observed Name".into(),
            ability_id: 1234567,
            target: None,
            kind: EventKind::Defensives,
            group: Some(DefensiveGroup::Healing),
        };
        let mut events = vec![event.clone(); 20_000];
        events[1].ability = "Alternate Name".into();
        let mut editor = CooldownEditor::new(Default::default(), &events, DefensiveGroup::Healing);
        assert_eq!(editor.observed_names.len(), 1);
        let names = &editor.observed_names[&event.ability_id];
        assert_eq!(names.display, "Observed Name");
        assert_eq!(names.search.len(), 2);
        assert!(names.search.contains("observed name") && names.search.contains("alternate name"));
        events.clear();
        // Painting/search can continue from the snapshot without the event list.
        assert_eq!(
            editor.observed_names[&event.ability_id].display,
            "Observed Name"
        );
        editor.refresh_names(&events);
        assert!(editor.observed_names.is_empty());
    }

    #[test]
    fn cooldown_editor_is_bounded_with_a_full_custom_spell_list() {
        for size in [
            egui::vec2(664.0, 420.0),
            egui::vec2(924.0, 548.0),
            egui::vec2(1384.0, 728.0),
        ] {
            for advanced in [false, true] {
                let (review, pull, stream) = fixture();
                let mut review_ui = ReviewUi::default();
                review_ui.review = Some(review);
                review_ui.active = true;
                review_ui.connected = true;
                review_ui.select(pull);
                let mut preferences = defensives::Preferences::default();
                for id in 1..=defensives::MAX_OVERRIDES as u64 {
                    preferences.overrides.insert(
                        id,
                        defensives::Rule {
                            group: Some(DefensiveGroup::Utility),
                            observation: defensives::Observation::Buff,
                        },
                    );
                }
                review_ui.cooldown_editor = Some(CooldownEditor::new(
                    preferences,
                    &review_ui.events,
                    DefensiveGroup::Utility,
                ));
                review_ui.cooldown_editor.as_mut().unwrap().advanced = advanced;
                let ctx = egui::Context::default();
                for _ in 0..3 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ui| {
                            let available = ui.available_rect_before_wrap();
                            let action = review_ui.draw_workspace(
                                ui,
                                &stream,
                                &pov_fixture(),
                                &PlaybackState::default(),
                                false,
                                None,
                                None,
                            );
                            assert!(available.contains_rect(action.rect.unwrap()));
                            assert!(
                                ui.min_rect().right() <= available.right() + 1.0,
                                "Cooldown editor expanded beyond the rail"
                            );
                            assert!(
                                ui.min_rect().bottom() <= available.bottom() + 1.0,
                                "Cooldown editor expanded beyond the window"
                            );
                            assert!(
                                action.command.is_none(),
                                "Editing a filter must not seek or pause playback"
                            );
                        },
                    );
                    let editor_rect = ctx
                        .memory(|memory| memory.area_rect(egui::Id::new("cooldown-editor")))
                        .unwrap();
                    assert!(
                        egui::Rect::from_min_size(egui::Pos2::ZERO, size)
                            .contains_rect(editor_rect),
                        "Editor escaped screen: {editor_rect:?}, screen {size:?}, advanced {advanced}"
                    );
                    assert!(
                        output.shapes.len() < 800,
                        "Spell editor should virtualize its rows"
                    );
                }
            }
        }
    }

    #[test]
    fn category_visibility_does_not_hide_events_or_change_tracked_spells() {
        let (review, pull, _) = fixture();
        let mut review_ui = ReviewUi::default();
        review_ui.review = Some(review);
        review_ui.select(pull.clone());
        review_ui.kind = EventKind::Defensives;
        review_ui.loaded_events = vec![EventKind::Defensives];
        review_ui
            .cooldowns
            .hidden_groups
            .push(DefensiveGroup::Healing);
        review_ui.events.push(RaidEvent {
            at_ms: pull.start_ms + 1000,
            actor_id: 1,
            target_actor_id: None,
            observed_buff: false,
            actor: "Healing officer".into(),
            class: "Priest".into(),
            ability: "Apotheosis".into(),
            ability_id: 200183,
            target: None,
            kind: EventKind::Defensives,
            group: Some(DefensiveGroup::Healing),
        });
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            review_ui.draw_events(ui, 500.0);
        });
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "Apotheosis")));
        assert_eq!(
            review_ui.cooldowns.filter_expression().unwrap(),
            defensives::Preferences::default()
                .filter_expression()
                .unwrap()
        );
        assert_eq!(review_ui.cooldown_filter, DefensiveGroup::Healing);
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "All rows")));
    }

    #[test]
    fn editor_tracks_optional_spells_and_restores_their_default() {
        let mut editor = CooldownEditor::new(Default::default(), &[], DefensiveGroup::Healing);
        assert_eq!(editor.category(120517), DefensiveGroup::Healing); // Optional Halo.
        assert_eq!(editor.category(642), DefensiveGroup::Personal);
        let mut rule = editor.draft.rule(120517).unwrap();
        assert_eq!(rule.group, None);
        rule.group = Some(editor.group);
        editor.apply_rule(120517, rule);
        assert_eq!(editor.draft.classify(120517), Some(DefensiveGroup::Healing));
        rule.group = None;
        editor.apply_rule(120517, rule);
        assert!(!editor.draft.overrides.contains_key(&120517));
        assert_eq!(editor.draft, defensives::Preferences::default());
    }

    #[test]
    fn editor_save_queues_changes_and_cancel_discards_them() {
        for save in [false, true] {
            let mut review_ui = ReviewUi::default();
            let mut editor = CooldownEditor::new(Default::default(), &[], DefensiveGroup::Healing);
            editor.apply_rule(
                120517,
                defensives::Rule {
                    group: Some(DefensiveGroup::Healing),
                    observation: defensives::Observation::Cast,
                },
            );
            review_ui.cooldown_editor = Some(editor);
            let ctx = egui::Context::default();
            let mut frame = |events: Vec<egui::Event>| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(980.0, 720.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| review_ui.draw_cooldown_editor(ui.ctx()),
                )
            };
            frame(vec![]);
            let output = frame(vec![]);
            let button = if save { "Save changes" } else { "Cancel" };
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == button => {
                        Some(text.pos + text.galley.rect.center().to_vec2())
                    }
                    _ => None,
                })
                .unwrap();
            for pressed in [true, false] {
                frame(vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            assert_eq!(review_ui.cooldowns, defensives::Preferences::default());
            if save {
                assert_eq!(
                    review_ui.cooldown_save.as_ref().unwrap().classify(120517),
                    Some(DefensiveGroup::Healing)
                );
            } else {
                assert!(review_ui.cooldown_save.is_none() && review_ui.cooldown_editor.is_none());
            }
        }
    }

    #[test]
    fn review_placeholder_is_only_painted_when_native_player_is_absent() {
        for present in [false, true] {
            let (review, pull, stream) = fixture();
            let mut review_ui = ReviewUi::default();
            review_ui.review = Some(review);
            review_ui.connected = true;
            review_ui.active = true;
            review_ui.select(pull);
            let ctx = egui::Context::default();
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(980.0, 720.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    review_ui.draw_workspace(
                        ui,
                        &stream,
                        &pov_fixture(),
                        &PlaybackState::default(),
                        present,
                        None,
                        None,
                    );
                },
            );
            assert_eq!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == "Opening replay…")),
                !present
            );
        }
    }

    #[test]
    #[ignore = "native visual fixture: shows editor and timeline toggle states, closes after 12 seconds"]
    fn cooldown_editor_visual_fixture() {
        struct App {
            review: ReviewUi,
            stream: Stream,
            started: Instant,
        }
        impl eframe::App for App {
            fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
                let elapsed = self.started.elapsed();
                if elapsed > Duration::from_secs(12) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                if let Some(editor) = self.review.cooldown_editor.as_mut() {
                    editor.advanced = elapsed > Duration::from_secs(3);
                }
                if elapsed > Duration::from_secs(6) {
                    self.review.cooldown_editor = None;
                    self.review.cooldowns.hidden_groups = if elapsed < Duration::from_secs(9) {
                        DefensiveGroup::ALL.to_vec()
                    } else {
                        Vec::new()
                    };
                }
                self.review.draw_workspace(
                    ui,
                    &self.stream,
                    &[self.stream.clone()],
                    &PlaybackState::default(),
                    false,
                    None,
                    None,
                );
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
        }
        eframe::run_native(
            "Brick cooldown editor visual fixture",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 720.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    #[cfg(target_os = "linux")]
                    {
                        use winit::platform::x11::EventLoopBuilderExtX11 as _;
                        builder.with_x11().with_any_thread(true);
                    }
                    #[cfg(target_os = "windows")]
                    {
                        use winit::platform::windows::EventLoopBuilderExtWindows as _;
                        builder.with_any_thread(true);
                    }
                })),
                ..Default::default()
            },
            Box::new(|cc| {
                cc.egui_ctx.set_visuals(egui::Visuals::dark());
                let (review, pull, mut stream) = fixture();
                stream.name = "Healing officer".into();
                let mut review_ui = ReviewUi::default();
                review_ui.review = Some(review);
                review_ui.connected = true;
                review_ui.active = true;
                review_ui.select(pull);
                review_ui.kind = EventKind::Defensives;
                review_ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
                review_ui.cooldown_editor = Some(CooldownEditor::new(
                    Default::default(),
                    &[],
                    DefensiveGroup::Healing,
                ));
                Ok(Box::new(App {
                    review: review_ui,
                    stream,
                    started: Instant::now(),
                }))
            }),
        )
        .expect("Editor visual fixture failed");
    }

    #[test]
    fn review_controls_fit_small_and_default_windows_with_long_event_names() {
        for size in [egui::vec2(924.0, 548.0), egui::vec2(1384.0, 728.0)] {
            for aligning in [false, true] {
                let (review, pull, stream) = fixture();
                let mut review_ui = ReviewUi::default();
                review_ui.review = Some(review);
                review_ui.connected = true;
                review_ui.active = true;
                review_ui.select(pull.clone());
                review_ui.aligning = aligning;
                review_ui.events = (0..100)
                    .map(|i| RaidEvent {
                        actor_id: 1,
                        observed_buff: false,
                        target_actor_id: None,
                        at_ms: pull.start_ms + i * 1000,
                        actor: "Long player name ".repeat(6),
                        class: "Priest".into(),
                        ability: "Long defensive name ".repeat(5),
                        ability_id: 33206,
                        target: Some("Another long name ".repeat(5)),
                        kind: EventKind::Defensives,
                        group: Some(DefensiveGroup::External),
                    })
                    .collect();
                review_ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
                review_ui.kind = EventKind::Defensives;
                let ctx = egui::Context::default();
                ctx.global_style_mut(|style| {
                    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                    style.spacing.button_padding = egui::vec2(14.0, 8.0);
                });
                let mut state = PlaybackState::default();
                state.ready = true;
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                for _ in 0..3 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            ..Default::default()
                        },
                        |ui| {
                            let available = ui.available_rect_before_wrap();
                            let action = review_ui.draw_workspace(
                                ui,
                                &stream,
                                &pov_fixture(),
                                &state,
                                false,
                                None,
                                None,
                            );
                            let video = action.rect.unwrap();
                            assert!(
                                available.contains_rect(video),
                                "Video escaped the window: {video:?}"
                            );
                            assert!(
                                ui.min_rect().right() <= available.right() + 1.0,
                                "Long names expanded the workspace width"
                            );
                            assert!(
                                ui.min_rect().bottom() <= available.bottom() + 1.0,
                                "Timeline expanded beyond the window bottom"
                            );
                        },
                    );
                    let label_center = |label: &str| {
                        output
                            .shapes
                            .iter()
                            .find_map(|shape| match &shape.shape {
                                egui::Shape::Text(text) if text.galley.text() == label => {
                                    Some(text.pos.y + text.galley.rect.center().y)
                                }
                                _ => None,
                            })
                            .unwrap()
                    };
                    assert!(
                        (label_center("Show in timeline") - label_center("Edit")).abs() <= 1.0,
                        "The category checkbox and Edit must be vertically centered together"
                    );
                }
            }
        }
    }
}
