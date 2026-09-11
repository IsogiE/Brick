use std::{
    collections::HashSet,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    discord_auth, presence,
    stream_player::StreamPlayer,
    stream_preferences::Preferences,
    streams::{self, Provider, Snapshot, Status, Stream, Vod},
};

const REFRESH: Duration = Duration::from_secs(30);
const MAX_STALE: Duration = Duration::from_secs(90);
const MUTED: Color32 = Color32::from_rgb(159, 169, 184);
const LIVE: Color32 = Color32::from_rgb(69, 211, 127);
const MEMBER_ROW_HEIGHT: f32 = 30.0;

enum Action {
    Refresh,
    Save(Provider, String),
    Remove(Provider),
    Recordings,
    RemoveRecording(Provider, String),
}
enum ResultData {
    Snapshot(Snapshot),
    Saved(Provider),
    Removed,
    Recordings(streams::Recordings),
    RecordingRemoved(Provider, String),
}
type WorkResult = Result<ResultData, streams::Error>;
type PlayerResult = Result<(String, String, Option<Preferences>), streams::Error>;

pub struct StreamsUi {
    snapshot: Option<Rc<Snapshot>>,
    received_at: Option<Instant>,
    last_attempt: Option<Instant>,
    work: Option<mpsc::Receiver<WorkResult>>,
    selected: Option<Stream>,
    focused: Option<(String, String)>,
    recordings_open: bool,
    recordings: Option<Rc<Vec<Vod>>>,
    recordings_library: crate::recordings_ui::Library,
    recordings_attempted: bool,
    can_delete_recordings: bool,
    confirm_remove_recording: Option<Vod>,
    pov_revision: u64,
    pov_cache_key: Option<(u64, Option<(i64, i64)>, String)>,
    pov_cache: Vec<Stream>,
    player: Option<StreamPlayer>,
    comparison: Option<crate::review_compare_ui::Comparison>,
    player_switch_pending: bool,
    preferences: Option<Preferences>,
    player_work: Option<mpsc::Receiver<PlayerResult>>,
    player_attempted: bool,
    player_retries: u8,
    player_retry_at: Option<Instant>,
    player_rect: Option<egui::Rect>,
    player_error: Option<String>,
    notice: Option<String>,
    notice_provider: Option<Provider>,
    saved_provider: Option<Provider>,
    edit_open: bool,
    drafts: [String; 2],
    confirm_remove: Option<Provider>,
    review: crate::review_ui::ReviewUi,
    warmup: crate::review_ui::ReviewUi,
}

impl Default for StreamsUi {
    fn default() -> Self {
        let review = crate::review_ui::ReviewUi::default();
        let warmup = review.metadata_peer();
        Self {
            snapshot: None,
            received_at: None,
            last_attempt: None,
            work: None,
            selected: None,
            focused: None,
            recordings_open: false,
            recordings: None,
            recordings_library: crate::recordings_ui::Library::default(),
            recordings_attempted: false,
            can_delete_recordings: false,
            confirm_remove_recording: None,
            pov_revision: 0,
            pov_cache_key: None,
            pov_cache: Vec::new(),
            player: None,
            comparison: None,
            player_switch_pending: false,
            preferences: None,
            player_work: None,
            player_attempted: false,
            player_retries: 0,
            player_retry_at: None,
            player_rect: None,
            player_error: None,
            notice: None,
            notice_provider: None,
            saved_provider: None,
            edit_open: false,
            drafts: [String::new(), String::new()],
            confirm_remove: None,
            review,
            warmup,
        }
    }
}

impl StreamsUi {
    pub fn profiles_changed(&mut self) {
        self.last_attempt = None;
        self.recordings_attempted = false;
    }

    pub fn reviewing(&self) -> bool {
        self.review.active() && !self.recordings_open
    }

    pub fn fullscreen(&self) -> bool {
        self.player
            .as_ref()
            .is_some_and(StreamPlayer::is_fullscreen)
            || self
                .comparison
                .as_ref()
                .is_some_and(|comparison| comparison.fullscreen())
    }

