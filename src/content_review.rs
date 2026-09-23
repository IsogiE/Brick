//! Viewer state for one scoped, asynchronous content alignment request.
use super::*;
use crate::content_alignment::{Key, RecordingClock, RecordingLookup, Status, Ticket};

// Keep following long-running jobs on the small production worker.
const MAX_CONTENT_POLLS: u16 = 1440;

#[derive(Default)]
pub(super) struct State {
    pub report_input: String,
    pub manual_report: Option<String>,
    key: Option<Key>,
    ticket: Option<Ticket>,
    saved: Vec<Ticket>,
    clocks: Vec<(Key, RecordingClock, Instant)>,
    shared_pending: bool,
    started: Option<Instant>,
    next_poll: Option<Instant>,
    polls: u16,
    failure: Option<String>,
    paused: bool,
}

impl State {
    #[cfg(test)]
    pub(super) fn prepared_clocks(&self) -> Vec<(Key, RecordingClock)> {
        self.clocks
            .iter()
            .map(|(key, clock, _)| (key.clone(), clock.clone()))
            .collect()
    }
    pub(super) fn seed_clocks(&mut self, clocks: Vec<(Key, RecordingClock)>, at: Instant) {
        for (key, clock) in clocks {
            self.clocks
                .retain(|(saved, _, _)| !saved.same_recording_report(&key));
            if self.clocks.len() >= 32 {
                self.clocks.remove(0);
            }
            self.clocks.push((key, clock, at));
        }
    }
}

impl ReviewUi {
    pub(super) fn sync_content_selection(&mut self) -> bool {
        self.content.saved.retain(|ticket| !ticket.expired());
        if self.content.ticket.as_ref().is_some_and(Ticket::expired) {
            self.content.ticket = None;
            self.content.failure = None;
            self.content.paused = false;
            self.content.polls = 0;
            self.content.started = None;
            self.content.next_poll = None;
        }
        if let Some(epoch) = self.recording_auth_epoch() {
            if self
                .recording_match_status
                .is_some_and(|status| status.0 != epoch)
            {
                if let Some(review) = self.review.as_mut() {
                    review.content_timing.clear();
                    review.marker_fallback.clear();
                }
                self.recording_match_status = Some((epoch, false));
            }
        }
        if let Some(review) = self.review.as_mut() {
            review.marker_fallback.retain(|_, ticket| {
                self.recording_match_status
                    .is_some_and(|status| status.0 == ticket.key.auth_epoch)
                    && ticket.permits_marker_backup()
            });
        }
        let desired = self.review.as_ref().and_then(|review| {
            if self.recording_housekeeping || (!self.active && !self.metadata_only) {
                return None;
            }
            let capability = review.content_capability.as_ref()?;
            let wanted = self.preferred_alignment_pull()?;
            let pull = review.matching_pull(wanted)?;
            Some(Key::new(
                &review.replay,
                pull,
                capability,
                self.recording_match_status.map_or(0, |s| s.0),
            ))
        });
        if self.content.key == desired {
            return self.restore_recording_clock() || self.restore_content_ticket();
        }
        if matches!(
            self.work_action,
            Some(Action::ContentSubmit(..) | Action::ContentPoll(..))
        ) {
            self.cancel_read();
        }
        if let Some(ticket) = self.content.ticket.take() {
            self.content.saved.retain(|saved| {
                saved.key != ticket.key && saved.key.guild_generation == crate::guild::generation()
            });
            if self.content.saved.len() >= 8 {
                self.content.saved.remove(0);
            }
            self.content.saved.push(ticket);
        }
        if let Some(key) = &desired {
            self.expire_recording_clock(key);
        }
        self.content.shared_pending = false;
        self.content.key = desired.clone();
        self.content.ticket = desired
            .as_ref()
            .and_then(|key| self.content.saved.iter().find(|t| &t.key == key).cloned());
        self.content.started = None;
        self.content.next_poll = None;
        self.content.polls = 0;
        self.content.failure = None;
        self.content.paused = false;
        self.restore_recording_clock() || self.restore_content_ticket()
    }

