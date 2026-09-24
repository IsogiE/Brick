//! Viewer state for one scoped, asynchronous content alignment request.
use super::*;
#[cfg(test)]
use crate::content_alignment::Status;
use crate::content_alignment::{Key, Ticket};

// Keep following long-running jobs on the small production worker.
const MAX_CONTENT_POLLS: u16 = 1440;

#[derive(Default)]
pub(super) struct State {
    pub report_input: String,
    pub samples: crate::content_alignment::sampling::Snapshot,
    pub manual_report: Option<String>,
    key: Option<Key>,
    ticket: Option<Ticket>,
    started: Option<Instant>,
    next_poll: Option<Instant>,
    polls: u16,
    failure: Option<String>,
    paused: bool,
    transport_retries: u8,
}

impl ReviewUi {
    pub(super) fn sync_content_selection(&mut self) -> bool {
        self.content
            .samples
            .tickets
            .retain(|ticket| !ticket.expired());
        let epoch = self
            .recording_auth_epoch()
            .unwrap_or_else(|| self.recording_match_status.map_or(0, |s| s.0));
        if self
            .recording_match_status
            .is_some_and(|status| status.0 != epoch)
        {
            self.content.samples = Default::default();
            if let Some(review) = self.review.as_mut() {
                review.content_timing.clear();
                review.marker_fallback.clear();
            }
            self.recording_match_status = Some((epoch, false));
        }
        let desired = self.review.as_ref().and_then(|review| {
            if self.recording_housekeeping
                || (!self.active && (!self.metadata_only || !self.preparing_recordings))
            {
                return None;
            }
            let cap = review.content_capability.as_ref()?;
            let plan = self.content.samples.plan(review, epoch);
            plan.pending.map(|ticket| ticket.key).or_else(|| {
                plan.next
                    .map(|pull| Key::new(&review.replay, &pull, cap, epoch))
            })
        });
        if self.content.key != desired {
            if matches!(
                self.work_action,
                Some(Action::ContentSubmit(..) | Action::ContentPoll(..))
            ) {
                self.cancel_read();
            }
            self.content.key = desired.clone();
            self.content.ticket = desired.as_ref().and_then(|key| {
                self.content
                    .samples
                    .tickets
                    .iter()
                    .find(|t| &t.key == key)
                    .cloned()
            });
            self.content.started = None;
            self.content.next_poll = None;
            self.content.polls = 0;
            self.content.failure = None;
            self.content.paused = false;
            self.content.transport_retries = 0;
        }
        self.apply_sample_model();
        if self.active && self.playback.is_none() {
            if let Some(pull) = self.pull.clone().filter(|pull| {
                self.review
                    .as_ref()
                    .is_some_and(|review| review.content_alignment(pull).is_some())
            }) {
                self.select(pull);
                return true;
            }
        }
        false
    }

    fn apply_sample_model(&mut self) {
        let epoch = self.recording_match_status.map_or(0, |s| s.0);
        let Some(review) = self.review.as_mut() else {
            return;
        };
        let plan = self.content.samples.plan(review, epoch);
        review.content_timing.clear();
        for alignment in plan.alignments {
            review.content_timing.insert(
                (alignment.key.report.clone(), alignment.key.pull_id),
                alignment,
            );
        }
        review.marker_fallback.retain(|_, ticket| {
            ticket.key.auth_epoch == epoch
                && ticket.permits_marker_backup()
                && review.content_capability.as_ref().is_some_and(|cap| {
                    review
                        .pulls
                        .iter()
                        .any(|p| ticket.key.matches(&review.replay, p, cap))
                })
        });
    }

    pub(super) fn check_recording_clock_selection(&mut self, _pull: &Pull) {
        self.apply_sample_model();
    }

    pub(super) fn next_content_action(&self) -> Option<Action> {
        let key = self.content.key.as_ref()?;
        if self.content.failure.is_some() || self.content.paused {
            return None;
        }
        let review = self.review.as_ref()?;
        let pull = review
            .pulls
            .iter()
            .find(|p| p.report == key.report && p.id == key.pull_id)?;
        if review.content_alignment(pull).is_some() {
            return None;
        }
        if self.content.next_poll.is_some_and(|at| Instant::now() < at) {
            return None;
        }
        if let Some(ticket) = &self.content.ticket {
            if !ticket.pending() {
                return None;
            }
            if self.content.next_poll.is_some_and(|at| Instant::now() < at) {
                return None;
            }
            return Some(Action::ContentPoll(ticket.clone()));
        }
        Some(Action::ContentSubmit(
            pull.clone(),
            review.replay.clone(),
            key.clone(),
        ))
    }

