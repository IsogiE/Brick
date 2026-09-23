//! One bounded encrypted snapshot per guild/account; reports and pulls are upserts.
use super::{prepared, Config, Pull, Replay, Review, Session};
use crate::{
    content_alignment::{Capability, Key, RecordingClock},
    guild::Access,
    streams::Stream,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

// Leave room for the authenticated-encryption envelope inside the 2 MiB file cap.
const MAX_BYTES: usize = 2 * 1024 * 1024 - 128;
const MAX_RECORDINGS: usize = 64;
const MAX_PULLS: usize = 4096;
const MAX_REPORTS: usize = 128;
pub(super) const AGE: Duration = Duration::from_secs(21 * 24 * 60 * 60);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    identity: String,
    path: String,
    replay: Replay,
    pulls: Vec<(String, u64)>,
    clocks: BTreeMap<String, RecordingClock>,
    complete: bool,
    updated_at: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u8,
    session: String,
    // A pull is stored once even when several streamers cover the same raid.
    reports: BTreeMap<String, BTreeMap<u64, Pull>>,
    recordings: Vec<Recording>,
}

pub(super) struct Cache {
    scope: String,
    document: Document,
    saved: Vec<u8>,
}
impl Cache {
    pub fn load(config: &Config, session: &Session, access: &Access) -> Option<Self> {
        access.check().ok()?;
        if config.discord_guild_id != access.guild_id || config.user_id != access.user_id {
            return None;
        }
        if session.cache_id.len() != 64 || !session.cache_id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        let scope = format!(
            "wcl-review-v1:{}:{}:{}:{}",
            config.client_id, config.discord_guild_id, config.guild_id, config.user_id
        );
        let saved = crate::protected_cache::load(&scope)
            .ok()?
            .unwrap_or_default();
        access.check().ok()?;
        let mut document = if saved.is_empty() {
            Document::default()
        } else {
            decode(&saved)?
        };
        // Reconnecting WCL may select another WCL account under the same Discord
        // account. Reuse the one filename but never reuse that account's reports.
        document = for_session(document, &session.cache_id);
        prune(&mut document, super::now_secs());
        let mut cache = Self {
            scope,
            document,
            saved,
        };
        cache.save(access);
        Some(cache)
    }

    pub fn invalidate(&mut self) {
        self.document.recordings.clear();
        self.document.reports.clear();
        let scope = self.scope.clone();
        self.save_with(|bytes| crate::protected_cache::save(&scope, bytes).is_ok());
    }

    pub fn hydrate(
        &self,
        cache: &mut prepared::Cache,
        capability: Option<&Capability>,
        epoch: u64,
    ) {
        for recording in &self.document.recordings {
            self.restore(recording, cache, capability, epoch);
        }
    }

    pub fn restore_stream(
        &self,
        stream: &Stream,
        cache: &mut prepared::Cache,
        capability: Option<&Capability>,
        epoch: u64,
    ) {
        let Some(identity) = prepared::source_identity(stream) else {
            return;
        };
        if let Some(recording) = self
            .document
            .recordings
            .iter()
            .find(|r| r.identity == identity)
        {
            self.restore(recording, cache, capability, epoch);
        }
    }

