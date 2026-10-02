//! Bounded, account-bound review snapshots with separate refresh and retention ages.
use super::{Pull, Review};
use crate::{
    content_alignment::{Key, RecordingClock},
    streams::Stream,
};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const LIVE_TTL: Duration = Duration::from_secs(10);
const RETENTION: Duration = super::persistent::AGE;
const CAPACITY: usize = 8;
const PULL_CAPACITY: usize = 4096;

#[derive(Clone)]
pub(crate) struct Entry {
    pub review: Review,
    pub status: (u64, bool),
    pub clocks: Vec<(Key, RecordingClock)>,
    pub sampling: crate::content_alignment::sampling::Snapshot,
    pub at: Instant,
    path: String,
    generation: u64,
    source_identity: String,
}

/// Bind live cache entries to the broadcast start, never just its channel URL.
pub(crate) fn source_identity(stream: &Stream) -> Option<String> {
    let path = crate::streams::review_path(stream).ok()?;
    if stream.recording_id.is_some() {
        return Some(path);
    }
    if stream.status != crate::streams::Status::Live {
        return None;
    }
    Some(format!(
        "{path}:{}:{}",
        stream.channel_id, stream.replay_start_ms?
    ))
}

#[derive(Default)]
pub(crate) struct Cache {
    entries: Vec<Entry>,
    suspended: bool,
    pub preferences: crate::defensives::Preferences,
}
impl Cache {
    pub fn set_connected(&mut self, connected: bool) {
        self.entries.clear();
        self.preferences = Default::default();
        self.suspended = !connected;
    }

    pub fn contains(&self, stream: &Stream) -> bool {
        self.find(stream).is_some()
    }

    pub fn background_contains(&self, stream: &Stream) -> bool {
        self.retained(stream).is_some_and(|entry| {
            let old_archive = stream.status != crate::streams::Status::Live
                && !entry.review.replay.growing
                && entry
                    .review
                    .replay
                    .start_ms()
                    .ok()
                    .and_then(|start| {
                        start.checked_add(
                            i64::try_from(entry.review.replay.available_seconds)
                                .ok()?
                                .checked_mul(1000)?,
                        )
                    })
                    .or(stream.replay_end_ms)
                    .is_some_and(|end| {
                        end < time::OffsetDateTime::now_utc()
                            .unix_timestamp()
                            .saturating_mul(1000)
                            .saturating_sub(24 * 60 * 60 * 1000)
                    });
            if old_archive {
                entry.at.elapsed() < Duration::from_secs(6 * 60 * 60)
            } else {
                self.contains(stream)
            }
        })
    }

    pub fn ready(&self, stream: &Stream) -> bool {
        self.retained(stream).is_some_and(|entry| {
            let plan = entry.sampling.plan(&entry.review, entry.status.0);
            plan.next.is_none()
                && plan.pending.is_none()
                && (!entry.sampling.tickets.is_empty() || entry.review.pulls.is_empty())
        })
    }

