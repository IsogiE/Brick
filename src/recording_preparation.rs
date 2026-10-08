//! Warm bounded review metadata before starting missing recording alignments.
use crate::{
    review_ui::ReviewUi,
    streams::{self, Snapshot, Status, Stream, Vod},
};
use eframe::egui;
use std::{
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

/// Own derived preparation inputs independently of the UI's repaint cadence.
/// Weak source identities also detect in-place Rc::make_mut edits.
#[derive(Default)]
pub(crate) struct CandidateList {
    snapshot: Weak<Snapshot>,
    recordings: Weak<Vec<Vod>>,
    catalog: Weak<Vec<Stream>>,
    mode: Option<(bool, bool, bool)>,
    visible_indices: Vec<usize>,
    pub(crate) streams: Vec<Stream>,
    pub(crate) visible_count: usize,
    pub(crate) revision: u64,
    pub(crate) warmup_index: Option<usize>,
}

pub(crate) struct CandidateInputs<'a> {
    pub snapshot: Option<&'a Rc<Snapshot>>,
    pub recordings: Option<&'a Rc<Vec<Vod>>>,
    pub catalog: &'a Rc<Vec<Stream>>,
    pub visible_indices: &'a [usize],
    pub active: bool,
    pub recordings_open: bool,
    pub selected: bool,
}