    fn restore(
        &self,
        recording: &Recording,
        cache: &mut prepared::Cache,
        capability: Option<&Capability>,
        epoch: u64,
    ) {
        let now = super::now_secs();
        if recording.updated_at > now || now - recording.updated_at >= AGE.as_secs() {
            return;
        }
        let mut review = Review {
            replay: recording.replay.clone(),
            pulls: recording
                .pulls
                .iter()
                .filter_map(|(report, id)| self.document.reports.get(report)?.get(id).cloned())
                .collect(),
            marker_timing: Default::default(),
            marker_fallback: Default::default(),
            content_capability: capability.cloned(),
            content_timing: Default::default(),
        };
        let mut clocks = Vec::new();
        if let Some(cap) = capability {
            let mut seen = BTreeSet::new();
            for pull in &review.pulls {
                let Some(clock) = recording.clocks.get(&pull.report) else {
                    continue;
                };
                let key = Key::new(&review.replay, pull, cap, epoch);
                if let Some(alignment) = clock.alignment(&key) {
                    review
                        .content_timing
                        .insert((pull.report.clone(), pull.id), alignment);
                    if seen.insert(pull.report.clone()) {
                        clocks.push((key, clock.clone()));
                    }
                }
            }
        }
        cache.restore_snapshot(
            recording.path.clone(),
            recording.identity.clone(),
            review,
            (epoch, recording.complete),
            clocks,
            Duration::from_secs(now - recording.updated_at).max(Duration::from_secs(61)),
        );
    }

    pub fn update(
        &mut self,
        access: &Access,
        stream: &Stream,
        review: &Review,
        complete: bool,
        clocks: &[(Key, RecordingClock)],
    ) {
        if access.check().is_err() {
            return;
        }
        self.merge(stream, review, complete, clocks, super::now_secs());
        self.save(access);
    }

    fn merge(
        &mut self,
        stream: &Stream,
        review: &Review,
        complete: bool,
        clocks: &[(Key, RecordingClock)],
        now: u64,
    ) {
        if review.pulls.len() > MAX_PULLS
            || !valid_replay(&review.replay)
            || !review.pulls.iter().all(valid_pull)
        {
            return;
        }
        let Some(identity) = prepared::source_identity(stream) else {
            return;
        };
        let Ok(path) = crate::streams::review_path(stream) else {
            return;
        };
        prune(&mut self.document, now);
        let previous = self
            .document
            .recordings
            .iter()
            .find(|r| r.identity == identity)
            .cloned();
        let mut recording = Recording {
            identity,
            path,
            replay: review.replay.clone(),
            pulls: review
                .pulls
                .iter()
                .map(|p| (p.report.clone(), p.id))
                .collect(),
            clocks: clocks
                .iter()
                .map(|(key, clock)| (key.report.clone(), clock.clone()))
                .collect(),
            complete,
            updated_at: previous.as_ref().map_or(now, |r| r.updated_at),
        };
        let metadata_changed = review.pulls.iter().any(|pull| {
            self.document
                .reports
                .get(&pull.report)
                .and_then(|r| r.get(&pull.id))
                .is_none_or(|old| serde_json::to_vec(old).ok() != serde_json::to_vec(pull).ok())
        });
        if let Some(old) = &previous {
            if old.replay.growing
                && recording.replay.growing
                && recording.replay.available_seconds >= old.replay.available_seconds
            {
                let mut comparison = recording.clone();
                comparison.replay.available_seconds = old.replay.available_seconds;
                if !metadata_changed
                    && serde_json::to_vec(old).ok() == serde_json::to_vec(&comparison).ok()
                {
                    recording = old.clone();
                }
            }
        }
        if metadata_changed
            || previous.as_ref().is_none_or(|old| {
                serde_json::to_vec(old).ok() != serde_json::to_vec(&recording).ok()
            })
        {
            recording.updated_at = now;
        }
        for pull in &review.pulls {
            self.document
                .reports
                .entry(pull.report.clone())
                .or_default()
                .insert(pull.id, pull.clone());
        }
        self.document
            .recordings
            .retain(|r| r.path != recording.path);
        self.document.recordings.push(recording);
        // Stable ordering means revisiting unchanged streams cannot rewrite the file.
        self.document.recordings.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then(a.identity.cmp(&b.identity))
        });
        prune(&mut self.document, now);
    }

    fn save(&mut self, access: &Access) {
        let scope = self.scope.clone();
        self.save_with(|bytes| {
            access.check().is_ok() && crate::protected_cache::save(&scope, bytes).is_ok()
        });
    }

    fn save_with(&mut self, write: impl FnOnce(&[u8]) -> bool) {
        while let Ok(bytes) = serde_json::to_vec(&self.document) {
            if bytes.len() > MAX_BYTES {
                if self.document.recordings.is_empty() {
                    return;
                }
                self.document.recordings.remove(0);
                prune(&mut self.document, super::now_secs());
                continue;
            }
            if bytes != self.saved && write(&bytes) {
                self.saved = bytes;
            }
            return;
        }
    }
}