    pub(super) fn restore_samples(
        &mut self,
        identity: &str,
        samples: crate::content_alignment::sampling::Snapshot,
    ) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.source_identity == identity)
        {
            samples.apply_to(&mut entry.review, entry.status.0);
            entry.sampling = samples;
        }
    }

    pub fn set_samples(
        &mut self,
        stream: &Stream,
        samples: crate::content_alignment::sampling::Snapshot,
    ) {
        if self.suspended {
            return;
        }
        let Some(identity) = source_identity(stream) else {
            return;
        };
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.source_identity == identity && entry.generation == crate::guild::generation()
        }) {
            samples.apply_to(&mut entry.review, entry.status.0);
            entry.sampling = samples;
        }
    }

    pub fn get(&self, stream: &Stream) -> Option<Entry> {
        self.find(stream).cloned()
    }

    /// Keep known pulls available while the authenticated worker refreshes them.
    /// Refresh callers still use `get`, so a displayed snapshot cannot suppress
    /// polling for new pulls. Account reset and broadcast identity apply to both.
    pub fn for_display(&self, stream: &Stream) -> Option<Entry> {
        let mut entry = self.retained(stream)?.clone();
        entry
            .clocks
            .retain(|(key, clock)| clock.alignment(key).is_some());
        Some(entry)
    }

    fn find(&self, stream: &Stream) -> Option<&Entry> {
        self.retained(stream).filter(|entry| {
            entry.at.elapsed()
                < if entry.review.replay.growing {
                    LIVE_TTL
                } else {
                    TTL
                }
                && entry
                    .clocks
                    .iter()
                    .all(|(key, clock)| clock.alignment(key).is_some())
        })
    }

    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self, age: Duration) {
        for entry in &mut self.entries {
            entry.at = Instant::now() - age;
        }
    }

    fn retained(&self, stream: &Stream) -> Option<&Entry> {
        if self.suspended {
            return None;
        }
        let identity = source_identity(stream)?;
        let path = crate::streams::review_path(stream).ok()?;
        self.entries.iter().find(|entry| {
            entry.path == path
                && entry.source_identity == identity
                && entry.generation == crate::guild::generation()
                && entry.at.elapsed() < RETENTION
        })
    }

    pub(super) fn restore_snapshot(
        &mut self,
        path: String,
        source_identity: String,
        review: Review,
        status: (u64, bool),
        clocks: Vec<(Key, RecordingClock)>,
        age: Duration,
    ) {
        let generation = crate::guild::request_generation();
        if self.suspended
            || age >= RETENTION
            || review.pulls.len() > PULL_CAPACITY
            || crate::guild::ensure_current(generation).is_err()
        {
            return;
        }
        let Some(at) = Instant::now().checked_sub(age) else {
            return;
        };
        self.entries.retain(|entry| {
            entry.path != path && entry.generation == generation && entry.at.elapsed() < RETENTION
        });
        while self.entries.len() >= CAPACITY
            || self
                .entries
                .iter()
                .map(|e| e.review.pulls.len())
                .sum::<usize>()
                + review.pulls.len()
                > PULL_CAPACITY
        {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            review,
            status,
            clocks,
            sampling: Default::default(),
            at,
            path,
            generation,
            source_identity,
        });
    }

    #[cfg(test)]
    pub fn record_clock(&mut self, key: &Key, clock: &RecordingClock) {
        for entry in &mut self.entries {
            if entry.generation != key.guild_generation || entry.status.0 != key.auth_epoch {
                continue;
            }
            let Some(cap) = entry.review.content_capability.as_ref() else {
                continue;
            };
            for pull in &entry.review.pulls {
                let wanted = Key::new(&entry.review.replay, pull, cap, key.auth_epoch);
                if wanted.same_recording_report(key) {
                    if let Some(alignment) = clock.alignment(&wanted) {
                        entry
                            .review
                            .content_timing
                            .insert((pull.report.clone(), pull.id), alignment);
                    }
                }
            }
            if entry.review.replay.video_id == key.video_id
                && entry.review.replay.provider == key.provider
            {
                entry
                    .clocks
                    .retain(|(saved, _)| !saved.same_recording_report(key));
                entry.clocks.push((key.clone(), clock.clone()));
            }
        }
    }

    /// Refresh only timing, preserving the metadata age and this viewer's samples.
    pub fn refresh_clocks(
        &mut self,
        stream: &Stream,
        mut review: Review,
        clocks: Vec<(Key, RecordingClock)>,
    ) {
        if self.suspended {
            return;
        }
        let Some(identity) = source_identity(stream) else {
            return;
        };
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.source_identity == identity && entry.generation == crate::guild::generation()
        }) {
            entry.sampling.apply_to(&mut review, entry.status.0);
            entry.review = review;
            entry.clocks = clocks;
        }
    }

    pub fn insert(
        &mut self,
        stream: &Stream,
        review: Review,
        status: (u64, bool),
        clocks: Vec<(Key, RecordingClock)>,
    ) {
        if self.suspended || review.pulls.len() > PULL_CAPACITY {
            return;
        }
        let Some(identity) = source_identity(stream) else {
            return;
        };
        let Ok(path) = crate::streams::review_path(stream) else {
            return;
        };
        let generation = crate::guild::request_generation();
        if crate::guild::ensure_current(generation).is_err() {
            return;
        }
        let sampling = self
            .entries
            .iter()
            .find(|entry| {
                entry.path == path
                    && entry.source_identity == identity
                    && entry.status.0 == status.0
            })
            .map(|entry| entry.sampling.clone())
            .unwrap_or_default();
        self.entries.retain(|entry| {
            entry.path != path && entry.generation == generation && entry.at.elapsed() < RETENTION
        });
        while self.entries.len() >= CAPACITY
            || self
                .entries
                .iter()
                .map(|entry| entry.review.pulls.len())
                .sum::<usize>()
                + review.pulls.len()
                > PULL_CAPACITY
        {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            review,
            status,
            clocks,
            sampling,
            at: Instant::now(),
            path,
            generation,
            source_identity: identity,
        });
    }
}