    pub(super) fn mark_content_started(&mut self, action: &Action) {
        if matches!(action, Action::ContentSubmit(..) | Action::ContentPoll(..)) {
            if let Action::ContentSubmit(_, _, key) = action {
                self.content.samples.planned = Some(key.clone());
            }
            self.content.started.get_or_insert_with(Instant::now);
            self.content.polls = self.content.polls.saturating_add(1);
        }
    }

    pub(super) fn accept_content(&mut self, ticket: Ticket) -> bool {
        if self.content.key.as_ref() != Some(&ticket.key)
            || self
                .recording_auth_epoch()
                .is_some_and(|epoch| epoch != ticket.key.auth_epoch)
            || crate::guild::ensure_current(ticket.key.guild_generation).is_err()
        {
            return false;
        }
        self.content.samples.remember(ticket.clone());
        self.content.failure = None;
        self.content.transport_retries = 0;
        self.content.next_poll = Some(Instant::now() + Duration::from_secs(5));
        self.content.paused = self.content.polls >= MAX_CONTENT_POLLS;
        let alignment = ticket.alignment();
        self.content.ticket = Some(ticket);
        let Some(alignment) = alignment else {
            return self
                .content
                .ticket
                .clone()
                .is_some_and(|ticket| self.accept_marker_backup(ticket));
        };
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let key = (alignment.key.report.clone(), alignment.key.pull_id);
        let Some(pull) = review
            .pulls
            .iter()
            .find(|p| p.report == key.0 && p.id == key.1)
            .cloned()
        else {
            return false;
        };
        if review
            .content_capability
            .as_ref()
            .is_none_or(|cap| !alignment.matches(&review.replay, &pull, cap))
        {
            return false;
        }
        review.content_timing.insert(key, alignment);
        // Only a still-waiting explicit selection starts playback. Background
        // completion never shifts a currently watched clock or a different fight.
        if self.active
            && self.playback.is_none()
            && self
                .pull
                .as_ref()
                .is_some_and(|p| pull_key(p) == pull_key(&pull))
        {
            self.select(pull);
            return true;
        }
        false
    }

    fn accept_marker_backup(&mut self, ticket: Ticket) -> bool {
        if !ticket.permits_marker_backup()
            || self.content.key.as_ref() != Some(&ticket.key)
            || self
                .recording_auth_epoch()
                .is_some_and(|epoch| epoch != ticket.key.auth_epoch)
            || crate::guild::ensure_current(ticket.key.guild_generation).is_err()
        {
            return false;
        }
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let Some(pull) = review
            .pulls
            .iter()
            .find(|pull| {
                pull.report == ticket.key.report
                    && pull.id == ticket.key.pull_id
                    && review
                        .content_capability
                        .as_ref()
                        .is_some_and(|cap| ticket.key.matches(&review.replay, pull, cap))
            })
            .cloned()
        else {
            return false;
        };
        if review.replay.start_ms().is_err() || review.content_alignment(&pull).is_some() {
            return false;
        }
        let key = (pull.report.clone(), pull.id);
        if review.marker_fallback.insert(key, ticket).is_none() {
            self.last_attempt = None;
        }
        if self.active
            && self.playback.is_none()
            && review.marker_alignment(&pull).is_some()
            && self
                .pull
                .as_ref()
                .is_some_and(|p| pull_key(p) == pull_key(&pull))
        {
            self.select(pull);
            return true;
        }
        false
    }

    pub(super) fn content_failed(&mut self, message: String) {
        if message == crate::content_alignment::TEMPORARY {
            self.content.transport_retries = self.content.transport_retries.saturating_add(1);
            let seconds = (5u64 << self.content.transport_retries.saturating_sub(1).min(4)).min(60);
            self.content.next_poll = Some(Instant::now() + Duration::from_secs(seconds));
            self.content.paused = self.content.polls >= MAX_CONTENT_POLLS;
            return;
        }
        self.content.failure = Some(message.chars().take(512).collect());
    }

    pub(super) fn preparation_failed(&self) -> bool {
        self.notice.is_some() || self.content.failure.is_some()
    }