    pub fn draw_fullscreen(&mut self, ui: &mut egui::Ui) {
        // Let a fullscreen pull menu consume Escape before closing the video.
        let mut exit = !egui::Popup::is_any_open(ui.ctx())
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut command = None;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(12, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().interact_size.y = 32.0;
                ui.horizontal(|ui| {
                    exit |= ui
                        .add_sized(
                            egui::vec2(128.0, 32.0),
                            egui::Button::new("Exit fullscreen"),
                        )
                        .clicked();
                    if self.review.active() {
                        let width = ui.available_width().min(420.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(width, 32.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                command = self.review.draw_fullscreen_navigation(ui);
                            },
                        );
                    }
                    let expanded = self
                        .comparison
                        .as_ref()
                        .and_then(|comparison| comparison.expanded_stream())
                        .or(self.selected.as_ref());
                    if let Some(stream) = expanded {
                        ui.add_sized(
                            egui::vec2(ui.available_width().max(0.0), 32.0),
                            egui::Label::new(format!(
                                "{} · {}",
                                stream.name,
                                stream.provider.label()
                            ))
                            .truncate(),
                        );
                    }
                });
            });
        // Dispatch before either fullscreen branch: comparison maps the primary
        // pull into both recordings, including when its secondary is expanded.
        if let Some(command) = command {
            if let Some(comparison) = &mut self.comparison {
                comparison.command(command, &self.review);
            } else if let Some(player) = &mut self.player {
                if let Err(error) = player.command(command) {
                    self.player_error = Some(error);
                }
            }
        }
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        if let Some(comparison) = self
            .comparison
            .as_mut()
            .filter(|comparison| comparison.fullscreen())
        {
            self.player_rect = comparison.fullscreen_rect(rect, exit);
            return;
        }
        self.player_rect = Some(rect);
        if let Some(player) = &mut self.player {
            if exit {
                player.exit_fullscreen();
            }
            if self.review.active() {
                let state = player.playback_state();
                self.review.observe_provider_playback(&state);
                if let Some(command) = self.review.pause_at_pull_end(&state) {
                    if let Some(comparison) = &mut self.comparison {
                        comparison.command(command, &self.review);
                    } else if let Err(error) = player.command(command) {
                        self.player_error = Some(error);
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn stop_player(&mut self) {
        self.comparison = None;
        self.review.set_comparing(false);
        self.review.cancel_marker(self.player.as_ref());
        self.player = None;
        self.player_switch_pending = false;
        self.player_work = None;
        self.player_attempted = false;
        self.player_retries = 0;
        self.player_retry_at = None;
        self.player_rect = None;
    }

    fn leave_unavailable_comparison(&mut self) {
        if self
            .comparison
            .as_ref()
            .is_some_and(|comparison| comparison.unavailable_for_review(&self.review))
        {
            if let Some(comparison) = self.comparison.take() {
                comparison.leave(self.player.as_mut(), &self.review);
            }
            self.review.set_comparing(false);
        }
    }

    fn recover_player(&mut self, error: String) {
        self.player = None;
        self.player_work = None;
        self.player_switch_pending = false;
        self.player_attempted = true;
        if self.player_retries == 0 {
            self.player_retries = 1;
            self.player_retry_at = Some(Instant::now() + Duration::from_secs(1));
            self.player_error = None;
        } else {
            self.player_retry_at = None;
            self.player_error = Some(error);
        }
    }

    fn finish_recording_review(&mut self) {
        // Review can also end during tick (tab switch or hiding Brick).
        // A saved VOD is never a live-stream selection.
        if !self.review.active()
            && self
                .selected
                .as_ref()
                .is_some_and(|stream| stream.recording_id.is_some())
        {
            self.recordings_open = true;
            self.selected = None;
            self.focused = None;
            self.player_error = None;
            self.stop_player();
        }
    }

    pub fn tick(&mut self, ctx: &egui::Context, authorized: bool, active: bool) -> bool {
        if !authorized {
            self.clear();
            return false;
        }
        if !active {
            self.stop_player();
        }
        if self.received_at.is_some_and(|at| at.elapsed() >= MAX_STALE) {
            self.pov_revision = self.pov_revision.wrapping_add(1);
            self.snapshot = None;
            if !self
                .selected
                .as_ref()
                .is_some_and(|s| s.recording_id.is_some())
            {
                self.selected = None;
                self.stop_player();
            }
            self.received_at = None;
            self.notice = Some("Live status couldn't be verified. Reconnecting…".into());
            self.notice_provider = None;
        }
        if let Some(rx) = &self.work {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Stream request stopped. Try again.".to_string().into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.work = None;
                match result {
                    Ok(ResultData::Snapshot(mut snapshot)) => {
                        snapshot.streams.sort_by_cached_key(|stream| {
                            (
                                crate::profile::role_order(stream.raid_role),
                                stream.name.to_lowercase(),
                                stream.user_id.clone(),
                                stream.provider.key(),
                                stream.channel_id.clone(),
                            )
                        });
                        self.pov_revision = self.pov_revision.wrapping_add(1);
                        self.saved_provider = None;
                        if let Some(selected) =
                            self.selected.as_ref().filter(|s| s.recording_id.is_none())
                        {
                            let current = snapshot
                                .streams
                                .iter()
                                .find(|stream| {
                                    stream.user_id == selected.user_id
                                        && stream.channel_id == selected.channel_id
                                        && stream.provider == selected.provider
                                        && (self.review.active() || stream.status == Status::Live)
                                })
                                .cloned();
                            if current.is_none() {
                                self.stop_player();
                            }
                            self.selected = current;
                        }
                        self.snapshot = Some(Rc::new(snapshot));
                        self.received_at = Some(Instant::now());
                    }
                    Ok(ResultData::Saved(provider)) => {
                        self.saved_provider = Some(provider);
                        self.notice = None;
                        self.start(ctx, Action::Refresh);
                    }
                    Ok(ResultData::Removed) => {
                        self.confirm_remove = None;
                        self.start(ctx, Action::Refresh);
                    }
                    Ok(ResultData::Recordings(recordings)) => {
                        self.pov_revision = self.pov_revision.wrapping_add(1);
                        self.can_delete_recordings = recordings.can_delete_recordings;
                        if !self.can_delete_recordings {
                            self.confirm_remove_recording = None;
                        }
                        let missing = self.selected.as_ref().is_some_and(|stream| {
                            stream.recording_id.as_ref().is_some_and(|id| {
                                !recordings.vods.iter().any(|vod| {
                                    vod.id == *id
                                        && vod.provider == stream.provider
                                        && vod.user_id == stream.user_id
                                })
                            })
                        });
                        self.recordings = Some(Rc::new(recordings.vods));
                        if missing {
                            self.selected = None;
                            self.recordings_open = true;
                            self.stop_player();
                            self.notice = Some("This VOD is no longer in Brick.".into());
                        }
                    }
                    Ok(ResultData::RecordingRemoved(provider, id)) => {
                        self.apply_recording_removal(&provider, &id);
                        self.notice = Some("VOD removed from Brick.".into());
                    }
                    Err(error) => {
                        self.saved_provider = None;
                        if error.access_denied {
                            self.clear();
                            return true;
                        }
                        self.notice = Some(error.message);
                    }
                }
            }
        }
        if presence::configured() && self.work.is_none() {
            let refresh = if active {
                REFRESH
            } else {
                Duration::from_secs(60)
            };
            if active
                && (self.recordings_open || self.review.active())
                && !self.recordings_attempted
            {
                self.start(ctx, Action::Recordings);
            } else if self.last_attempt.is_none_or(|at| at.elapsed() >= refresh) {
                self.start(ctx, Action::Refresh);
            }
        }
        // One metadata-only observer finds new raid pulls while Brick is idle.
        // The VPS queues all covered POVs; no background video decoder is
        // created on the client. The selected review takes over while watching.
        let warmup_stream = self
            .snapshot
            .as_ref()
            .filter(|_| !active || self.selected.is_none())
            .and_then(|snapshot| {
                snapshot
                    .streams
                    .iter()
                    .filter(|s| s.status == Status::Live)
                    .min_by_key(|s| (&s.user_id, s.provider.key(), &s.channel_id))
            });
        self.warmup.tick(ctx, warmup_stream);
        if self.review.tick(
            ctx,
            self.selected
                .as_ref()
                .filter(|_| active && !self.recordings_open),
        ) && (!self.player_switch_pending || !self.review.active())
        {
            if self.review.active() && self.comparison.is_some() {
                self.player = None;
                self.player_work = None;
                self.player_attempted = false;
                self.player_switch_pending = false;
                if let Some(comparison) = &mut self.comparison {
                    comparison.primary_changed();
                }
            } else {
                self.stop_player();
            }
        }
        self.finish_recording_review();
        if let Some(player) = &mut self.player {
            if self.review.active() {
                player.poll_playback(ctx);
                if !self.player_switch_pending {
                    // Follow native VOD controls before any pull-boundary check,
                    // including fullscreen frames where the timeline is hidden.
                    self.review
                        .observe_provider_playback(&player.playback_state());
                }
            }
            crate::stream_player::pump_events();
        }
        if let Some(error) = self.player.as_ref().and_then(StreamPlayer::failure) {
            self.recover_player(error);
        }
        if active && !self.recordings_open && !self.player_switch_pending {
            if let Some(player) = &mut self.player {
                if let Some(command) = self.review.sync_marker(ctx, player, None, None) {
                    if let Err(error) = player.command(command) {
                        self.player_error = Some(error);
                    }
                }
            }
        }
        self.leave_unavailable_comparison();
        if let Some(comparison) = &mut self.comparison {
            comparison.tick(
                ctx,
                self.player.as_mut().filter(|_| !self.player_switch_pending),
                &mut self.review,
            );
        }
        self.leave_unavailable_comparison();
        false
    }

    pub fn repaint_after(&self, active: bool) -> Duration {
        #[cfg(target_os = "linux")]
        if self.player.is_some() {
            return Duration::from_millis(33);
        }
        if self.player.is_some() && self.review.active() {
            return Duration::from_millis(500);
        }
        if active && presence::configured() && self.work.is_none() {
            return self
                .last_attempt
                .map(|at| REFRESH.saturating_sub(at.elapsed()))
                .unwrap_or(Duration::ZERO);
        }
        Duration::from_secs(60)
    }

    fn start(&mut self, ctx: &egui::Context, action: Action) {
        if self.work.is_some() {
            return;
        }
        self.notice = None;
        if matches!(action, Action::Recordings) {
            self.recordings_attempted = true;
        }
        self.notice_provider = match &action {
            Action::Save(provider, _) | Action::Remove(provider) => Some(provider.clone()),
            _ => None,
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = access_token().and_then(|token| match action {
                Action::Refresh => streams::fetch(&token).map(ResultData::Snapshot),
                Action::Save(provider, url) => {
                    let parsed = url::Url::parse(&url).ok();
                    let matches =
                        parsed
                            .as_ref()
                            .and_then(url::Url::host_str)
                            .is_some_and(|host| match provider {
                                Provider::Twitch => {
                                    ["twitch.tv", "www.twitch.tv", "m.twitch.tv"].contains(&host)
                                }
                                Provider::Youtube => [
                                    "youtube.com",
                                    "www.youtube.com",
                                    "m.youtube.com",
                                    "youtu.be",
                                ]
                                .contains(&host),
                            });
                    if !matches {
                        return Err(format!(
                            "Use a {} link in the {} field.",
                            provider.label(),
                            provider.label()
                        )
                        .into());
                    }
                    streams::save(&token, &url).map(|()| ResultData::Saved(provider))
                }
                Action::Remove(provider) => {
                    streams::remove(&token, &provider).map(|()| ResultData::Removed)
                }
                Action::Recordings => streams::fetch_recordings(&token).map(ResultData::Recordings),
                Action::RemoveRecording(provider, id) => {
                    streams::remove_recording(&token, &provider, &id)
                        .map(|()| ResultData::RecordingRemoved(provider, id))
                }
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.work = Some(rx);
        self.last_attempt = Some(Instant::now());
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.finish_recording_review();
        self.player_rect = None;
        if self.review.active() && !self.recordings_open {
            if let Some(stream) = self.selected.clone() {
                let span = self.review.report_span();
                let key = (
                    self.pov_revision,
                    span,
                    format!(
                        "{}:{}:{}:{}",
                        stream.user_id,
                        stream.provider.key(),
                        stream.channel_id,
                        stream.recording_id.as_deref().unwrap_or("")
                    ),
                );
                if self.pov_cache_key.as_ref() != Some(&key) {
                    self.pov_cache = review_povs(
                        self.snapshot
                            .as_ref()
                            .map(|s| s.streams.as_slice())
                            .unwrap_or_default(),
                        self.recordings
                            .as_deref()
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                        &stream,
                        span,
                    );
                    self.review.set_recording_labels(recording_labels(
                        self.recordings
                            .as_deref()
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                        &self.pov_cache,
                    ));
                    self.pov_cache_key = Some(key);
                }
                let state = if self.player_switch_pending {
                    // A retained hidden child still belongs to the previous
                    // POV until its new authenticated navigation starts.
                    let mut state = crate::stream_player::PlaybackState::default();
                    state.playback_intent =
                        self.review.playback().map(|playback| playback.autoplay);
                    state
                } else {
                    self.player
                        .as_ref()
                        .map(StreamPlayer::playback_state)
                        .unwrap_or_default()
                };
                let state = self.comparison.as_ref().map_or_else(
                    || state.clone(),
                    |comparison| comparison.state_for_controls(state.clone()),
                );
                let action = self.review.draw_workspace(
                    ui,
                    &stream,
                    &self.pov_cache,
                    &state,
                    self.player.is_some() && !self.player_switch_pending,
                    self.player_error.as_deref(),
                    self.comparison.as_mut(),
                );
                if action.close_comparison {
                    if let Some(comparison) = self.comparison.take() {
                        comparison.leave(self.player.as_mut(), &self.review);
                    }
                    self.review.set_comparing(false);
                }
                self.player_rect = self.comparison.as_mut().map_or(action.rect, |comparison| {
                    comparison.split_video(action.rect)
                });
                if action.compare {
                    if let Some((at_ms, playing)) = self.review.comparison_position(&state) {
                        if let Some(other) = self
                            .pov_cache
                            .iter()
                            .find(|candidate| {
                                (candidate.user_id != stream.user_id
                                    || candidate.provider != stream.provider
                                    || candidate.recording_id != stream.recording_id)
                                    && (candidate.recording_id.is_some()
                                        || candidate.status == Status::Live)
                                    && self.review.pov_selection_context(&state).is_some_and(
                                        |(pull, moment)| {
                                            self.review.pov_covers_moment(candidate, pull, moment)
                                        },
                                    )
                            })
                            .cloned()
                        {
                            self.comparison = Some(crate::review_compare_ui::Comparison::new(
                                &self.review,
                                other,
                                at_ms,
                                playing,
                            ));
                            self.review.set_comparing(true);
                            if let Some(player) = &mut self.player {
                                let _ =
                                    player.command(crate::stream_player::PlaybackCommand::Pause);
                            }
                            self.player_rect = self
                                .comparison
                                .as_mut()
                                .and_then(|comparison| comparison.split_video(action.rect));
                        }
                    }
                }
                if action.reload {
                    self.stop_player();
                    self.player_rect = action.rect;
                    self.player_error = None;
                }
                if let Some(command) = action.command {
                    if let Some(comparison) = &mut self.comparison {
                        comparison.command(command, &self.review);
                    } else if let Some(player) = &mut self.player {
                        if let Err(error) = player.command(command) {
                            self.player_error = Some(error);
                        }
                    }
                }
                if let Some(next_stream) = action.stream {
                    self.player_retries = 0;
                    self.player_retry_at = None;
                    if let Some(comparison) = &mut self.comparison {
                        comparison.avoid_duplicate(ui.ctx(), &next_stream, &stream);
                        comparison.primary_changed();
                    }
                    let stream = next_stream;
                    // Keep one paused, hidden media child while the selected
                    // POV's authenticated replay metadata is checked.
                    let retain = self
                        .player
                        .as_ref()
                        .is_some_and(StreamPlayer::can_reuse_for_replay);
                    if retain {
                        if let Some(player) = &mut self.player {
                            let _ = player.command(crate::stream_player::PlaybackCommand::Pause);
                            player.set_visible(false);
                        }
                        self.player_switch_pending = true;
                        self.player_work = None;
                        self.player_attempted = false;
                    } else {
                        self.player = None;
                        self.player_work = None;
                        self.player_attempted = false;
                        self.player_switch_pending = false;
                    }
                    self.player_error = None;
                    self.focused = Some((stream.user_id.clone(), stream.name.clone()));
                    self.selected = Some(stream);
                }
                self.finish_recording_review();
                return;
            }
        }

        ui.horizontal(|ui| {
            if self.recordings_open {
                ui.label(
                    RichText::new("VODs")
                        .size(18.0)
                        .strong()
                        .color(Color32::from_rgb(239, 242, 247)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        presence::configured() && self.work.is_none(),
                        action_button("Your streams").min_size(egui::vec2(108.0, 32.0)),
                    )
                    .clicked()
                {
                    for (i, provider) in [Provider::Twitch, Provider::Youtube].iter().enumerate() {
                        self.drafts[i] = self
                            .snapshot
                            .as_ref()
                            .and_then(|s| {
                                s.own_streams
                                    .iter()
                                    .find(|stream| &stream.provider == provider)
                            })
                            .map(|s| s.url.clone())
                            .unwrap_or_default();
                    }
                    self.confirm_remove = None;
                    self.edit_open = true;
                }
                let refresh_button = action_button("Refresh");
                let refresh_button = if self.notice.is_some() && self.notice_provider.is_none() {
                    refresh_button
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(204, 112, 112)))
                } else {
                    refresh_button
                };
                let refresh = ui.add_enabled(
                    self.work.is_none() && presence::configured(),
                    refresh_button,
                );
                let refresh = if self.notice_provider.is_none() {
                    if let Some(error) = &self.notice {
                        refresh.on_hover_text(error)
                    } else {
                        refresh
                    }
                } else {
                    refresh
                };
                if refresh.clicked() {
                    self.notice = None;
                    self.start(
                        ui.ctx(),
                        if self.recordings_open {
                            Action::Recordings
                        } else {
                            Action::Refresh
                        },
                    );
                }
                if ui
                    .add_enabled(
                        self.work.is_none(),
                        action_button(if self.recordings_open {
                            "Live streams"
                        } else {
                            "VODs"
                        }),
                    )
                    .clicked()
                {
                    self.recordings_open = !self.recordings_open;
                    self.confirm_remove_recording = None;
                    self.focused = None;
                    self.selected = None;
                    self.notice = None;
                    self.stop_player();
                    if self.recordings_open {
                        self.start(ui.ctx(), Action::Recordings);
                    }
                }
            });
        });
        ui.add_space(12.0);
        if self.recordings_open && self.confirm_remove_recording.is_none() {
            if let Some(notice) = &self.notice {
                ui.label(RichText::new(notice).small().color(MUTED));
                ui.add_space(6.0);
            }
        }
        if !presence::configured() {
            ui.label("Streams aren't available in this build yet.");
            return;
        }
        if self.recordings_open {
            self.draw_recordings(ui);
            self.draw_editor(ui.ctx());
            self.draw_recording_removal(ui.ctx());
            return;
        }
        let snapshot = self.snapshot.clone();
        let live = snapshot
            .as_ref()
            .map(|s| s.streams.as_slice())
            .unwrap_or_default();
        let empty_message = if (self.notice.is_some() && self.notice_provider.is_none())
            || snapshot.as_ref().is_some_and(|s| s.unverified_count > 0)
        {
            "No streams available"
        } else {
            "No one is live right now."
        };
        let people = live_people(live);
        let height = ui.available_height().max(330.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(176.0, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(176.0);
                    ui.label(
                        RichText::new(format!("LIVE NOW  {}", people.len()))
                            .small()
                            .strong()
                            .color(MUTED),
                    );
                    ui.add_space(10.0);
                    let count = people.len();
                    if count == 0 {
                        let message = if self.snapshot.is_none() && self.work.is_some() {
                            "Checking who's live…"
                        } else {
                            empty_message
                        };
                        ui.label(RichText::new(message).color(MUTED));
                    }
                    ui.spacing_mut().item_spacing.y = 2.0;
                    egui::ScrollArea::vertical()
                        .id_salt(("stream-members", self.recordings_open))
                        .max_height((height - 36.0).max(80.0))
                        .show_rows(ui, MEMBER_ROW_HEIGHT, count, |ui, rows| {
                            for index in rows {
                                let stream = people[index];
                                let selected = self
                                    .focused
                                    .as_ref()
                                    .is_some_and(|(id, _)| id == &stream.user_id);
                                if member_row(
                                    ui,
                                    &stream.name,
                                    stream.raid_role,
                                    None,
                                    selected,
                                    true,
                                )
                                .clicked()
                                {
                                    self.stop_player();
                                    self.player_error = None;
                                    self.focused =
                                        Some((stream.user_id.clone(), stream.name.clone()));
                                    self.selected = Some(stream.clone());
                                }
                            }
                        });
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if let Some(stream) = self.selected.clone() {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let platforms: Vec<_> = live
                                .iter()
                                .filter(|s| s.user_id == stream.user_id)
                                .collect();
                            let reserved =
                                platforms.len() as f32 * (80.0 + ui.spacing().item_spacing.x);
                            let name_width = (ui.available_width() - reserved).max(40.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(name_width, 32.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&stream.name)
                                                .strong()
                                                .color(Color32::from_rgb(239, 242, 247)),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&stream.name);
                                },
                            );
                            for platform in platforms {
                                let selected = platform.provider == stream.provider;
                                if ui
                                    .add(
                                        action_button(platform.provider.label())
                                            .fill(if selected {
                                                Color32::from_rgb(41, 47, 60)
                                            } else {
                                                Color32::from_rgb(36, 41, 50)
                                            })
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                if selected {
                                                    Color32::from_rgb(84, 99, 122)
                                                } else {
                                                    Color32::from_rgb(51, 58, 70)
                                                },
                                            )),
                                    )
                                    .clicked()
                                {
                                    self.stop_player();
                                    self.player_error = None;
                                    self.selected = Some(platform.clone());
                                }
                            }
                        },
                    );
                    ui.add_space(8.0);
                    let size = egui::vec2(
                        ui.available_width(),
                        (ui.available_width() * 9.0 / 16.0)
                            .min((ui.available_height() - 112.0).max(160.0)),
                    );
                    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 8.0, Color32::from_rgb(12, 14, 19));
                    if self.player.is_none() {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            if self.player_error.is_some() {
                                "Player unavailable"
                            } else {
                                "Opening stream…"
                            },
                            egui::FontId::proportional(16.0),
                            MUTED,
                        );
                    }
                    self.player_rect = Some(rect);
                    ui.add_space(if ui.ctx().content_rect().height() < 640.0 {
                        4.0
                    } else {
                        10.0
                    });
                    let mut caption = egui::text::LayoutJob::default();
                    caption.append(
                        if self.review.playback().is_some() {
                            "REPLAY  ·  "
                        } else {
                            "LIVE  ·  "
                        },
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(11.0),
                            color: LIVE,
                            ..Default::default()
                        },
                    );
                    caption.append(
                        &stream.name,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: Color32::from_rgb(239, 242, 247),
                            ..Default::default()
                        },
                    );
                    caption.append(
                        &format!("  ·  {}", stream.provider.label()),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(11.0),
                            color: MUTED,
                            ..Default::default()
                        },
                    );
                    ui.add_sized(
                        egui::vec2(ui.available_width(), 22.0),
                        egui::Label::new(caption)
                            .halign(egui::Align::Center)
                            .truncate(),
                    );
                    if self.review.draw(ui, &stream) {
                        self.stop_player();
                        self.player_rect = Some(rect);
                        self.player_error = None;
                    }
                    if let Some(playback) = self.review.playback() {
                        ui.hyperlink_to("Open replay in browser", &playback.public_url);
                    }
                    if let Some(error) = &self.player_error {
                        ui.label(RichText::new(error).small().color(MUTED));
                    }
                } else {
                    empty_view(
                        ui,
                        if live.is_empty() {
                            empty_message
                        } else {
                            "Select a player"
                        },
                    );
                }
            });
        });
        self.draw_editor(ui.ctx());
        self.draw_recording_removal(ui.ctx());
    }

    fn draw_recordings(&mut self, ui: &mut egui::Ui) {
        let recordings = self.recordings.clone().unwrap_or_default();
        let action = self.recordings_library.draw(
            ui,
            &recordings,
            self.work.is_some(),
            self.can_delete_recordings,
            self.work.is_some(),
        );
        match action {
            Some(crate::recordings_ui::Action::Review(index)) => {
                self.open_recording(&recordings[index]);
            }
            Some(crate::recordings_ui::Action::OpenBrowser(index)) => {
                if let Err(error) = crate::browser::open(&recordings[index].url) {
                    self.notice = Some(error);
                }
            }
            Some(crate::recordings_ui::Action::Remove(index)) => {
                self.notice = None;
                self.confirm_remove_recording = Some(recordings[index].clone());
            }
            None => {}
        }
    }

    fn open_recording(&mut self, vod: &Vod) {
        self.stop_player();
        self.selected = Some(vod.as_stream());
        self.focused = Some((vod.user_id.clone(), vod.name.clone()));
        self.recordings_open = false;
        self.confirm_remove_recording = None;
        self.player_error = None;
        self.notice = None;
        self.review.open_recording();
    }

    fn apply_recording_removal(&mut self, provider: &Provider, id: &str) {
        self.pov_revision = self.pov_revision.wrapping_add(1);
        if let Some(recordings) = &mut self.recordings {
            Rc::make_mut(recordings).retain(|vod| &vod.provider != provider || vod.id != id);
        }
        if self.selected.as_ref().is_some_and(|stream| {
            &stream.provider == provider && stream.recording_id.as_deref() == Some(id)
        }) {
            self.selected = None;
            self.recordings_open = true;
            self.stop_player();
        }
        self.confirm_remove_recording = None;
    }

    fn draw_recording_removal(&mut self, ctx: &egui::Context) {
        let Some(vod) = self.confirm_remove_recording.clone() else {
            return;
        };
        let mut cancel = false;
        let mut remove = false;
        let modal = egui::Modal::new(egui::Id::new("remove-recording")).show(ctx, |ui| {
            ui.set_width(400.0_f32.min((ctx.content_rect().width() - 64.0).max(240.0)));
            ui.heading("Remove VOD from Brick?");
            ui.add_space(8.0);
            ui.label(RichText::new(recording_title(&vod)).strong());
            ui.label(format!(
                "{} · {} · {}",
                vod.name,
                vod.provider.label(),
                recording_day_label(recording_day(&vod))
            ));
            ui.add_space(8.0);
            ui.label(format!(
                "This removes the saved link from Brick. The video stays on {}.",
                vod.provider.label()
            ));
            if let Some(notice) = &self.notice {
                ui.add_space(8.0);
                ui.label(RichText::new(notice).color(Color32::from_rgb(244, 144, 144)));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                cancel = ui
                    .add_enabled(self.work.is_none(), egui::Button::new("Cancel"))
                    .clicked();
                remove = ui
                    .add_enabled(
                        self.work.is_none() && self.can_delete_recordings,
                        egui::Button::new(if self.work.is_some() {
                            "Removing…"
                        } else {
                            "Remove from Brick"
                        }),
                    )
                    .clicked();
            });
        });
        cancel |= self.work.is_none() && modal.should_close();
        if cancel {
            self.confirm_remove_recording = None;
            self.notice = None;
        } else if remove {
            self.start(ctx, Action::RemoveRecording(vod.provider, vod.id));
        }
    }

    fn draw_editor(&mut self, ctx: &egui::Context) {
        if !self.edit_open {
            return;
        }
        let mut action = None;
        let mut done = false;
        // The eframe root and ordinary egui windows share the middle layer.
        // A modal stays above the stream placeholder and blocks background clicks.
        let modal = egui::Modal::new(egui::Id::new("stream-editor"))
            .frame(egui::Frame::new().fill(Color32::from_rgb(29, 33, 41)).stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(51, 58, 70))).corner_radius(10).inner_margin(18))
            .show(ctx, |ui| {
                ui.set_width(470.0_f32.min((ctx.content_rect().width() - 72.0).max(280.0)));
                ui.horizontal(|ui| {
                    ui.heading("Your streams");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        done |= ui.button("×").on_hover_text("Close").clicked();
                    });
                });
                ui.separator();
                ui.label(RichText::new("Add one or both platforms.").strong());
                ui.add_space(10.0);
                egui::ScrollArea::vertical().id_salt("stream-setup-help").max_height((ctx.content_rect().height() - 220.0).max(220.0)).show(ui, |ui| {
                    for (index, provider) in [Provider::Twitch, Provider::Youtube].into_iter().enumerate() {
                        let twitch = provider == Provider::Twitch;
                        let own = self.snapshot.as_ref().and_then(|s| s.own_streams.iter().find(|stream| stream.provider == provider));
                        let ended = own.is_some_and(|stream| stream.broadcast_state.as_deref() == Some("ended"));
                        egui::Frame::new().fill(Color32::from_rgb(22, 25, 32)).corner_radius(8).inner_margin(14).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(if twitch { "Twitch channel" } else { "YouTube broadcast" }).strong().size(15.0));
                                ui.label(RichText::new(if twitch { "SET UP ONCE" } else { "NEW LINK EACH BROADCAST" }).size(10.0).color(if twitch { LIVE } else { Color32::from_rgb(231, 190, 108) }));
                            });
                            ui.add_space(6.0);
                            ui.label(RichText::new(if twitch {
                                "Paste your channel link once. Brick will find your future broadcasts automatically."
                            } else {
                                "Start or schedule your stream in YouTube, choose Share, and paste that broadcast's video link here."
                            }).small().color(MUTED));
                            ui.add_space(8.0);
                            let hint = if twitch { "https://twitch.tv/yourchannel" } else { "https://youtube.com/live/your-video-id" };
                            ui.add_enabled(self.work.is_none(), egui::TextEdit::singleline(&mut self.drafts[index]).hint_text(hint).desired_width(f32::INFINITY).char_limit(512));
                            if self.notice_provider.as_ref() == Some(&provider) {
                                if let Some(error) = &self.notice {
                                    ui.label(RichText::new(error).small().color(Color32::from_rgb(244, 144, 144)));
                                }
                            }
                            if !twitch {
                                ui.collapsing("Where do I find the right link?", |ui| {
                                    ui.label(RichText::new("1. Open the live or scheduled broadcast on YouTube.\n2. Choose Share, then Copy link.\n3. Paste it above. Use a new link for your next broadcast.").small().color(MUTED));
                                    ui.label(RichText::new("Use the video's link, rather than your YouTube channel page.").small().color(MUTED));
                                });
                            }
                            if self.saved_provider.as_ref() == Some(&provider) {
                                ui.add_space(6.0);
                                ui.label(RichText::new("Link saved. Checking live status…").small().color(MUTED));
                            } else if let Some(own) = own {
                                ui.add_space(6.0);
                                ui.label(RichText::new(submission_status(own)).small().color(if own.status == Status::Live { LIVE } else { MUTED }));
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if own.is_some() {
                                    let confirming = self.confirm_remove.as_ref() == Some(&provider);
                                    if ui.add_enabled(self.work.is_none(), action_button(if confirming { "Confirm removal" } else { "Remove" })).clicked() {
                                        if confirming { action = Some(Action::Remove(provider.clone())); self.drafts[index].clear(); }
                                        else { self.confirm_remove = Some(provider.clone()); }
                                    }
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let label = if twitch { "Save channel" } else if ended { "Add next broadcast" } else if own.is_some() { "Replace broadcast" } else { "Add broadcast" };
                                    let changed = own.is_none_or(|stream| stream.url != self.drafts[index].trim());
                                    if ui.add_enabled(self.work.is_none() && changed && !self.drafts[index].trim().is_empty(), action_button(label)).clicked() {
                                        action = Some(Action::Save(provider.clone(), self.drafts[index].trim().to_owned()));
                                    }
                                });
                            });
                        });
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { done |= ui.add(action_button("Done")).clicked(); });
            });
        if let Some(action) = action {
            self.notice = None;
            self.start(ctx, action);
        }
        if done || modal.should_close() {
            self.edit_open = false;
            self.drafts = [String::new(), String::new()];
            self.confirm_remove = None;
            if self.notice_provider.take().is_some() {
                self.notice = None;
            }
        }
    }

    pub fn update_player(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        allowed: bool,
    ) -> bool {
        let secondary_fullscreen = self
            .comparison
            .as_ref()
            .is_some_and(|comparison| comparison.fullscreen());
        let denied = if secondary_fullscreen && allowed {
            if let Some(player) = &self.player {
                // Keep only the selected POV expanded if provider requests race.
                player.exit_fullscreen();
                // Keep the comparison decoder mapped beneath its expanded peer.
                // Unmapping can make providers pause or buffer on fullscreen.
                player.set_visible(!self.player_switch_pending);
            }
            false
        } else {
            self.update_primary_player(frame, ctx, allowed)
        };
        let primary_fullscreen = self
            .player
            .as_ref()
            .is_some_and(StreamPlayer::is_fullscreen);
        if let Some(comparison) = &mut self.comparison {
            comparison.update_player(
                frame,
                ctx,
                &self.review,
                self.preferences.clone(),
                allowed && !denied,
            );
            comparison.cover_for_fullscreen(allowed && !denied && primary_fullscreen);
        }
        if let Some(player) = &self.player {
            player.cover_for_fullscreen(allowed && !denied && secondary_fullscreen);
            player.update_overlays(ctx);
        }
        if let Some(comparison) = &self.comparison {
            comparison.update_overlays(ctx);
        }
        denied
    }

    fn update_primary_player(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        allowed: bool,
    ) -> bool {
        if !allowed || self.recordings_open {
            self.stop_player();
            return false;
        }
        if self.review.active() && self.review.playback().is_none() {
            if self.player_switch_pending {
                if let Some(player) = &self.player {
                    player.set_visible(false);
                }
            } else if self.comparison.is_some() {
                self.player = None;
                self.player_attempted = false;
                self.player_work = None;
            } else {
                self.stop_player();
            }
            return false;
        }
        if let Some(player) = &self.player {
            player.set_visible(!self.player_switch_pending);
        }
        let Some(rect) = self.player_rect else {
            self.stop_player();
            return false;
        };
        if let Some(at) = self.player_retry_at {
            if let Some(wait) = at.checked_duration_since(Instant::now()) {
                ctx.request_repaint_after(wait);
                return false;
            }
            self.player_retry_at = None;
            self.player_attempted = false;
        }
        if let Some(player) = &mut self.player {
            if let Err(error) = player.set_bounds(rect, ctx.pixels_per_point()) {
                self.player_error = Some(error);
                self.player = None;
                self.player_switch_pending = false;
            }
            if !self.player_switch_pending {
                return false;
            }
        }
        if let Some(rx) = &self.player_work {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("The player stopped loading. Try again."
                        .to_string()
                        .into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.player_work = None;
                match result {
                    Ok((url, token, preferences)) => {
                        // Preparation may finish after another pull was selected.
                        // Only its credentials/preferences are reusable; read the
                        // latest playback intent before constructing the player.
                        let url = player_url_for_playback(&url, self.review.playback());
                        self.preferences = preferences;
                        if let Some(player) = &mut self.player {
                            if self.player_switch_pending {
                                match player.load_replay(ctx, &url, &token) {
                                    Ok(()) => {
                                        player.set_visible(true);
                                    }
                                    Err(error) => {
                                        self.player_error = Some(error);
                                        self.player = None;
                                    }
                                }
                                self.player_switch_pending = false;
                                return false;
                            }
                        }
                        match StreamPlayer::new(
                            frame,
                            ctx,
                            &url,
                            &token,
                            rect,
                            ctx.pixels_per_point(),
                            self.preferences.clone(),
                        ) {
                            Ok(player) => {
                                self.player = Some(player);
                                self.player_switch_pending = false;
                            }
                            Err(error) => self.player_error = Some(error),
                        }
                    }
                    Err(error) => {
                        if error.access_denied {
                            self.clear();
                            return true;
                        }
                        self.player_error = Some(error.message);
                    }
                }
            }
        } else if !self.player_attempted {
            let Some(stream) = &self.selected else {
                return false;
            };
            let stream = stream.clone();
            let preferences = self.preferences.clone();
            let playback = self.review.playback().cloned();
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let result = access_token().and_then(|token| {
                    let preferences = Some(preferences.unwrap_or_else(Preferences::load));
                    streams::player_url_for_stream(&stream).map(|url| {
                        let url = player_url_for_playback(&url, playback.as_ref());
                        (url, token, preferences)
                    })
                });
                let _ = tx.send(result);
                ctx.request_repaint();
            });
            self.player_work = Some(rx);
            self.player_attempted = true;
        }
        false
    }
}

