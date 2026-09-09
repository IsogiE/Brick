use crate::{
    defensives::DefensiveGroup,
    discord_auth,
    stream_player::{PlaybackCommand, PlaybackState},
    streams::{Status, Stream},
    warcraftlogs::{while_current, Client, EventKind, Pull, RaidEvent, Replay, Review},
};
use eframe::egui::{self, Color32, RichText};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(244, 100, 56);
const MUTED: Color32 = Color32::from_rgb(145, 155, 173);
const DEATH: Color32 = Color32::from_rgb(237, 82, 93);

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
    Health(Pull, Vec<crate::warcraftlogs::HealthBand>),
    HealthTargets(Pull),
    Align(Replay, Pull, i64),
}
enum Data {
    Review(Review),
    Authentication,
    Aligned(String, String, i64),
    Events(String, EventKind, Vec<RaidEvent>),
    Health(
        String,
        Vec<crate::warcraftlogs::HealthBand>,
        crate::warcraftlogs::HealthWindow,
    ),
    HealthTargets(String, Vec<crate::warcraftlogs::HealthTarget>),
}
type Outcome = (u64, String, Result<Data, String>, bool);

#[derive(Default)]
pub struct WorkspaceAction {
    pub rect: Option<egui::Rect>,
    pub reload: bool,
    pub command: Option<PlaybackCommand>,
    pub stream: Option<Stream>,
}

pub struct ReviewUi {
    client: Arc<Mutex<Option<Client>>>,
    work: Option<mpsc::Receiver<Outcome>>,
    work_action: Option<Action>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    key: String,
    last_attempt: Option<Instant>,
    review: Option<Review>,
    notice: Option<String>,
    connected: bool,
    active: bool,
    popup_open: bool,
    signing_in: bool,
    playback: Option<Playback>,
    pull: Option<Pull>,
    kind: EventKind,
    events: Vec<RaidEvent>,
    requested_events: Vec<EventKind>,
    loaded_events: Vec<EventKind>,
    show_all_deaths: bool,
    selected_event: Option<(i64, String)>,
    scroll_to_event: bool,
    aligning: bool,
    alignment_save: Option<i64>,
    event_notice: Option<String>,
    search: String,
    scrub: Option<f64>,
    pending_focus: Option<(Pull, i64, bool)>,
    pull_menu_cursor: Option<usize>,
    pov_menu: PovMenuState,
    range_epoch: Instant,
    range_pause_sent: bool,
    health_window: Option<Arc<crate::warcraftlogs::HealthWindow>>,
    health_names: Vec<crate::replay_ocr::BossName>,
    health_enabled: bool,
    health_bands: Vec<crate::warcraftlogs::HealthBand>,
    health_attempted: Vec<crate::warcraftlogs::HealthBand>,
    health_window_bands: Vec<crate::warcraftlogs::HealthBand>,
    health_last_query: Option<Instant>,
    health_last_observation: Option<Instant>,
    health_targets: Vec<crate::warcraftlogs::HealthTarget>,
    health_targets_requested: bool,
}

impl Default for ReviewUi {
    fn default() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            work: None,
            work_action: None,
            cancel: Arc::new(AtomicBool::new(false)),
            generation: 0,
            key: String::new(),
            last_attempt: None,
            review: None,
            notice: None,
            connected: false,
            active: false,
            popup_open: false,
            signing_in: false,
            playback: None,
            pull: None,
            kind: EventKind::Deaths,
            events: Vec::new(),
            requested_events: Vec::new(),
            loaded_events: Vec::new(),
            show_all_deaths: false,
            selected_event: None,
            scroll_to_event: false,
            aligning: false,
            alignment_save: None,
            event_notice: None,
            search: String::new(),
            scrub: None,
            pending_focus: None,
            pull_menu_cursor: None,
            pov_menu: PovMenuState::default(),
            range_epoch: Instant::now(),
            range_pause_sent: false,
            health_window: None,
            health_names: Vec::new(),
            health_enabled: false,
            health_bands: Vec::new(),
            health_attempted: Vec::new(),
            health_window_bands: Vec::new(),
            health_last_query: None,
            health_last_observation: None,
            health_targets: Vec::new(),
            health_targets_requested: false,
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
    pub fn obscures_player(&self) -> bool {
        self.popup_open
    }
    pub fn playback(&self) -> Option<&Playback> {
        self.playback.as_ref()
    }

    pub(crate) fn observation_context(&self) -> Option<(&Replay, &Pull)> {
        if !self.active || self.pending_focus.is_some() || self.popup_open || self.aligning {
            return None;
        }
        let review = self.review.as_ref()?;
        let playback = self.playback.as_ref()?;
        (playback.broadcast_id == review.replay.broadcast_id)
            .then_some((&review.replay, self.pull.as_ref()?))
    }

    pub(crate) fn observation_boss_names(&self) -> &[crate::replay_ocr::BossName] {
        &self.health_names
    }