    fn expire_recording_clock(&mut self, key: &Key) {
        let stale = self.content.clocks.iter().any(|(saved, _, checked)| {
            saved.same_recording_report(key) && checked.elapsed() >= Duration::from_secs(60)
        });
        if stale {
            self.content
                .clocks
                .retain(|(saved, _, _)| !saved.same_recording_report(key));
            if let Some(review) = self.review.as_mut() {
                review.content_timing.retain(|_, alignment| {
                    !alignment.key.same_recording_report(key)
                        || alignment.signature_revision != alignment.result.evidence_hash
                });
            }
        }
    }

    pub(super) fn check_recording_clock_selection(&mut self, pull: &Pull) {
        let key = self.review.as_ref().and_then(|review| {
            review.content_capability.as_ref().map(|cap| {
                Key::new(
                    &review.replay,
                    pull,
                    cap,
                    self.recording_match_status.map_or(0, |s| s.0),
                )
            })
        });
        if let Some(key) = key {
            self.expire_recording_clock(&key);
        }
    }

    fn restore_recording_clock(&mut self) -> bool {
        let Some(key) = self.content.key.clone() else {
            return false;
        };
        if self.review.as_ref().is_some_and(|review| {
            review.pulls.iter().any(|pull| {
                pull.report == key.report
                    && pull.id == key.pull_id
                    && review.content_alignment(pull).is_some()
            })
        }) {
            return false;
        }
        let clock = self
            .content
            .clocks
            .iter()
            .find(|(saved, clock, _)| {
                saved.same_recording_report(&key) && clock.alignment(&key).is_some()
            })
            .map(|(_, clock, _)| clock.clone());
        let Some(clock) = clock else {
            return false;
        };
        self.apply_recording_clock(&key, &clock)
    }

    fn apply_recording_clock(&mut self, key: &Key, clock: &RecordingClock) -> bool {
        if self.content.key.as_ref() != Some(key)
            || self
                .recording_match_status
                .is_some_and(|status| status.0 != key.auth_epoch)
            || self
                .recording_auth_epoch()
                .is_some_and(|epoch| epoch != key.auth_epoch)
            || crate::guild::ensure_current(key.guild_generation).is_err()
        {
            return false;
        }
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let Some(cap) = review.content_capability.as_ref() else {
            return false;
        };
        for pull in &review.pulls {
            let wanted = Key::new(&review.replay, pull, cap, key.auth_epoch);
            if wanted.same_recording_report(key) && review.content_alignment(pull).is_none() {
                if let Some(alignment) = clock.alignment(&wanted) {
                    review
                        .content_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                }
            }
        }
        if self.active && self.playback.is_none() {
            if let Some(pull) = self
                .pull
                .clone()
                .filter(|pull| review.content_alignment(pull).is_some())
            {
                self.select(pull);
                return true;
            }
        }
        false
    }

    pub(super) fn accept_recording_clock(&mut self, key: Key, lookup: RecordingLookup) -> bool {
        if self.content.key.as_ref() != Some(&key)
            || self
                .recording_match_status
                .is_some_and(|status| status.0 != key.auth_epoch)
            || self
                .recording_auth_epoch()
                .is_some_and(|epoch| epoch != key.auth_epoch)
            || crate::guild::ensure_current(key.guild_generation).is_err()
        {
            return false;
        }
        self.content.failure = None;
        self.content.shared_pending = lookup.pending;
        self.content.next_poll = Some(Instant::now() + Duration::from_secs(5));
        self.content.paused = self.content.polls >= MAX_CONTENT_POLLS;
        let Some(clock) = lookup.clock else {
            return false;
        };
        self.content.shared_pending = false;
        self.content.clocks.retain(|(saved, _, _)| {
            !saved.same_recording_report(&key)
                && saved.guild_generation == key.guild_generation
                && saved.auth_epoch == key.auth_epoch
        });
        if self.content.clocks.len() >= 32 {
            self.content.clocks.remove(0);
        }
        self.content
            .clocks
            .push((key.clone(), clock.clone(), Instant::now()));
        if let Ok(mut cache) = self.prepared.lock() {
            cache.record_clock(&key, &clock);
        }
        self.apply_recording_clock(&key, &clock)
    }

