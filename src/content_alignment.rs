//! Authenticated per-fight content alignment, separate from marker calibration.
use crate::{
    guild, streams,
    warcraftlogs::{
        boss_signature::{BossSignature, SCHEMA},
        Pull, Replay,
    },
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

const PATH: &str = "/v1/streams/review/content-jobs";
const INVALID: &str = "The video alignment service returned an invalid response.";
const CANCELED: &str = "Video alignment was canceled.";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Capability {
    pub schema: String,
    pub algorithm_revision: String,
}
impl Capability {
    pub fn valid(&self) -> bool {
        self.schema == SCHEMA && digest(&self.algorithm_revision)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub provider: streams::Provider,
    pub video_id: String,
    pub broadcast_id: String,
    pub started_at: String,
    pub timeline_revision: Option<String>,
    pub available_seconds: u64,
    pub report: String,
    pub pull_id: u64,
    pub encounter: u64,
    pub difficulty: u64,
    pub report_start_ms: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub algorithm_revision: String,
    pub guild_generation: u64,
    pub auth_epoch: u64,
}
impl Key {
    pub fn new(replay: &Replay, pull: &Pull, capability: &Capability, auth_epoch: u64) -> Self {
        Self {
            provider: replay.provider.clone(),
            video_id: replay.video_id.clone(),
            broadcast_id: replay.broadcast_id.clone(),
            started_at: replay.started_at.clone(),
            timeline_revision: replay.timeline_revision.clone(),
            available_seconds: replay.available_seconds,
            report: pull.report.clone(),
            pull_id: pull.id,
            encounter: pull.encounter,
            difficulty: pull.difficulty,
            report_start_ms: pull.report_start_ms,
            start_ms: pull.start_ms,
            end_ms: pull.end_ms,
            algorithm_revision: capability.algorithm_revision.clone(),
            guild_generation: guild::request_generation(),
            auth_epoch,
        }
    }
    pub fn duration(&self) -> f64 {
        (self.end_ms - self.start_ms) as f64 / 1000.0
    }
    pub fn matches(&self, replay: &Replay, pull: &Pull, capability: &Capability) -> bool {
        let current = Self::new(replay, pull, capability, self.auth_epoch);
        *self == current
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Timeline {
    pub provider: streams::Provider,
    pub video_id: String,
    pub revision: String,
    pub duration_seconds: f64,
    pub raw_started_at_ms: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Scope {
    pub guild_id: String,
    pub member_hash: String,
    pub provider: streams::Provider,
    pub video_id: String,
    pub report: String,
    pub pull_id: u64,
    pub signature_revision: String,
    pub algorithm_revision: String,
    pub timeline_revision: String,
    pub timeline_hash: String,
    pub timeline: Timeline,
    pub duration_seconds: f64,
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    Running,
    CleanupPending,
    Complete,
    Failed,
    Canceled,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Coverage {
    pub fight_start_seconds: f64,
    pub fight_end_seconds: f64,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResultData {
    pub video_seconds: f64,
    pub seek_video_seconds: f64,
    pub clipped_start: bool,
    pub uncertainty_seconds: f64,
    pub coverage: Coverage,
    pub evidence_hash: String,
    pub method_version: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Progress {
    pub stage: String,
    pub completed_units: u64,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Job {
    pub id: String,
    pub status: Status,
    pub cleanup_pending: bool,
    pub progress: Progress,
    pub error: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub result: Option<ResultData>,
    pub scope: Option<Scope>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    job: Job,
}

#[derive(Clone, Debug)]
pub(crate) struct Ticket {
    pub key: Key,
    pub guild_id: String,
    pub member_hash: String,
    pub job: Job,
}
#[derive(Clone, Debug)]
pub(crate) struct Alignment {
    pub key: Key,
    pub result: ResultData,
    pub timeline: Timeline,
    pub signature_revision: String,
    pub timeline_hash: String,
    pub expires_at: i64,
}
impl Alignment {
    pub fn matches(&self, replay: &Replay, pull: &Pull, capability: &Capability) -> bool {
        self.key.matches(replay, pull, capability) && self.expires_at > now_ms()
    }
    pub fn seek(&self, elapsed: f64) -> Option<f64> {
        let seconds = self.result.video_seconds + elapsed;
        (elapsed.is_finite()
            && elapsed >= self.result.coverage.fight_start_seconds
            && elapsed <= self.result.coverage.fight_end_seconds
            && seconds >= 0.0
            && seconds < self.timeline.duration_seconds)
            .then_some(seconds)
    }
    pub fn version(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        for field in [
            &self.signature_revision,
            &self.timeline_hash,
            &self.result.evidence_hash,
            &self.key.algorithm_revision,
        ] {
            hash.update(field.as_bytes());
        }
        for value in [
            self.result.video_seconds,
            self.result.coverage.fight_start_seconds,
            self.result.coverage.fight_end_seconds,
        ] {
            hash.update(value.to_bits().to_le_bytes());
        }
        hash.finalize().into()
    }
    pub fn first_video_seconds(&self) -> f64 {
        self.result.video_seconds + self.result.coverage.fight_start_seconds
    }
}

/// A shared fixed clock for a continuous recording and one exact WCL report.
/// The server retains private source inputs; viewers receive only timing.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecordingClock {
    pub report_start_ms: i64,
    pub report_seconds: f64,
    pub video_seconds: f64,
    pub uncertainty_seconds: f64,
    pub evidence_hash: String,
    pub algorithm_revision: String,
    pub timeline: Timeline,
    pub timeline_hash: String,
    pub expires_at: i64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordingLookup {
    pub clock: Option<RecordingClock>,
    pub pending: bool,
    pub conflict: bool,
}
impl Key {
    pub fn same_recording_report(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.video_id == other.video_id
            && self.broadcast_id == other.broadcast_id
            && self.started_at == other.started_at
            && self.timeline_revision == other.timeline_revision
            && self.available_seconds == other.available_seconds
            && self.report == other.report
            && self.report_start_ms == other.report_start_ms
            && self.algorithm_revision == other.algorithm_revision
            && self.guild_generation == other.guild_generation
            && self.auth_epoch == other.auth_epoch
    }
}
impl RecordingClock {
    pub fn alignment(&self, key: &Key) -> Option<Alignment> {
        let raw_start = if key.started_at.is_empty() {
            None
        } else {
            Some(
                (time::OffsetDateTime::parse(
                    &key.started_at,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok()?
                .unix_timestamp_nanos()
                    / 1_000_000) as i64,
            )
        };
        if self.timeline.raw_started_at_ms != raw_start
            || !finite(self.timeline.duration_seconds, 0.001, 604800.0)
            || self.report_start_ms != key.report_start_ms
            || self.algorithm_revision != key.algorithm_revision
            || self.timeline.provider != key.provider
            || self.timeline.video_id != key.video_id
            || key.timeline_revision.as_deref() != Some(self.timeline.revision.as_str())
            || self.timeline.duration_seconds != key.available_seconds as f64
            || !digest(&self.timeline_hash)
            || !digest(&self.evidence_hash)
            || !finite(self.report_seconds, 0.0, 7.0 * 86400.0)
            || !finite(self.video_seconds, -3600.0, self.timeline.duration_seconds)
            || !finite(self.uncertainty_seconds, 0.0, 0.999999)
            || self.expires_at <= now_ms()
            || key.end_ms <= key.start_ms
        {
            return None;
        }
        let origin = self.video_seconds
            + key.start_ms.checked_sub(key.report_start_ms)? as f64 / 1000.0
            - self.report_seconds;
        let start = (-origin).max(0.0);
        let end = key.duration().min(self.timeline.duration_seconds - origin);
        if !origin.is_finite() || end <= start {
            return None;
        }
        Some(Alignment {
            key: key.clone(),
            timeline: self.timeline.clone(),
            timeline_hash: self.timeline_hash.clone(),
            // This is a recording-clock measurement, not another submitted signature.
            signature_revision: self.evidence_hash.clone(),
            expires_at: self.expires_at,
            result: ResultData {
                video_seconds: origin,
                seek_video_seconds: origin.max(0.0),
                clipped_start: origin < 0.0,
                uncertainty_seconds: self.uncertainty_seconds,
                coverage: Coverage {
                    fight_start_seconds: start,
                    fight_end_seconds: end,
                },
                evidence_hash: self.evidence_hash.clone(),
                method_version: self.algorithm_revision.clone(),
            },
        })
    }
}
pub(crate) fn recording_lookup(
    access: &guild::Access,
    key: &Key,
    cancel: &AtomicBool,
) -> Result<RecordingLookup, String> {
    current(access, key, cancel)?;
    let bytes = streams::request(
        Method::POST,
        &format!("{PATH}/recording"),
        access,
        Some(
            serde_json::json!({"provider":key.provider,"videoId":key.video_id,
            "report":key.report,"reportStartMs":key.report_start_ms}),
        ),
    )
    .map_err(|e| e.message)?;
    if bytes.len() > 32 * 1024 {
        return Err(INVALID.into());
    }
    let result: RecordingLookup = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
    current(access, key, cancel)?;
    if result.conflict && (result.clock.is_some() || result.pending) {
        return Err(INVALID.into());
    }
    if let Some(clock) = &result.clock {
        // A non-overlapping pull is a cache miss, but malformed identity is not.
        if !finite(clock.report_seconds, 0.0, 7.0 * 86400.0) {
            return Err(INVALID.into());
        }
        let mut probe = key.clone();
        probe.start_ms = key
            .report_start_ms
            .checked_add((clock.report_seconds * 1000.0).round() as i64)
            .ok_or(INVALID)?;
        probe.end_ms = probe.start_ms.saturating_add(3_600_000);
        if clock.alignment(&probe).is_none() {
            return Err(INVALID.into());
        }
    }
    Ok(result)
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn finite(value: f64, low: f64, high: f64) -> bool {
    value.is_finite() && value >= low && value <= high
}
fn member_hash(access: &guild::Access) -> String {
    format!("{:x}", Sha256::digest(access.user_id.as_bytes()))
}
fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn current(access: &guild::Access, key: &Key, cancel: &AtomicBool) -> Result<(), String> {
    access.check()?;
    guild::ensure_current(key.guild_generation)?;
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELED.into())
    } else {
        Ok(())
    }
}

impl Job {
    fn validate(
        &self,
        key: &Key,
        guild_id: &str,
        member: &str,
        pinned: Option<&Self>,
    ) -> Result<(), String> {
        if !digest(&self.id)
            || self.created_at <= 0
            || self.expires_at <= self.created_at
            || self.expires_at - self.created_at > 7 * 86_400_000
            || self.expires_at <= now_ms()
            || !["media", "evidence", "match", "validate"].contains(&self.progress.stage.as_str())
            || self.progress.completed_units > 1024
            || self.error.as_ref().is_some_and(|e| {
                e.len() > 128
                    || !e
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
        {
            return Err(INVALID.into());
        }
        if let Some(previous) = pinned {
            if self.id != previous.id
                || self.created_at != previous.created_at
                || self.expires_at != previous.expires_at
                || self.progress.completed_units < previous.progress.completed_units
                || (self.status != Status::Canceled && self.scope != previous.scope)
            {
                return Err(INVALID.into());
            }
        }
        if self.status == Status::Canceled {
            return if self.scope.is_none() && self.result.is_none() {
                Ok(())
            } else {
                Err(INVALID.into())
            };
        }
        let scope = self.scope.as_ref().ok_or(INVALID)?;
        let timeline = &scope.timeline;
        let raw_started_at_ms = if key.started_at.is_empty() {
            None
        } else {
            Some(
                (time::OffsetDateTime::parse(
                    &key.started_at,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| INVALID)?
                .unix_timestamp_nanos()
                    / 1_000_000) as i64,
            )
        };
        if scope.guild_id != guild_id
            || scope.member_hash != member
            || scope.provider != key.provider
            || scope.video_id != key.video_id
            || scope.report != key.report
            || scope.pull_id != key.pull_id
            || scope.duration_seconds != key.duration()
            || scope.algorithm_revision != key.algorithm_revision
            || !digest(&scope.signature_revision)
            || key.timeline_revision.as_deref() != Some(scope.timeline_revision.as_str())
            || !digest(&scope.algorithm_revision)
            || !digest(&scope.timeline_revision)
            || !digest(&scope.timeline_hash)
            || timeline.provider != key.provider
            || timeline.video_id != key.video_id
            || timeline.revision != scope.timeline_revision
            || timeline.duration_seconds != key.available_seconds as f64
            || timeline.raw_started_at_ms != raw_started_at_ms
            || !finite(timeline.duration_seconds, 0.001, 604800.0)
            || timeline
                .raw_started_at_ms
                .is_some_and(|t| !(1_500_000_000_000..=4_000_000_000_000).contains(&t))
        {
            return Err(INVALID.into());
        }
        if self.status == Status::Complete && !self.cleanup_pending {
            let result = self.result.as_ref().ok_or(INVALID)?;
            let origin = result.video_seconds;
            let coverage = &result.coverage;
            if self.error.is_some()
                || self.progress.stage != "validate"
                || !finite(origin, -key.duration(), timeline.duration_seconds)
                || origin + key.duration() <= 0.0
                || origin >= timeline.duration_seconds
                || !finite(result.uncertainty_seconds, 0.0, 1.0)
                || result.uncertainty_seconds >= 1.0
                || result.seek_video_seconds != origin.max(0.0)
                || result.clipped_start != (origin < 0.0)
                || !digest(&result.evidence_hash)
                || result.method_version != key.algorithm_revision
                || !finite(
                    coverage.fight_start_seconds,
                    (-origin).max(0.0),
                    key.duration(),
                )
                || !finite(
                    coverage.fight_end_seconds,
                    coverage.fight_start_seconds,
                    key.duration().min(timeline.duration_seconds - origin),
                )
                || coverage.fight_end_seconds <= coverage.fight_start_seconds
            {
                return Err(INVALID.into());
            }
        } else if self.result.is_some() {
            return Err(INVALID.into());
        }
        Ok(())
    }
}
impl Ticket {
    pub fn alignment(&self) -> Option<Alignment> {
        self.job
            .validate(&self.key, &self.guild_id, &self.member_hash, None)
            .ok()?;
        if self.job.status != Status::Complete || self.job.cleanup_pending {
            return None;
        }
        let scope = self.job.scope.as_ref()?;
        Some(Alignment {
            key: self.key.clone(),
            result: self.job.result.clone()?,
            timeline: scope.timeline.clone(),
            signature_revision: scope.signature_revision.clone(),
            timeline_hash: scope.timeline_hash.clone(),
            expires_at: self.job.expires_at,
        })
    }
    pub fn permits_marker_backup(&self) -> bool {
        self.job.status == Status::Failed
            && !self.job.cleanup_pending
            && self
                .job
                .validate(&self.key, &self.guild_id, &self.member_hash, None)
                .is_ok()
    }
    pub fn expired(&self) -> bool {
        self.job.expires_at <= now_ms()
    }
    pub fn pending(&self) -> bool {
        matches!(
            self.job.status,
            Status::Pending | Status::Running | Status::CleanupPending
        ) || self.job.cleanup_pending
    }
}
fn parse(bytes: &[u8]) -> Result<Job, String> {
    if bytes.len() > 32 * 1024 {
        return Err(INVALID.into());
    }
    serde_json::from_slice::<Envelope>(bytes)
        .map(|e| e.job)
        .map_err(|_| INVALID.into())
}

pub(crate) fn submit(
    access: &guild::Access,
    key: Key,
    signature: &BossSignature,
    cancel: &AtomicBool,
) -> Result<Ticket, String> {
    current(access, &key, cancel)?;
    if signature.report != key.report
        || signature.pull_id != key.pull_id
        || signature.report_start_ms != key.report_start_ms
        || signature.fight_start_ms != key.start_ms - key.report_start_ms
        || signature.fight_end_ms != key.end_ms - key.report_start_ms
        || signature.encounter_id != key.encounter
        || signature.difficulty != key.difficulty
        || !signature.complete
    {
        return Err(INVALID.into());
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Submission<'a> {
        provider: &'a streams::Provider,
        video_id: &'a str,
        signature: &'a BossSignature,
    }
    let body = serde_json::to_value(Submission {
        provider: &key.provider,
        video_id: &key.video_id,
        signature,
    })
    .map_err(|_| INVALID)?;
    let response =
        streams::request(Method::POST, PATH, access, Some(body)).map_err(|e| e.message)?;
    let job = parse(&response)?;
    let member = member_hash(access);
    job.validate(&key, &access.guild_id, &member, None)?;
    if let Err(error) = current(access, &key, cancel) {
        return Err(error);
    }
    Ok(Ticket {
        key,
        guild_id: access.guild_id.clone(),
        member_hash: member,
        job,
    })
}
pub(crate) fn poll(
    access: &guild::Access,
    ticket: &Ticket,
    cancel: &AtomicBool,
) -> Result<Ticket, String> {
    if access.guild_id != ticket.guild_id || member_hash(access) != ticket.member_hash {
        return Err(CANCELED.into());
    }
    current(access, &ticket.key, cancel)?;
    let response = streams::request(
        Method::GET,
        &format!("{PATH}/{}", ticket.job.id),
        access,
        None,
    )
    .map_err(|e| e.message)?;
    let job = parse(&response)?;
    job.validate(
        &ticket.key,
        &ticket.guild_id,
        &ticket.member_hash,
        Some(&ticket.job),
    )?;
    current(access, &ticket.key, cancel)?;
    Ok(Ticket {
        job,
        ..ticket.clone()
    })
}

#[cfg(test)]
pub(crate) fn test_ticket() -> (Replay, Pull, Capability, Ticket) {
    let capability = Capability {
        schema: SCHEMA.into(),
        algorithm_revision: "a".repeat(64),
    };
    let replay = Replay {
        provider: streams::Provider::Youtube,
        video_id: "abcDEF_12-3".into(),
        broadcast_id: "abcDEF_12-3".into(),
        started_at: String::new(),
        timeline_revision: Some("b".repeat(64)),
        available_seconds: 600,
    };
    let pull = Pull {
        report: "AbCdEfGhIjKlMnOp".into(),
        id: 21,
        encounter: 100,
        difficulty: 5,
        report_start_ms: 1_700_000_000_000,
        remaining: None,
        name: "Boss".into(),
        kill: true,
        last_phase: None,
        last_phase_is_intermission: false,
        start_ms: 1_700_000_010_000,
        end_ms: 1_700_000_190_000,
        seconds: 0,
    };
    let key = Key::new(&replay, &pull, &capability, 0);
    let member = format!("{:x}", Sha256::digest(b"123"));
    let timeline = Timeline {
        provider: key.provider.clone(),
        video_id: key.video_id.clone(),
        revision: "b".repeat(64),
        duration_seconds: 600.0,
        raw_started_at_ms: None,
    };
    let scope = Scope {
        guild_id: guild::ADVANCE.into(),
        member_hash: member.clone(),
        provider: key.provider.clone(),
        video_id: key.video_id.clone(),
        report: key.report.clone(),
        pull_id: key.pull_id,
        signature_revision: "c".repeat(64),
        algorithm_revision: capability.algorithm_revision.clone(),
        timeline_revision: timeline.revision.clone(),
        timeline_hash: "d".repeat(64),
        timeline,
        duration_seconds: key.duration(),
    };
    let result = ResultData {
        video_seconds: 15.25,
        seek_video_seconds: 15.25,
        clipped_start: false,
        uncertainty_seconds: 0.25,
        coverage: Coverage {
            fight_start_seconds: 0.0,
            fight_end_seconds: 180.0,
        },
        evidence_hash: "e".repeat(64),
        method_version: capability.algorithm_revision.clone(),
    };
    let job = Job {
        id: "f".repeat(64),
        status: Status::Complete,
        cleanup_pending: false,
        progress: Progress {
            stage: "validate".into(),
            completed_units: 4,
        },
        error: None,
        created_at: now_ms() - 1000,
        expires_at: now_ms() + 86_400_000,
        result: Some(result),
        scope: Some(scope),
    };
    (
        replay,
        pull,
        capability,
        Ticket {
            key,
            guild_id: guild::ADVANCE.into(),
            member_hash: member,
            job,
        },
    )
}

#[cfg(test)]
pub(crate) fn test_recording_clock() -> RecordingClock {
    let (_, _, _, ticket) = test_ticket();
    let scope = ticket.job.scope.unwrap();
    let result = ticket.job.result.unwrap();
    RecordingClock {
        report_start_ms: ticket.key.report_start_ms,
        report_seconds: (ticket.key.start_ms - ticket.key.report_start_ms) as f64 / 1000.0,
        video_seconds: result.video_seconds,
        uncertainty_seconds: result.uncertainty_seconds,
        evidence_hash: result.evidence_hash,
        algorithm_revision: ticket.key.algorithm_revision,
        timeline: scope.timeline,
        timeline_hash: scope.timeline_hash,
        expires_at: ticket.job.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recording_clock_maps_later_pulls_and_clips_to_real_video_bounds() {
        let (_, _, _, ticket) = test_ticket();
        let clock = test_recording_clock();
        let mut later = ticket.key.clone();
        later.pull_id += 1;
        later.start_ms += 60_000;
        later.end_ms += 60_000;
        assert!(ticket.key.same_recording_report(&later));
        assert_eq!(clock.alignment(&later).unwrap().seek(11.125), Some(86.375));
        later.start_ms += 500_000;
        later.end_ms += 500_000;
        let end = clock.alignment(&later).unwrap();
        assert_eq!(end.seek(20.0), Some(595.25));
        assert_eq!(end.seek(30.0), None);
        later.start_ms += 600_000;
        later.end_ms += 600_000;
        assert!(clock.alignment(&later).is_none());
        let mut clipped = clock.clone();
        clipped.video_seconds = -12.5;
        let alignment = clipped.alignment(&ticket.key).unwrap();
        assert_eq!(alignment.seek(12.0), None);
        assert_eq!(alignment.seek(12.5), Some(0.0));
    }
    #[test]
    fn recording_clock_scope_and_invalid_results_never_cross_recordings_or_accounts() {
        let (_, _, _, ticket) = test_ticket();
        for change in 0..8 {
            let mut key = ticket.key.clone();
            match change {
                0 => key.video_id = "different12".into(),
                1 => key.report = "OtherReport12345".into(),
                2 => key.report_start_ms += 1,
                3 => key.timeline_revision = Some("0".repeat(64)),
                4 => key.algorithm_revision = "0".repeat(64),
                5 => key.guild_generation += 1,
                6 => key.auth_epoch += 1,
                _ => key.available_seconds += 1,
            }
            assert!(!key.same_recording_report(&ticket.key));
        }
        for change in 0..7 {
            let mut clock = test_recording_clock();
            match change {
                0 => clock.video_seconds = f64::NAN,
                1 => clock.uncertainty_seconds = 1.0,
                2 => clock.expires_at = 0,
                3 => clock.timeline.video_id = "different12".into(),
                4 => clock.report_start_ms += 1,
                5 => clock.report_seconds = f64::INFINITY,
                _ => clock.timeline.duration_seconds += 1.0,
            }
            assert!(clock.alignment(&ticket.key).is_none());
        }
    }
    fn valid(ticket: &Ticket) -> bool {
        ticket
            .job
            .validate(&ticket.key, &ticket.guild_id, &ticket.member_hash, None)
            .is_ok()
    }

    #[test]
    fn absent_provider_utc_is_valid_and_never_used_for_relative_seek() {
        let (replay, pull, cap, ticket) = test_ticket();
        assert!(replay.start_ms().is_err());
        assert!(valid(&ticket));
        let alignment = ticket.alignment().unwrap();
        assert!(alignment.matches(&replay, &pull, &cap));
        assert_eq!(alignment.seek(11.125), Some(26.375));
        assert_eq!(alignment.seek(-0.001), None);
        assert_eq!(alignment.seek(180.001), None);
    }
    #[test]
    fn clipped_fight_keeps_negative_origin_but_seeks_only_covered_media() {
        let (_, _, _, mut ticket) = test_ticket();
        let result = ticket.job.result.as_mut().unwrap();
        result.video_seconds = -12.5;
        result.seek_video_seconds = 0.0;
        result.clipped_start = true;
        result.coverage.fight_start_seconds = 12.5;
        assert!(valid(&ticket));
        let alignment = ticket.alignment().unwrap();
        assert_eq!(alignment.first_video_seconds(), 0.0);
        assert_eq!(alignment.seek(12.0), None);
        assert_eq!(alignment.seek(12.5), Some(0.0));
        assert_eq!(alignment.seek(32.5), Some(20.0));
        ticket
            .job
            .result
            .as_mut()
            .unwrap()
            .coverage
            .fight_start_seconds = 0.0;
        assert!(!valid(&ticket));
    }
    #[test]
    fn exact_identity_and_subsecond_result_are_required() {
        for field in 0..14 {
            let (_, _, _, mut ticket) = test_ticket();
            let scope = ticket.job.scope.as_mut().unwrap();
            match field {
                0 => scope.guild_id = "123".into(),
                1 => scope.member_hash = "0".repeat(64),
                2 => scope.video_id = "otherVID_12".into(),
                3 => scope.report = "OtherReport00001".into(),
                4 => scope.pull_id += 1,
                5 => scope.duration_seconds += 0.001,
                6 => scope.algorithm_revision = "0".repeat(64),
                7 => scope.signature_revision = "bad".into(),
                8 => scope.timeline_revision = "0".repeat(64),
                9 => scope.timeline.video_id = "otherVID_12".into(),
                10 => scope.timeline.duration_seconds = f64::NAN,
                11 => scope.timeline.raw_started_at_ms = Some(1),
                12 => ticket.job.result.as_mut().unwrap().uncertainty_seconds = 1.0,
                _ => ticket.job.result.as_mut().unwrap().method_version = "0".repeat(64),
            }
            assert!(!valid(&ticket), "identity mutation {field} accepted");
            assert!(ticket.alignment().is_none());
        }
    }
    #[test]
    fn first_job_response_must_match_captured_provider_duration_and_raw_utc() {
        let (_, _, _, mut ticket) = test_ticket();
        ticket.job.scope.as_mut().unwrap().timeline.duration_seconds += 1.0;
        assert!(!valid(&ticket));
        ticket.job.scope.as_mut().unwrap().timeline.duration_seconds -= 1.0;
        ticket
            .job
            .scope
            .as_mut()
            .unwrap()
            .timeline
            .raw_started_at_ms = Some(1_700_000_000_000);
        assert!(!valid(&ticket));
        ticket.key.started_at = "2023-11-14T22:13:20Z".into();
        assert!(valid(&ticket));
        ticket.key.started_at = "not-a-provider-date".into();
        assert!(!valid(&ticket));
    }
    #[test]
    fn polls_cannot_change_signature_media_descriptor_or_job_identity() {
        let (_, _, _, ticket) = test_ticket();
        for field in 0..7 {
            let mut job = ticket.job.clone();
            match field {
                0 => job.id = "0".repeat(64),
                1 => job.scope.as_mut().unwrap().signature_revision = "0".repeat(64),
                2 => job.scope.as_mut().unwrap().timeline_hash = "0".repeat(64),
                3 => job.scope.as_mut().unwrap().timeline.duration_seconds += 1.0,
                4 => {
                    job.scope.as_mut().unwrap().timeline.raw_started_at_ms = Some(1_700_000_000_000)
                }
                5 => job.expires_at += 1,
                _ => job.progress.completed_units = 0,
            }
            assert!(job
                .validate(
                    &ticket.key,
                    &ticket.guild_id,
                    &ticket.member_hash,
                    Some(&ticket.job)
                )
                .is_err());
        }
    }
    #[test]
    fn pending_failed_cleanup_and_canceled_jobs_never_grant_timing() {
        let (_, _, _, ticket) = test_ticket();
        for status in [
            Status::Pending,
            Status::Running,
            Status::CleanupPending,
            Status::Failed,
            Status::Canceled,
        ] {
            let mut other = ticket.clone();
            other.job.status = status;
            assert!(!valid(&other));
            other.job.result = None;
            if status == Status::Canceled {
                other.job.scope = None;
            }
            assert!(valid(&other));
            assert!(other.alignment().is_none());
        }
        let mut other = ticket;
        other.job.cleanup_pending = true;
        assert!(!valid(&other));
        other.job.result = None;
        assert!(valid(&other));
        assert!(other.alignment().is_none());
    }
    #[test]
    fn coverage_and_derived_clipping_fields_cannot_lie() {
        for field in 0..9 {
            let (_, _, _, mut ticket) = test_ticket();
            let result = ticket.job.result.as_mut().unwrap();
            match field {
                0 => result.coverage.fight_start_seconds = -1.0,
                1 => result.coverage.fight_end_seconds = 181.0,
                2 => result.coverage.fight_end_seconds = 0.0,
                3 => result.clipped_start = true,
                4 => result.seek_video_seconds += 1.0,
                5 => result.video_seconds = -180.0,
                6 => result.uncertainty_seconds = -0.1,
                7 => result.video_seconds = f64::INFINITY,
                _ => result.coverage.fight_start_seconds = f64::NAN,
            }
            assert!(!valid(&ticket), "coverage mutation {field} accepted");
        }
    }
    #[test]
    fn changed_fight_recording_epoch_and_algorithm_invalidate_cached_mapping() {
        let (replay, pull, cap, ticket) = test_ticket();
        let alignment = ticket.alignment().unwrap();
        let mut other = pull.clone();
        other.end_ms += 1;
        assert!(!alignment.matches(&replay, &other, &cap));
        other = pull.clone();
        other.report = "OtherReport00001".into();
        assert!(!alignment.matches(&replay, &other, &cap));
        let mut video = replay.clone();
        video.timeline_revision = Some("0".repeat(64));
        assert!(!alignment.matches(&video, &pull, &cap));
        video = replay.clone();
        video.available_seconds += 1;
        assert!(!alignment.matches(&video, &pull, &cap));
        let mut capability = cap;
        capability.algorithm_revision = "0".repeat(64);
        assert!(!alignment.matches(&replay, &pull, &capability));
    }
    #[test]
    fn old_account_or_canceled_selection_cannot_start_a_poll() {
        let (_, _, _, ticket) = test_ticket();
        let wrong = guild::Access::new(
            "fixture-never-sent".into(),
            guild::ADVANCE.into(),
            "456".into(),
            guild::generation(),
        );
        assert!(poll(&wrong, &ticket, &AtomicBool::new(false)).is_err());
        let right = guild::Access::new(
            "fixture-never-sent".into(),
            guild::ADVANCE.into(),
            "123".into(),
            guild::generation(),
        );
        assert!(poll(&right, &ticket, &AtomicBool::new(true)).is_err());
        let mut stale = ticket;
        stale.key.guild_generation = guild::generation().wrapping_sub(1);
        assert!(poll(&right, &stale, &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn unix_backup_requires_a_valid_finished_failure() {
        let (_, _, _, mut failed) = test_ticket();
        failed.job.status = Status::Failed;
        failed.job.result = None;
        failed.job.error = Some("alignment_not_found".into());
        assert!(failed.permits_marker_backup());
        for change in 0..5 {
            let mut ticket = failed.clone();
            match change {
                0 => ticket.job.cleanup_pending = true,
                1 => ticket.job.expires_at = 1,
                2 => ticket.job.scope.as_mut().unwrap().video_id = "other".into(),
                3 => ticket.job.status = Status::Canceled,
                _ => ticket.job.scope.as_mut().unwrap().algorithm_revision = "0".repeat(64),
            }
            assert!(!ticket.permits_marker_backup());
        }
    }
    #[test]
    fn expired_and_oversized_or_unknown_json_are_rejected() {
        let (_, _, _, mut ticket) = test_ticket();
        ticket.job.expires_at = now_ms() - 1;
        assert!(!valid(&ticket));
        assert!(ticket.alignment().is_none());
        assert!(parse(&vec![b' '; 32 * 1024 + 1]).is_err());
        assert!(parse(br#"{"job":null,"extra":true}"#).is_err());
        assert!(!Capability {
            schema: SCHEMA.into(),
            algorithm_revision: "A".repeat(64)
        }
        .valid());
    }
}