fn for_session(document: Document, session: &str) -> Document {
    if document.session == session {
        document
    } else {
        Document {
            version: 1,
            session: session.to_owned(),
            ..Default::default()
        }
    }
}

fn valid_replay(replay: &Replay) -> bool {
    super::valid_video(replay)
        && replay.available_seconds <= 7 * 86400
        && replay.started_at.len() <= 64
        && replay
            .timeline_revision
            .as_ref()
            .is_none_or(|r| r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn valid_pull(pull: &Pull) -> bool {
    super::report_code(&pull.report)
        && pull.id > 0
        && pull.name.len() <= 400
        && pull.report_start_ms > 0
        && pull.start_ms >= pull.report_start_ms
        && pull.end_ms > pull.start_ms
        && pull
            .end_ms
            .checked_sub(pull.report_start_ms)
            .is_some_and(|span| span <= 7 * super::DAY)
        && pull
            .remaining
            .is_none_or(|n| n.is_finite() && (0.0..=100.0).contains(&n))
}
fn decode(bytes: &[u8]) -> Option<Document> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let d: Document = serde_json::from_slice(bytes).ok()?;
    if d.version != 1
        || d.session.len() != 64
        || d.recordings.len() > MAX_RECORDINGS
        || d.reports.len() > MAX_REPORTS
        || d.reports.values().map(BTreeMap::len).sum::<usize>() > MAX_PULLS
        || d.recordings.iter().any(|r| {
            r.identity.len() > 320
                || r.path.len() > 200
                || !r.path.starts_with("/v1/streams/")
                || !valid_replay(&r.replay)
                || r.pulls.len() > MAX_PULLS
                || r.clocks.len() > MAX_REPORTS
                || r.pulls
                    .iter()
                    .any(|(report, id)| d.reports.get(report).and_then(|p| p.get(id)).is_none())
        })
        || d.reports.iter().any(|(report, pulls)| {
            pulls
                .iter()
                .any(|(id, p)| p.report != *report || p.id != *id || !valid_pull(p))
        })
    {
        return None;
    }
    Some(d)
}
fn prune(d: &mut Document, now: u64) {
    d.recordings
        .retain(|r| r.updated_at <= now && now - r.updated_at < AGE.as_secs());
    loop {
        let wanted: BTreeSet<_> = d
            .recordings
            .iter()
            .flat_map(|r| r.pulls.iter().cloned())
            .collect();
        d.reports.retain(|report, pulls| {
            pulls.retain(|id, _| wanted.contains(&(report.clone(), *id)));
            !pulls.is_empty()
        });
        if d.recordings.len() <= MAX_RECORDINGS
            && d.reports.len() <= MAX_REPORTS
            && d.reports.values().map(BTreeMap::len).sum::<usize>() <= MAX_PULLS
        {
            break;
        }
        if d.recordings.is_empty() {
            break;
        }
        d.recordings.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::{Provider, Status};

    fn fixture() -> (Cache, Stream, Review) {
        let document = Document {
            version: 1,
            session: "a".repeat(64),
            ..Default::default()
        };
        let saved = serde_json::to_vec(&document).unwrap();
        let cache = Cache {
            scope: "fixture-only".into(),
            document,
            saved,
        };
        let stream = Stream {
            user_id: "101".into(),
            name: "Fixture".into(),
            raid_role: None,
            provider: Provider::Twitch,
            channel_id: "123".into(),
            url: "https://www.twitch.tv/fixture".into(),
            status: Status::Live,
            broadcast_state: None,
            recording_id: None,
            replay_start_ms: Some(1_790_000_000_000),
            replay_end_ms: None,
        };
        let replay = Replay {
            provider: Provider::Twitch,
            video_id: "1234567890".into(),
            broadcast_id: "12345".into(),
            started_at: "2026-09-17T17:00:00Z".into(),
            timeline_revision: Some("a".repeat(64)),
            available_seconds: 3600,
            growing: true,
        };
        let pull = Pull {
            report: "abcdefghijklmnop".into(),
            id: 1,
            encounter: 3135,
            difficulty: 5,
            report_start_ms: 1_790_000_000_000,
            remaining: Some(75.0),
            name: "Boss".into(),
            kill: false,
            last_phase: None,
            last_phase_is_intermission: false,
            start_ms: 1_790_000_060_000,
            end_ms: 1_790_000_180_000,
            seconds: 60,
        };
        let review = Review {
            replay,
            pulls: vec![pull],
            marker_timing: Default::default(),
            marker_fallback: Default::default(),
            content_capability: None,
            content_timing: Default::default(),
        };
        (cache, stream, review)
    }

    #[test]
    fn repeated_reads_and_live_duration_do_not_write_but_pull_updates_replace_one_row() {
        let (mut cache, stream, mut review) = fixture();
        let now = super::super::now_secs();
        let mut writes = 0;
        cache.merge(&stream, &review, true, &[], now);
        cache.save_with(|_| {
            writes += 1;
            true
        });
        assert_eq!(writes, 1);
        for second in 1..100 {
            review.replay.available_seconds += 15;
            cache.merge(&stream, &review, true, &[], now + second);
            cache.save_with(|_| {
                writes += 1;
                true
            });
        }
        assert_eq!(writes, 1);
        review.pulls[0].kill = true;
        review.pulls[0].remaining = Some(0.0);
        cache.merge(&stream, &review, true, &[], now + 100);
        cache.save_with(|_| {
            writes += 1;
            true
        });
        assert_eq!(writes, 2);
        assert_eq!(cache.document.recordings.len(), 1);
        assert_eq!(cache.document.reports.len(), 1);
        assert_eq!(cache.document.reports["abcdefghijklmnop"].len(), 1);
        assert!(cache.document.reports["abcdefghijklmnop"][&1].kill);
    }

    #[test]
    fn five_streams_share_one_report_and_restart_restores_stale_pulls_without_transport() {
        let (mut cache, mut stream, review) = fixture();
        let now = super::super::now_secs();
        for id in 101..106 {
            stream.user_id = id.to_string();
            cache.merge(&stream, &review, true, &[], now);
        }
        cache.save_with(|bytes| {
            assert!(bytes.len() < 8192);
            true
        });
        assert_eq!(cache.document.reports.len(), 1);
        assert_eq!(cache.document.reports["abcdefghijklmnop"].len(), 1);
        let recovered = Cache {
            scope: cache.scope,
            document: decode(&cache.saved).unwrap(),
            saved: cache.saved,
        };
        let mut hot = prepared::Cache::default();
        recovered.hydrate(&mut hot, None, 4);
        let entry = hot.for_display(&stream).unwrap();
        assert_eq!(entry.review.pulls.len(), 1);
        assert_eq!(entry.status.0, 4);
        assert!(!hot.contains(&stream), "Disk snapshots must still refresh");
        stream.replay_start_ms = Some(1_790_000_001_000);
        assert!(hot.for_display(&stream).is_none());
        hot.set_connected(false);
        assert!(hot.for_display(&stream).is_none());
    }

    #[test]
    fn cache_caps_counts_bytes_expiry_and_future_timestamps_without_integer_overflow() {
        let (mut cache, mut stream, mut review) = fixture();
        let now = super::super::now_secs();
        for id in 1..100 {
            stream.user_id = id.to_string();
            cache.merge(&stream, &review, true, &[], now);
        }
        assert_eq!(cache.document.recordings.len(), MAX_RECORDINGS);
        // Worst-case bounded labels force byte eviction before any disk write.
        review.pulls = (1..=MAX_PULLS as u64)
            .map(|id| {
                let mut p = review.pulls[0].clone();
                p.id = id;
                p.name = "界".repeat(100);
                p
            })
            .collect();
        cache.merge(&stream, &review, true, &[], now);
        cache.save_with(|bytes| {
            assert!(bytes.len() <= MAX_BYTES);
            true
        });
        assert!(
            cache
                .document
                .reports
                .values()
                .map(BTreeMap::len)
                .sum::<usize>()
                <= MAX_PULLS
        );
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_none());
        review.pulls[0].end_ms = i64::MAX;
        assert!(!valid_pull(&review.pulls[0]));
        review.pulls[0].report_start_ms = i64::MIN;
        assert!(!valid_pull(&review.pulls[0]));
        for record in &mut cache.document.recordings {
            record.updated_at = u64::MAX;
        }
        prune(&mut cache.document, now);
        assert!(cache.document.recordings.is_empty());
        assert!(cache.document.reports.is_empty());
        let (mut cache, stream, review) = fixture();
        cache.merge(&stream, &review, true, &[], now - AGE.as_secs());
        prune(&mut cache.document, now);
        assert!(cache.document.recordings.is_empty());
        assert!(cache.document.reports.is_empty());
    }

    #[test]
    fn persisted_clocks_rebind_to_current_authorization_and_expiry_keeps_pull_metadata() {
        let (mut cache, mut stream, mut review) = fixture();
        let (replay, pull, cap, ticket) = crate::content_alignment::test_ticket();
        stream.provider = replay.provider.clone();
        stream.recording_id = Some(replay.video_id.clone());
        review.replay = replay;
        review.pulls = vec![pull];
        review.content_capability = Some(cap.clone());
        let clock = crate::content_alignment::test_recording_clock();
        cache.merge(
            &stream,
            &review,
            true,
            &[(ticket.key, clock)],
            super::super::now_secs(),
        );
        let bytes = serde_json::to_vec(&cache.document).unwrap();
        cache.document = decode(&bytes).unwrap();
        let mut hot = prepared::Cache::default();
        cache.hydrate(&mut hot, Some(&cap), 7);
        let entry = hot.for_display(&stream).unwrap();
        let aligned = entry
            .review
            .content_alignment(&entry.review.pulls[0])
            .unwrap();
        assert_eq!(aligned.key.auth_epoch, 7);
        assert_eq!(aligned.result.video_seconds, 15.25);
        for clock in cache.document.recordings[0].clocks.values_mut() {
            clock.expires_at = 1;
        }
        let mut hot = prepared::Cache::default();
        cache.hydrate(&mut hot, Some(&cap), 8);
        let entry = hot.for_display(&stream).unwrap();
        assert_eq!(entry.review.pulls.len(), 1);
        assert!(entry.clocks.is_empty());
        assert!(entry
            .review
            .content_alignment(&entry.review.pulls[0])
            .is_none());
    }

    #[test]
    fn reconnecting_another_wcl_account_cannot_restore_the_old_snapshot() {
        let (mut cache, stream, review) = fixture();
        cache.merge(&stream, &review, true, &[], super::super::now_secs());
        let same = for_session(
            decode(&serde_json::to_vec(&cache.document).unwrap()).unwrap(),
            &"a".repeat(64),
        );
        assert_eq!(same.recordings.len(), 1);
        let other = for_session(same, &"b".repeat(64));
        assert!(other.recordings.is_empty());
        assert!(other.reports.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the isolated Secret Service used by CI"]
    fn linux_keyring_roundtrip_review_snapshot_survives_restart_and_refresh_without_duplicate_files(
    ) {
        use sha2::{Digest, Sha256};
        let (_, stream, review) = fixture();
        let config = Config {
            client_id: uuid::Uuid::new_v4().to_string(),
            guild_id: 123,
            user_id: "101".into(),
            discord_guild_id: crate::guild::ADVANCE.into(),
            content_alignment: None,
        };
        let session = Session {
            cache_id: super::super::random(),
            client_id: config.client_id.clone(),
            user_id: config.user_id.clone(),
            access_token: "synthetic-access-token".into(),
            refresh_token: Some("synthetic-refresh-token".into()),
            expires_at: super::super::now_secs() + 3600,
        };
        let access = Access::new(
            "synthetic-discord".into(),
            config.discord_guild_id.clone(),
            config.user_id.clone(),
            crate::guild::generation(),
        );
        let mut cache = Cache::load(&config, &session, &access).unwrap();
        let path = crate::addon::config_dir().unwrap().join(format!(
            "guild-cache-{}.dat",
            hex::encode(Sha256::digest(&cache.scope))
        ));
        let scope = cache.scope.clone();
        cache.update(&access, &stream, &review, true, &[]);
        let ciphertext = std::fs::read(&path).unwrap();
        assert!(!ciphertext.windows(16).any(|w| w == b"abcdefghijklmnop"));
        cache.update(&access, &stream, &review, true, &[]);
        assert_eq!(std::fs::read(&path).unwrap(), ciphertext);
        drop(cache);
        let restored = Cache::load(&config, &session, &access).unwrap();
        assert_eq!(restored.document.recordings.len(), 1);
        let mut client = super::super::Client::new().unwrap();
        client.config = Some(config.clone());
        client.session = Some(session.clone());
        client
            .save_token(
                super::super::Token {
                    access_token: "new-synthetic-access".into(),
                    refresh_token: Some("rotated-synthetic-refresh".into()),
                    expires_in: 3600,
                    token_type: "Bearer".into(),
                },
                session.refresh_token.clone(),
            )
            .unwrap();
        assert_eq!(client.session.as_ref().unwrap().cache_id, session.cache_id);
        assert_eq!(
            Cache::load(&config, client.session.as_ref().unwrap(), &access)
                .unwrap()
                .document
                .recordings
                .len(),
            1
        );
        client.review_cache = Some(restored);
        client.invalidate_review_cache();
        let saved: Session = serde_json::from_slice(
            &super::super::store(&config)
                .unwrap()
                .load()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_ne!(saved.cache_id, session.cache_id);
        assert_eq!(
            saved.expires_at,
            client.session.as_ref().unwrap().expires_at
        );
        let empty = Cache::load(&config, &saved, &access).unwrap();
        assert!(empty.document.recordings.is_empty());
        assert_eq!(empty.scope, scope);
        let mut damaged = std::fs::read(&path).unwrap();
        *damaged.last_mut().unwrap() ^= 1;
        std::fs::write(&path, &damaged).unwrap();
        assert!(Cache::load(&config, &saved, &access).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        std::fs::remove_file(path).unwrap();
        crate::credential_store::Store::new(&format!("guild-cache-key-v1:{scope}"))
            .unwrap()
            .remove()
            .unwrap();
        super::super::store(&config).unwrap().remove().unwrap();
    }

    #[test]
    fn failed_atomic_save_remains_retryable_and_unchanged_success_skips_writes() {
        let (mut cache, stream, review) = fixture();
        cache.merge(&stream, &review, true, &[], super::super::now_secs());
        let before = cache.saved.clone();
        cache.save_with(|_| false);
        assert_eq!(cache.saved, before);
        cache.save_with(|_| true);
        assert_ne!(cache.saved, before);
        cache.save_with(|_| panic!("unchanged snapshot must not write"));
    }
}
