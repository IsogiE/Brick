use crate::{
    discord_auth,
    review_compare::{Commands, Controller, RecordingClock, Status},
    review_ui::ReviewUi,
    stream_player::{PlaybackCommand, PlaybackState, StreamPlayer},
    stream_preferences::Preferences,
    streams::{self, Stream},
    warcraftlogs::{Pull, Review},
};
use eframe::egui::{self, Color32, RichText};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Instant,
};

const MUTED: Color32 = Color32::from_rgb(145, 155, 173);
type Preparation = (String, Result<(String, String), String>);
static PREPARING_CREDENTIALS: AtomicBool = AtomicBool::new(false);
struct PreparationPermit;
impl PreparationPermit {
    fn acquire() -> Option<Self> {
        PREPARING_CREDENTIALS
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()
            .map(|_| Self)
    }
}
impl Drop for PreparationPermit {
    fn drop(&mut self) {
        PREPARING_CREDENTIALS.store(false, Ordering::Release);
    }
}

/// A second, explicitly requested media child. Metadata uses the same protected
/// WCL client and cancellation rules as the primary workspace; no player pool.
pub(crate) struct Comparison {
    selected: Stream,
    cancelled: Arc<AtomicBool>,
    metadata: ReviewUi,
    player: Option<StreamPlayer>,
    work: Option<mpsc::Receiver<Preparation>>,
    attempted: bool,
    navigating: bool,
    primary_key: String,
    secondary_key: String,
    clock_versions: Option<[ClockVersion; 2]>,
    controller: Option<Controller>,
    desired: (i64, bool),
    rect: Option<egui::Rect>,
    error: Option<String>,
    search: String,
    popup: bool,
}

impl Drop for Comparison {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Comparison {
    #[cfg(test)]
    pub(crate) fn metadata_for_test(&mut self) -> &mut ReviewUi {
        &mut self.metadata
    }

    pub fn new(review: &ReviewUi, selected: Stream, at_ms: i64, playing: bool) -> Self {
        Self {
            selected,
            cancelled: Arc::new(AtomicBool::new(false)),
            metadata: review.metadata_peer(),
            player: None,
            work: None,
            attempted: false,
            navigating: true,
            primary_key: String::new(),
            secondary_key: String::new(),
            clock_versions: None,
            controller: None,
            desired: (at_ms, playing),
            rect: None,
            error: None,
            search: String::new(),
            popup: false,
        }
    }

    pub fn expanded_stream(&self) -> Option<&Stream> {
        self.fullscreen().then_some(&self.selected)
    }

    pub fn fullscreen(&self) -> bool {
        self.player
            .as_ref()
            .is_some_and(StreamPlayer::is_fullscreen)
    }

    pub fn fullscreen_rect(&mut self, rect: egui::Rect, exit: bool) {
        self.rect = Some(rect);
        if exit {
            if let Some(player) = &self.player {
                player.exit_fullscreen();
            }
        }
    }

    pub fn leave(mut self, primary: Option<&mut StreamPlayer>, review: &ReviewUi) {
        self.save_position();
        if let Some(player) = &mut self.player {
            let _ = player.command(PlaybackCommand::Pause);
        }
        if self
            .controller
            .as_ref()
            .is_none_or(|c| !matches!(c.status(), Status::Playing | Status::Paused))
        {
            if let (Some(primary), Some((metadata, pull))) = (primary, review.comparison_context())
            {
                if let Some(seconds) = recording_clock(metadata, pull)
                    .ok()
                    .and_then(|clock| clock.video_seconds(self.desired.0))
                {
                    let command = if self.desired.1 {
                        PlaybackCommand::Seek(seconds)
                    } else {
                        PlaybackCommand::SeekPaused(seconds)
                    };
                    let _ = primary.command(command);
                }
            }
        }
        // Drop ends the only secondary child and cancels its metadata worker.
    }

    pub fn avoid_duplicate(
        &mut self,
        ctx: &egui::Context,
        next_primary: &Stream,
        old_primary: &Stream,
    ) {
        if stream_key(&self.selected) == stream_key(next_primary) {
            self.choose_secondary(ctx, old_primary.clone());
        }
    }

    fn choose_secondary(&mut self, ctx: &egui::Context, stream: Stream) {
        self.save_position();
        self.selected = stream;
        self.controller = None;
        self.clock_versions = None;
        self.error = None;
        self.attempted = false;
        self.navigating = true;
        self.secondary_key.clear();
        self.search.clear();
        if let Some(player) = &mut self.player {
            let _ = player.command(PlaybackCommand::Pause);
            player.set_visible(false);
        }
        self.metadata.tick(ctx, Some(&self.selected));
    }

    pub fn obscures_player(&self) -> bool {
        self.popup
    }

    pub fn draw_video_timing(&mut self, ui: &mut egui::Ui, primary_pull: &Pull) {
        let pull = self
            .metadata
            .comparison_metadata()
            .and_then(|review| matching_pull(review, primary_pull))
            .cloned();
        ui.push_id("comparison-video-timing", |ui| {
            if let Some(pull) = pull {
                self.metadata.draw_video_timing(ui, &pull);
            } else {
                ui.label(RichText::new("Timing unavailable").small().color(MUTED));
            }
        });
    }

    pub fn state_for_controls(&self, mut state: PlaybackState) -> PlaybackState {
        let intent = self
            .controller
            .as_ref()
            .map_or(self.desired.1, Controller::wants_playing);
        // The barrier's temporary pauses must not look like a user Pause.
        state.playback_intent = Some(intent);
        // Preserve the primary's actual SDK position and buffering state.
        // Marking it buffering because only its peer is catching up made the
        // timeline discard that position and fall back to the initial target.
        state
    }

    pub fn position(&self) -> (i64, bool) {
        self.controller.as_ref().map_or(self.desired, |controller| {
            (controller.position_ms(), controller.wants_playing())
        })
    }

    pub fn unavailable_for_review(&self, primary: &ReviewUi) -> bool {
        let Some((_, pull)) = primary.comparison_context() else {
            return false;
        };
        // Unknown metadata is still loading. A completed response with no
        // matching pull, or without this precise moment, is definitive.
        self.metadata
            .comparison_metadata()
            .is_some_and(|review| !covers_moment(review, pull, self.position().0))
    }

    pub fn draw_inline(
        &mut self,
        ui: &mut egui::Ui,
        povs: &[Stream],
        primary: &Stream,
        labels: &crate::review_ui::RecordingLabels,
        available: &[usize],
    ) -> bool {
        let mut chosen = None;
        ui.label(RichText::new("Compare").small().color(MUTED));
        let menu = egui::ComboBox::from_id_salt("comparison-pov")
            .width(230.0)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .height(330.0)
            .selected_text(crate::review_ui::pov_display_label(labels, &self.selected))
            .show_ui(ui, |ui| {
                ui.set_min_width(260.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Find a player")
                        .desired_width(250.0),
                );
                ui.separator();
                let search = self.search.trim().to_lowercase();
                let mut shown = 0;
                for stream in available
                    .iter()
                    .filter_map(|index| povs.get(*index))
                    .filter(|stream| stream_key(stream) != stream_key(primary))
                {
                    if !search.is_empty() && !stream.name.to_lowercase().contains(&search) {
                        continue;
                    }
                    shown += 1;
                    if ui
                        .add(
                            egui::Button::new(
                                crate::review_ui::recording_label(labels, stream).map_or_else(
                                    || crate::review_ui::pov_display_label(labels, stream),
                                    |recording| {
                                        format!(
                                            "{} · {}\n{} · {}",
                                            stream.name,
                                            stream.provider.label(),
                                            recording.when,
                                            recording.title
                                        )
                                    },
                                ),
                            )
                            .truncate()
                            .selected(stream_key(stream) == stream_key(&self.selected)),
                        )
                        .clicked()
                    {
                        chosen = Some(stream.clone());
                        ui.close();
                    }
                }
                if shown == 0 {
                    ui.label("No other POVs at this moment.");
                }
            });
        self.popup = menu.inner.is_some() && egui::Popup::is_any_open(ui.ctx());
        let close = ui.button("Single view").clicked();
        if let Some(stream) = chosen {
            self.choose_secondary(ui.ctx(), stream);
        }
        close
    }