    pub(crate) fn observation_health_window(
        &self,
    ) -> Option<&Arc<crate::warcraftlogs::HealthWindow>> {
        self.health_window.as_ref()
    }

    pub(crate) fn prepare_health_observation(&mut self) {
        self.health_enabled = true;
    }

    /// Search the log using observed health, not an assumed time delay. Preserve
    /// every recognized label/percent alternative and every same-name NPC.
    pub(crate) fn observe_health(&mut self, observation: &crate::replay_observer::Observation) {
        if observation.observed_at.elapsed() >= Duration::from_secs(10)
            || self.health_last_observation == Some(observation.observed_at)
        {
            return;
        }
        let Some((replay, pull)) = self.observation_context() else {
            return;
        };
        if observation.identity != crate::replay_observer::Identity::new(replay, pull) {
            return;
        }
        self.health_last_observation = Some(observation.observed_at);
        let mut bands = Vec::<crate::warcraftlogs::HealthBand>::new();
        let mut covered = self.health_window.is_some();
        for reading in &observation.health_readings {
            let candidate = &reading.candidate;
            let Some(name) = self
                .health_names
                .iter()
                .find(|name| name.id == candidate.boss_name_id)
            else {
                return;
            };
            let mut game_ids: Vec<_> = self
                .health_targets
                .iter()
                .filter(|target| crate::replay_ocr::normalize_boss_name(&target.name) == name.name)
                .map(|target| target.game_id)
                .collect();
            game_ids.sort_unstable();
            game_ids.dedup();
            if game_ids.is_empty()
                || !candidate.percent.is_finite()
                || !(0.0..=100.0).contains(&candidate.percent)
                || candidate.decimal_places > 2
            {
                return;
            }
            let unit = 10.0_f64.powi(-i32::from(candidate.decimal_places));
            let lower = (candidate.percent - unit).max(0.0);
            let upper = (candidate.percent + unit).min(100.0);
            covered &= self.health_window_bands.iter().any(|band| {
                band.game_ids == game_ids && band.min_percent <= lower && band.max_percent >= upper
            });
            // This is a log search band, not a playback offset. Extra health
            // coverage lets several subsequent frames share one bounded query.
            let band = crate::warcraftlogs::HealthBand {
                game_ids,
                min_percent: (lower - 3.0).floor().max(0.0),
                max_percent: (upper + 3.0).ceil().min(100.0),
            };
            if !bands.contains(&band) {
                bands.push(band);
            }
            if bands.len() > 32 {
                return;
            }
        }
        if covered || bands.is_empty() {
            return;
        }
        bands.sort_by(|a, b| {
            a.game_ids
                .cmp(&b.game_ids)
                .then(a.min_percent.total_cmp(&b.min_percent))
                .then(a.max_percent.total_cmp(&b.max_percent))
        });
        let mut merged = Vec::<crate::warcraftlogs::HealthBand>::new();
        for band in bands {
            if let Some(last) = merged.last_mut().filter(|last| {
                last.game_ids == band.game_ids && band.min_percent <= last.max_percent
            }) {
                last.max_percent = last.max_percent.max(band.max_percent);
            } else {
                merged.push(band);
            }
        }
        self.health_bands = merged;
    }

    fn clear_health(&mut self) {
        self.health_window = None;
        self.health_names.clear();
        self.health_enabled = false;
        self.health_bands.clear();
        self.health_attempted.clear();
        self.health_window_bands.clear();
        self.health_last_query = None;
        self.health_last_observation = None;
        self.health_targets.clear();
        self.health_targets_requested = false;
    }