    fn restore_content_ticket(&mut self) -> bool {
        let Some(ticket) = self.content.ticket.as_ref() else {
            return false;
        };
        if ticket.job.status == Status::Failed {
            return self.accept_marker_backup(ticket.clone());
        }
        if ticket.alignment().is_none()
            || self.review.as_ref().is_some_and(|review| {
                review.pulls.iter().any(|pull| {
                    pull.report == ticket.key.report
                        && pull.id == ticket.key.pull_id
                        && review.content_alignment(pull).is_some()
                })
            })
        {
            return false;
        }
        self.accept_content(ticket.clone())
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
        self.content.failure = None;
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
        self.content.failure = Some(message.chars().take(512).collect());
    }

    pub(super) fn content_poll_pending(&self) -> bool {
        !self.content.paused
            && self.content.failure.is_none()
            && (self.content.shared_pending
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
        let content_mode = self
            .review
            .as_ref()
            .is_some_and(|review| review.content_required());
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
        if !content_mode && self.content.failure.is_none() {
            return;
        }
        let mut retry = false;
        ui.horizontal_wrapped(|ui| {
            let (label, retryable) = if let Some(failure) = self.content.failure.as_deref() {
                (failure, true)
            } else if self
                .review
                .as_ref()
                .is_none_or(|r| r.content_capability.is_none())
            {
                ("Video alignment is unavailable on this server.", false)
            } else if self
                .review
                .as_ref()
                .zip(self.pull.as_ref())
                .is_some_and(|(review, pull)| review.marker_backup_allowed(pull))
            {
                if self
                    .review
                    .as_ref()
                    .zip(self.pull.as_ref())
                    .is_some_and(|(review, pull)| review.marker_alignment(pull).is_some())
                {
                    ("Video aligned using the backup timing.", false)
                } else {
                    ("Checking backup video timing…", false)
                }
            } else if self.review.as_ref().zip(self.pull.as_ref()).is_some_and(|(review,pull)| review.content_alignment(pull).is_some()) {
                ("Video aligned", false)
            } else if self.content.paused {
                (
                    "This video is still processing. Check again when ready.",
                    true,
                )
            } else if self.content.shared_pending {
                ("This recording is being aligned. You can browse other recordings while it finishes.", false)
            } else if let Some(ticket) = &self.content.ticket {
                match ticket.job.status {
                    Status::Complete if ticket.alignment().is_some() => ("Video aligned", false),
                    Status::Complete => (
                        "This video alignment has expired. Reload the report.",
                        false,
                    ),
                    Status::Failed | Status::Canceled => (
                        "This video could not be aligned. Choose another recording or report.",
                        false,
                    ),
                    Status::Pending => ("Waiting for video alignment…", false),
                    _ => ("Aligning this recording for everyone. You can browse other recordings while it finishes.", false),
                }
            } else if self.content.key.is_some() {
                ("Preparing video alignment…", false)
            } else {
                ("Choose a pull to align this video.", false)
            };
            ui.add(egui::Label::new(RichText::new(label).small().color(MUTED)).truncate());
            if retryable
                && ui
                    .add_enabled(self.work.is_none(), egui::Button::new("Check again"))
                    .clicked()
            {
                retry = true;
            }
        });
        if retry {
            self.content.failure = None;
            self.content.paused = false;
            self.content.polls = 0;
            self.content.next_poll = None;
            self.content.started = None;
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
    fn shared_recording_clock_unlocks_all_linked_pulls_without_another_submission() {
        let (mut ui, ticket) = viewer();
        let mut later = ui.pull.clone().unwrap();
        later.id += 1;
        later.start_ms += 60_000;
        later.end_ms += 60_000;
        ui.review.as_mut().unwrap().pulls.push(later.clone());
        assert!(ui.accept_recording_clock(
            ticket.key,
            RecordingLookup {
                clock: Some(crate::content_alignment::test_recording_clock()),
                pending: false,
                conflict: false
            }
        ));
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        ui.select(later);
        ui.sync_content_selection();
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 75.25);
        assert!(ui.next_content_action().is_none());
        assert!(!ui.content_waiting());
    }
    #[test]
    fn existing_shared_job_is_polled_without_busy_loop_or_fabricated_playback() {
        let (mut ui, ticket) = viewer();
        assert!(!ui.accept_recording_clock(
            ticket.key,
            RecordingLookup {
                clock: None,
                pending: true,
                conflict: false
            }
        ));
        assert!(ui.playback.is_none());
        assert!(ui.content_poll_pending());
        assert!(ui.next_content_action().is_none());
        ui.content.next_poll = Some(Instant::now() - Duration::from_secs(1));
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
    }
    #[test]
    fn recording_lookup_has_priority_over_unloaded_event_lists() {
        let (ui, _) = viewer();
        assert!(matches!(ui.next_action(), Some(Action::ContentSubmit(..))));
    }
    #[test]
    fn stale_shared_clock_is_rechecked_on_selection_without_interrupting_current_playback() {
        let (mut ui, ticket) = viewer();
        ui.accept_recording_clock(
            ticket.key,
            RecordingLookup {
                clock: Some(crate::content_alignment::test_recording_clock()),
                pending: false,
                conflict: false,
            },
        );
        ui.content.clocks[0].2 = Instant::now() - Duration::from_secs(61);
        ui.sync_content_selection();
        assert!(ui.playback.is_some());
        let mut later = ui.pull.clone().unwrap();
        later.id += 1;
        later.start_ms += 60_000;
        later.end_ms += 60_000;
        ui.review.as_mut().unwrap().pulls.push(later.clone());
        ui.select(later);
        ui.sync_content_selection();
        assert!(ui.content.clocks.is_empty());
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
    }
    #[test]
    fn shared_clock_cannot_publish_into_a_changed_account_or_selection() {
        let (mut ui, ticket) = viewer();
        ui.recording_match_status = Some((ticket.key.auth_epoch + 1, false));
        assert!(!ui.accept_recording_clock(
            ticket.key,
            RecordingLookup {
                clock: Some(crate::content_alignment::test_recording_clock()),
                pending: false,
                conflict: false
            }
        ));
        assert!(ui.playback.is_none());
        assert!(ui.content.clocks.is_empty());
    }
    #[test]
    fn selection_waits_without_marker_seek_then_uses_only_valid_content_result() {
        let (mut ui, ticket) = viewer();
        assert!(ui.playback.is_none());
        assert!(ui.content_waiting());
        assert!(!ui.marker_sync.busy());
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        assert!(ui.accept_content(ticket));
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 15.25);
        assert!(ui.playback.as_ref().unwrap().content_timing);
        assert!(ui.review.as_ref().unwrap().marker_timing.is_empty());
        assert!(
            matches!(ui.seek_absolute(ui.pull.as_ref().unwrap().start_ms+11_125),Some(PlaybackCommand::Seek(seconds)) if seconds==26.375)
        );
    }
    #[test]
    fn changed_pull_media_or_wcl_account_cannot_publish_a_completed_job() {
        for change in 0..3 {
            let (mut ui, ticket) = viewer();
            match change {
                0 => {
                    let mut other = ui.pull.clone().unwrap();
                    other.id += 1;
                    ui.review.as_mut().unwrap().pulls.push(other.clone());
                    ui.select(other);
                }
                1 => ui.review.as_mut().unwrap().replay.timeline_revision = Some("0".repeat(64)),
                _ => ui.recording_match_status = Some((1, false)),
            }
            ui.sync_content_selection();
            assert!(!ui.accept_content(ticket));
            assert!(ui.playback.is_none());
            assert!(ui.review.as_ref().unwrap().content_timing.is_empty());
        }
    }
    #[test]
    fn active_poll_cancels_on_selection_change_and_stale_generation_is_ignored() {
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
        assert!(ui.cancel.load(Ordering::Relaxed));
        assert_ne!(ui.generation, old);
        assert!(!ui.accept_content(ticket));
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
        assert!(ui.playback.is_none());
        let (_, good) = viewer();
        ui.close_review();
        ui.sync_content_selection();
        assert!(!ui.accept_content(good));
        assert!(ui.playback.is_none());
    }
    #[test]
    fn content_result_never_repositions_an_already_watched_video() {
        let (mut ui, ticket) = viewer();
        assert!(ui.accept_content(ticket.clone()));
        ui.playback.as_mut().unwrap().seconds = 50.0;
        assert!(!ui.accept_content(ticket));
        assert_eq!(ui.playback.as_ref().unwrap().seconds, 50.0);
    }
    #[test]
    fn unavailable_capability_never_submits_and_does_not_fabricate_upload_origin() {
        let (mut ui, _) = viewer();
        ui.review.as_mut().unwrap().content_capability = None;
        ui.sync_content_selection();
        assert!(ui.next_content_action().is_none());
        assert!(ui.playback.is_none());
        assert!(ui
            .review
            .as_ref()
            .unwrap()
            .pull_video_start(ui.pull.as_ref().unwrap())
            .is_nan());
    }
    #[test]
    fn completed_ticket_restores_after_metadata_replacement_without_resubmission() {
        let (mut ui, ticket) = viewer();
        assert!(ui.accept_content(ticket));
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
        ui.content.ticket.as_mut().unwrap().job.expires_at = 1;
        ticket.job.expires_at = 1;
        ui.content.saved.push(ticket);
        ui.sync_content_selection();
        assert!(ui.content.saved.is_empty());
        assert!(matches!(
            ui.next_content_action(),
            Some(Action::ContentSubmit(..))
        ));
        assert!(ui.playback.is_none());
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
        assert!(ui.playback.is_none());
        assert!(ui
            .review
            .as_ref()
            .unwrap()
            .pull_video_start(ui.pull.as_ref().unwrap())
            .is_nan());
        assert!(ui.accept_content(ticket));
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
            assert_eq!(ui.accept_content(ticket), status == Status::Failed);
            assert_eq!(ui.playback.is_some(), status == Status::Failed);
            if let Some(playback) = ui.playback.as_ref() {
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
    fn failed_live_match_without_marker_never_seeks_to_a_metadata_estimate() {
        let (mut ui, mut ticket) = backup_viewer();
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ui.accept_content(ticket);
        ui.select(ui.pull.clone().unwrap());
        let review = ui.review.as_ref().unwrap();
        let pull = ui.pull.as_ref().unwrap();
        assert!(review.marker_backup_allowed(pull));
        assert!(!review.has_precise_timing(pull));
        assert!(review.pull_video_start(pull).is_nan());
        assert!(review.video_seconds(pull, 0.0).is_none());
        assert!(crate::review_compare_ui::recording_clock(review, pull).is_err());
        assert!(ui.playback.is_none());
        // A later verified lookup can satisfy the still-waiting explicit selection.
        let mut refreshed = ui.review.clone().unwrap();
        marker(&mut ui);
        refreshed.marker_timing = ui.review.as_ref().unwrap().marker_timing.clone();
        ui.accept_review(refreshed);
        assert!(ui.sync_content_selection());
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
            assert!(ui.playback.is_none());
        }
    }
    #[test]
    fn request_errors_and_missing_provider_origin_do_not_authorize_backup() {
        let (mut ui, _) = backup_viewer();
        marker(&mut ui);
        ui.content_failed("Temporary service error".into());
        assert!(ui.review.as_ref().unwrap().marker_fallback.is_empty());
        assert!(ui.playback.is_none());
        let (mut ui, mut ticket) = viewer();
        marker(&mut ui);
        ticket.job.status = Status::Failed;
        ticket.job.result = None;
        ui.accept_content(ticket);
        assert!(ui.review.as_ref().unwrap().marker_fallback.is_empty());
        assert!(ui.playback.is_none());
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