    pub fn split_video(&mut self, rect: Option<egui::Rect>) -> Option<egui::Rect> {
        let rect = rect?;
        let middle = rect.center().x;
        self.rect = Some(egui::Rect::from_min_max(
            egui::pos2(middle + 5.0, rect.top()),
            rect.max,
        ));
        Some(egui::Rect::from_min_max(
            rect.min,
            egui::pos2(middle - 5.0, rect.bottom()),
        ))
    }

    fn save_position(&mut self) {
        if let Some(controller) = &self.controller {
            self.desired = (controller.position_ms(), controller.wants_playing());
        }
    }

    pub fn primary_changed(&mut self) {
        self.save_position();
        self.controller = None;
        self.clock_versions = None;
        self.primary_key.clear();
        self.error = None;
        if let Some(player) = &mut self.player {
            let _ = player.command(PlaybackCommand::Pause);
        }
    }

    pub fn command(&mut self, command: PlaybackCommand, review: &ReviewUi) {
        let now = Instant::now();
        if let Some((metadata, pull)) = review.comparison_context() {
            if let Err(error) = self.refresh_clocks(metadata, pull, now) {
                self.error = Some(error);
            }
        }
        match command {
            PlaybackCommand::Play | PlaybackCommand::Pause => {
                self.desired.1 = matches!(command, PlaybackCommand::Play);
                if let Some(controller) = &mut self.controller {
                    if self.desired.1 && matches!(controller.status(), Status::Failed(_)) {
                        match controller.seek(controller.position_ms(), true, now) {
                            Ok(()) => self.error = None,
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    } else {
                        controller.set_playing(self.desired.1, now);
                    }
                }
            }
            PlaybackCommand::Seek(seconds) | PlaybackCommand::SeekPaused(seconds) => {
                if let Some((metadata, pull)) = review.comparison_context() {
                    if let Ok(clock) = recording_clock(metadata, pull) {
                        if let Some(at_ms) = clock.encounter_ms(seconds) {
                            self.desired = (at_ms, matches!(command, PlaybackCommand::Seek(_)));
                            self.error = None;
                            // A new pull changes the controller's allowed range.
                            let key = context_key(metadata, pull);
                            if key != self.primary_key {
                                self.controller = None;
                                self.primary_key.clear();
                            } else if let Some(controller) = &mut self.controller {
                                if let Err(error) = controller.seek(at_ms, self.desired.1, now) {
                                    self.error = Some(error.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        primary: Option<&mut StreamPlayer>,
        review: &ReviewUi,
    ) {
        self.metadata.tick(ctx, Some(&self.selected));
        if self.metadata.comparison_metadata().is_none() {
            if let Some(error) = self.metadata.comparison_notice() {
                self.error = Some(error.to_owned());
            }
        }
        if let Some(player) = &mut self.player {
            player.poll_playback(ctx);
        }
        if let Some(error) = self.player.as_ref().and_then(StreamPlayer::failure) {
            self.player = None;
            self.controller = None;
            self.error = Some(error);
        }
        let Some(primary) = primary else {
            if let Some(secondary) = &mut self.player {
                pause_if_needed(secondary);
            }
            return;
        };
        let Some((primary_review, pull)) = review.comparison_context() else {
            pause_if_needed(primary);
            if let Some(secondary) = &mut self.player {
                pause_if_needed(secondary);
            }
            return;
        };
        if let Err(error) = self.refresh_clocks(primary_review, pull, Instant::now()) {
            self.error = Some(error);
        }
        if self.error.is_some() || self.navigating || self.player.is_none() {
            pause_if_needed(primary);
            if let Some(secondary) = &mut self.player {
                pause_if_needed(secondary);
            }
            return;
        }
        if self.controller.is_none() {
            let Some(secondary_review) = self.metadata.comparison_metadata() else {
                return;
            };
            let Some(secondary_pull) = matching_pull(secondary_review, pull) else {
                self.error = Some("This POV does not contain the selected pull.".into());
                pause_if_needed(primary);
                if let Some(secondary) = &mut self.player {
                    pause_if_needed(secondary);
                }
                return;
            };
            let clocks = recording_clock(primary_review, pull).and_then(|first| {
                recording_clock(secondary_review, secondary_pull).map(|second| [first, second])
            });
            let range = [pull.start_ms, pull.end_ms];
            match clocks.and_then(|clocks| {
                Controller::new(
                    clocks,
                    range,
                    self.desired.0.clamp(range[0], range[1]),
                    self.desired.1,
                    Instant::now(),
                )
                .map_err(|e| e.to_string())
            }) {
                Ok(controller) => self.controller = Some(controller),
                Err(error) => {
                    self.error = Some(error);
                    pause_if_needed(primary);
                    if let Some(secondary) = &mut self.player {
                        pause_if_needed(secondary);
                    }
                    return;
                }
            }
        }
        let Some(secondary) = &mut self.player else {
            return;
        };
        let controller = self.controller.as_mut().unwrap();
        let commands = controller.tick(
            [&primary.playback_state(), &secondary.playback_state()],
            Instant::now(),
        );
        if matches!(controller.status(), Status::Failed(_)) {
            // Native commands already have their own bounded retry. A failed
            // pair holds its current frames; the ordinary Play/Seek controls
            // can start a fresh barrier without a second recovery loop.
            controller.set_playing(false, Instant::now());
        }
        self.desired = (controller.position_ms(), controller.wants_playing());
        if let Err(error) = apply_commands(commands, primary, secondary) {
            self.error = Some(error);
            pause_if_needed(primary);
            pause_if_needed(secondary);
        }
    }

    fn refresh_clocks(
        &mut self,
        primary_review: &Review,
        pull: &Pull,
        now: Instant,
    ) -> Result<(), String> {
        let key = context_key(primary_review, pull);
        if key != self.primary_key {
            self.controller = None;
            self.clock_versions = None;
            self.primary_key = key;
        }
        let Some(secondary_review) = self.metadata.comparison_metadata() else {
            return Ok(());
        };
        let Some(secondary_pull) = matching_pull(secondary_review, pull) else {
            return Ok(());
        };
        let versions = [
            ClockVersion::new(primary_review, pull)?,
            ClockVersion::new(secondary_review, secondary_pull)?,
        ];
        let clocks = [
            recording_clock(primary_review, pull)?,
            recording_clock(secondary_review, secondary_pull)?,
        ];
        if let Some(previous) = self.clock_versions.replace(versions) {
            if previous != versions {
                self.save_position();
                // Keep the controller's canonical moment and play intent. Old
                // SDK frames cannot release the new paused-seek barrier.
                if let Some(controller) = &mut self.controller {
                    replace_changed_clocks(controller, previous, versions, clocks, now)?;
                }
                self.error = None;
            }
        }
        Ok(())
    }

    pub fn update_player(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        review: &ReviewUi,
        preferences: Option<Preferences>,
        visible: bool,
    ) {
        if let Some(player) = &self.player {
            player.set_visible(visible && !self.navigating);
        }
        if !visible {
            return;
        }
        let Some(rect) = self.rect else {
            return;
        };
        if let Some(player) = &mut self.player {
            if let Err(error) = player.set_bounds(rect, ctx.pixels_per_point()) {
                self.error = Some(error);
                self.player = None;
            }
        }
        let Some((_, pull)) = review.comparison_context() else {
            return;
        };
        let Some(metadata) = self.metadata.comparison_metadata() else {
            return;
        };
        let Some(secondary_pull) = matching_pull(metadata, pull) else {
            self.error = Some("This POV does not contain the selected pull.".into());
            return;
        };
        let key = stream_key(&self.selected);
        if self.secondary_key != key {
            self.navigating = true;
        }
        if !self.navigating {
            return;
        }
        let target = recording_clock(metadata, secondary_pull)
            .ok()
            .and_then(|clock| clock.video_seconds(self.desired.0));
        let Some(target) = target else {
            self.error = Some("This POV does not contain the selected moment.".into());
            return;
        };
        if let Some(rx) = &self.work {
            match rx.try_recv() {
                Ok((prepared_key, result)) => {
                    self.work = None;
                    if prepared_key != key {
                        self.attempted = false;
                        return;
                    }
                    match result {
                        Ok((url, token)) => {
                            let playback = crate::review_ui::Playback {
                                seconds: target,
                                autoplay: false,
                                broadcast_id: metadata.replay.broadcast_id.clone(),
                                public_url: metadata.replay.public_url(target.floor() as u64),
                            };
                            let url =
                                crate::streams_ui::player_url_for_playback(&url, Some(&playback));
                            let result = if let Some(player) = &mut self.player {
                                player.load_replay(ctx, &url, &token)
                            } else {
                                StreamPlayer::new(
                                    frame,
                                    ctx,
                                    &url,
                                    &token,
                                    rect,
                                    ctx.pixels_per_point(),
                                    preferences,
                                )
                                .map(|player| self.player = Some(player))
                            };
                            match result {
                                Ok(()) => {
                                    self.navigating = false;
                                    self.secondary_key = key;
                                    self.error = None;
                                    if let Some(player) = &self.player {
                                        player.set_visible(true);
                                    }
                                }
                                Err(error) => self.error = Some(error),
                            }
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.work = None;
                    self.error = Some("The second video stopped loading. Try again.".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        } else if !self.attempted {
            let Some(permit) = PreparationPermit::acquire() else {
                return;
            };
            let cancelled = self.cancelled.clone();
            let selected = self.selected.clone();
            let ctx = ctx.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            self.work = Some(rx);
            self.attempted = true;
            thread::spawn(move || {
                let _permit = permit;
                let result = (|| {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err("Comparison closed.".into());
                    }
                    let token = discord_auth::current_or_refreshed_access_token()?
                        .ok_or("Sign in to Discord again.")?;
                    let url = streams::player_url_for_stream(&selected).map_err(|e| e.message)?;
                    Ok((url, token))
                })();
                if !cancelled.load(Ordering::Relaxed) {
                    let _ = tx.send((key, result));
                }
                ctx.request_repaint();
            });
        }
    }
}

fn pause_if_needed(player: &mut StreamPlayer) {
    let state = player.playback_state();
    if (state.playing || state.playback_intent == Some(true))
        && state.playback_intent != Some(false)
    {
        let _ = player.command(PlaybackCommand::Pause);
    }
}

fn apply_commands(
    commands: Commands,
    primary: &mut StreamPlayer,
    secondary: &mut StreamPlayer,
) -> Result<(), String> {
    if let Some(command) = commands.primary {
        primary.command(command)?;
    }
    if let Some(command) = commands.secondary {
        secondary.command(command)?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ClockVersion {
    recording_start_ms: i64,
    pull_start_ms: i64,
    available_seconds: u64,
    correction: i64,
}

impl ClockVersion {
    fn new(review: &Review, pull: &Pull) -> Result<Self, String> {
        Ok(Self {
            recording_start_ms: review.replay.start_ms()?,
            pull_start_ms: pull.start_ms,
            available_seconds: review.replay.available_seconds,
            correction: review
                .timing
                .get(&pull.report)
                .copied()
                .unwrap_or(crate::replay_timing::DEFAULT_SECONDS),
        })
    }
}

fn replace_changed_clocks(
    controller: &mut Controller,
    previous: [ClockVersion; 2],
    next: [ClockVersion; 2],
    clocks: [RecordingClock; 2],
    now: Instant,
) -> Result<(), String> {
    let mut failure = None;
    for side in 0..2 {
        if previous[side] != next[side] {
            if let Err(error) = controller.replace_clock(side, clocks[side], now) {
                failure = Some(error.to_string());
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

pub(crate) fn recording_clock(review: &Review, pull: &Pull) -> Result<RecordingClock, String> {
    let seconds = (pull.start_ms - review.replay.start_ms()?) as f64 / 1000.0
        + review
            .timing
            .get(&pull.report)
            .copied()
            .unwrap_or(crate::replay_timing::DEFAULT_SECONDS) as f64;
    RecordingClock::new(
        pull.start_ms,
        seconds,
        review.replay.available_seconds as f64,
    )
    .map_err(|e| e.to_string())
}

fn matching_pull<'a>(review: &'a Review, selected: &Pull) -> Option<&'a Pull> {
    review
        .pulls
        .iter()
        .find(|pull| pull.report == selected.report && pull.id == selected.id)
        .or_else(|| {
            review.pulls.iter().find(|pull| {
                pull.encounter == selected.encounter
                    && pull.difficulty == selected.difficulty
                    && (pull.start_ms - selected.start_ms).abs() <= 3_000
            })
        })
}

fn covers_moment(review: &Review, selected: &Pull, at_ms: i64) -> bool {
    matching_pull(review, selected)
        .and_then(|pull| recording_clock(review, pull).ok())
        .and_then(|clock| clock.video_seconds(at_ms))
        .is_some()
}

fn context_key(review: &Review, pull: &Pull) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        review.replay.provider.label(),
        review.replay.video_id,
        review.replay.broadcast_id,
        pull.report,
        pull.id,
        pull.start_ms,
        pull.end_ms,
    )
}

fn stream_key(stream: &Stream) -> String {
    format!(
        "{}:{}:{}:{}",
        stream.user_id,
        stream.provider.label(),
        stream.channel_id,
        stream.recording_id.as_deref().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static TEST_CLOCK: std::cell::Cell<Instant> = std::cell::Cell::new(
            Instant::now() - std::time::Duration::from_secs(1)
        );
    }

    fn test_now() -> Instant {
        TEST_CLOCK.with(|clock| {
            let next = clock.get() + std::time::Duration::from_millis(1);
            clock.set(next);
            next
        })
    }

    fn timing_reviews() -> ([Review; 2], Pull) {
        let replay = crate::warcraftlogs::Replay {
            provider: streams::Provider::Youtube,
            video_id: "abcDEF_12-3".into(),
            broadcast_id: "abcDEF_12-3".into(),
            started_at: "2026-09-01T12:00:00Z".into(),
            available_seconds: 5000,
        };
        let start = replay.start_ms().unwrap() + 100_375;
        let pull = Pull {
            report: "abcdefghABCDEFGH".into(),
            id: 1,
            encounter: 1,
            difficulty: 5,
            report_start_ms: start - 50_000,
            remaining: Some(25.0),
            last_phase: None,
            last_phase_is_intermission: false,
            name: "Test encounter".into(),
            kill: false,
            start_ms: start,
            end_ms: start + 300_000,
            seconds: 100,
        };
        let reviews = std::array::from_fn(|side| {
            let mut replay = replay.clone();
            if side == 1 {
                replay.video_id = "different12".into();
                replay.broadcast_id = "different12".into();
            }
            Review {
                replay,
                pulls: vec![pull.clone()],
                timing: std::collections::HashMap::from([(pull.report.clone(), side as i64 * 7)]),
            }
        });
        (reviews, pull)
    }

    fn paused_target(command: Option<PlaybackCommand>) -> f64 {
        match command {
            Some(PlaybackCommand::SeekPaused(seconds)) => seconds,
            _ => panic!("Timing edits must stage both videos paused"),
        }
    }

    fn settled_sample(seconds: f64, playing: bool) -> PlaybackState {
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = seconds;
        state.playing = playing;
        state.mark_polled_at(test_now());
        state
    }

    #[test]
    fn either_timing_control_preserves_canonical_time_and_intent_then_rejects_old_samples() {
        for side in 0..2 {
            for playing in [false, true] {
                let (mut reviews, pull) = timing_reviews();
                let at_ms = pull.start_ms + 12_375;
                let clocks = std::array::from_fn(|i| recording_clock(&reviews[i], &pull).unwrap());
                let mut controller = Controller::new(
                    clocks,
                    [pull.start_ms, pull.end_ms],
                    at_ms,
                    playing,
                    test_now(),
                )
                .unwrap();
                let empty = PlaybackState::default();
                controller.tick([&empty, &empty], test_now());
                let initial =
                    clocks.map(|clock| settled_sample(clock.video_seconds(at_ms).unwrap(), false));
                controller.tick([&initial[0], &initial[1]], test_now());
                if playing {
                    let running = clocks
                        .map(|clock| settled_sample(clock.video_seconds(at_ms).unwrap(), true));
                    controller.tick([&running[0], &running[1]], test_now());
                }
                let old = std::array::from_fn(|i| ClockVersion::new(&reviews[i], &pull).unwrap());
                let old_identity = context_key(&reviews[side], &pull);
                *reviews[side].timing.get_mut(&pull.report).unwrap() += 3;
                assert_eq!(old_identity, context_key(&reviews[side], &pull));
                let next = std::array::from_fn(|i| ClockVersion::new(&reviews[i], &pull).unwrap());
                let clocks = std::array::from_fn(|i| recording_clock(&reviews[i], &pull).unwrap());
                // Even a numerically matching callback requested before the
                // correction must not acknowledge the new seek.
                let stale =
                    clocks.map(|clock| settled_sample(clock.video_seconds(at_ms).unwrap(), false));
                replace_changed_clocks(&mut controller, old, next, clocks, test_now()).unwrap();
                assert_eq!(controller.position_ms(), at_ms);
                assert_eq!(controller.wants_playing(), playing);
                let commands = controller.tick([&stale[0], &stale[1]], test_now());
                assert_eq!(
                    paused_target(commands.primary),
                    clocks[0].video_seconds(at_ms).unwrap()
                );
                assert_eq!(
                    paused_target(commands.secondary),
                    clocks[1].video_seconds(at_ms).unwrap()
                );
                let commands = controller.tick([&stale[0], &stale[1]], test_now());
                assert!(commands.primary.is_none() && commands.secondary.is_none());
                assert_eq!(controller.status(), Status::Preparing);
                let fresh =
                    clocks.map(|clock| settled_sample(clock.video_seconds(at_ms).unwrap(), false));
                let commands = controller.tick([&fresh[0], &fresh[1]], test_now());
                assert_eq!(
                    matches!(commands.primary, Some(PlaybackCommand::Play)),
                    playing
                );
                assert_eq!(
                    matches!(commands.secondary, Some(PlaybackCommand::Play)),
                    playing
                );
                assert_eq!(controller.position_ms(), at_ms);
                if !playing {
                    assert_eq!(controller.status(), Status::Paused);
                    replace_changed_clocks(&mut controller, next, next, clocks, test_now())
                        .unwrap();
                    assert_eq!(controller.status(), Status::Paused);
                }
            }
        }
    }

    #[test]
    fn comparison_clock_uses_visible_default_and_honors_explicit_zero() {
        let (mut reviews, pull) = timing_reviews();
        let zero = recording_clock(&reviews[0], &pull)
            .unwrap()
            .video_seconds(pull.start_ms)
            .unwrap();
        reviews[0].timing.clear();
        let default = recording_clock(&reviews[0], &pull)
            .unwrap()
            .video_seconds(pull.start_ms)
            .unwrap();
        assert_eq!(default - zero, crate::replay_timing::DEFAULT_SECONDS as f64);
        assert_eq!(reviews[1].timing[&pull.report], 7);
    }

    #[test]
    fn secondary_requires_the_selected_pull_and_precise_corrected_media_coverage() {
        let (mut reviews, pull) = timing_reviews();
        let at_ms = pull.start_ms + 12_375;
        assert!(covers_moment(&reviews[1], &pull, at_ms));
        reviews[1].pulls.clear();
        assert!(!covers_moment(&reviews[1], &pull, at_ms));
        reviews[1].pulls.push(pull.clone());
        reviews[1].timing.insert(pull.report.clone(), -113);
        assert!(!covers_moment(&reviews[1], &pull, at_ms));
        reviews[1].timing.insert(pull.report.clone(), -112);
        assert!(covers_moment(&reviews[1], &pull, at_ms));
        reviews[1].timing.insert(pull.report.clone(), 7);
        reviews[1].replay.available_seconds = 119;
        assert!(!covers_moment(&reviews[1], &pull, at_ms));
        reviews[1].replay.available_seconds = 120;
        assert!(covers_moment(&reviews[1], &pull, at_ms));
    }

    #[test]
    fn ordinary_play_rearms_failed_pair_at_its_existing_canonical_moment() {
        let (reviews, pull) = timing_reviews();
        let clocks = std::array::from_fn(|i| recording_clock(&reviews[i], &pull).unwrap());
        let at_ms = pull.start_ms + 12_375;
        let now = test_now();
        let mut controller =
            Controller::new(clocks, [pull.start_ms, pull.end_ms], at_ms, true, now).unwrap();
        let empty = PlaybackState::default();
        let timeout = now + std::time::Duration::from_secs(31);
        let commands = controller.tick([&empty, &empty], timeout);
        assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
        assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
        controller.set_playing(false, timeout);
        assert!(matches!(controller.status(), Status::Failed(_)));
        let mut comparison = Comparison::new(&ReviewUi::default(), stream(), at_ms, false);
        comparison.controller = Some(controller);
        comparison.command(PlaybackCommand::Play, &ReviewUi::default());
        assert_eq!(comparison.position(), (at_ms, true));
        let commands = comparison
            .controller
            .as_mut()
            .unwrap()
            .tick([&empty, &empty], test_now());
        assert_eq!(
            paused_target(commands.primary),
            clocks[0].video_seconds(at_ms).unwrap()
        );
        assert_eq!(
            paused_target(commands.secondary),
            clocks[1].video_seconds(at_ms).unwrap()
        );
    }

    pub(super) fn stream() -> Stream {
        Stream {
            recording_id: Some("abcDEF_12-3".into()),
            replay_start_ms: None,
            replay_end_ms: None,
            user_id: "101".into(),
            name: "Test player".into(),
            provider: streams::Provider::Youtube,
            channel_id: "test-channel".into(),
            url: "https://www.youtube.com/watch?v=abcDEF_12-3".into(),
            status: streams::Status::Offline,
            broadcast_state: None,
        }
    }

    #[test]
    fn credential_preparation_permit_survives_comparison_replacement() {
        let permit = PreparationPermit::acquire().unwrap();
        assert!(PreparationPermit::acquire().is_none());
        drop(permit);
        assert!(PreparationPermit::acquire().is_some());
        let comparison = Comparison::new(&ReviewUi::default(), stream(), 1000, false);
        let cancelled = comparison.cancelled.clone();
        drop(comparison);
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn comparison_uses_two_equal_bounded_rectangles_after_resize() {
        let mut comparison = Comparison::new(&ReviewUi::default(), stream(), 1000, false);
        for width in [960.0, 1440.0, 2560.0] {
            let outer =
                egui::Rect::from_min_size(egui::pos2(8.0, 180.0), egui::vec2(width - 16.0, 450.0));
            let primary = comparison.split_video(Some(outer)).unwrap();
            let secondary = comparison.rect.unwrap();
            assert!(outer.contains_rect(primary) && outer.contains_rect(secondary));
            assert_eq!(primary.width(), secondary.width());
            assert_eq!(secondary.left() - primary.right(), 10.0);
            assert!(!primary.intersects(secondary));
        }
    }

    #[test]
    fn saved_recordings_have_distinct_selection_identities() {
        let first = stream();
        let mut second = first.clone();
        second.recording_id = Some("other-vod".into());
        assert_ne!(stream_key(&first), stream_key(&second));
        second.recording_id = None;
        assert_ne!(stream_key(&first), stream_key(&second));
    }

    #[test]
    fn temporary_barrier_pause_does_not_change_user_intent() {
        let comparison = Comparison::new(&ReviewUi::default(), stream(), 1000, true);
        let mut primary = PlaybackState::default();
        primary.ready = true;
        primary.seconds = 127.25;
        primary.mark_polled_now();
        let state = comparison.state_for_controls(primary.clone());
        assert_eq!(state.playback_intent, Some(true));
        assert!(!state.buffering);
        assert_eq!(state.seconds, primary.seconds);
        assert!(state.is_fresh());
        primary.buffering = true;
        let buffering = comparison.state_for_controls(primary);
        assert!(
            buffering.buffering,
            "Real primary buffering must remain visible"
        );
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod native_test {
    use super::*;
    use crate::warcraftlogs::Replay;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        io::Read,
        sync::{Arc, Mutex},
        time::Duration,
    };

    const START: i64 = 1_800_000_000_000;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SeekProbe {
        Playing,
        Paused,
        Replace,
        Forward,
        ForwardPaused,
        ForwardToggle,
        ForwardBurst,
        ForwardProvider,
    }

    struct Driver {
        players: [Option<StreamPlayer>; 2],
        urls: [String; 2],
        controller: Controller,
        timing_offsets: [f64; 2],
        available_seconds: [u64; 2],
        outcome: Arc<Mutex<Option<Result<(), String>>>>,
        started: Instant,
        phase_at: Instant,
        hold: Duration,
        phase: u8,
        steady_reseeks: u8,
        anchor: i64,
        activated: bool,
        captures: [bool; 2],
        output: Option<std::path::PathBuf>,
        finished: bool,
        seek_probe: Option<SeekProbe>,
        trace_at: Instant,
        burst_step: u8,
        provider_play_since: Option<Instant>,
    }
    impl Driver {
        fn finish(&mut self, ctx: &egui::Context, result: Result<(), String>) {
            self.players = [None, None];
            self.finished = true;
            *self.outcome.lock().unwrap() = Some(result);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        fn next(&mut self, phase: u8) {
            self.phase = phase;
            self.steady_reseeks = 0;
            self.phase_at = Instant::now();
            self.anchor = self.controller.position_ms();
            eprintln!(
                "Native comparison phase {phase}, status {:?}",
                self.controller.status()
            );
        }
        fn adjust_timing(&mut self, side: usize, delta: f64) -> Result<(), String> {
            let before = (
                self.controller.position_ms(),
                self.controller.wants_playing(),
            );
            self.timing_offsets[side] += delta;
            let clock = RecordingClock::new(
                START,
                self.timing_offsets[side],
                self.available_seconds[side] as f64,
            )
            .map_err(|error| error.to_string())?;
            self.controller
                .replace_clock(side, clock, Instant::now())
                .map_err(|error| error.to_string())?;
            if before
                != (
                    self.controller.position_ms(),
                    self.controller.wants_playing(),
                )
            {
                return Err("Timing change reset the canonical moment or play intent".into());
            }
            Ok(())
        }
        fn tick(&mut self, ctx: &egui::Context) -> Result<bool, String> {
            if self.started.elapsed() > Duration::from_secs(85) + self.hold {
                let states: Vec<_> = self.players.iter().map(|p| p.as_ref().map(|p| {
                    let s=p.playback_state();
                    format!("ready={} playing={} buffering={} pending_seek={} pending_intent={:?}",s.ready,s.playing,s.buffering,s.seeking.is_some(),s.playback_intent)
                })).collect();
                return Err(format!(
                    "Comparison timed out in phase {} ({:?}); {:?}",
                    self.phase,
                    self.controller.status(),
                    states
                ));
            }
            if self.phase == 10 {
                let primary = self.players[0]
                    .as_mut()
                    .ok_or("Primary was released with the comparison")?;
                primary.poll_playback(ctx);
                let state = primary.playback_state();
                if self.phase_at.elapsed() >= Duration::from_secs(5) {
                    if !state.ready || !state.playing || state.buffering {
                        return Err("Primary did not continue after releasing the peer".into());
                    }
                    return Ok(true);
                }
                return Ok(false);
            }
            let [Some(first), Some(second)] = &mut self.players else {
                return Ok(false);
            };
            first.poll_playback(ctx);
            second.poll_playback(ctx);
            if let Some(error) = first.failure().or_else(|| second.failure()) {
                return Err(error);
            }
            let commands = self.controller.tick(
                [&first.playback_state(), &second.playback_state()],
                Instant::now(),
            );
            if matches!(self.phase, 1 | 8)
                && matches!(commands.primary, Some(PlaybackCommand::SeekPaused(_)))
                && matches!(commands.secondary, Some(PlaybackCommand::SeekPaused(_)))
            {
                self.steady_reseeks += 1;
                if self.steady_reseeks > 1 {
                    return Err(
                        "Comparison repeatedly restarted both videos during uninterrupted playback"
                            .into(),
                    );
                }
            }
            if std::env::var_os("BRICK_COMPARE_TRACE").is_some()
                && (commands.primary.is_some()
                    || commands.secondary.is_some()
                    || (self.seek_probe.is_some()
                        && self.trace_at.elapsed() >= Duration::from_millis(500)))
            {
                self.trace_at = Instant::now();
                let command = |value: Option<PlaybackCommand>| match value {
                    Some(PlaybackCommand::Play) => "play".into(),
                    Some(PlaybackCommand::Pause) => "pause".into(),
                    Some(PlaybackCommand::Seek(at)) => format!("seek-play:{at:.3}"),
                    Some(PlaybackCommand::SeekPaused(at)) => format!("seek-pause:{at:.3}"),
                    None => "none".into(),
                };
                let states = [first.playback_state(), second.playback_state()].map(|state| {
                    let ages = state.observation_window().map(|[requested, received]| {
                        [
                            requested.elapsed().as_secs_f64(),
                            received.elapsed().as_secs_f64(),
                        ]
                    });
                    format!(
                        "at={:.3},playing={},buffering={},seek={:?},pending={:?},pause_hint={},play_hint={},sample_ages={ages:?}",
                        state.seconds, state.playing, state.buffering, state.seeking, state.playback_intent, state.pause_intent, state.play_intent
                    )
                });
                eprintln!(
                    "Comparison trace phase={} status={:?} commands=[{},{}] states={states:?}",
                    self.phase,
                    self.controller.status(),
                    command(commands.primary),
                    command(commands.secondary)
                );
            }
            apply_commands(commands, first, second)?;
            if let Status::Failed(error) = self.controller.status() {
                return Err(error.to_string());
            }
            if self.phase == 39 {
                let first = self.players[0].as_ref().unwrap().playback_state();
                let second = self.players[1].as_ref().unwrap().playback_state();
                let first_ready = first.ready
                    && first.is_fresh()
                    && !first.playing
                    && !first.buffering
                    && first.seeking.is_none()
                    && first.playback_intent.is_none()
                    && (first.seconds - (self.timing_offsets[0] + 178.125)).abs() <= 0.25;
                let second_advancing = second.ready
                    && second.is_fresh()
                    && second.playing
                    && !second.buffering
                    && second.seconds >= self.timing_offsets[1] + 177.875;
                if first_ready && second_advancing {
                    if self
                        .provider_play_since
                        .get_or_insert_with(Instant::now)
                        .elapsed()
                        >= Duration::from_secs(1)
                    {
                        return Err("Provider Play left its ready peer and shared timeline paused while video advanced".into());
                    }
                } else {
                    self.provider_play_since = None;
                }
            }
            match self.phase {
                0 if self.controller.status() == Status::Playing => self.next(1),
                1 if self.controller.position_ms() >= self.anchor + 1500
                    && self.phase_at.elapsed() >= self.hold =>
                {
                    if let Some(probe) = self.seek_probe {
                        if matches!(probe, SeekProbe::Paused | SeekProbe::ForwardPaused) {
                            self.controller.set_playing(false, Instant::now());
                            self.next(31);
                        } else {
                            let target = match probe {
                                SeekProbe::Replace => 90_375,
                                SeekProbe::Forward
                                | SeekProbe::ForwardPaused
                                | SeekProbe::ForwardToggle
                                | SeekProbe::ForwardBurst
                                | SeekProbe::ForwardProvider => 178_125,
                                _ => 72_125,
                            };
                            self.controller
                                .seek(START + target, true, Instant::now())
                                .map_err(|error| error.to_string())?;
                            self.next(30);
                        }
                        return Ok(false);
                    }
                    if let Some(output) = &self.output {
                        for (side, player) in self.players.iter().enumerate() {
                            let player = player.as_ref().unwrap();
                            if let Some(capture) = player.take_frame_capture() {
                                let capture = capture?;
                                std::fs::write(
                                    output.join(format!("comparison-{side}.png")),
                                    capture.png,
                                )
                                .map_err(|_| "Could not write the explicit private capture")?;
                                self.captures[side] = true;
                            }
                            if !self.captures[side] && !player.frame_capture_pending() {
                                player.request_frame_capture(ctx);
                            }
                        }
                        if !self.captures.iter().all(|captured| *captured) {
                            return Ok(false);
                        }
                    }
                    self.players[0]
                        .as_ref()
                        .unwrap()
                        .diagnostic_provider_command(PlaybackCommand::Pause)?;
                    self.next(20);
                }
                20 if self.controller.status() == Status::Paused => self.next(21),
                21 if self.phase_at.elapsed() >= Duration::from_secs(1) => {
                    if (self.controller.position_ms() - self.anchor).abs() > 500
                        || self
                            .players
                            .iter()
                            .any(|player| player.as_ref().unwrap().playback_state().playing)
                    {
                        return Err("Primary provider Pause did not hold both videos".into());
                    }
                    self.players[0]
                        .as_ref()
                        .unwrap()
                        .diagnostic_provider_command(PlaybackCommand::Play)?;
                    self.next(22);
                }
                22 if self.controller.status() == Status::Playing => {
                    self.players[1]
                        .as_ref()
                        .unwrap()
                        .diagnostic_provider_command(PlaybackCommand::Pause)?;
                    self.next(23);
                }
                23 if self.controller.status() == Status::Paused => self.next(24),
                24 if self.phase_at.elapsed() >= Duration::from_secs(1) => {
                    if (self.controller.position_ms() - self.anchor).abs() > 500
                        || self
                            .players
                            .iter()
                            .any(|player| player.as_ref().unwrap().playback_state().playing)
                    {
                        return Err("Secondary provider Pause did not hold both videos".into());
                    }
                    self.players[1]
                        .as_ref()
                        .unwrap()
                        .diagnostic_provider_command(PlaybackCommand::Play)?;
                    self.next(25);
                }
                25 if self.controller.status() == Status::Playing => {
                    self.controller.set_playing(false, Instant::now());
                    self.next(2);
                }
                2 if self.controller.status() == Status::Paused => self.next(3),
                3 if self.phase_at.elapsed() >= Duration::from_secs(1) => {
                    if (self.controller.position_ms() - self.anchor).abs() > 500 {
                        return Err("Paused comparison moved".into());
                    }
                    self.controller
                        .seek(START + 12_375, false, Instant::now())
                        .map_err(|e| e.to_string())?;
                    self.next(4);
                }
                4 if self.controller.status() == Status::Paused => {
                    if (self.controller.position_ms() - (START + 12_375)).abs() > 500 {
                        return Err("Paused comparison seek missed its shared target".into());
                    }
                    self.adjust_timing(0, 3.0)?;
                    self.next(5);
                }
                5 if self.controller.status() == Status::Paused => {
                    if self.controller.position_ms() != self.anchor {
                        return Err("Paused timing change moved the encounter cursor".into());
                    }
                    for side in 0..2 {
                        let state = self.players[side].as_ref().unwrap().playback_state();
                        let expected =
                            self.timing_offsets[side] + (self.anchor - START) as f64 / 1000.0;
                        if state.playing || (state.seconds - expected).abs() > 0.5 {
                            return Err(
                                "Paused timing change did not reach both video targets".into()
                            );
                        }
                    }
                    self.controller
                        .seek(START + 20_125, true, Instant::now())
                        .map_err(|e| e.to_string())?;
                    self.next(6);
                }
                6 if self.controller.status() == Status::Playing => {
                    self.adjust_timing(1, -2.0)?;
                    self.next(7);
                }
                7 if self.controller.status() == Status::Playing => self.next(8),
                8 if self.controller.status() == Status::Playing
                    && self.controller.position_ms() >= self.anchor + 1500 =>
                {
                    for side in 0..2 {
                        let state = self.players[side].as_ref().unwrap().playback_state();
                        let expected = self.timing_offsets[side]
                            + (self.controller.position_ms() - START) as f64 / 1000.0;
                        if !state.playing || (state.seconds - expected).abs() > 1.5 {
                            return Err(
                                "Playing timing change did not resume both adjusted videos".into(),
                            );
                        }
                    }
                    self.next(9);
                }
                9 if self.controller.status() == Status::Playing => {
                    let second_id = self.players[1].as_ref().unwrap().diagnostic_player_id();
                    if second_id == self.players[0].as_ref().unwrap().diagnostic_player_id() {
                        return Err("Comparison reused a single child for both views".into());
                    }
                    #[cfg(target_os = "linux")]
                    let released = self.players[1].as_ref().unwrap().diagnostic_drop_probe();
                    self.players[1] = None;
                    crate::stream_player::pump_events();
                    #[cfg(target_os = "linux")]
                    if !released.is_some_and(|released| released()) {
                        return Err("The second native child was retained after exit".into());
                    }
                    self.next(10);
                }
                30 if self.seek_probe == Some(SeekProbe::Replace)
                    && self.phase_at.elapsed() >= Duration::from_millis(150) =>
                {
                    self.controller
                        .seek(START + 72_125, true, Instant::now())
                        .map_err(|error| error.to_string())?;
                    self.next(33);
                }
                30 if self.seek_probe == Some(SeekProbe::ForwardToggle)
                    && self.phase_at.elapsed() >= Duration::from_millis(150) =>
                {
                    self.controller.set_playing(false, Instant::now());
                    self.next(34);
                }
                34 if self.phase_at.elapsed() >= Duration::from_millis(150) => {
                    self.controller.set_playing(true, Instant::now());
                    self.next(35);
                }
                30 if self.seek_probe == Some(SeekProbe::ForwardBurst)
                    && self.phase_at.elapsed() >= Duration::from_millis(50) =>
                {
                    self.controller.set_playing(false, Instant::now());
                    self.next(36);
                }
                36 if self.phase_at.elapsed() >= Duration::from_millis(50) => {
                    let now = Instant::now();
                    match self.burst_step {
                        0 => self
                            .controller
                            .seek(START + 180_125, false, now)
                            .map_err(|e| e.to_string())?,
                        1 | 4 => self.controller.set_playing(true, now),
                        2 => self
                            .controller
                            .seek(START + 177_125, true, now)
                            .map_err(|e| e.to_string())?,
                        3 => self.controller.set_playing(false, now),
                        5 => self
                            .controller
                            .seek(START + 178_125, true, now)
                            .map_err(|e| e.to_string())?,
                        _ => return Err("Unexpected rapid command step".into()),
                    }
                    self.burst_step += 1;
                    self.phase_at = now;
                    if self.burst_step == 6 {
                        self.next(37);
                    }
                }
                30 if self.seek_probe == Some(SeekProbe::ForwardProvider)
                    && self.phase_at.elapsed() >= Duration::from_millis(150) =>
                {
                    self.players[1]
                        .as_ref()
                        .unwrap()
                        .diagnostic_provider_command(PlaybackCommand::Play)?;
                    self.next(39);
                }
                31 if self.controller.status() == Status::Paused => {
                    self.controller
                        .seek(
                            START
                                + if self.seek_probe == Some(SeekProbe::ForwardPaused) {
                                    178_125
                                } else {
                                    72_125
                                },
                            false,
                            Instant::now(),
                        )
                        .map_err(|error| error.to_string())?;
                    self.next(32);
                }
                30 | 32 | 33 | 35 | 37 | 39
                    if !(self.phase == 30
                        && matches!(
                            self.seek_probe,
                            Some(
                                SeekProbe::ForwardToggle
                                    | SeekProbe::ForwardBurst
                                    | SeekProbe::ForwardProvider
                            )
                        ))
                        && self.controller.status()
                            == if self.phase == 32 {
                                Status::Paused
                            } else {
                                Status::Playing
                            } =>
                {
                    for side in 0..2 {
                        let state = self.players[side].as_ref().unwrap().playback_state();
                        let target = self.timing_offsets[side]
                            + if matches!(
                                self.seek_probe,
                                Some(
                                    SeekProbe::Forward
                                        | SeekProbe::ForwardPaused
                                        | SeekProbe::ForwardToggle
                                        | SeekProbe::ForwardBurst
                                        | SeekProbe::ForwardProvider
                                )
                            ) {
                                178.125
                            } else {
                                72.125
                            };
                        if state.seeking.is_some()
                            || state.playback_intent.is_some()
                            || state.buffering
                            || state.playing != (self.phase != 32)
                            || (state.seconds - target).abs()
                                > if self.phase == 32 { 0.5 } else { 2.0 }
                        {
                            return Err(format!(
                                "Comparison seek missed side {side}: target={target:.3},actual={:.3}",
                                state.seconds
                            ));
                        }
                    }
                    if self.seek_probe == Some(SeekProbe::ForwardBurst) {
                        self.next(38);
                    } else {
                        return Ok(true);
                    }
                }
                38 if self.phase_at.elapsed() >= Duration::from_secs(2)
                    && self.controller.status() == Status::Playing
                    && self.controller.position_ms() >= self.anchor + 1_000 =>
                {
                    if self.players.iter().any(|player| {
                        let state = player.as_ref().unwrap().playback_state();
                        !state.is_fresh()
                            || !state.playing
                            || state.buffering
                            || state.seeking.is_some()
                            || state.playback_intent.is_some()
                    }) {
                        return Err("Rapid controls left stale pending playback state".into());
                    }
                    return Ok(true);
                }
                _ => (),
            }
            Ok(false)
        }
    }
    impl eframe::App for Driver {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            let ctx = &ctx;
            if self.finished {
                return;
            }
            crate::stream_player::pump_events();
            let mut error = None;
            ui.scope(|ui| {
                ui.heading("Brick · Two-player comparison check");
                ui.label(format!(
                    "Shared timeline · {:?} · phase {}",
                    self.controller.status(),
                    self.phase
                ));
                let available = ui.available_rect_before_wrap();
                let half = (available.width() - 10.0) / 2.0;
                let height = if self.phase >= 5 {
                    (available.height() - 30.0).max(100.0)
                } else {
                    available.height()
                };
                let rects = [
                    egui::Rect::from_min_size(available.min, egui::vec2(half, height)),
                    egui::Rect::from_min_size(
                        egui::pos2(available.left() + half + 10.0, available.top()),
                        egui::vec2(half, height),
                    ),
                ];
                if !self.activated {
                    match activate(frame) {
                        Ok(()) => self.activated = true,
                        Err(e) => error = Some(e),
                    }
                    return;
                }
                for (side, rect) in rects.into_iter().enumerate() {
                    if let Some(player) = &mut self.players[side] {
                        if let Err(e) = player.set_bounds(rect, ctx.pixels_per_point()) {
                            error = Some(e);
                        }
                    } else if self.phase == 0 {
                        match StreamPlayer::new(
                            frame,
                            ctx,
                            &self.urls[side],
                            "local-test-101",
                            rect,
                            ctx.pixels_per_point(),
                            None,
                        ) {
                            Ok(player) => self.players[side] = Some(player),
                            Err(e) => error = Some(e),
                        }
                    }
                }
            });
            if let Some(error) = error {
                self.finish(ctx, Err(error));
                return;
            }
            match self.tick(ctx) {
                Ok(true) => self.finish(ctx, Ok(())),
                Err(error) => self.finish(ctx, Err(error)),
                _ => (),
            }
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }
    fn activate(frame: &eframe::Frame) -> Result<(), String> {
        let handle = frame.window_handle().map_err(|_| "Missing test window")?;
        #[cfg(target_os = "linux")]
        {
            use x11rb::{
                connection::Connection as _,
                protocol::xproto::{ConnectionExt as _, InputFocus},
            };
            let window = match handle.as_raw() {
                RawWindowHandle::Xlib(h) => h.window as u32,
                RawWindowHandle::Xcb(h) => h.window.get(),
                _ => return Err("X11 test display required".into()),
            };
            let (connection, _) = x11rb::connect(None).map_err(|_| "Test display unavailable")?;
            connection
                .map_window(window)
                .map_err(|_| "Could not map test window")?;
            connection
                .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
                .map_err(|_| "Could not focus test window")?;
            connection
                .flush()
                .map_err(|_| "Could not activate test window")?;
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SetForegroundWindow, ShowWindow, SW_RESTORE,
            };
            let RawWindowHandle::Win32(h) = handle.as_raw() else {
                return Err("Win32 test window required".into());
            };
            // SAFETY: the handle is this live test's own top-level window.
            unsafe {
                ShowWindow(h.hwnd.get() as *mut _, SW_RESTORE);
                SetForegroundWindow(h.hwnd.get() as *mut _);
            }
        }
        Ok(())
    }
    #[test]
    #[ignore = "requires explicit local replay fixtures and a native display"]
    fn two_real_players_share_pause_seek_resize_and_release() {
        assert_eq!(
            option_env!("BRICK_PRESENCE_API_URL"),
            Some("http://127.0.0.1:18083"),
            "Build this smoke with the explicit loopback endpoint"
        );
        let load = |name: &str| {
            let path = std::env::var_os(name).expect("Set both explicit replay fixture paths");
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .unwrap()
                .take(65537)
                .read_to_end(&mut bytes)
                .unwrap();
            assert!(bytes.len() <= 65536, "Replay fixture is too large");
            serde_json::from_slice::<Replay>(&bytes).expect("Replay metadata fixture required")
        };
        let first = load("BRICK_REPLAY_FIXTURE");
        let second = load("BRICK_REPLAY_ALT_FIXTURE");
        let primary_offset = std::env::var("BRICK_REPLAY_TEST_OFFSET")
            .ok()
            .map(|value| {
                value
                    .parse::<f64>()
                    .expect("Primary offset must be seconds")
            })
            .unwrap_or(10.125);
        let canonical_ms = first.start_ms().expect("Primary UTC origin required")
            + (primary_offset * 1000.0).round() as i64;
        let secondary_offset =
            (canonical_ms - second.start_ms().expect("Secondary UTC origin required")) as f64
                / 1000.0;
        let offsets = [primary_offset, secondary_offset];
        let replays = [first, second];
        let seek_probe = std::env::var("BRICK_COMPARE_BACKWARD_SEEK")
            .ok()
            .map(|value| match value.as_str() {
                "playing" => SeekProbe::Playing,
                "paused" => SeekProbe::Paused,
                "replace" => SeekProbe::Replace,
                _ => panic!("Backward seek mode must be playing, paused, or replace"),
            });
        let seek_probe = if let Ok(value) = std::env::var("BRICK_COMPARE_FORWARD_SEEK") {
            assert!(seek_probe.is_none(), "Choose only one seek probe");
            Some(match value.as_str() {
                "playing" => SeekProbe::Forward,
                "paused" => SeekProbe::ForwardPaused,
                "toggle" => SeekProbe::ForwardToggle,
                "burst" => SeekProbe::ForwardBurst,
                "provider" => SeekProbe::ForwardProvider,
                _ => {
                    panic!("Forward seek mode must be playing, paused, toggle, burst, or provider")
                }
            })
        } else {
            seek_probe
        };
        let initial_ms = match seek_probe {
            Some(
                SeekProbe::Forward
                | SeekProbe::ForwardPaused
                | SeekProbe::ForwardToggle
                | SeekProbe::ForwardBurst
                | SeekProbe::ForwardProvider,
            ) => 10_000,
            Some(_) => 145_125,
            None => 0,
        };
        let end_ms = if seek_probe.is_some() {
            300_000
        } else {
            60_000
        };
        let urls = std::array::from_fn(|side| {
            assert!(
                offsets[side].is_finite()
                    && offsets[side] >= 0.0
                    && offsets[side] + end_ms as f64 / 1000.0 + 30.0
                        < replays[side].available_seconds.min(604800) as f64
            );
            let default_member = if side == 0 {
                "101"
            } else if replays[side].provider == streams::Provider::Youtube {
                "102"
            } else {
                "103"
            };
            let member = std::env::var(if side == 0 {
                "BRICK_REPLAY_TEST_MEMBER"
            } else {
                "BRICK_REPLAY_ALT_MEMBER"
            })
            .unwrap_or_else(|_| default_member.into());
            assert!(
                matches!(member.as_str(), "101" | "102" | "103"),
                "Local fixture member required"
            );
            let mut url = url::Url::parse("http://127.0.0.1:18083").unwrap();
            url.set_path(&format!(
                "/v1/streams/player/{member}/{}",
                replays[side].provider.key()
            ));
            url.query_pairs_mut()
                .append_pair(
                    "at",
                    &format!("{:.3}", offsets[side] + initial_ms as f64 / 1000.0),
                )
                .append_pair("broadcast", &replays[side].broadcast_id)
                .append_pair("paused", "1");
            url.to_string()
        });
        let clocks = std::array::from_fn(|side| {
            RecordingClock::new(START, offsets[side], replays[side].available_seconds as f64)
                .unwrap()
        });
        let hold = std::env::var("BRICK_COMPARE_HOLD_SECONDS")
            .ok()
            .map(|value| {
                value
                    .parse::<u64>()
                    .expect("Comparison hold must be whole seconds")
            })
            .unwrap_or(0);
        assert!(
            hold <= 30,
            "Comparison stress hold is limited to 30 seconds"
        );
        let hold = Duration::from_secs(hold);
        let now = Instant::now();
        let controller = Controller::new(
            clocks,
            [START, START + end_ms],
            START + initial_ms,
            true,
            now,
        )
        .unwrap();
        let outcome = Arc::new(Mutex::new(None));
        let result = outcome.clone();
        let output = std::env::var_os("BRICK_COMPARE_CAPTURE_DIR").map(std::path::PathBuf::from);
        if let Some(output) = &output {
            std::fs::create_dir_all(output).unwrap();
        }
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("Brick comparison check")
                .with_inner_size([1400.0, 760.0])
                .with_position([10.0, 10.0])
                .with_active(true),
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
        };
        eframe::run_native(
            "Brick comparison check",
            options,
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(egui::Visuals::dark());
                Ok(Box::new(Driver {
                    players: [None, None],
                    urls,
                    controller,
                    timing_offsets: offsets,
                    available_seconds: std::array::from_fn(|side| replays[side].available_seconds),
                    outcome,
                    started: now,
                    phase_at: now,
                    hold,
                    phase: 0,
                    steady_reseeks: 0,
                    anchor: START,
                    activated: false,
                    captures: [false, false],
                    output,
                    finished: false,
                    seek_probe,
                    trace_at: now,
                    burst_step: 0,
                    provider_play_since: None,
                }))
            }),
        )
        .expect("Native comparison window failed");
        result
            .lock()
            .unwrap()
            .take()
            .expect("Comparison closed without result")
            .unwrap();
    }
}