    pub fn tick(&mut self, ctx: &egui::Context, stream: Option<&Stream>) -> bool {
        let key = stream
            .map(|s| format!("{}:{}:{}", s.user_id, s.provider.key(), s.channel_id))
            .unwrap_or_default();
        let mut changed = false;
        if self.key != key {
            self.cancel_read();
            self.clear_health();
            self.key = key;
            self.review = None;
            self.notice = None;
            self.last_attempt = None;
            self.playback = None;
            self.pull = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.selected_event = None;
            self.popup_open = false;
            if stream.is_none() {
                self.active = false;
                self.pending_focus = None;
            }
            changed = true;
        }
        if !self.active
            && matches!(
                self.work_action,
                Some(Action::Events(..) | Action::Health(..) | Action::HealthTargets(..))
            )
        {
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
                        self.connected = connected;
                        match result {
                            Ok(Data::Review(review)) => {
                                changed |= self.accept_review(review);
                                changed |= self.restore_pov_position();
                            }
                            Ok(Data::Aligned(broadcast, report, seconds)) => {
                                if let Some(review) = self
                                    .review
                                    .as_mut()
                                    .filter(|r| r.replay.broadcast_id == broadcast)
                                {
                                    review.timing.insert(report, seconds);
                                }
                                self.reset_playback_range();
                                self.aligning = false;
                                self.notice = None;
                            }
                            Ok(Data::Authentication) => {
                                self.clear_health();
                                self.review = None;
                                self.pull = None;
                                self.events.clear();
                                self.requested_events.clear();
                                self.loaded_events.clear();
                                self.selected_event = None;
                                self.notice = None;
                                self.last_attempt = None;
                            }
                            Ok(Data::Events(key, kind, events)) => {
                                if self.pull.as_ref().is_some_and(|p| pull_key(p) == key) {
                                    self.events.retain(|event| event.kind != kind);
                                    self.events.extend(events);
                                    self.events.sort_by_key(|event| event.at_ms);
                                    self.loaded_events.push(kind);
                                }
                            }
                            Ok(Data::Health(key, bands, window)) => {
                                if self.pull.as_ref().is_some_and(|p| pull_key(p) == key) {
                                    self.health_window = Some(Arc::new(window));
                                    self.health_window_bands = bands;
                                }
                            }
                            Ok(Data::HealthTargets(key, targets)) => {
                                if self.pull.as_ref().is_some_and(|p| pull_key(p) == key) {
                                    let mut names: Vec<_> = targets
                                        .iter()
                                        .map(|trace| {
                                            crate::replay_ocr::normalize_boss_name(&trace.name)
                                        })
                                        .filter(|name| !name.is_empty())
                                        .collect();
                                    names.sort();
                                    names.dedup();
                                    self.health_names = if names.len() <= 32 {
                                        names
                                            .into_iter()
                                            .enumerate()
                                            .map(|(index, name)| crate::replay_ocr::BossName {
                                                id: index as u32 + 1,
                                                name,
                                            })
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };
                                    self.health_targets = targets;
                                }
                            }
                            Err(error) => {
                                if matches!(
                                    action,
                                    Some(Action::Health(..) | Action::HealthTargets(..))
                                ) {
                                    // Optional alignment evidence remains unavailable;
                                    // it must not replace or erase the selected pull.
                                } else if let Some(Action::Events(pull, _)) = action {
                                    if self
                                        .pull
                                        .as_ref()
                                        .is_some_and(|p| pull_key(p) == pull_key(&pull))
                                    {
                                        self.event_notice = Some(error);
                                    }
                                } else {
                                    self.notice = Some(error);
                                }
                            }
                        }
                        if !connected {
                            self.clear_health();
                            self.review = None;
                            self.pull = None;
                            self.events.clear();
                            self.requested_events.clear();
                            self.loaded_events.clear();
                            self.selected_event = None;
                            self.playback = None;
                            self.active = false;
                            changed = true;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.work = None;
                    self.work_action = None;
                    self.signing_in = false;
                    if !self.cancel.load(Ordering::Relaxed) {
                        self.notice = Some("Warcraft Logs stopped loading. Try again.".into());
                    }
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if let Some(stream) = stream {
            if let Some(action) = self.next_action() {
                self.start(ctx, stream, action);
            }
        }
        changed
    }

    // Keep the worker until it finishes its bounded current request. Derive the
    // next read from the current UI selection instead of queuing obsolete pulls.
    fn cancel_read(&mut self) {
        if !matches!(
            self.work_action,
            Some(
                Action::Refresh
                    | Action::Events(..)
                    | Action::Health(..)
                    | Action::HealthTargets(..)
            )
        ) || self.cancel.swap(true, Ordering::Relaxed)
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
            Some(Action::Health(..)) => self.health_attempted.clear(),
            Some(Action::HealthTargets(..)) => self.health_targets_requested = false,
            _ => (),
        }
    }

    fn next_action(&self) -> Option<Action> {
        if self.work.is_some() {
            return None;
        }
        let missing = [EventKind::Deaths, EventKind::Defensives]
            .into_iter()
            .find(|kind| !self.requested_events.contains(kind));
        if let Some((pull, kind)) = self.pull.clone().zip(missing).filter(|_| self.active) {
            Some(Action::Events(pull, kind))
        } else if let Some(pull) = self
            .pull
            .clone()
            .filter(|_| self.active && self.health_enabled && !self.health_targets_requested)
        {
            Some(Action::HealthTargets(pull))
        } else if let Some(pull) = self.pull.clone().filter(|_| {
            self.active
                && self.health_enabled
                && !self.health_bands.is_empty()
                && self.health_bands != self.health_attempted
                && self
                    .health_last_query
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(15))
        }) {
            Some(Action::Health(pull, self.health_bands.clone()))
        } else if self
            .last_attempt
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            Some(Action::Refresh)
        } else {
            None
        }
    }

    fn accept_review(&mut self, review: Review) -> bool {
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
            self.clear_health();
            self.pull = None;
            self.playback = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.selected_event = None;
            self.scroll_to_event = false;
            self.scrub = None;
            self.aligning = false;
            self.event_notice = None;
        } else if let Some(current) = current {
            if self
                .pull
                .as_ref()
                .is_some_and(|old| old.start_ms != current.start_ms || old.end_ms != current.end_ms)
            {
                self.events.clear();
                self.requested_events.clear();
                self.loaded_events.clear();
                self.selected_event = None;
                self.scroll_to_event = false;
                self.clear_health();
                self.range_epoch = Instant::now();
                self.range_pause_sent = false;
            }
            self.pull = Some(current.clone());
        }
        self.review = Some(review);
        self.notice = unavailable.then(|| "This pull is no longer available.".into());
        broadcast_changed || unavailable
    }

    fn start(&mut self, ctx: &egui::Context, stream: &Stream, action: Action) {
        if self.work.is_some() {
            return;
        }
        let client = self.client.clone();
        let stream = stream.clone();
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
            self.requested_events.push(*kind);
        }
        if let Action::Health(_, bands) = &action {
            self.health_attempted = bands.clone();
            self.health_last_query = Some(Instant::now());
        }
        if matches!(action, Action::HealthTargets(..)) {
            self.health_targets_requested = true;
        }
        self.signing_in = matches!(action, Action::Connect);
        thread::spawn(move || {
            let mut connected = false;
            let result = (|| {
                let token =
                    while_current(&cancel, discord_auth::current_or_refreshed_access_token)??
                        .ok_or("Sign in to Discord again.")?;
                let mut lock = client.lock().map_err(|_| "Warcraft Logs is unavailable.")?;
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
                    Action::Refresh => client.review(&token, &stream).map(Data::Review),
                    Action::Align(replay, pull, seconds) => client
                        .align_video(&replay, &pull, seconds)
                        .map(|_| Data::Aligned(replay.broadcast_id, pull.report, seconds)),
                    Action::Events(pull, kind) => client
                        .events(&token, &pull, kind)
                        .map(|events| Data::Events(pull_key(&pull), kind, events)),
                    Action::Health(pull, bands) => client
                        .health_band_window(&token, &pull, &bands)
                        .map(|window| Data::Health(pull_key(&pull), bands, window)),
                    Action::HealthTargets(pull) => client
                        .health_targets(&token, &pull)
                        .map(|targets| Data::HealthTargets(pull_key(&pull), targets)),
                };
                connected = client.connected();
                result
            })();
            let _ = tx.send((generation, key, result, connected));
            ctx.request_repaint();
        });
    }

    fn select(&mut self, pull: Pull) {
        let Some(review) = &self.review else {
            return;
        };
        let seconds = (pull_video_start(review, &pull) - 5.0).max(0.0);
        let playback = Playback {
            seconds,
            autoplay: true,
            broadcast_id: review.replay.broadcast_id.clone(),
            public_url: review.replay.public_url(seconds as u64),
        };
        self.cancel_read();
        self.clear_health();
        self.pending_focus = None;
        self.playback = Some(playback);
        self.aligning = false;
        self.pull = Some(pull);
        self.events.clear();
        self.requested_events.clear();
        self.loaded_events.clear();
        self.selected_event = None;
        self.event_notice = None;
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
        let review = self.review.as_ref()?;
        let start = review.replay.start_ms().ok()?;
        let correction = self
            .pull
            .as_ref()
            .and_then(|pull| review.timing.get(&pull.report))
            .copied()
            .unwrap_or(0);
        let seconds = (at_ms - start) as f64 / 1000.0 + correction as f64;
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
                        self.start(ui.ctx(), stream, Action::Disconnect);
                        ui.close();
                    }
                });
                self.popup_open = menu.inner.is_some();
            } else if self.signing_in {
                ui.small("Finish signing in in your browser…");
            } else if ui
                .add_enabled(
                    self.work.is_none(),
                    egui::Button::new("Connect Warcraft Logs"),
                )
                .clicked()
            {
                self.notice = None;
                self.start(ui.ctx(), stream, Action::Connect);
            }
        });
        if let Some(notice) = &self.notice {
            ui.add(egui::Label::new(RichText::new(notice).small().color(MUTED)).truncate())
                .on_hover_text(notice);
        }
        changed
    }

    pub fn draw_workspace(
        &mut self,
        ui: &mut egui::Ui,
        stream: &Stream,
        povs: &[Stream],
        state: &PlaybackState,
        player_error: Option<&str>,
    ) -> WorkspaceAction {
        let mut action = WorkspaceAction::default();
        ui.visuals_mut().selection.bg_fill = Color32::from_rgb(113, 52, 33);
        ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
        ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 34, 42);
        ui.visuals_mut().widgets.inactive.bg_fill = Color32::from_rgb(30, 34, 42);

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
                    let badge = if pull.kill {
                        "Kill".into()
                    } else {
                        pull.remaining
                            .map(|n| format!("Wipe · {n:.1}% remaining"))
                            .unwrap_or_else(|| "Wipe".into())
                    };
                    ui.label(RichText::new(badge).color(if pull.kill {
                        Color32::from_rgb(71, 208, 129)
                    } else {
                        DEATH
                    }));
                    ui.label(
                        RichText::new(clock((pull.end_ms - pull.start_ms) as f64 / 1000.0))
                            .color(MUTED),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let menu = ui.menu_button("…", |ui| {
                        if ui
                            .add_enabled(
                                state.ready && self.work.is_none(),
                                egui::Button::new("Align video…"),
                            )
                            .clicked()
                        {
                            self.aligning = true;
                            ui.close();
                        }
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
                    if ui.button("Back to streams").clicked() {
                        leave = true;
                    }
                });
            });
        });
        // Query actual popup memory: an InnerResponse also exists on the frame
        // a popup closes, which otherwise hides the player for an extra frame.
        self.popup_open = egui::Popup::is_any_open(ui.ctx());
        if let Some(i) = selected.filter(|i| Some(*i) != index) {
            self.select(pulls[i].clone());
            action.command = self
                .playback
                .as_ref()
                .map(|p| PlaybackCommand::Seek(p.seconds));
        }
        ui.add_space(5.0);
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new("POV").small().color(MUTED));
                if let Some(index) = draw_pov_selector(ui, povs, stream, &mut self.pov_menu) {
                    self.capture_pov_position(state);
                    action.stream = Some(povs[index].clone());
                }
                ui.label(
                    RichText::new(format!(
                        "{} stream{}",
                        povs.len(),
                        if povs.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(MUTED),
                );
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
        let rail_width = if workspace.width() >= 1100.0 {
            270.0
        } else {
            235.0
        };
        let gap = 14.0;
        let left = egui::Rect::from_min_max(
            workspace.min,
            egui::pos2(workspace.right() - rail_width - gap, workspace.bottom()),
        );
        let timeline_height = if self.aligning { 238.0 } else { 198.0 };
        let video = egui::Rect::from_min_max(
            left.min,
            egui::pos2(
                left.right(),
                (left.bottom() - timeline_height - 8.0).max(left.top() + 100.0),
            ),
        );
        ui.painter()
            .rect_filled(video, 5.0, Color32::from_rgb(10, 12, 16));
        if let Some(error) = player_error {
            ui.scope_builder(egui::UiBuilder::new().max_rect(video.shrink(24.0)), |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((video.height() / 2.0 - 55.0).max(0.0));
                    ui.label(error);
                    if ui.button("Retry player").clicked() {
                        action.reload = true;
                    }
                    if let Some(playback) = &self.playback {
                        ui.hyperlink_to("Open video in browser", &playback.public_url);
                    }
                });
            });
        } else {
            ui.painter().text(
                video.center(),
                egui::Align2::CENTER_CENTER,
                if self.popup_open && state.ready {
                    if state.seeking.is_some() {
                        "Seeking to the selected moment…"
                    } else if state.buffering {
                        "Video is buffering…"
                    } else if state.playing {
                        "Video keeps playing while this menu is open"
                    } else {
                        "Video paused"
                    }
                } else if self.playback.is_some() {
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
                } else {
                    "Choose a pull to watch"
                },
                egui::FontId::proportional(15.0),
                MUTED,
            );
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
        ui.scope_builder(egui::UiBuilder::new().max_rect(rail), |ui| {
            ui.set_clip_rect(rail.intersect(ui.clip_rect()));
            if let Some(command) = self.draw_events(ui, rail.height()) {
                action.command = Some(command);
            }
        });
        if let Some(seconds) = self.alignment_save.take() {
            if let (Some(review), Some(pull)) = (&self.review, &self.pull) {
                self.start(
                    ui.ctx(),
                    stream,
                    Action::Align(review.replay.clone(), pull.clone(), seconds),
                );
            }
        }
        if action.stream.is_some() {
            self.cancel_read();
            self.playback = None;
            self.review = None;
            self.pull = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.selected_event = None;
        }
        if leave || disconnect {
            self.cancel_read();
            self.active = false;
            self.playback = None;
            self.pull = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.selected_event = None;
            self.pending_focus = None;
            self.popup_open = false;
            action.reload = true;
            if disconnect {
                self.start(ui.ctx(), stream, Action::Disconnect);
            }
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
            self.notice = Some("This POV has no recording of the selected pull.".into());
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
        let pull = self.pull.clone()?;
        let duration = (pull.end_ms - pull.start_ms) as f64 / 1000.0;
        let video_start = pull_video_start(self.review.as_ref()?, &pull);
        let confirmed_position = self.confirmed_video_position(state);
        let elapsed = confirmed_position
            .or_else(|| self.playback.as_ref().map(|playback| playback.seconds))
            .map(|seconds| seconds - video_start)
            .unwrap_or(0.0);
        let current = elapsed.clamp(0.0, duration);
        let mut command = None;
        if let Some(target) = state.seeking {
            let target = target - video_start;
            ui.label(
                RichText::new(format!("Seeking to {}…", relative_clock(target)))
                    .small()
                    .color(MUTED),
            );
        } else if state.buffering {
            ui.label(RichText::new("Buffering…").small().color(MUTED));
        } else if confirmed_position.is_none() {
            ui.label(RichText::new("Waiting for video…").small().color(MUTED));
        } else if elapsed < 0.0 {
            ui.label(
                RichText::new(format!("Pull starts in {}", clock(-elapsed)))
                    .small()
                    .color(MUTED),
            );
        } else if elapsed >= duration {
            ui.label(
                RichText::new(if elapsed - duration >= 1.0 {
                    format!("Video is {} past this pull", clock(elapsed - duration))
                } else {
                    "Pull finished".into()
                })
                .small()
                .color(MUTED),
            );
        } else {
            ui.add_space(18.0);
        }
        let playing = playback_intent(state, self.playback.as_ref());
        let replay = confirmed_position.is_some() && elapsed >= duration && !self.aligning;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    state.ready,
                    egui::Button::new(if replay {
                        "Replay pull"
                    } else if playing {
                        "Pause"
                    } else {
                        "Play"
                    }),
                )
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
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    if self.scrub.is_some() || confirmed_position.is_some() {
                        relative_clock(self.scrub.unwrap_or(elapsed))
                    } else {
                        "–:––".into()
                    },
                    clock(duration)
                ))
                .color(MUTED),
            );
            let mut position = self.scrub.unwrap_or(current);
            ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(60.0);
            let response = ui.add_enabled(
                state.ready,
                egui::Slider::new(&mut position, 0.0..=duration).show_value(false),
            );
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
        });
        if self.aligning {
            ui.horizontal_wrapped(|ui| {
                ui.small("Pause at the moment this pull begins.");
                if ui
                    .add_enabled(
                        state.ready
                            && !state.playing
                            && !playback_intent(state, self.playback.as_ref())
                            && state.seeking.is_none()
                            && !state.buffering
                            && state.is_fresh()
                            && self.work.is_none(),
                        egui::Button::new("Set pull start here"),
                    )
                    .clicked()
                {
                    let raw_start = (pull.start_ms
                        - self
                            .review
                            .as_ref()
                            .unwrap()
                            .replay
                            .start_ms()
                            .unwrap_or(pull.start_ms)) as f64
                        / 1000.0;
                    let seconds = (state.seconds - raw_start).round() as i64;
                    if (-600..=600).contains(&seconds) {
                        self.alignment_save = Some(seconds);
                    } else {
                        self.notice = Some(
                            "Move the video within 10 minutes of this pull's estimated start."
                                .into(),
                        );
                    }
                }
                if ui
                    .add_enabled(self.work.is_none(), egui::Button::new("Reset timing"))
                    .clicked()
                {
                    self.alignment_save = Some(0);
                }
                if ui.small_button("Cancel").clicked() {
                    self.aligning = false;
                }
            });
        }
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 135.0),
            egui::Sense::click(),
        );
        let grid =
            egui::Rect::from_min_max(egui::pos2(rect.left() + 132.0, rect.top() + 20.0), rect.max);
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
                "Raid cooldowns",
                Some(DefensiveGroup::Raid),
                Color32::from_rgb(99, 193, 159),
            ),
        ];
        let mut chosen = None;
        for (lane, (label, group, color)) in lanes.iter().enumerate() {
            let y = grid.top() + lane as f32 * 28.0 + 13.0;
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
                    painter.text(
                        center,
                        egui::Align2::CENTER_CENTER,
                        "×",
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(15, 18, 24),
                    );
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
        // A clamped cursor implies this frame belongs to the selected pull.
        // Keep the true elapsed label and hide it while viewing other footage.
        if self.scrub.is_some()
            || (confirmed_position.is_some() && (0.0..=duration).contains(&elapsed))
        {
            let x =
                grid.left() + self.scrub.unwrap_or(current) as f32 / duration as f32 * grid.width();
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
    }

    fn confirmed_video_position(&self, state: &PlaybackState) -> Option<f64> {
        (state.ready
            && state.seconds.is_finite()
            && state.is_fresh_since(self.range_epoch)
            && state.seeking.is_none()
            && !state.buffering)
            .then_some(state.seconds)
    }

    fn pause_at_pull_end(&mut self, state: &PlaybackState) -> Option<PlaybackCommand> {
        // A prior POV's sample, pending seek or stalled frame must never pause
        // the newly selected replay. Timestamp freshness uses SDK request start.
        if !self.active
            || self.aligning
            || self.playback.is_none()
            || !state.ready
            || !state.seconds.is_finite()
            || !state.is_fresh_since(self.range_epoch)
            || state.seeking.is_some()
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

    fn draw_events(&mut self, ui: &mut egui::Ui, height: f32) -> Option<PlaybackCommand> {
        ui.horizontal(|ui| {
            for kind in [EventKind::Deaths, EventKind::Defensives] {
                let count = self.events.iter().filter(|e| e.kind == kind).count();
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
                .hint_text("Find a player or defensive")
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
            ui.label(RichText::new("Major defensive casts").small().color(MUTED));
        }
        ui.separator();
        if let Some(notice) = &self.event_notice {
            ui.label(RichText::new(notice).small().color(MUTED));
            if ui.button("Retry").clicked() {
                self.requested_events
                    .retain(|kind| self.loaded_events.contains(kind));
                self.event_notice = None;
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
        scroll.max_height((height - 120.0).max(50.0)).show_rows(
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
                    let detail = if event.kind == EventKind::Deaths {
                        "Died".into()
                    } else if let Some(target) = &event.target {
                        format!("{} on {}", event.ability, target)
                    } else {
                        event.ability.clone()
                    };
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
#[derive(Default)]
struct PovMenuState {
    search: String,
    cursor: Option<String>,
}

fn pov_key(stream: &Stream) -> String {
    format!(
        "{}:{}:{}",
        stream.user_id,
        stream.provider.key(),
        stream.channel_id
    )
}

fn pov_unavailable(stream: &Stream, current: &Stream) -> Option<&'static str> {
    if pov_key(stream) == pov_key(current) {
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
    let label = format!("{} · {}", current.name, current.provider.label());
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
            let rows = matching_povs(povs, &menu.search);
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
                ui.label("No matching players.");
            }
            const ROW_HEIGHT: f32 = 44.0;
            let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
            let height = ui.available_height().max(ROW_HEIGHT);
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
            scroll.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                for index in range {
                    let (source_index, pov) = rows[index];
                    let watching = pov_key(pov) == pov_key(current);
                    let unavailable = pov_unavailable(pov, current);
                    let enabled = unavailable.is_none();
                    let row = ui
                        .add_enabled_ui(enabled, |ui| {
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_HEIGHT),
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
                                format!("{} · {status}", pov.provider.label()),
                                egui::FontId::proportional(11.0),
                                if watching { ACCENT } else { MUTED },
                            );
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
                        row.on_hover_text(format!("{} · {}", pov.name, pov.provider.label()));
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

/// Give the native menu a stable viewport instead of reusing its last measured
/// height. Opening a late pull also puts that pull in view immediately.
fn draw_pull_selector(
    ui: &mut egui::Ui,
    pulls: &[Pull],
    current: Option<usize>,
    cursor: &mut Option<usize>,
    pending_label: Option<&str>,
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
    let label = current
        .map(|index| {
            format!(
                "{} · Pull {} of {}",
                pulls[index].name,
                index + 1,
                pulls.len()
            )
        })
        .unwrap_or_else(|| pending_label.unwrap_or("Choose a pull").into());
    let button = ui
        .add_enabled_ui(!pulls.is_empty(), |ui| {
            ui.add_sized(
                egui::vec2(252.0, ui.spacing().interact_size.y),
                egui::Button::new(&label).right_text("   ").truncate(),
            )
        })
        .inner;
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
                let label = format!(
                    "{} · {} · {} · {}",
                    index + 1,
                    pull.name,
                    if pull.kill { "Kill" } else { "Wipe" },
                    clock((pull.end_ms - pull.start_ms) as f64 / 1000.0),
                );
                let row = ui.add_sized(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    egui::Button::new(&label)
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
    } else if let Some(target) = &event.target {
        format!("{} · {} on {}", event.actor, event.ability, target)
    } else {
        format!("{} · {}", event.actor, event.ability)
    }
}

fn pull_video_start(review: &Review, pull: &Pull) -> f64 {
    (pull.start_ms - review.replay.start_ms().unwrap_or(pull.start_ms)) as f64 / 1000.0
        + review.timing.get(&pull.report).copied().unwrap_or(0) as f64
}

fn encounter_moment(review: &Review, pull: &Pull, video_seconds: f64) -> i64 {
    pull.start_ms + ((video_seconds - pull_video_start(review, pull)) * 1000.0).round() as i64
}

fn playback_intent(state: &PlaybackState, playback: Option<&Playback>) -> bool {
    if let Some(intent) = state.playback_intent {
        intent
    } else if !state.ready || state.seeking.is_some() || state.buffering {
        playback.is_none_or(|p| p.autoplay)
    } else {
        state.playing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{Provider, Status};
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
            name: "A very long encounter name".into(),
            kill: false,
            start_ms: start,
            end_ms: start + 210_000,
            seconds: 19_800,
        };
        let stream = Stream {
            user_id: "101".into(),
            name: "A guildmate with a long character name".into(),
            provider: Provider::Youtube,
            channel_id: replay.video_id.clone(),
            url: replay.public_url(0),
            status: Status::Live,
            broadcast_state: None,
        };
        (
            Review {
                replay,
                pulls: vec![pull.clone()],
                timing: HashMap::from([(pull.report.clone(), 9)]),
            },
            pull,
            stream,
        )
    }

    #[test]
    fn health_observation_loads_names_before_requesting_log_samples() {
        let (review, pull, _) = fixture();
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull);
        ui.requested_events = vec![EventKind::Deaths, EventKind::Defensives];
        ui.last_attempt = Some(Instant::now());
        assert!(ui.next_action().is_none());
        ui.prepare_health_observation();
        assert!(matches!(ui.next_action(), Some(Action::HealthTargets(_))));
        ui.health_targets_requested = true;
        // Without a real observed percentage there is no bulk health request.
        assert!(ui.next_action().is_none());
    }

    #[test]
    fn observed_health_search_preserves_same_name_actors_and_disjoint_alternatives() {
        use crate::replay_ocr::{BossName, ClockRegion, HealthCandidate};
        let (review, pull, _) = fixture();
        let identity = crate::replay_observer::Identity::new(&review.replay, &pull);
        let mut ui = ReviewUi::default();
        ui.review = Some(review);
        ui.active = true;
        ui.select(pull);
        ui.health_names = vec![BossName {
            id: 1,
            name: "boss".into(),
        }];
        ui.health_targets = vec![
            crate::warcraftlogs::HealthTarget {
                actor: 1,
                game_id: 11,
                name: "Boss".into(),
            },
            crate::warcraftlogs::HealthTarget {
                actor: 2,
                game_id: 12,
                name: "Boss".into(),
            },
            crate::warcraftlogs::HealthTarget {
                actor: 3,
                game_id: 13,
                name: "Different".into(),
            },
        ];
        let region = ClockRegion {
            x: 0.1,
            y: 0.1,
            width: 0.1,
            height: 0.03,
        };
        let mut observation = crate::replay_observer::Observation {
            identity,
            generation: 1,
            width: 1000,
            height: 600,
            observed_at: Instant::now(),
            before_seconds: 100.0,
            after_seconds: 100.1,
            sampling_uncertainty_seconds: 0.1,
            readings: Vec::new(),
            health_readings: [0.0, 74.5]
                .into_iter()
                .map(|percent| crate::replay_observer::HealthReading {
                    region_id: 1001,
                    candidate: HealthCandidate {
                        boss_name_id: 1,
                        percent,
                        decimal_places: 1,
                        name_region: region,
                        percent_region: region,
                    },
                })
                .collect(),
            health_assessment: None,
            phase: crate::replay_observer::Phase::Discovery,
            processing_duration: Duration::ZERO,
            model_load_duration: Duration::ZERO,
        };
        ui.observe_health(&observation);
        assert_eq!(ui.health_bands.len(), 2);
        assert!(ui
            .health_bands
            .iter()
            .all(|band| band.game_ids == vec![11, 12]));
        assert!(ui.health_bands[0].max_percent < ui.health_bands[1].min_percent);
        let original = ui.health_bands.clone();
        observation.identity.video_id = "different-pov".into();
        observation.observed_at = Instant::now();
        observation.health_readings[1].candidate.percent = 40.0;
        ui.observe_health(&observation);
        assert_eq!(ui.health_bands, original);
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
        other.timing.insert(pull.report.clone(), -3);
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
            assert_eq!(*at_ms, pull.start_ms - 5_000);
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
                    at_ms: pull.start_ms + 5_000,
                    actor: "Previous pull".into(),
                    class: "Priest".into(),
                    ability: "Died".into(),
                    ability_id: 0,
                    target: None,
                    kind: EventKind::Deaths,
                    group: None,
                }],
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
        assert!(ui.event_notice.is_none());
        assert!(
            matches!(ui.next_action(), Some(Action::Events(pull, EventKind::Deaths)) if pull.id == 31)
        );
    }

    #[test]
    fn returning_to_same_pov_does_not_accept_its_cancelled_generation() {
        let (review, _, stream) = fixture();
        let key = format!(
            "{}:{}:{}",
            stream.user_id,
            stream.provider.key(),
            stream.channel_id
        );
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
        tx.send((generation, key, Ok(Data::Review(review)), true))
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

    struct SelectorHarness {
        ctx: egui::Context,
        pulls: Vec<Pull>,
        current: Option<usize>,
        cursor: Option<usize>,
        button: egui::Rect,
        popup: egui::Id,
        size: egui::Vec2,
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
            }
        }
        fn frame(&mut self, events: Vec<egui::Event>) -> Option<usize> {
            let mut chosen = None;
            let _ = self.ctx.run_ui(
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

    #[test]
    fn review_controls_fit_small_and_default_windows_with_long_event_names() {
        for size in [egui::vec2(924.0, 548.0), egui::vec2(1384.0, 728.0)] {
            for aligning in [false, true] {
                let (review, pull, stream) = fixture();
                let mut review_ui = ReviewUi::default();
                review_ui.review = Some(review);
                review_ui.active = true;
                review_ui.select(pull.clone());
                review_ui.aligning = aligning;
                review_ui.events = (0..100)
                    .map(|i| RaidEvent {
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
                    let _ = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            ..Default::default()
                        },
                        |ui| {
                            let available = ui.available_rect_before_wrap();
                            let action =
                                review_ui.draw_workspace(ui, &stream, &pov_fixture(), &state, None);
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
                }
            }
        }
    }
}