/// One lookup per distinct report covers matching pulls without exporting events.
/// Prioritize the selected report, and bound unusual multi-report catalogues.
pub(crate) fn clocks(
    review: &mut Review,
    preferred: Option<&Pull>,
    epoch: u64,
    mut lookup: impl FnMut(&Key) -> Option<RecordingClock>,
) -> Vec<(Key, RecordingClock)> {
    let Some(cap) = review.content_capability.clone() else {
        return Vec::new();
    };
    let preferred = preferred.and_then(|pull| review.matching_pull(pull));
    let mut keys = Vec::new();
    for pull in preferred.into_iter().chain(review.pulls.iter()) {
        let key = Key::new(&review.replay, pull, &cap, epoch);
        if !keys
            .iter()
            .any(|saved: &Key| saved.same_recording_report(&key))
        {
            keys.push(key);
            if keys.len() == 64 {
                break;
            }
        }
    }
    let mut clocks = Vec::new();
    for key in keys {
        let clock = lookup(&key).filter(|clock| clock.alignment(&key).is_some());
        review.content_timing.retain(|_, alignment| {
            !alignment.shared_clock || !alignment.key.same_recording_report(&key)
        });
        let Some(clock) = clock else {
            continue;
        };
        for pull in &review.pulls {
            let wanted = Key::new(&review.replay, pull, &cap, epoch);
            if wanted.same_recording_report(&key) {
                if let Some(alignment) = clock.alignment(&wanted) {
                    review
                        .content_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                }
            }
        }
        clocks.push((key, clock));
    }
    clocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        content_alignment::{test_recording_clock, test_ticket},
        streams::{Provider, Status},
    };

    fn fixture() -> (Stream, Review, Vec<(Key, RecordingClock)>) {
        let (replay, pull, cap, ticket) = test_ticket();
        let stream = Stream {
            user_id: "101".into(),
            name: "Fixture".into(),
            raid_role: None,
            provider: Provider::Youtube,
            channel_id: replay.video_id.clone(),
            url: replay.public_url(0),
            status: Status::Offline,
            broadcast_state: None,
            recording_id: Some(replay.video_id.clone()),
            replay_start_ms: None,
            replay_end_ms: None,
        };
        let review = Review {
            replay,
            pulls: vec![pull],
            content_capability: Some(cap),

            content_timing: Default::default(),
        };
        (stream, review, vec![(ticket.key, test_recording_clock())])
    }

    #[test]
    fn prepared_cache_bounds_memory_age_and_guild_and_reuses_newly_completed_clock() {
        let (stream, review, clocks) = fixture();
        let mut cache = Cache::default();
        cache.insert(
            &stream,
            review.clone(),
            (clocks[0].0.auth_epoch, true),
            Vec::new(),
        );
        cache.record_clock(&clocks[0].0, &clocks[0].1);
        assert_eq!(cache.get(&stream).unwrap().review.content_timing.len(), 1);
        cache.entries[0].at = Instant::now() - TTL;
        assert!(cache.get(&stream).is_none());
        cache.entries[0].at = Instant::now();
        cache.entries[0].generation = crate::guild::generation().wrapping_add(1);
        assert!(cache.get(&stream).is_none());
        for index in 0..CAPACITY + 3 {
            let mut other = stream.clone();
            other.user_id = (1000 + index).to_string();
            cache.insert(
                &other,
                review.clone(),
                (clocks[0].0.auth_epoch, true),
                clocks.clone(),
            );
        }
        assert_eq!(cache.entries.len(), CAPACITY);
        assert!(cache.get(&stream).is_none());
        let mut large = review.clone();
        large.pulls = vec![large.pulls[0].clone(); PULL_CAPACITY + 1];
        cache.insert(
            &stream,
            large,
            (clocks[0].0.auth_epoch, true),
            clocks.clone(),
        );
        assert!(cache.get(&stream).is_none());
        cache.set_connected(false);
        // An old in-flight read cannot repopulate the cache after logout starts.
        cache.insert(&stream, review, (clocks[0].0.auth_epoch, true), clocks);
        assert!(cache.get(&stream).is_none());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn old_archives_do_not_repeatedly_fetch_metadata_or_realign_a_valid_clock() {
        let (mut stream, review, clocks) = fixture();
        // Uploads without a known media origin still have an archive date.
        stream.replay_end_ms = Some(1_700_001_000_000);
        let mut cache = Cache::default();
        let samples =
            crate::content_alignment::sampling::test_samples(&review, clocks[0].0.auth_epoch);
        cache.insert(&stream, review, (clocks[0].0.auth_epoch, true), Vec::new());
        cache.set_samples(&stream, samples);
        cache.age_for_test(Duration::from_secs(120));
        assert!(
            cache.get(&stream).is_none(),
            "Selected review still requests fresh metadata"
        );
        assert!(cache.background_contains(&stream));
        assert!(
            cache.ready(&stream),
            "Metadata age must not expire the recording offset"
        );
        cache.age_for_test(Duration::from_secs(6 * 60 * 60));
        assert!(!cache.background_contains(&stream));
        assert!(cache.ready(&stream));
        for ticket in &mut cache.entries[0].sampling.tickets {
            ticket.job.expires_at = 1;
        }
        assert!(
            !cache.ready(&stream),
            "Actually expired precision must be refreshed"
        );
        cache.set_connected(false);
        assert!(!cache.background_contains(&stream));
    }

    #[test]
    fn live_refresh_age_preserves_display_without_crossing_a_restarted_broadcast() {
        let (mut stream, mut review, clocks) = fixture();
        stream.recording_id = None;
        stream.status = Status::Live;
        stream.replay_start_ms = Some(1_790_000_000_000);
        review.replay.growing = true;
        let mut cache = Cache::default();
        cache.insert(
            &stream,
            review.clone(),
            (clocks[0].0.auth_epoch, true),
            Vec::new(),
        );
        assert!(cache.get(&stream).is_some());
        let mut restarted = stream.clone();
        restarted.replay_start_ms = Some(1_790_000_060_000);
        assert!(cache.get(&restarted).is_none());
        assert!(cache.for_display(&restarted).is_none());
        cache.age_for_test(Duration::from_secs(11));
        assert!(cache.get(&stream).is_none());
        assert!(!cache.contains(&stream));
        assert_eq!(cache.for_display(&stream).unwrap().review.pulls.len(), 1);
        cache.age_for_test(RETENTION);
        assert!(cache.for_display(&stream).is_none());
        cache.insert(
            &restarted,
            review,
            (clocks[0].0.auth_epoch, true),
            Vec::new(),
        );
        assert!(cache.get(&stream).is_none());
        assert!(cache.for_display(&stream).is_none());
        assert!(cache.get(&restarted).is_some());
        cache.set_connected(false);
        assert!(cache.for_display(&restarted).is_none());
    }

    #[test]
    fn refreshing_another_recording_retains_old_display_within_memory_and_age_limits() {
        let (stream, review, clocks) = fixture();
        let mut cache = Cache::default();
        cache.insert(
            &stream,
            review.clone(),
            (clocks[0].0.auth_epoch, true),
            Vec::new(),
        );
        cache.age_for_test(Duration::from_secs(120));
        let mut other = stream.clone();
        other.user_id = "202".into();
        cache.insert(&other, review, (clocks[0].0.auth_epoch, true), Vec::new());
        assert!(cache.get(&stream).is_none());
        assert!(cache.for_display(&stream).is_some());
        assert!(cache.get(&other).is_some());
        cache.entries[0].generation = crate::guild::generation().wrapping_add(1);
        assert!(cache.for_display(&stream).is_none());
        cache.age_for_test(RETENTION);
        assert!(cache.for_display(&other).is_none());
    }

    #[test]
    fn saved_server_clock_survives_empty_viewer_samples_and_cache_restore() {
        let (stream, mut review, saved) = fixture();
        let epoch = saved[0].0.auth_epoch;
        let clocks = super::clocks(&mut review, None, epoch, |_| Some(saved[0].1.clone()));
        let expected = review
            .content_alignment(&review.pulls[0])
            .unwrap()
            .result
            .video_seconds;
        let mut cache = Cache::default();
        cache.insert(&stream, review, (epoch, true), clocks);
        cache.set_samples(&stream, Default::default());
        let entry = cache.get(&stream).unwrap();
        assert_eq!(
            entry
                .review
                .content_alignment(&entry.review.pulls[0])
                .unwrap()
                .result
                .video_seconds,
            expected
        );
        assert!(entry.sampling.tickets.is_empty());
        cache.age_for_test(Duration::from_secs(120));
        let entry = cache.for_display(&stream).unwrap();
        cache.refresh_clocks(&stream, entry.review, entry.clocks);
        assert!(
            cache.get(&stream).is_none(),
            "Clock lookup must not renew metadata age"
        );
        assert!(
            cache
                .for_display(&stream)
                .unwrap()
                .review
                .content_timing
                .len()
                == 1
        );
    }

    #[test]
    fn saved_clocks_cover_each_report_once_and_authoritative_absence_clears_only_shared_timing() {
        let (_, mut review, saved) = fixture();
        let mut second = review.pulls[0].clone();
        second.report = "ZYXWzyxw87654321".into();
        second.report_start_ms += 1000;
        second.start_ms += 1000;
        second.end_ms += 1000;
        review.pulls.push(second.clone());
        let mut calls = Vec::new();
        let clocks = super::clocks(&mut review, Some(&second), saved[0].0.auth_epoch, |key| {
            calls.push(key.report.clone());
            let mut clock = saved[0].1.clone();
            clock.report_start_ms = key.report_start_ms;
            Some(clock)
        });
        assert_eq!(
            calls,
            vec![second.report.clone(), review.pulls[0].report.clone()]
        );
        assert_eq!(clocks.len(), 2);
        assert_eq!(review.content_timing.len(), 2);
        let mut calls = 0;
        super::clocks(&mut review, None, saved[0].0.auth_epoch, |_| {
            calls += 1;
            None
        });
        assert_eq!(calls, 2);
        assert!(review.content_timing.is_empty());
    }

    #[test]
    fn missing_or_conflicting_recording_clock_never_invents_ready_playback() {
        let (_, mut review, clocks) = fixture();
        assert!(super::clocks(&mut review, None, clocks[0].0.auth_epoch, |_| None).is_empty());
        assert!(review.content_timing.is_empty());
        let mut wrong = clocks[0].1.clone();
        wrong.report_start_ms += 1000;
        super::clocks(&mut review, None, clocks[0].0.auth_epoch, |_| {
            Some(wrong.clone())
        });
        assert!(review.content_timing.is_empty());
    }
}