impl CandidateList {
    pub(crate) fn update(&mut self, input: CandidateInputs<'_>) {
        let snapshot = input.snapshot.map(Rc::downgrade).unwrap_or_default();
        let recordings = input.recordings.map(Rc::downgrade).unwrap_or_default();
        let catalog = Rc::downgrade(input.catalog);
        let mode = (input.active, input.recordings_open, input.selected);
        if self.mode == Some(mode)
            && Weak::ptr_eq(&self.snapshot, &snapshot)
            && Weak::ptr_eq(&self.recordings, &recordings)
            && Weak::ptr_eq(&self.catalog, &catalog)
            && self.visible_indices == input.visible_indices
        {
            return;
        }
        self.warmup_index = input.snapshot.and_then(|snapshot| {
            snapshot
                .streams
                .iter()
                .enumerate()
                .filter(|(_, stream)| stream.status == Status::Live)
                .min_by_key(|(_, stream)| {
                    (&stream.user_id, stream.provider.key(), &stream.channel_id)
                })
                .map(|(index, _)| index)
        });
        self.snapshot = snapshot;
        self.recordings = recordings;
        self.catalog = catalog;
        self.mode = Some(mode);
        self.visible_indices.clear();
        self.visible_indices
            .extend_from_slice(input.visible_indices);
        self.streams.clear();
        if input.active && input.recordings_open {
            if let Some(recordings) = input.recordings {
                self.streams.extend(
                    input
                        .visible_indices
                        .iter()
                        .filter_map(|index| recordings.get(*index))
                        .map(Vod::as_stream)
                        .take(8),
                );
            }
        }
        if !input.recordings_open {
            if let Some(snapshot) = input.snapshot {
                self.streams.extend(
                    snapshot
                        .streams
                        .iter()
                        .filter(|stream| stream.status == Status::Live)
                        .take(8)
                        .cloned(),
                );
            }
        }
        self.visible_count = self.streams.len();
        if !input.active || input.recordings_open || !input.selected {
            for stream in input.catalog.iter() {
                if self.streams.len() == 8 {
                    break;
                }
                if !self.streams.iter().any(|candidate| {
                    streams::review_path(candidate).ok() == streams::review_path(stream).ok()
                }) {
                    self.streams.push(stream.clone());
                }
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Default)]
pub(crate) struct Preparation {
    current: Option<Stream>,
    aligning: bool,
    attempted: Vec<(String, bool, Instant, Duration)>,
    observed: Option<(u64, u64, u64)>,
    next_check: Option<Instant>,
    #[cfg(test)]
    pub(crate) scans: usize,
    #[cfg(test)]
    pub(crate) readiness_checks: usize,
}
impl Preparation {
    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        candidates: &[Stream],
        visible_count: usize,
        inputs_revision: u64,
        selected: Option<&Stream>,
        peer: &mut ReviewUi,
    ) {
        let now = Instant::now();
        // A selected review owns the shared WCL client. Finish an already
        // useful read of that same source, but do not export background boss
        // signatures or start another VOD while foreground logs are loading.
        let candidates = if selected.is_some() {
            peer.prepare_alignment(false);
            &[][..]
        } else {
            candidates
        };
        if candidates.is_empty() {
            self.observed = None;
            self.next_check = None;
            // Clicking the VOD being prepared hands off that same read. Do not
            // cancel it only to queue a duplicate behind the shared WCL client.
            if let Some(stream) = self.current.as_ref().filter(|stream| {
                selected.is_some_and(|selected| {
                    crate::warcraftlogs::prepared::source_identity(selected)
                        == crate::warcraftlogs::prepared::source_identity(stream)
                })
            }) {
                if peer.metadata_busy() {
                    peer.tick(ctx, Some(stream));
                    return;
                }
            }
            self.current = None;
            peer.tick(ctx, None);
            return;
        }
        if let Some(stream) = &self.current {
            if candidates.iter().any(|candidate| {
                crate::warcraftlogs::prepared::source_identity(candidate)
                    == crate::warcraftlogs::prepared::source_identity(stream)
            }) {
                peer.tick(ctx, Some(stream));
                if peer.metadata_busy() {
                    return;
                }
            } else {
                peer.tick(ctx, None);
                if peer.metadata_busy() {
                    return;
                }
            }
            if let Some(stream) = &self.current {
                if let Some(path) = crate::warcraftlogs::prepared::source_identity(stream) {
                    let visible = candidates.iter().take(visible_count).any(|candidate| {
                        crate::warcraftlogs::prepared::source_identity(candidate).as_ref()
                            == Some(&path)
                    });
                    if let Some(attempt) = self
                        .attempted
                        .iter_mut()
                        .find(|(key, aligning, _, _)| key == &path && *aligning == self.aligning)
                    {
                        attempt.2 = now;
                        attempt.3 = peer.preparation_retry_delay(visible);
                    }
                }
            }
            self.current = None;
            self.observed = None;
        }
        if peer.metadata_busy() {
            peer.tick(ctx, None);
            return;
        }
        // Worker completion and selection hand-off above stay immediate. The
        // expensive cache/readiness scan depends on data and time, never paints.
        let Some(revision) = peer.with_prepared(|cache| cache.revision()) else {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        };
        let observed = (inputs_revision, revision, crate::guild::generation());
        if self.observed == Some(observed) && self.next_check.is_none_or(|deadline| now < deadline)
        {
            self.schedule(ctx, now);
            return;
        }
        if self.observed.is_some_and(|old| old.2 != observed.2) {
            self.attempted.clear();
        }
        self.attempted
            .retain(|(_, _, at, delay)| now.saturating_duration_since(*at) < *delay);
        let Some((next, deadline, revision)) = peer.with_prepared(|cache| {
            let (next, deadline) = self.scan(candidates, cache, now);
            (next, deadline, cache.revision())
        }) else {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        };
        self.observed = Some((inputs_revision, revision, observed.2));
        self.next_check = deadline;
        if let Some((stream, aligning)) = next {
            if self.attempted.len() >= 128 {
                self.attempted.remove(0);
            }
            self.attempted.push((
                crate::warcraftlogs::prepared::source_identity(stream).unwrap(),
                aligning,
                now,
                Duration::from_secs(5 * 60),
            ));
            self.aligning = aligning;
            peer.prepare_alignment(aligning);
            self.current = Some(stream.clone());
            peer.tick(ctx, Some(stream));
        } else {
            peer.tick(ctx, None);
            self.schedule(ctx, now);
        }
    }

    fn schedule(&self, ctx: &egui::Context, now: Instant) {
        if let Some(deadline) = self.next_check {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    fn scan<'a>(
        &mut self,
        candidates: &'a [Stream],
        cache: &crate::warcraftlogs::prepared::Cache,
        now: Instant,
    ) -> (Option<(&'a Stream, bool)>, Option<Instant>) {
        #[cfg(test)]
        {
            self.scans += 1;
        }
        let mut deadline: Option<Instant> = None;
        let mut wake_at = |at| {
            deadline = Some(deadline.map_or(at, |old| old.min(at)));
        };
        // Metadata first, then alignment; share the existing eight-entry budget.
        for aligning in [false, true] {
            for stream in candidates.iter().take(8) {
                let Some(path) = crate::warcraftlogs::prepared::source_identity(stream) else {
                    continue;
                };
                // Check cooldown before the potentially expensive sampling plan.
                if let Some((_, _, at, delay)) = self
                    .attempted
                    .iter()
                    .find(|(key, mode, _, _)| key == &path && *mode == aligning)
                {
                    wake_at(*at + *delay);
                    continue;
                }
                let metadata = cache.background_contains(stream);
                let needed = if aligning {
                    if metadata {
                        #[cfg(test)]
                        {
                            self.readiness_checks += 1;
                        }
                        !cache.ready(stream)
                    } else {
                        false
                    }
                } else {
                    !metadata
                };
                if needed {
                    return (Some((stream, aligning)), deadline);
                }
                if metadata {
                    if let Some(wait) = cache.background_recheck_after(stream) {
                        wake_at(now + wait);
                    }
                }
            }
        }
        (None, deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warcraftlogs::{prepared::Cache, Review};

    fn fixture() -> (Stream, Review) {
        let (replay, pull, capability, _) = crate::content_alignment::test_ticket();
        let stream = serde_json::from_value(serde_json::json!({
            "userId":"101", "name":"Fixture", "provider":"youtube",
            "channelId":replay.video_id, "url":replay.public_url(0),
            "status":"offline", "recordingId":replay.video_id,
        }))
        .unwrap();
        (
            stream,
            Review {
                alternative_pulls: Default::default(),
                replay,
                pulls: vec![pull],
                content_capability: Some(capability),
                content_timing: Default::default(),
            },
        )
    }

    #[test]
    fn preparation_candidates_rebuild_only_when_owned_inputs_change() {
        let (mut stream, _) = fixture();
        stream.status = Status::Live;
        let mut snapshot: Rc<Snapshot> = Rc::new(
            serde_json::from_value(serde_json::json!({
                "streams":[], "ownStreams":[], "providers":{"twitch":true,"youtube":true}
            }))
            .unwrap(),
        );
        Rc::make_mut(&mut snapshot).streams.push(stream.clone());
        let mut catalog = Rc::new(vec![stream]);
        let vod: Vod = serde_json::from_value(serde_json::json!({
            "id":"987", "userId":"202", "name":"Archive", "provider":"twitch",
            "url":"https://www.twitch.tv/videos/987", "startedAt":"2026-09-08T08:00:00Z",
            "endedAt":"2026-09-08T22:00:00Z"
        }))
        .unwrap();
        let mut recordings = Rc::new(vec![vod]);
        let mut list = CandidateList::default();
        let update = |list: &mut CandidateList,
                      snapshot: &Rc<Snapshot>,
                      recordings: &Rc<Vec<Vod>>,
                      catalog: &Rc<Vec<Stream>>,
                      indices: &[usize],
                      mode: (bool, bool, bool)| {
            list.update(CandidateInputs {
                snapshot: Some(snapshot),
                recordings: Some(recordings),
                catalog,
                visible_indices: indices,
                active: mode.0,
                recordings_open: mode.1,
                selected: mode.2,
            });
        };
        for _ in 0..200 {
            update(
                &mut list,
                &snapshot,
                &recordings,
                &catalog,
                &[],
                (false, false, false),
            );
        }
        assert_eq!(list.revision, 1);
        assert_eq!(list.streams.len(), 1);
        assert_eq!(list.visible_count, 1);
        assert_eq!(list.warmup_index, Some(0));
        // Mutating the current snapshot invalidates its Weak identity too.
        Rc::make_mut(&mut snapshot).streams[0].status = Status::Offline;
        update(
            &mut list,
            &snapshot,
            &recordings,
            &catalog,
            &[],
            (false, false, false),
        );
        assert_eq!(list.revision, 2);
        assert_eq!(list.visible_count, 0);
        assert_eq!(list.warmup_index, None);
        Rc::make_mut(&mut catalog).clear();
        update(
            &mut list,
            &snapshot,
            &recordings,
            &catalog,
            &[],
            (false, false, false),
        );
        assert_eq!(list.revision, 3);
        assert!(list.streams.is_empty());
        update(
            &mut list,
            &snapshot,
            &recordings,
            &catalog,
            &[0],
            (true, true, false),
        );
        assert_eq!(list.streams[0].recording_id.as_deref(), Some("987"));
        assert_eq!(list.visible_count, 1);
        Rc::make_mut(&mut recordings)[0].id = "654".into();
        update(
            &mut list,
            &snapshot,
            &recordings,
            &catalog,
            &[0],
            (true, true, false),
        );
        assert_eq!(list.streams[0].recording_id.as_deref(), Some("654"));
        update(
            &mut list,
            &snapshot,
            &recordings,
            &catalog,
            &[],
            (true, true, false),
        );
        assert!(
            list.streams.is_empty(),
            "scroll/filter input invalidates visible candidates"
        );
        list.update(CandidateInputs {
            snapshot: None,
            recordings: None,
            catalog: &catalog,
            visible_indices: &[],
            active: false,
            recordings_open: false,
            selected: false,
        });
        assert!(list.streams.is_empty());
        assert_eq!(list.warmup_index, None);
    }

    #[test]
    fn preparation_cooldown_skips_readiness_until_its_real_deadline() {
        let (stream, review) = fixture();
        let mut cache = Cache::default();
        cache.insert(&stream, review, (0, true), vec![]);
        assert!(cache.background_contains(&stream));
        let path = crate::warcraftlogs::prepared::source_identity(&stream).unwrap();
        let now = Instant::now();
        let delay = Duration::from_secs(5);
        let mut preparation = Preparation::default();
        preparation.attempted.push((path, true, now, delay));
        let candidates = [stream];
        let (next, deadline) = preparation.scan(&candidates, &cache, now);
        assert!(next.is_none());
        assert_eq!(preparation.readiness_checks, 0);
        assert_eq!(deadline, Some(now + delay));
        // The scheduler removes expired attempts before considering work again.
        preparation.attempted.retain(|(_, _, at, cooldown)| {
            (now + delay).saturating_duration_since(*at) < *cooldown
        });
        let (next, _) = preparation.scan(&candidates, &cache, now + delay);
        assert!(next.is_some_and(|(_, aligning)| aligning));
        assert_eq!(preparation.readiness_checks, 1);
    }

    #[test]
    fn preparation_cache_deadline_tracks_metadata_age_and_timing_expiry() {
        let (stream, mut review) = fixture();
        review.replay.growing = true;
        let mut cache = Cache::default();
        cache.insert(&stream, review.clone(), (0, true), vec![]);
        let initial = cache.background_recheck_after(&stream).unwrap();
        assert!(initial > Duration::from_secs(9) && initial <= Duration::from_secs(10));
        cache.age_for_test(Duration::from_secs(11));
        assert!(!cache.background_contains(&stream));
        assert_eq!(
            cache.background_recheck_after(&stream),
            Some(Duration::ZERO)
        );
        review.replay.growing = false;
        let (_, _, _, ticket) = crate::content_alignment::test_ticket();
        let mut clock = crate::content_alignment::test_recording_clock();
        clock.expires_at =
            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 + 2_000;
        cache.insert(&stream, review, (0, true), vec![(ticket.key, clock)]);
        let wait = cache.background_recheck_after(&stream).unwrap();
        assert!(wait > Duration::from_secs(1) && wait <= Duration::from_secs(2));
    }
}
