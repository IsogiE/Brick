//! Short-lived, account-bound review data prepared while browsing recordings.
use super::{Pull, Review};
use crate::{
    content_alignment::{Key, RecordingClock},
    streams::Stream,
};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const LIVE_TTL: Duration = Duration::from_secs(10);
const CAPACITY: usize = 8;
const PULL_CAPACITY: usize = 4096;

#[derive(Clone)]
pub(crate) struct Entry {
    pub review: Review,
    pub status: (u64, bool),
    pub clocks: Vec<(Key, RecordingClock)>,
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
    tickets: Vec<crate::content_alignment::Ticket>,
}
impl Cache {
    pub fn set_connected(&mut self, connected: bool) {
        self.entries.clear();
        self.tickets.clear();
        self.preferences = Default::default();
        self.suspended = !connected;
    }

    pub fn contains(&self, stream: &Stream) -> bool {
        self.find(stream).is_some()
    }

    pub fn ready(&self, stream: &Stream) -> bool {
        self.find(stream).is_some_and(|entry| {
            entry
                .review
                .pulls
                .iter()
                .any(|pull| entry.review.has_precise_timing(pull))
        })
    }

    pub fn remember_ticket(&mut self, ticket: crate::content_alignment::Ticket) {
        if self.suspended
            || ticket.expired()
            || crate::guild::ensure_current(ticket.key.guild_generation).is_err()
        {
            return;
        }
        self.tickets.retain(|old| {
            !old.expired()
                && old.key != ticket.key
                && old.key.guild_generation == ticket.key.guild_generation
                && old.key.auth_epoch == ticket.key.auth_epoch
        });
        if self.tickets.len() >= 64 {
            self.tickets.remove(0);
        }
        self.tickets.push(ticket);
    }

    pub fn ticket(&self, key: &Key) -> Option<crate::content_alignment::Ticket> {
        if self.suspended || key.guild_generation != crate::guild::generation() {
            return None;
        }
        self.tickets
            .iter()
            .find(|ticket| &ticket.key == key && !ticket.expired())
            .cloned()
    }

    pub fn get(&self, stream: &Stream) -> Option<Entry> {
        self.find(stream).cloned()
    }

    fn find(&self, stream: &Stream) -> Option<&Entry> {
        if self.suspended {
            return None;
        }
        let identity = source_identity(stream)?;
        let path = crate::streams::review_path(stream).ok()?;
        self.entries.iter().find(|entry| {
            entry.path == path
                && entry.source_identity == identity
                && entry.generation == crate::guild::generation()
                && entry.at.elapsed()
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
        self.entries.retain(|entry| {
            entry.path != path && entry.generation == generation && entry.at.elapsed() < TTL
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
            at: Instant::now(),
            path,
            generation,
            source_identity: identity,
        });
    }
}

/// One lookup covers every pull from the same report; never export boss events
/// or submit inference jobs merely because a recording is visible in the list.
pub(crate) fn clocks(
    review: &mut Review,
    preferred: Option<&Pull>,
    epoch: u64,
    mut lookup: impl FnMut(&Key) -> Option<RecordingClock>,
) -> Vec<(Key, RecordingClock)> {
    let Some(cap) = review.content_capability.as_ref() else {
        return Vec::new();
    };
    let Some(pull) = preferred
        .and_then(|pull| review.matching_pull(pull))
        .or(review.pulls.first())
    else {
        return Vec::new();
    };
    let key = Key::new(&review.replay, pull, cap, epoch);
    let Some(clock) = lookup(&key) else {
        return Vec::new();
    };
    for pull in &review.pulls {
        let wanted = Key::new(&review.replay, pull, cap, epoch);
        if wanted.same_recording_report(&key) {
            if let Some(alignment) = clock.alignment(&wanted) {
                review
                    .content_timing
                    .insert((pull.report.clone(), pull.id), alignment);
            }
        }
    }
    vec![(key, clock)]
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
            marker_timing: Default::default(),
            marker_fallback: Default::default(),
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
    fn live_cache_expires_before_pull_poll_and_cannot_cross_a_restarted_broadcast() {
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
        cache.entries[0].at = Instant::now() - Duration::from_secs(11);
        assert!(cache.get(&stream).is_none());
        cache.insert(
            &restarted,
            review,
            (clocks[0].0.auth_epoch, true),
            Vec::new(),
        );
        assert!(cache.get(&stream).is_none());
        assert!(cache.get(&restarted).is_some());
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