fn empty_view(ui: &mut egui::Ui, message: &str) {
    let size = egui::vec2(ui.available_width(), ui.available_height().max(240.0));
    egui::Frame::new()
        .fill(Color32::from_rgb(25, 28, 36))
        .corner_radius(10)
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                size,
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.label(
                        RichText::new(message)
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(239, 242, 247)),
                    );
                },
            );
        });
}

fn member_row(
    ui: &mut egui::Ui,
    name: &str,
    role: Option<crate::profile::RaidRole>,
    recordings: Option<usize>,
    selected: bool,
    live: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), MEMBER_ROW_HEIGHT),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            name,
        )
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        if selected || response.hovered() || response.has_focus() {
            painter.rect_filled(
                rect,
                4.0,
                if selected {
                    Color32::from_rgb(36, 42, 52)
                } else {
                    Color32::from_rgb(29, 34, 42)
                },
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0_f32, MUTED),
                egui::StrokeKind::Inside,
            );
        }
        if live {
            painter.circle_filled(egui::pos2(rect.left() + 10.0, rect.center().y), 3.0, LIVE);
        }
        let count_width = recordings
            .map(|count| {
                painter
                    .text(
                        egui::pos2(rect.right() - 8.0, rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        count.to_string(),
                        egui::FontId::proportional(11.0),
                        MUTED,
                    )
                    .width()
                    + 12.0
            })
            .unwrap_or(0.0);
        let mut inset = if live { 22.0 } else { 10.0 };
        if role.is_some() {
            crate::profile::paint_role_icon(
                ui,
                role,
                egui::Rect::from_min_size(
                    egui::pos2(rect.left() + inset, rect.center().y - 9.0),
                    egui::vec2(18.0, 18.0),
                ),
            );
            inset += 23.0;
        }
        let color = if selected {
            Color32::from_rgb(239, 242, 247)
        } else {
            Color32::from_rgb(209, 217, 229)
        };
        let mut text = egui::text::LayoutJob::simple_singleline(
            name.to_owned(),
            egui::FontId::proportional(13.0),
            color,
        );
        text.wrap.max_width = (rect.width() - inset - count_width - 8.0).max(1.0);
        text.wrap.max_rows = 1;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(text));
        painter.galley(
            egui::pos2(rect.left() + inset, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    response
        .on_hover_text(name)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn recording_time(vod: &Vod) -> &str {
    vod.started_at
        .as_deref()
        .or(vod.ended_at.as_deref())
        .unwrap_or("")
}

fn recording_title(vod: &Vod) -> &str {
    if vod.title.trim().is_empty() {
        "VOD"
    } else {
        &vod.title
    }
}

fn recording_overlaps(vod: &Vod, (start, end): (i64, i64)) -> bool {
    let parse = |text: &str| {
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .ok()
            .and_then(|time| i64::try_from(time.unix_timestamp_nanos() / 1_000_000).ok())
    };
    let Some(recording_start) = vod.started_at.as_deref().and_then(parse) else {
        return false;
    };
    if start >= end || recording_start >= end {
        return false;
    }
    match vod.ended_at.as_deref() {
        Some(text) => parse(text)
            .is_some_and(|recording_end| recording_end > start && recording_end > recording_start),
        // An archive still being finalized can have no end yet. The authenticated
        // replay metadata decides whether the selected pull is actually available.
        None => true,
    }
}

fn review_povs(
    live: &[Stream],
    vods: &[Vod],
    selected: &Stream,
    span: Option<(i64, i64)>,
) -> Vec<Stream> {
    // A saved raid's menu should not list unrelated broadcasts live today.
    let mut povs = if selected.recording_id.is_some() {
        Vec::new()
    } else {
        live.to_vec()
    };
    if let Some(span) = span {
        povs.extend(
            vods.iter()
                .filter(|vod| recording_overlaps(vod, span))
                .map(Vod::as_stream),
        );
    }
    // Refreshes of live status must never remove the selected saved recording.
    let same = |stream: &Stream| {
        stream.user_id == selected.user_id
            && stream.provider == selected.provider
            && stream.channel_id == selected.channel_id
            && stream.recording_id == selected.recording_id
    };
    if !povs.iter().any(same) {
        povs.push(selected.clone());
    }
    let selected_identity = pov_recording_identity(selected);
    // A live YouTube registration and its saved video are exact aliases. Keep
    // the active transport, but retain known archive bounds when deduplicating.
    let archive_ranges: std::collections::HashMap<_, _> = vods
        .iter()
        .filter_map(|vod| {
            let stream = vod.as_stream();
            Some((pov_recording_identity(&stream), stream.replay_range()?))
        })
        .collect();
    for stream in &mut povs {
        if stream.replay_range().is_none() {
            if let Some(&(start, end)) = archive_ranges.get(&pov_recording_identity(stream)) {
                stream.replay_start_ms = Some(start);
                stream.replay_end_ms = Some(end);
            }
        }
    }
    let mut seen = HashSet::new();
    povs.retain(|stream| {
        let identity = pov_recording_identity(stream);
        // Preserve the selected transport identity even if it was appended
        // after a saved/live alias. Never interrupt the current player to dedup.
        (identity != selected_identity || same(stream)) && seen.insert(identity)
    });
    povs
}

fn pov_recording_identity(stream: &Stream) -> (String, &'static str, String, Option<String>) {
    // YouTube registrations contain a video ID, so matching live/saved IDs are
    // exact aliases. Twitch registrations contain only a channel name; do not
    // infer an archive match from member/provider or raid-date proximity.
    let exact_youtube = stream.provider == Provider::Youtube
        && stream.channel_id.len() == 11
        && stream
            .channel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && stream
            .recording_id
            .as_deref()
            .is_none_or(|id| id == stream.channel_id);
    (
        stream.user_id.clone(),
        stream.provider.key(),
        stream.channel_id.clone(),
        if exact_youtube {
            None
        } else {
            stream.recording_id.clone()
        },
    )
}

fn recording_labels(vods: &[Vod], povs: &[Stream]) -> crate::review_ui::RecordingLabels {
    let wanted: HashSet<_> = povs
        .iter()
        .filter_map(|stream| {
            stream
                .recording_id
                .as_deref()
                .or_else(|| {
                    (stream.provider == Provider::Youtube).then_some(stream.channel_id.as_str())
                })
                .map(|id| format!("{}:{id}", stream.provider.key()))
        })
        .collect();
    vods.iter()
        .filter_map(|vod| {
            let key = format!("{}:{}", vod.provider.key(), vod.id);
            if !wanted.contains(&key) {
                return None;
            }
            let when = time::OffsetDateTime::parse(
                recording_time(vod),
                &time::format_description::well_known::Rfc3339,
            )
            .ok()
            .map(|at| {
                let at = at.to_offset(time::UtcOffset::UTC);
                format!(
                    "{} {} · {:02}:{:02} UTC",
                    at.day(),
                    &at.month().to_string()[..3],
                    at.hour(),
                    at.minute()
                )
            })
            .unwrap_or_else(|| "VOD date unavailable".into());
            Some((
                key,
                crate::review_ui::RecordingLabel {
                    when,
                    title: recording_title(vod).to_owned(),
                },
            ))
        })
        .collect()
}

fn recording_day(vod: &Vod) -> &str {
    recording_time(vod).get(..10).unwrap_or("")
}

fn recording_day_label(day: &str) -> String {
    let parts: Vec<_> = day.split('-').collect();
    if parts.len() == 3 {
        if let (Ok(year), Ok(month), Ok(day)) = (
            parts[0].parse::<i32>(),
            parts[1].parse::<u8>(),
            parts[2].parse::<u8>(),
        ) {
            if let Ok(month) = time::Month::try_from(month) {
                if time::Date::from_calendar_date(year, month, day).is_ok() {
                    return format!("{day} {month} {year}");
                }
            }
        }
    }
    "Date unavailable".into()
}

fn action_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::from_rgb(239, 242, 247)))
        .fill(Color32::from_rgb(36, 41, 50))
        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(51, 58, 70)))
        .corner_radius(8)
        .min_size(egui::vec2(80.0, 32.0))
}