    pub(super) fn content_poll_pending(&self) -> bool {
        !self.content.paused
            && self.content.failure.is_none()
            && (self.content.next_poll.is_some()
                || self.content.ticket.as_ref().is_some_and(|t| t.pending()))
    }

    pub(super) fn content_waiting(&self) -> bool {
        self.review
            .as_ref()
            .zip(self.pull.as_ref())
            .is_some_and(|(review, pull)| {
                review.content_required() && !review.has_precise_timing(pull)
            })
    }

    pub(super) fn draw_content_controls(&mut self, ui: &mut egui::Ui, stream: &Stream) {
        if !self.active || !self.connected {
            return;
        }
        let mut load = None;
        if self
            .review
            .as_ref()
            .is_none_or(|review| review.pulls.is_empty())
        {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Warcraft Logs report").small().color(MUTED));
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.content.report_input)
                        .hint_text("Report link or code")
                        .char_limit(512)
                        .desired_width(250.0),
                );
                if ui
                    .add_enabled(self.work.is_none(), egui::Button::new("Load report"))
                    .clicked()
                    || (edit.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && self.work.is_none())
                {
                    match report_code_input(&self.content.report_input) {
                        Some(code) => load = Some(code),
                        None => {
                            self.content.failure = Some(
                                "Enter a Warcraft Logs report link or its 16-character code."
                                    .into(),
                            )
                        }
                    }
                }
            });
        }
        if let Some(code) = load {
            self.cancel_read();
            self.content.manual_report = Some(code.clone());
            self.content.key = None;
            self.content.ticket = None;
            self.content.failure = None;
            self.pull = None;
            self.playback = None;
            self.events.clear();
            self.requested_events.clear();
            self.loaded_events.clear();
            self.open_first_pull = false;
            self.start(ui.ctx(), Some(stream), Action::Report(code));
        }
    }
}