fn submission_status(stream: &Stream) -> &'static str {
    match stream.status {
        Status::Checking => "Link saved. Checking live status…",
        Status::Unknown => "Link saved. Live status couldn't be checked. Brick will retry.",
        Status::Live => "Live now.",
        Status::Offline => match stream.broadcast_state.as_deref() {
            Some("ended") => {
                "This broadcast has ended. Add your next broadcast's link to go live here again."
            }
            Some("upcoming") => "Scheduled. Brick will show you when this broadcast starts.",
            _ if stream.provider == Provider::Twitch => {
                "Channel saved. You'll appear automatically when you go live."
            }
            _ => "Broadcast saved. Brick will check when it goes live.",
        },
    }
}

fn live_people(streams: &[Stream]) -> Vec<&Stream> {
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    for stream in streams {
        if seen.insert(&stream.user_id) {
            people.push(stream);
        }
    }
    people
}

fn access_token() -> Result<String, streams::Error> {
    discord_auth::current_access_token().map_err(|message| streams::Error {
        message,
        access_denied: true,
    })
}

pub(crate) fn player_url_for_playback(
    url: &str,
    playback: Option<&crate::review_ui::Playback>,
) -> String {
    let mut url = url::Url::parse(url).expect("Validated player URL");
    let recording = url
        .query_pairs()
        .find(|(key, _)| key == "recording")
        .map(|(_, id)| id.into_owned());
    url.set_query(None);
    if let Some(recording) = recording {
        url.query_pairs_mut().append_pair("recording", &recording);
    }
    if let Some(playback) = playback {
        url.query_pairs_mut()
            .append_pair("at", &format!("{:.3}", playback.seconds))
            .append_pair("broadcast", &playback.broadcast_id);
        if !playback.autoplay {
            url.query_pairs_mut().append_pair("paused", "1");
        }
    }
    url.into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn stream_actions_keep_painted_geometry_on_hover() {
        crate::ui::tests::assert_static_button_hover(
            &["Twitch", "VODs", "Refresh", "Your streams"],
            |ui| {
                ui.horizontal(|ui| {
                    for label in ["Twitch", "VODs", "Refresh", "Your streams"] {
                        ui.add(super::action_button(label));
                    }
                });
            },
        );
    }

    #[test]
    fn fullscreen_toolbar_keeps_video_bounds_stable_with_long_pov_name() {
        for width in [720.0, 980.0, 1920.0] {
            let mut host = super::StreamsUi::default();
            let mut stream = recording("987", "1").as_stream();
            stream.name =
                "A player with a very long display name that must fit the fullscreen toolbar"
                    .into();
            host.selected = Some(stream);
            host.review.open_recording();
            let ctx = egui::Context::default();
            let mut previous = None;
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 560.0),
                        )),
                        ..Default::default()
                    },
                    |ui| host.draw_fullscreen(ui),
                );
                let video = host.player_rect.unwrap();
                assert!(video.top() <= 56.0 && video.left() >= 0.0 && video.right() <= width);
                if let Some(previous) = previous {
                    assert_eq!(video, previous);
                }
                previous = Some(video);
                assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "Choose a pull")));
            }
        }
    }

    #[test]
    fn native_wrapper_recovery_is_automatic_bounded_and_resets_on_new_selection() {
        let mut streams = super::StreamsUi::default();
        streams.recover_player("Temporary loading failure".into());
        assert!(streams.player_error.is_none());
        assert!(streams.player_attempted);
        assert!(streams.player_retry_at.is_some());
        assert_eq!(streams.player_retries, 1);
        streams.recover_player("Recording unavailable".into());
        assert!(streams.player_retry_at.is_none());
        assert_eq!(
            streams.player_error.as_deref(),
            Some("Recording unavailable")
        );
        assert_eq!(streams.player_retries, 1);
        streams.stop_player();
        streams.recover_player("A different recording".into());
        assert!(streams.player_error.is_none());
        assert!(streams.player_retry_at.is_some());
    }

    #[test]
    fn comparison_is_disposed_on_exit_inactive_view_and_authorization_loss() {
        let selected = crate::streams::Stream {
            user_id: "101".into(),
            raid_role: None,
            name: "Example player".into(),
            provider: crate::streams::Provider::Youtube,
            channel_id: "test-channel".into(),
            url: "https://www.youtube.com/watch?v=abcDEF_12-3".into(),
            status: crate::streams::Status::Offline,
            broadcast_state: None,
            recording_id: Some("abcDEF_12-3".into()),
            replay_start_ms: None,
            replay_end_ms: None,
        };
        for exit in 0..3 {
            let mut host = super::StreamsUi::default();
            host.comparison = Some(crate::review_compare_ui::Comparison::new(
                &host.review,
                selected.clone(),
                1000,
                false,
            ));
            match exit {
                0 => host.stop_player(),
                1 => {
                    host.tick(&eframe::egui::Context::default(), false, true);
                }
                _ => {
                    host.tick(&eframe::egui::Context::default(), true, false);
                }
            }
            assert!(host.comparison.is_none());
            assert!(host.player.is_none());
        }
    }

    use super::*;

    fn snapshot() -> Snapshot {
        serde_json::from_value(serde_json::json!({"streams":[{"userId":"1","name":"Guildmate","provider":"twitch","channelId":"guildmate","url":"https://www.twitch.tv/guildmate","status":"live"}],"ownStream":null,"providers":{"twitch":true,"youtube":true}})).unwrap()
    }

    fn recording(id: &str, member: &str) -> Vod {
        serde_json::from_value(serde_json::json!({
            "id":id,"userId":member,"name":"Guildmate","provider":"twitch",
            "url":format!("https://www.twitch.tv/videos/{id}"),"title":"Raid recording",
            "startedAt":"2026-09-08T08:00:00Z","endedAt":"2026-09-08T22:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn offline_recording_selection_survives_live_refresh_and_expiry() {
        let ctx = egui::Context::default();
        let mut ui = StreamsUi::default();
        ui.open_recording(&recording("987", "1"));
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.received_at = Some(Instant::now() - MAX_STALE);
        let mut live = snapshot();
        live.streams.clear();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        tx.send(Ok(ResultData::Snapshot(live))).unwrap();
        ui.tick(&ctx, true, true);
        assert_eq!(
            ui.selected.as_ref().unwrap().recording_id.as_deref(),
            Some("987")
        );
    }

    #[test]
    fn leaving_streams_during_a_vod_returns_to_the_library_and_discards_player_work() {
        let ctx = egui::Context::default();
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.received_at = Some(Instant::now());
        let (_tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        // The recording's owner need not appear in the current live directory.
        ui.open_recording(&recording("987", "2"));
        ui.tick(&ctx, true, true);
        assert!(ui.review.active());
        let (prepare, rx) = mpsc::channel();
        ui.player_work = Some(rx);
        ui.player_switch_pending = true;

        ui.tick(&ctx, true, false);
        assert!(!ui.review.active());
        assert!(ui.recordings_open);
        assert!(ui.selected.is_none());
        assert!(ui.focused.is_none());
        assert!(ui.player_work.is_none());
        assert!(!ui.player_switch_pending);
        assert!(prepare
            .send(Err("Late preparation".to_string().into()))
            .is_err());

        ui.tick(&ctx, true, true);
        assert!(ui.recordings_open);
        assert!(ui.selected.is_none());
        assert!(ui.player_error.is_none());
        assert_eq!(live_people(&ui.snapshot.as_ref().unwrap().streams).len(), 1);
    }

    #[test]
    fn an_ended_review_cannot_draw_a_saved_recording_as_a_live_stream() {
        let ctx = egui::Context::default();
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        // Explicitly leaving review must return a saved recording to the list.
        ui.selected = Some(recording("987", "2").as_stream());
        assert!(!ui.review.active());
        let _ = ctx.run_ui(egui::RawInput::default(), |root| ui.draw(root));
        assert!(ui.recordings_open);
        assert!(ui.selected.is_none());
        assert!(ui.player_rect.is_none());
    }

    #[test]
    fn removal_clears_matching_associations_and_player_preparation_only() {
        let removed = recording("987", "1");
        let other = recording("654", "1");
        let mut ui = StreamsUi::default();
        ui.recordings = Some(Rc::new(vec![removed.clone(), recording("987", "2"), other]));
        ui.open_recording(&removed);
        ui.confirm_remove_recording = Some(removed);
        let (tx, rx) = mpsc::channel();
        ui.player_work = Some(rx);
        ui.apply_recording_removal(&Provider::Twitch, "987");
        assert!(ui.selected.is_none());
        assert!(ui.recordings_open);
        assert!(ui.confirm_remove_recording.is_none());
        assert_eq!(ui.recordings.as_ref().unwrap()[0].id, "654");
        assert_eq!(ui.recordings.as_ref().unwrap().len(), 1);
        assert!(tx
            .send(Err("Obsolete player preparation".to_owned().into()))
            .is_err());
    }

    #[test]
    fn library_permission_and_confirmation_are_cleared_with_authorization() {
        let mut ui = StreamsUi::default();
        ui.can_delete_recordings = true;
        ui.recordings = Some(Rc::new(vec![recording("987", "1")]));
        ui.confirm_remove_recording = Some(recording("987", "1"));
        ui.tick(&egui::Context::default(), false, false);
        assert!(!ui.can_delete_recordings);
        assert!(ui.recordings.is_none());
        assert!(ui.confirm_remove_recording.is_none());
    }

    #[test]
    fn recorded_povs_include_long_preraid_and_unknown_end_without_other_nights() {
        let at = |s| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .unix_timestamp()
                * 1000
        };
        let span = (at("2026-09-08T18:00:00Z"), at("2026-09-08T21:00:00Z"));
        let first = recording("987", "1");
        let mut unknown_end = recording("654", "2");
        unknown_end.ended_at = None;
        let mut previous = recording("321", "3");
        previous.started_at = Some("2026-09-07T18:00:00Z".into());
        previous.ended_at = Some("2026-09-07T22:00:00Z".into());
        let mut future = recording("111", "4");
        future.started_at = Some("2026-09-09T18:00:00Z".into());
        let mut malformed = recording("222", "5");
        malformed.ended_at = Some("invalid".into());
        let vods = vec![first.clone(), unknown_end, previous, future, malformed];
        let result = review_povs(&[], &vods, &first.as_stream(), Some(span));
        assert_eq!(
            result
                .iter()
                .map(|s| s.recording_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["987", "654"]
        );
        assert_eq!(review_povs(&[], &[], &first.as_stream(), None).len(), 1);
    }

    #[test]
    fn youtube_aliases_deduplicate_but_distinct_archives_and_twitch_do_not() {
        let mut vod = recording("abcDEF_12-3", "1");
        vod.provider = Provider::Youtube;
        vod.url = "https://www.youtube.com/watch?v=abcDEF_12-3".into();
        let saved = vod.as_stream();
        let mut live = saved.clone();
        live.recording_id = None;
        live.status = Status::Live;
        live.replay_start_ms = None;
        live.replay_end_ms = None;
        assert_eq!(
            pov_recording_identity(&saved),
            pov_recording_identity(&live)
        );
        let start = time::OffsetDateTime::parse(
            "2026-09-08T18:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
        .unix_timestamp()
            * 1000;
        let span = (start, start + 10_800_000);
        let mut other = vod.clone();
        other.id = "xyzDEF_12-3".into();
        let result = review_povs(&[live.clone()], &[vod.clone(), other], &live, Some(span));
        assert_eq!(
            result
                .iter()
                .filter(|stream| stream.channel_id == live.channel_id)
                .count(),
            1
        );
        assert_eq!(result.len(), 2, "Distinct archives must remain available");
        assert_eq!(
            result
                .iter()
                .find(|stream| stream.channel_id == live.channel_id)
                .unwrap()
                .replay_range(),
            saved.replay_range()
        );
        assert!(saved.replay_range().is_some());
        assert!(result
            .iter()
            .any(|stream| stream.channel_id == live.channel_id && stream.recording_id.is_none()));
        let result = review_povs(&[live], &[vod], &saved, Some(span));
        assert!(result
            .iter()
            .any(|stream| stream.recording_id == saved.recording_id));
        let twitch_saved = recording("987", "1").as_stream();
        let mut twitch_live = twitch_saved.clone();
        twitch_live.recording_id = None;
        assert_ne!(
            pov_recording_identity(&twitch_live),
            pov_recording_identity(&twitch_saved)
        );
        let mut different = saved.clone();
        different.recording_id = Some("xyzDEF_12-3".into());
        different.channel_id = "xyzDEF_12-3".into();
        assert_ne!(
            pov_recording_identity(&saved),
            pov_recording_identity(&different)
        );
    }

    #[test]
    fn archive_labels_use_cached_date_time_and_title_without_changing_member_name() {
        let first = recording("987", "1");
        let mut second = recording("654", "1");
        second.started_at = Some("2026-09-08T16:15:00Z".into());
        second.title = "Second recording".into();
        let povs = [first.as_stream(), second.as_stream()];
        let labels = recording_labels(&[first, second], &povs);
        let a = crate::review_ui::recording_label(&labels, &povs[0]).unwrap();
        let b = crate::review_ui::recording_label(&labels, &povs[1]).unwrap();
        assert_ne!(a.when, b.when);
        assert_eq!(b.title, "Second recording");
        assert!(b.when.contains("16:15 UTC"));
        assert_eq!(povs[0].name, povs[1].name);
    }

    #[test]
    fn preparing_saved_replay_keeps_its_recording_identity_for_latest_seek() {
        let url =
            "https://brick.example/v1/streams/player/1/twitch?recording=987&at=1&broadcast=old";
        let playback = crate::review_ui::Playback {
            seconds: 125.25,
            autoplay: false,
            broadcast_id: "321".into(),
            public_url: String::new(),
        };
        let parsed = url::Url::parse(&player_url_for_playback(url, Some(&playback))).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().collect();
        assert_eq!(query["recording"], "987");
        assert_eq!(query["at"], "125.250");
        assert_eq!(query["broadcast"], "321");
        assert_eq!(query["paused"], "1");
        let plain = url::Url::parse(&player_url_for_playback(url, None)).unwrap();
        assert_eq!(plain.query(), Some("recording=987"));
    }

    #[test]
    fn player_preparation_uses_the_latest_pull_position_and_pause_intent() {
        let earlier =
            "https://brick.example/v1/streams/player/101/youtube?at=30&broadcast=abcDEF_12-3";
        let latest = crate::review_ui::Playback {
            seconds: 151.375,
            autoplay: false,
            broadcast_id: "abcDEF_12-3".into(),
            public_url: String::new(),
        };
        let url = url::Url::parse(&player_url_for_playback(earlier, Some(&latest))).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "at").unwrap().1,
            "151.375"
        );
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "paused")
                .unwrap()
                .1,
            "1"
        );
        assert_eq!(url.query_pairs().count(), 3);
        assert!(url::Url::parse(&player_url_for_playback(earlier, None))
            .unwrap()
            .query()
            .is_none());
    }

    #[test]
    fn losing_authorization_discards_private_data_and_late_results() {
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.saved_provider = Some(Provider::Twitch);
        ui.drafts[0] = "https://twitch.tv/privateguildmate".into();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.tick(&egui::Context::default(), false, false);
        assert!(ui.snapshot.is_none());
        assert!(ui.saved_provider.is_none());
        assert!(ui.drafts.iter().all(String::is_empty));
        assert!(tx.send(Ok(ResultData::Snapshot(snapshot()))).is_err());
    }

    #[test]
    fn saved_submission_waits_for_verification_without_reporting_failure() {
        let mut ui = StreamsUi::default();
        ui.saved_provider = Some(Provider::Twitch);
        let ctx = egui::Context::default();
        for (status, unverified, expected) in [
            ("checking", 0, "Checking live status"),
            ("offline", 0, "automatically when you go live"),
            ("live", 0, "Live now"),
            ("unknown", 1, "couldn't be checked"),
        ] {
            let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
                "streams": [],
                "ownStreams": [{"userId":"1","name":"Me","provider":"twitch",
                    "channelId":"mychannel","url":"https://www.twitch.tv/mychannel","status":status}],
                "providers": {"twitch":true,"youtube":true},
                "unverifiedCount": unverified
            })).unwrap();
            let (tx, rx) = mpsc::channel();
            ui.work = Some(rx);
            tx.send(Ok(ResultData::Snapshot(snapshot))).unwrap();
            ui.tick(&ctx, true, false);
            assert!(ui.saved_provider.is_none());
            assert!(ui.notice.is_none());
            let current = ui.snapshot.as_ref().unwrap();
            assert!(submission_status(&current.own_streams[0]).contains(expected));
            assert_eq!(current.unverified_count, unverified);
            assert!(ui.selected.is_none());
        }
    }

    #[test]
    fn stream_editor_stays_above_the_player_placeholder_and_closes_with_escape() {
        let mut streams = StreamsUi::default();
        let current = snapshot();
        streams.selected = current.streams.first().cloned();
        streams.snapshot = Some(Rc::new(current));
        streams.edit_open = true;
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(980.0, 720.0),
            )),
            ..Default::default()
        };
        let covered = egui::pos2(490.0, 400.0);
        // Exercise the real editor above a root-layer video surface even in
        // development builds with no presence API configured.
        for _ in 0..3 {
            let _ = ctx.run_ui(input.clone(), |ui| {
                ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::BLACK);
                streams.draw_editor(ui.ctx());
            });
        }
        let modal_layer = ctx
            .memory(|memory| memory.top_modal_layer())
            .expect("Editor blocks background input");
        assert_eq!(
            ctx.layer_id_at(covered),
            Some(modal_layer),
            "The stream must not cover the registration fields"
        );
        let mut escape = input;
        escape.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run_ui(escape, |ui| streams.draw_editor(ui.ctx()));
        assert!(!streams.edit_open);
        assert!(
            streams.selected.is_some(),
            "Closing setup preserves the stream selection"
        );
    }

    #[test]
    fn waiting_for_stream_requests_does_not_animate_the_native_window() {
        let mut streams = StreamsUi::default();
        let (_tx, rx) = mpsc::channel();
        streams.work = Some(rx);
        streams.snapshot = Some(Rc::new(snapshot()));
        let ctx = egui::Context::default();
        for _ in 0..5 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| streams.draw(ui));
        }
        let output = ctx.run_ui(egui::RawInput::default(), |ui| streams.draw(ui));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::from_secs(1)
        );
    }

    #[test]
    fn ended_or_changed_stream_clears_selection_and_player_work() {
        let mut ui = StreamsUi::default();
        ui.selected = Some(snapshot().streams.remove(0));
        let mut ended = snapshot();
        ended.streams.clear();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        tx.send(Ok(ResultData::Snapshot(ended))).unwrap();
        ui.tick(&egui::Context::default(), true, false);
        assert!(ui.selected.is_none());
        assert!(ui.notice.is_none());
    }

    #[test]
    fn unverified_live_data_expires_even_when_hidden() {
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.selected = Some(snapshot().streams.remove(0));
        ui.received_at = Some(Instant::now() - MAX_STALE);
        ui.tick(&egui::Context::default(), true, false);
        assert!(ui.snapshot.is_none());
        assert!(ui.selected.is_none());
    }

    #[test]
    fn dual_platform_streams_share_one_player_sidebar_entry() {
        let mut snapshot = snapshot();
        let mut youtube = snapshot.streams[0].clone();
        youtube.provider = Provider::Youtube;
        youtube.channel_id = "abcDEF_12-3".into();
        snapshot.streams.push(youtube);
        assert_eq!(live_people(&snapshot.streams).len(), 1);
    }

    #[test]
    fn recording_dates_remain_readable_and_auth_loss_clears_history() {
        let vods: Vec<Vod> = serde_json::from_value(serde_json::json!([
            {"userId":"2", "name":"Zed", "provider":"youtube", "url":"https://www.youtube.com/watch?v=abcDEF_12-3", "title":"Raid", "startedAt":"2026-09-08T18:00:00Z"},
            {"userId":"1", "name":"Andy", "provider":"twitch", "url":"https://www.twitch.tv/videos/1", "title":"Raid", "startedAt":"2026-09-07T18:00:00Z"},
            {"userId":"1", "name":"Andy", "provider":"youtube", "url":"https://www.youtube.com/watch?v=abcDEF_12-3", "title":"Raid", "startedAt":"2026-09-07T18:00:00Z"}
        ])).unwrap();
        assert_eq!(recording_day(&vods[1]), recording_day(&vods[2]));
        assert_eq!(
            recording_day_label(recording_day(&vods[0])),
            "8 September 2026"
        );
        assert_eq!(recording_day_label("2026-02-30"), "Date unavailable");
        let mut ui = StreamsUi::default();
        ui.recordings = Some(Rc::new(vods));
        ui.recordings_open = true;
        ui.tick(&egui::Context::default(), false, true);
        assert!(ui.recordings.is_none());
        assert!(!ui.recordings_open);
    }

    // Explicitly opt in with the local fixture and its isolated Secret Service
    // profile. This renders the actual native media child without starting the
    // add-on watcher or changing the production application/session.
    #[cfg(target_os = "linux")]
    mod native_smoke {
        use super::*;
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        use std::{
            collections::HashSet,
            sync::{Arc, Mutex},
        };
        use x11rb::{
            connection::Connection as _,
            protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, MapState},
        };

        #[derive(Default)]
        struct Outcome {
            complete: bool,
            failure: Option<String>,
            captures: Vec<String>,
        }

        struct Driver {
            streams: StreamsUi,
            outcome: Arc<Mutex<Outcome>>,
            started: Instant,
            phase_started: Instant,
            phase: u8,
            baseline: Option<HashSet<u32>>,
            media_children: Vec<u32>,
            sampled_media: bool,
            clicked_media: bool,
            cycles: u8,
            drop_probe: Option<Box<dyn Fn() -> bool>>,
        }

        fn resource_sample(label: &str) -> (usize, usize) {
            let mut processes = Vec::new();
            for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                let Some(end) = stat.rfind(')') else {
                    continue;
                };
                let fields: Vec<_> = stat[end + 2..].split_whitespace().collect();
                if fields.len() < 13 {
                    continue;
                }
                let Some(parent) = fields[1].parse::<u32>().ok() else {
                    continue;
                };
                let ticks =
                    fields[11].parse::<u64>().unwrap_or(0) + fields[12].parse::<u64>().unwrap_or(0);
                let rss = std::fs::read_to_string(entry.path().join("status"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find(|line| line.starts_with("VmRSS:"))
                            .and_then(|line| line.split_whitespace().nth(1))
                            .and_then(|value| value.parse::<u64>().ok())
                    })
                    .unwrap_or(0);
                let name = stat[stat.find('(').unwrap_or(0) + 1..end].to_owned();
                processes.push((pid, parent, ticks, rss, name));
            }
            let mut selected = HashSet::from([std::process::id()]);
            loop {
                let before = selected.len();
                for (pid, parent, ..) in &processes {
                    if selected.contains(parent) {
                        selected.insert(*pid);
                    }
                }
                if before == selected.len() {
                    break;
                }
            }
            let mut rows: Vec<_> = processes.into_iter().filter(|(pid, ..)| selected.contains(pid)).map(|(pid, parent, ticks, rss, name)| {
                let pss = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find(|line| line.starts_with("Pss:"))
                            .and_then(|line| line.split_whitespace().nth(1))
                            .and_then(|value| value.parse::<u64>().ok())
                    });
                serde_json::json!({"pid":pid,"parent":parent,"cpuTicks":ticks,"rssKiB":rss,"pssKiB":pss,"name":name})
            }).collect();
            rows.sort_by_key(|row| row["pid"].as_u64());
            let total: u64 = rows.iter().filter_map(|row| row["rssKiB"].as_u64()).sum();
            let total_pss = rows
                .iter()
                .map(|row| row["pssKiB"].as_u64())
                .collect::<Option<Vec<_>>>()
                .map(|values| values.into_iter().sum::<u64>());
            let network_processes = rows
                .iter()
                .filter(|row| row["name"] == "WebKitNetworkPr")
                .count();
            let renderer_processes = rows
                .iter()
                .filter(|row| row["name"] == "WebKitWebProces")
                .count();
            eprintln!(
                "Native resource sample {}",
                serde_json::json!({"label":label,"atMillis":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64,"totalRssKiB":total,"totalPssKiB":total_pss,"processes":rows})
            );
            (network_processes, renderer_processes)
        }

        fn window_id(frame: &eframe::Frame) -> Result<u32, String> {
            match frame
                .window_handle()
                .map_err(|error| error.to_string())?
                .as_raw()
            {
                RawWindowHandle::Xlib(handle) => handle
                    .window
                    .try_into()
                    .map_err(|_| "Invalid X11 window".into()),
                RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
                _ => Err("The native smoke test requires X11/XWayland.".into()),
            }
        }

        fn mapped_children(window: u32) -> Result<HashSet<u32>, String> {
            let operation = || -> Result<HashSet<u32>, Box<dyn std::error::Error>> {
                let (connection, _) = x11rb::connect(None)?;
                let mut children = HashSet::new();
                let mut parents = vec![window];
                for _ in 0..8 {
                    let mut next = Vec::new();
                    for parent in parents {
                        if let Ok(tree) = connection.query_tree(parent)?.reply() {
                            for child in tree.children {
                                if connection.get_window_attributes(child)?.reply().is_ok_and(
                                    |attributes| attributes.map_state == MapState::VIEWABLE,
                                ) {
                                    children.insert(child);
                                    next.push(child);
                                }
                            }
                        }
                    }
                    if next.is_empty() {
                        break;
                    }
                    parents = next;
                }
                Ok(children)
            };
            operation().map_err(|error| error.to_string())
        }

        fn capture(window: u32, name: &str) -> Result<String, String> {
            let operation = || -> Result<String, Box<dyn std::error::Error>> {
                let (connection, screen) = x11rb::connect(None)?;
                let setup = connection.setup();
                let root = &setup.roots[screen];
                let geometry = connection.get_geometry(window)?.reply()?;
                let shot = connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        window,
                        0,
                        0,
                        geometry.width,
                        geometry.height,
                        u32::MAX,
                    )?
                    .reply()?;
                let format = setup
                    .pixmap_formats
                    .iter()
                    .find(|format| format.depth == shot.depth)
                    .ok_or("Missing image format")?;
                if format.bits_per_pixel != 32 {
                    return Err("Expected a 32-bit X11 screenshot".into());
                }
                let visual = root
                    .allowed_depths
                    .iter()
                    .flat_map(|depth| &depth.visuals)
                    .find(|visual| visual.visual_id == shot.visual)
                    .ok_or("Missing root visual")?;
                let stride = (geometry.width as usize * 32).div_ceil(format.scanline_pad as usize)
                    * format.scanline_pad as usize
                    / 8;
                let channel = |pixel: u32, mask: u32| -> u8 {
                    if mask == 0 {
                        return 0;
                    }
                    let shift = mask.trailing_zeros();
                    (((pixel & mask) >> shift) * 255 / (mask >> shift)) as u8
                };
                let mut image =
                    image::RgbaImage::new(geometry.width as u32, geometry.height as u32);
                for y in 0..geometry.height as usize {
                    for x in 0..geometry.width as usize {
                        let bytes: [u8; 4] =
                            shot.data[y * stride + x * 4..y * stride + x * 4 + 4].try_into()?;
                        let pixel = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                            u32::from_le_bytes(bytes)
                        } else {
                            u32::from_be_bytes(bytes)
                        };
                        image.put_pixel(
                            x as u32,
                            y as u32,
                            image::Rgba([
                                channel(pixel, visual.red_mask),
                                channel(pixel, visual.green_mask),
                                channel(pixel, visual.blue_mask),
                                255,
                            ]),
                        );
                    }
                }
                let directory = std::path::Path::new("/tmp/brick-streams-smoke/results");
                std::fs::create_dir_all(directory)?;
                let path = directory.join(format!("native-{name}.png"));
                image.save(&path)?;
                Ok(path.to_string_lossy().into_owned())
            };
            operation().map_err(|error| error.to_string())
        }

        impl Driver {
            fn select(&mut self, provider: Provider) -> Result<(), String> {
                let member = if provider == Provider::Twitch {
                    "103"
                } else {
                    "102"
                };
                let stream =
                    self.streams
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| {
                            snapshot.streams.iter().find(|stream| {
                                stream.user_id == member && stream.provider == provider
                            })
                        })
                        .cloned()
                        .ok_or_else(|| {
                            format!(
                                "Fixture member {member} has no live {} stream",
                                provider.label()
                            )
                        })?;
                self.streams.stop_player();
                self.streams.player_error = None;
                self.streams.focused = Some((stream.user_id.clone(), stream.name.clone()));
                self.streams.selected = Some(stream);
                Ok(())
            }

            fn next(&mut self, phase: u8) {
                self.phase = phase;
                self.phase_started = Instant::now();
                self.sampled_media = false;
                self.clicked_media = false;
                eprintln!("Native stream smoke phase {phase}");
            }

            fn save_capture(&self, window: u32, name: &str) -> Result<(), String> {
                let path = capture(window, name)?;
                eprintln!("Native stream smoke screenshot: {path}");
                self.outcome.lock().unwrap().captures.push(path);
                Ok(())
            }

            fn assert_media_gone(&self, window: u32) -> Result<(), String> {
                if self.streams.player.is_some() {
                    return Err("Player still exists after closing its view".into());
                }
                let mapped = mapped_children(window)?;
                if self
                    .media_children
                    .iter()
                    .any(|child| mapped.contains(child))
                {
                    return Err("A native media child remains mapped after player shutdown".into());
                }
                if self.drop_probe.as_ref().is_some_and(|probe| !probe()) {
                    return Err(
                        "The native WebKit widget remains alive after player shutdown".into(),
                    );
                }
                Ok(())
            }

            fn track_media(&mut self, window: u32) -> Result<(), String> {
                self.media_children = mapped_children(window)?
                    .difference(self.baseline.as_ref().unwrap())
                    .copied()
                    .collect();
                self.drop_probe = self
                    .streams
                    .player
                    .as_ref()
                    .and_then(|player| player.diagnostic_drop_probe());
                if self.media_children.is_empty() {
                    return Err("The real media player created no mapped native child".into());
                }
                Ok(())
            }

            fn advance(
                &mut self,
                ctx: &egui::Context,
                frame: &eframe::Frame,
            ) -> Result<(), String> {
                if self.started.elapsed() > Duration::from_secs(105) {
                    return Err("Native stream smoke timed out".into());
                }
                let window = window_id(frame)?;
                if self.baseline.is_none() {
                    self.baseline = Some(mapped_children(window)?);
                    resource_sample("idle-before-streams");
                }
                let elapsed = self.phase_started.elapsed();
                match self.phase {
                    0 if self.streams.snapshot.is_some() => {
                        self.select(Provider::Youtube)?;
                        self.next(1);
                    }
                    1 | 3 => {
                        let expected = if self.phase == 1 {
                            Provider::Youtube
                        } else {
                            Provider::Twitch
                        };
                        if self
                            .streams
                            .selected
                            .as_ref()
                            .is_none_or(|stream| stream.provider != expected)
                        {
                            return Err("The selected platform changed during the automated capture sequence".into());
                        }
                        if let Some(error) = &self.streams.player_error {
                            return Err(error.clone());
                        }
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(5)
                            && !self.clicked_media
                        {
                            let rect = self.streams.player_rect.ok_or("Player has no bounds")?;
                            let scale = ctx.pixels_per_point() as f64;
                            let (x, y) = if self.phase == 1 {
                                (
                                    rect.width() as f64 * scale / 2.0,
                                    rect.height() as f64 * scale / 2.0,
                                )
                            } else {
                                (24.0, rect.height() as f64 * scale - 24.0)
                            };
                            self.streams
                                .player
                                .as_ref()
                                .unwrap()
                                .diagnostic_click(x, y)?;
                            self.clicked_media = true;
                        }
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(10)
                            && !self.sampled_media
                        {
                            self.streams.player.as_ref().unwrap().diagnostic_details();
                            for child in
                                mapped_children(window)?.difference(self.baseline.as_ref().unwrap())
                            {
                                let provider = if self.phase == 1 { "youtube" } else { "twitch" };
                                match capture(*child, &format!("{provider}-early-child-{child}")) {
                                    Ok(path) => eprintln!("Native media child screenshot: {path}"),
                                    Err(error) => eprintln!(
                                        "Native child {child} has no readable drawable: {error}"
                                    ),
                                }
                            }
                            if std::env::var_os("BRICK_STREAM_DIAGNOSTIC").is_some() {
                                self.streams.player.as_ref().unwrap().diagnostic_html();
                            }
                            self.sampled_media = true;
                        }
                        let capture_after = if self.phase == 1 { 30 } else { 15 };
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(capture_after)
                        {
                            self.track_media(window)?;
                            resource_sample(if self.phase == 1 {
                                "youtube-open"
                            } else {
                                "twitch-open"
                            });
                            self.media_children = mapped_children(window)?
                                .difference(self.baseline.as_ref().unwrap())
                                .copied()
                                .collect();
                            if self.media_children.is_empty() {
                                return Err(
                                    "The real media player created no mapped native child".into()
                                );
                            }
                            for child in &self.media_children {
                                let provider = if self.phase == 1 { "youtube" } else { "twitch" };
                                if let Ok(path) =
                                    capture(*child, &format!("{provider}-late-child-{child}"))
                                {
                                    eprintln!("Native media child screenshot: {path}");
                                }
                            }
                            self.save_capture(
                                window,
                                if self.phase == 1 { "youtube" } else { "twitch" },
                            )?;
                            if self.phase == 1 {
                                self.streams.tick(ctx, true, false);
                                self.next(2);
                            } else {
                                self.streams.edit_open = true;
                                self.next(4);
                            }
                        }
                    }
                    2 if elapsed > Duration::from_secs(2) => {
                        self.assert_media_gone(window)?;
                        self.save_capture(window, "updates-cleanup")?;
                        resource_sample("after-switch-to-updates");
                        if std::env::var_os("BRICK_STREAM_YOUTUBE_ONLY").is_some() {
                            self.outcome.lock().unwrap().complete = true;
                            self.next(10);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            return Ok(());
                        }
                        self.select(Provider::Twitch)?;
                        self.next(3);
                    }
                    4 if elapsed > Duration::from_secs(2) => {
                        let children = mapped_children(window)?;
                        if self.streams.player.is_none()
                            || !self
                                .media_children
                                .iter()
                                .all(|child| children.contains(child))
                        {
                            return Err("Opening settings replaced or hid the media child".into());
                        }
                        self.save_capture(window, "dialog-over-player")?;
                        resource_sample("after-dialog-open");
                        self.streams.tick(ctx, false, false);
                        if self.streams.snapshot.is_some()
                            || self.streams.selected.is_some()
                            || self.streams.player_work.is_some()
                            || self.streams.recordings.is_some()
                        {
                            return Err("Logout retained stream data or work".into());
                        }
                        self.next(5);
                    }
                    5 if elapsed > Duration::from_secs(2) => {
                        self.assert_media_gone(window)?;
                        self.save_capture(window, "logout-cleanup")?;
                        resource_sample("after-logout");
                        self.next(6);
                    }
                    6 if self.streams.snapshot.is_some() => {
                        self.select(Provider::Twitch)?;
                        self.next(7);
                    }
                    7 if self.streams.player.is_some() && elapsed > Duration::from_secs(3) => {
                        self.track_media(window)?;
                        resource_sample(&format!("repeat-{}-open", self.cycles + 1));
                        self.streams.stop_player();
                        self.next(8);
                    }
                    8 if elapsed > Duration::from_secs(1) => {
                        self.assert_media_gone(window)?;
                        self.cycles += 1;
                        resource_sample(&format!("repeat-{}-closed", self.cycles));
                        if self.cycles < 5 {
                            self.select(Provider::Twitch)?;
                            self.next(7);
                        } else {
                            self.streams.clear();
                            resource_sample("final-idle-start");
                            self.next(9);
                        }
                    }
                    9 if elapsed > Duration::from_secs(5) => {
                        self.assert_media_gone(window)?;
                        let (network_processes, renderer_processes) =
                            resource_sample("final-idle-end");
                        if network_processes > 1 || renderer_processes != 0 {
                            return Err(format!(
                                "Player shutdown retained {network_processes} network processes and {renderer_processes} renderer processes"
                            ));
                        }
                        self.outcome.lock().unwrap().complete = true;
                        self.next(10);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                }
                Ok(())
            }
        }

        impl eframe::App for Driver {
            fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
                if self.phase >= 10 {
                    return;
                }
                let authorized = self.phase != 5 && self.phase < 9;
                let active = !matches!(self.phase, 2 | 8) && authorized;
                if self.streams.tick(ctx, authorized, active) {
                    self.outcome.lock().unwrap().failure =
                        Some("The fixture rejected the secure test session".into());
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                if let Err(error) = self.advance(ctx, frame) {
                    let _ =
                        window_id(frame).and_then(|window| self.save_capture(window, "failure"));
                    self.outcome.lock().unwrap().failure = Some(error);
                    self.streams.clear();
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ctx.request_repaint_after(Duration::from_millis(if self.phase == 9 {
                    250
                } else {
                    33
                }));
            }

            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                let ctx = ui.ctx().clone();
                egui::Frame::central_panel(ui.style())
                    .fill(Color32::from_rgb(18, 21, 27))
                    .inner_margin(20)
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        ui.label(RichText::new("Brick · Native stream lifecycle test").strong());
                        ui.add_space(16.0);
                        if matches!(self.phase, 2 | 8) {
                            ui.heading("Updates");
                            ui.label(
                                "The stream player has closed. This native view remains usable.",
                            );
                        } else if self.phase == 5 || self.phase >= 9 {
                            ui.heading("Sign in to Discord");
                            ui.label(
                                "Private stream data and the native player have been cleared.",
                            );
                        } else {
                            ui.add_enabled_ui(false, |ui| self.streams.draw(ui));
                        }
                    });
                if self.streams.update_player(
                    frame,
                    &ctx,
                    !matches!(self.phase, 2 | 5 | 8) && self.phase < 9,
                ) {
                    self.outcome.lock().unwrap().failure =
                        Some("Player authorization failed".into());
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        #[test]
        #[ignore = "requires an isolated secure test session, localhost fixture and an X11 desktop"]
        fn player_lifecycle() {
            use winit::platform::x11::EventLoopBuilderExtX11 as _;
            assert_eq!(
                option_env!("BRICK_PRESENCE_API_URL"),
                Some("http://127.0.0.1:18080")
            );
            assert_eq!(option_env!("BRICK_DISCORD_CLIENT_ID"), Some("brick-test"));
            assert!(
                discord_auth::current_access_token().is_ok(),
                "Unlock the isolated test profile's Secret Service first"
            );
            let outcome = Arc::new(Mutex::new(Outcome::default()));
            let app_outcome = outcome.clone();
            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_title("Brick · Native stream smoke")
                    .with_inner_size([1080.0, 820.0])
                    .with_position([120.0, 80.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    builder.with_x11().with_any_thread(true);
                })),
                ..Default::default()
            };
            eframe::run_native(
                "Brick native stream smoke",
                options,
                Box::new(move |cc| {
                    cc.egui_ctx.set_visuals(egui::Visuals::dark());
                    Ok(Box::new(Driver {
                        streams: StreamsUi::default(),
                        outcome: app_outcome,
                        started: Instant::now(),
                        phase_started: Instant::now(),
                        phase: 0,
                        baseline: None,
                        media_children: Vec::new(),
                        sampled_media: false,
                        clicked_media: false,
                        cycles: 0,
                        drop_probe: None,
                    }))
                }),
            )
            .expect("Native lifecycle test window failed");
            let outcome = outcome.lock().unwrap();
            assert!(
                outcome.failure.is_none(),
                "{}",
                outcome.failure.as_deref().unwrap_or_default()
            );
            assert!(outcome.complete, "The lifecycle sequence did not complete");
            assert_eq!(
                outcome.captures.len(),
                if std::env::var_os("BRICK_STREAM_YOUTUBE_ONLY").is_some() {
                    2
                } else {
                    5
                }
            );
        }
    }
}