fn report_code_input(input: &str) -> Option<String> {
    let input = input.trim();
    let valid = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if valid(input) {
        return Some(input.into());
    }
    let url = url::Url::parse(input).ok()?;
    if url.scheme() != "https"
        || ![Some("www.warcraftlogs.com"), Some("warcraftlogs.com")].contains(&url.host_str())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let code = path.strip_prefix("/reports/")?;
    valid(code).then(|| code.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn viewer() -> (ReviewUi, Ticket) {
        let (replay, pull, cap, ticket) = crate::content_alignment::test_ticket();
        let mut ui = ReviewUi::default();
        ui.active = true;
        ui.connected = true;
        ui.connection_checked = true;
        ui.review = Some(Review {
            replay,
            pulls: vec![pull.clone()],
            marker_timing: Default::default(),
            marker_fallback: Default::default(),
            content_capability: Some(cap),
            content_timing: Default::default(),
        });
        ui.recording_match_status = Some((0, false));
        ui.select(pull);
        ui.sync_content_selection();
        (ui, ticket)
    }
    #[test]
    fn two_authenticated_samples_cover_later_pulls_without_repeated_jobs() {
        let (mut ui, ticket) = viewer();
        let mut later = ui.pull.clone().unwrap();
        later.id += 1;
        later.start_ms += 240000;
        later.end_ms += 240000;
        ui.review.as_mut().unwrap().pulls.push(later.clone());
        ui.accept_content(ticket);
        ui.sync_content_selection();
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        ui.content.samples =
            crate::content_alignment::sampling::test_samples(ui.review.as_ref().unwrap(), 0);
        ui.sync_content_selection();
        ui.select(later);
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 255.25);
        assert!(ui.next_content_action().is_none());
    }
    #[test]
    fn another_viewers_pending_sample_backs_off_without_interrupting_playback() {
        let (mut ui, _) = viewer();
        let action = ui.next_content_action().unwrap();
        ui.mark_content_started(&action);
        let planned = ui.content.samples.planned.clone();
        ui.content_failed(crate::content_alignment::TEMPORARY.into());
        ui.sync_content_selection();
        assert!(ui.playback.is_some());
        assert!(ui.content_poll_pending());
        assert!(ui.next_content_action().is_none());
        assert_eq!(ui.content.samples.planned, planned);
        ui.content.next_poll = Some(Instant::now() - Duration::from_secs(1));
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
    }
    #[test]
    fn estimated_playback_gets_events_before_a_new_signature_export() {
        let (mut ui, _) = viewer();
        assert!(matches!(ui.next_action(), Some(Action::Events(..))));
        ui.loaded_events = vec![EventKind::Deaths, EventKind::Defensives];
        assert!(matches!(ui.next_action(), Some(Action::ContentSubmit(..))));
    }
    #[test]
    fn transient_poll_failure_keeps_playback_and_retries_the_same_job() {
        let (mut ui, mut ticket) = viewer();
        ticket.job.status = Status::Running;
        ticket.job.result = None;
        ui.accept_content(ticket.clone());
        ui.content_failed(crate::content_alignment::TEMPORARY.into());
        assert!(ui.playback.is_some());
        assert!(ui.content.failure.is_none());
        assert!(ui.next_content_action().is_none());
        ui.content.next_poll = Some(Instant::now() - Duration::from_secs(1));
        assert!(
            matches!(ui.next_content_action(), Some(Action::ContentPoll(next)) if next.job.id == ticket.job.id)
        );
    }
    #[test]
    fn background_preparation_selects_one_anchor_without_a_player() {
        let (ui, _) = viewer();
        let mut peer = ui.preparation_peer();
        peer.review = ui.review.clone();
        let mut later = peer.review.as_ref().unwrap().pulls[0].clone();
        later.id += 1;
        later.start_ms += 60_000;
        later.end_ms += 60_000;
        peer.review.as_mut().unwrap().pulls.push(later);
        peer.alignment_priority = None;
        peer.prepare_alignment(false);
        peer.sync_content_selection();
        assert!(peer.next_content_action().is_none());
        peer.prepare_alignment(true);
        peer.sync_content_selection();
        let anchor = peer.content.key.clone();
        assert!(matches!(
            peer.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        peer.sync_content_selection();
        assert_eq!(peer.content.key, anchor);
        assert!(peer.playback.is_none());
    }
    #[test]
    fn changing_account_cannot_publish_a_sample() {
        let (mut ui, ticket) = viewer();
        ui.recording_match_status = Some((1, false));
        ui.sync_content_selection();
        assert!(!ui.accept_content(ticket));
        assert!(ui.content.samples.tickets.is_empty());
    }
    #[test]
    fn selection_plays_estimate_then_uses_verified_timing_on_the_next_seek() {
        let (mut ui, ticket) = viewer();
        assert!(ui.playback.is_some());
        assert!(ui.content_waiting());
        assert!(!ui.marker_sync.busy());
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        assert!(!ui.accept_content(ticket));
        ui.select(ui.pull.clone().unwrap());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        assert!(ui.playback.as_ref().unwrap().content_timing);
        assert!(ui.review.as_ref().unwrap().marker_timing.is_empty());
        assert!(
            matches!(ui.seek_absolute(ui.pull.as_ref().unwrap().start_ms+11_125),Some(PlaybackCommand::Seek(seconds)) if seconds==26.375)
        );
    }
    #[test]
    fn estimated_playback_stops_at_the_displayed_pull_end_without_a_result() {
        let (mut ui, _) = viewer();
        let (_, end) = ui.active_playback_range().unwrap();
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = end;
        state.mark_polled_now();
        assert!(matches!(
            ui.pause_at_pull_end(&state),
            Some(PlaybackCommand::Pause)
        ));
        assert!(ui.pause_at_pull_end(&state).is_none());
    }
    #[test]
    fn provider_seek_pins_its_new_clock_before_a_background_result_arrives() {
        let (mut ui, ticket) = viewer();
        let origin = ui.active_playback_range().unwrap().0;
        let epoch = Instant::now() - Duration::from_millis(100);
        ui.range_epoch = epoch;
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.seconds = origin + 10.0;
        state.mark_polled_at(epoch + Duration::from_millis(10));
        ui.observe_provider_playback(&state);
        state.seconds = origin + 30.0;
        state.mark_polled_at(epoch + Duration::from_millis(20));
        ui.observe_provider_playback(&state);
        assert_eq!(ui.provider_observation.unwrap().1, origin + 30.0);
        assert_eq!(ui.active_playback_range().unwrap().0, origin);
        ui.accept_content(ticket);
        assert_eq!(ui.active_playback_range().unwrap().0, origin);
        assert_eq!(
            ui.comparison_position(&state).unwrap().0,
            ui.pull.as_ref().unwrap().start_ms + 30_000
        );
    }
    #[test]
    fn comparison_clock_adoption_and_exit_use_the_clock_that_comparison_seeks() {
        for playing in [false, true] {
            let (mut ui, ticket) = viewer();
            let pull = ui.pull.clone().unwrap();
            let at_ms = pull.start_ms + 30_000;
            ui.set_comparing(true);
            let mut secondary = ui.review.clone().unwrap();
            secondary.replay.video_id = "different12".into();
            secondary.replay.broadcast_id = "different12".into();
            let stream = Stream {
                replay_start_ms: None,
                replay_end_ms: None,
                recording_id: Some(secondary.replay.video_id.clone()),
                user_id: "202".into(),
                raid_role: None,
                name: "Second POV".into(),
                provider: secondary.replay.provider.clone(),
                channel_id: secondary.replay.video_id.clone(),
                url: secondary.replay.public_url(0),
                status: crate::streams::Status::Offline,
                broadcast_state: None,
            };
            let mut comparison =
                crate::review_compare_ui::Comparison::new(&ui, stream, at_ms, playing);
            comparison.metadata_for_test().review = Some(secondary);
            let mut now = Instant::now() - Duration::from_millis(100);
            ui.range_epoch = now;
            let mut next = || {
                now += Duration::from_millis(1);
                now
            };
            let sample = |seconds, playing, at| {
                let mut state = PlaybackState::default();
                state.ready = true;
                state.seconds = seconds;
                state.playing = playing;
                state.mark_polled_at(at);
                state
            };
            let initial = ui.active_playback_range().unwrap().0 + 30.0;
            let empty = PlaybackState::default();
            comparison
                .refresh_controller_for_test(&ui, next())
                .tick([&empty, &empty], next());
            let ready = sample(initial, false, next());
            comparison
                .refresh_controller_for_test(&ui, next())
                .tick([&ready, &ready], next());
            if playing {
                let running = sample(initial, true, next());
                comparison
                    .refresh_controller_for_test(&ui, next())
                    .tick([&running, &running], next());
            }

            // A callback started before the new clock cannot acknowledge its
            // seek, even when it happens to report the corrected position.
            let stale = [sample(45.25, false, next()), sample(initial, false, next())];
            ui.accept_content(ticket);
            let controller = comparison.refresh_controller_for_test(&ui, next());
            assert_eq!(controller.position_ms(), at_ms);
            assert_eq!(controller.wants_playing(), playing);
            let commands = controller.tick([&stale[0], &stale[1]], next());
            assert!(matches!(
                commands.primary,
                Some(PlaybackCommand::SeekPaused(45.25))
            ));
            assert!(
                matches!(commands.secondary, Some(PlaybackCommand::SeekPaused(s)) if s == initial)
            );
            let commands = controller.tick([&stale[0], &stale[1]], next());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_eq!(
                controller.status(),
                crate::review_compare::Status::Preparing
            );

            let fresh = [sample(45.25, false, next()), sample(initial, false, next())];
            let commands = controller.tick([&fresh[0], &fresh[1]], next());
            assert_eq!(
                matches!(commands.primary, Some(PlaybackCommand::Play)),
                playing
            );
            assert_eq!(
                matches!(commands.secondary, Some(PlaybackCommand::Play)),
                playing
            );
            assert_eq!(controller.position_ms(), at_ms);
            assert_eq!(ui.comparison_position(&fresh[0]), Some((at_ms, false)));

            ui.set_comparing(false);
            assert_eq!(ui.active_playback_range().unwrap(), (15.25, 195.25));
            assert_eq!(ui.comparison_position(&fresh[0]), Some((at_ms, false)));
            let end = sample(195.25, true, next());
            assert!(matches!(
                ui.pause_at_pull_end(&end),
                Some(PlaybackCommand::Pause)
            ));
            assert!(ui.pause_at_pull_end(&end).is_none());
        }
    }
    #[test]
    fn timeline_label_stays_inside_pull_bounds_while_provider_settles() {
        let (mut ui, _) = viewer();
        let (start, end) = ui.active_playback_range().unwrap();
        let ctx = egui::Context::default();
        for (seconds, label) in [(start - 2.0, "0:00 / 3:00"), (end + 2.0, "3:00 / 3:00")] {
            let mut state = PlaybackState::default();
            state.ready = true;
            state.seconds = seconds;
            state.mark_polled_now();
            let output = ctx.run_ui(Default::default(), |egui| {
                ui.draw_timeline(egui, &state);
            });
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == label)),
                "The timer and slider must share the selected pull's bounds"
            );
        }
    }
    #[test]
    fn new_alignment_keeps_the_watched_timeline_at_zero_until_explicit_navigation() {
        let (mut ui, ticket) = viewer();
        let original = ui.playback.as_ref().unwrap().seconds;
        let before = ui.active_playback_range().unwrap();
        assert_eq!(original - before.0, 0.0);
        assert!(!ui.accept_content(ticket));
        assert_eq!(ui.active_playback_range().unwrap(), before);
        assert_eq!(
            ui.playback.as_ref().unwrap().seconds - ui.active_playback_range().unwrap().0,
            0.0
        );
        assert_eq!(
            ui.comparison_position(&PlaybackState::default()).unwrap().0,
            ui.pull.as_ref().unwrap().start_ms
        );
        let pull = ui.pull.clone().unwrap();
        ui.select(pull.clone());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        assert_eq!(
            ui.playback.as_ref().unwrap().seconds - ui.active_playback_range().unwrap().0,
            0.0
        );
        ui.seek_absolute(pull.start_ms + 30_000).unwrap();
        assert_eq!(
            ui.playback.as_ref().unwrap().seconds - ui.active_playback_range().unwrap().0,
            30.0
        );
    }
    #[test]
    fn changed_catalogue_media_or_wcl_account_cannot_publish_a_completed_job() {
        for change in 0..3 {
            let (mut ui, ticket) = viewer();
            match change {
                0 => {
                    let mut other = ui.pull.clone().unwrap();
                    other.id += 1;
                    ui.review.as_mut().unwrap().pulls = vec![other.clone()];
                    ui.select(other);
                }
                1 => ui.review.as_mut().unwrap().replay.timeline_revision = Some("0".repeat(64)),
                _ => ui.recording_match_status = Some((1, false)),
            }
            ui.sync_content_selection();
            assert!(!ui.accept_content(ticket));
            assert!(ui.playback.is_some());
            assert!(ui.review.as_ref().unwrap().content_timing.is_empty());
        }
    }
    #[test]
    fn active_sample_poll_survives_selection_changes_without_moving_the_new_pull() {
        let (mut ui, ticket) = viewer();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.work_action = Some(Action::ContentPoll(ticket.clone()));
        ui.cancel = Arc::new(AtomicBool::new(false));
        let old = ui.generation;
        let mut other = ui.pull.clone().unwrap();
        other.id += 1;
        ui.review.as_mut().unwrap().pulls.push(other.clone());
        ui.select(other);
        ui.sync_content_selection();
        assert!(!ui.cancel.load(Ordering::Relaxed));
        assert_eq!(ui.generation, old);
        let position = ui.playback.as_ref().unwrap().seconds;
        assert!(!ui.accept_content(ticket));
        assert_eq!(ui.playback.as_ref().unwrap().seconds, position);
        drop(tx);
    }
    #[test]
    fn reopening_pending_scope_reuses_job_and_polling_has_a_bound() {
        let (mut ui, mut ticket) = viewer();
        ticket.job.status = Status::Pending;
        ticket.job.result = None;
        assert!(!ui.accept_content(ticket.clone()));
        let first = ui.pull.clone().unwrap();
        let mut other = first.clone();
        other.id += 1;
        ui.review.as_mut().unwrap().pulls.push(other.clone());
        ui.select(other);
        ui.sync_content_selection();
        ui.select(first);
        ui.sync_content_selection();
        ui.content.next_poll = Some(Instant::now() - Duration::from_secs(1));
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentPoll(_))
        ));
        ui.content.polls = MAX_CONTENT_POLLS;
        ui.accept_content(ticket);
        assert!(ui.content.paused);
        assert!(ui.next_content_action().is_none());
    }
    #[test]
    fn terminal_job_and_closed_view_do_not_restart_analysis_or_seek() {
        let (mut ui, mut ticket) = viewer();
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ticket.job.error = Some("budget_exhausted".into());
        ui.accept_content(ticket);
        assert!(ui.next_content_action().is_none());
        assert!(ui.playback.is_some());
        let (_, good) = viewer();
        ui.close_review();
        ui.sync_content_selection();
        assert!(!ui.accept_content(good));
        assert!(ui.playback.is_none());
    }
    #[test]
    fn content_result_never_repositions_an_already_watched_video() {
        let (mut ui, ticket) = viewer();
        assert!(!ui.accept_content(ticket.clone()));
        ui.playback.as_mut().unwrap().seconds = 50.0;
        assert!(!ui.accept_content(ticket));
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 50.0);
    }
    #[test]
    fn unavailable_capability_keeps_upload_watchable_without_certifying_an_origin() {
        let (mut ui, _) = viewer();
        ui.review.as_mut().unwrap().content_capability = None;
        ui.sync_content_selection();
        assert!(ui.next_content_action().is_none());
        assert!(ui.playback.is_some());
        assert!(ui
            .review
            .as_ref()
            .unwrap()
            .pull_video_start(ui.pull.as_ref().unwrap())
            .is_finite());
        assert!(!ui
            .review
            .as_ref()
            .unwrap()
            .has_precise_timing(ui.pull.as_ref().unwrap()));
    }
    #[test]
    fn completed_ticket_restores_after_metadata_replacement_without_resubmission() {
        let (mut ui, ticket) = viewer();
        assert!(!ui.accept_content(ticket));
        ui.select(ui.pull.clone().unwrap());
        ui.review.as_mut().unwrap().content_timing.clear();
        ui.playback = None;
        assert!(ui.sync_content_selection());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        assert!(ui.next_content_action().is_none());
    }
    #[test]
    fn only_expired_tickets_allow_a_new_submission() {
        let (mut ui, mut ticket) = viewer();
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ticket.job.error = Some("budget_exhausted".into());
        ui.accept_content(ticket.clone());
        ui.sync_content_selection();
        assert!(ui.next_content_action().is_none());
        ui.content.samples.tickets[0].job.expires_at = 1;
        ui.prepared.lock().unwrap().set_connected(false);
        ui.sync_content_selection();
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        assert!(ui.playback.is_some());
    }
    fn backup_viewer() -> (ReviewUi, Ticket) {
        let (mut ui, mut ticket) = viewer();
        let review = ui.review.as_mut().unwrap();
        review.replay.started_at = "2023-11-14T22:13:20Z".into();
        ticket
            .job
            .scope
            .as_mut()
            .unwrap()
            .timeline
            .raw_started_at_ms = review.replay.start_ms().ok();
        ticket.key = Key::new(
            &review.replay,
            &review.pulls[0],
            review.content_capability.as_ref().unwrap(),
            0,
        );
        ui.sync_content_selection();
        (ui, ticket)
    }
    fn marker(ui: &mut ReviewUi) {
        let pull = ui.pull.as_ref().unwrap();
        ui.review.as_mut().unwrap().marker_timing.insert(
            (pull.report.clone(), pull.id),
            crate::replay_sync::Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: 18.125,
                uncertainty_seconds: 0.1,
            },
        );
    }
    #[test]
    fn live_match_wins_even_when_a_unix_backup_is_cached() {
        let (mut ui, ticket) = backup_viewer();
        marker(&mut ui);
        ui.select(ui.pull.clone().unwrap());
        assert!(ui.playback.is_some());
        assert!(ui
            .review
            .as_ref()
            .unwrap()
            .pull_video_start(ui.pull.as_ref().unwrap())
            .is_finite());
        assert!(!ui
            .review
            .as_ref()
            .unwrap()
            .has_precise_timing(ui.pull.as_ref().unwrap()));
        assert!(!ui.accept_content(ticket));
        ui.select(ui.pull.clone().unwrap());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        assert!(ui.playback.as_ref().unwrap().content_timing);
    }
    #[test]
    fn only_terminal_failure_enables_verified_marker_backup() {
        for status in [
            Status::Pending,
            Status::Running,
            Status::CleanupPending,
            Status::Canceled,
            Status::Failed,
        ] {
            let (mut ui, mut ticket) = backup_viewer();
            marker(&mut ui);
            ticket.job.status = status;
            ticket.job.result = None;
            let previous = ui.playback.as_ref().unwrap().seconds;
            assert!(!ui.accept_content(ticket));
            assert_eq!(ui.playback.as_ref().unwrap().seconds, previous);
            assert_eq!(
                ui.review
                    .as_ref()
                    .unwrap()
                    .marker_backup_allowed(ui.pull.as_ref().unwrap()),
                status == Status::Failed
            );
            ui.select(ui.pull.clone().unwrap());
            if status == Status::Failed {
                let playback = ui.playback.as_ref().unwrap();
                assert_eq!(playback.seconds, 18.125);
                assert!(!playback.content_timing);
                assert!(!ui.content_waiting());
                assert!(matches!(
                    ui.seek_absolute(ui.pull.as_ref().unwrap().start_ms + 11_125),
                    Some(PlaybackCommand::Seek(29.25))
                ));
            }
        }
    }
    #[test]
    fn failed_live_match_remains_watchable_at_an_estimate_until_verified_timing_arrives() {
        let (mut ui, mut ticket) = backup_viewer();
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ui.accept_content(ticket);
        ui.select(ui.pull.clone().unwrap());
        let review = ui.review.as_ref().unwrap();
        let pull = ui.pull.as_ref().unwrap();
        assert!(review.marker_backup_allowed(pull));
        assert!(!review.has_precise_timing(pull));
        assert_eq!(
            review.pull_video_start(pull),
            review.estimated_video_start(pull)
        );
        assert!(review.video_seconds(pull, 0.0).is_some());
        assert!(crate::review_compare_ui::recording_clock(review, pull).is_ok());
        assert!(ui.playback.is_some());
        // Background precision does not interrupt playback; the next selection uses it.
        let mut refreshed = ui.review.clone().unwrap();
        marker(&mut ui);
        refreshed.marker_timing = ui.review.as_ref().unwrap().marker_timing.clone();
        ui.accept_review(refreshed);
        assert!(!ui.sync_content_selection());
        ui.select(ui.pull.clone().unwrap());
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 18.125);
    }
    #[test]
    fn marker_backup_permission_does_not_cross_pull_media_account_or_expiry() {
        for change in 0..5 {
            let (mut ui, mut ticket) = backup_viewer();
            ticket.job.status = Status::Failed;
            ticket.job.result = None;
            ui.accept_content(ticket);
            match change {
                0 => ui.review.as_mut().unwrap().replay.timeline_revision = Some("0".repeat(64)),
                1 => ui.review.as_mut().unwrap().pulls[0].start_ms += 1,
                2 => ui.recording_match_status = Some((1, false)),
                3 => {
                    for ticket in ui.review.as_mut().unwrap().marker_fallback.values_mut() {
                        ticket.job.expires_at = 1;
                    }
                    ui.content.ticket.as_mut().unwrap().job.expires_at = 1;
                }
                _ => {
                    ui.review
                        .as_mut()
                        .unwrap()
                        .content_capability
                        .as_mut()
                        .unwrap()
                        .algorithm_revision = "0".repeat(64)
                }
            }
            ui.sync_content_selection();
            let review = ui.review.as_ref().unwrap();
            assert!(!review.marker_backup_allowed(&review.pulls[0]));
            assert!(ui.playback.is_some());
        }
    }
    #[test]
    fn request_errors_and_missing_provider_origin_do_not_authorize_backup() {
        let (mut ui, _) = backup_viewer();
        marker(&mut ui);
        ui.content_failed("Temporary service error".into());
        assert!(ui.review.as_ref().unwrap().marker_fallback.is_empty());
        assert!(ui.playback.is_some());
        let (mut ui, mut ticket) = viewer();
        marker(&mut ui);
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ui.accept_content(ticket);
        assert!(ui.review.as_ref().unwrap().marker_fallback.is_empty());
        assert!(ui.playback.is_some());
    }

    #[test]
    fn report_import_only_accepts_codes_or_official_https_report_links() {
        for input in [
            "AbCdEfGhIjKlMnOp",
            " https://www.warcraftlogs.com/reports/AbCdEfGhIjKlMnOp#fight=21 ",
            "https://warcraftlogs.com/reports/AbCdEfGhIjKlMnOp/",
        ] {
            assert_eq!(
                report_code_input(input).as_deref(),
                Some("AbCdEfGhIjKlMnOp")
            );
        }
        for input in [
            "http://warcraftlogs.com/reports/AbCdEfGhIjKlMnOp",
            "https://warcraftlogs.com.evil.test/reports/AbCdEfGhIjKlMnOp",
            "https://user:password@warcraftlogs.com/reports/AbCdEfGhIjKlMnOp",
            "https://warcraftlogs.com/reports/AbCdEfGhIjKlMnOp/extra",
            "short",
        ] {
            assert!(report_code_input(input).is_none());
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
#[path = "content_review_smoke.rs"]
mod native_smoke;
