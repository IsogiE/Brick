//! Single-hop timing derived from a direct clock and matching report fights.
//!
//! Like a direct report clock, this uses the existing continuous-report-clock
//! assumption, including outside the overlap. It neither fits affine drift nor
//! creates a server clock or a successful GPU sample. Growing recordings use
//! their normal direct clocks; this bridge is restricted to archived recordings.
//! Callers must supply complete
//! current catalogs and fresh authoritative target absence, and revoke derived
//! timing whenever that absence, any source clock, or its metadata changes.
use super::Pull;
use crate::content_alignment::{Capability, Key, RecordingClock};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_PULLS: usize = 5_000;
const MAX_SOURCES: usize = 20;
const BOUNDARY_FLOOR: f64 = 0.100;

/// Only direct API clocks belong here. Derived deliberately has no conversion.
pub(crate) struct Source<'a> {
    pub key: &'a Key,
    pub clock: &'a RecordingClock,
    pub pulls: &'a [Pull],
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    pub report: String,
    pub pull_id: u64,
    pub encounter: u64,
    pub difficulty: u64,
    pub report_start_ms: i64,
    pub start_ms: i64,
    pub end_ms: i64,
}
impl From<&Pull> for Identity {
    fn from(pull: &Pull) -> Self {
        Self {
            report: pull.report.clone(),
            pull_id: pull.id,
            encounter: pull.encounter,
            difficulty: pull.difficulty,
            report_start_ms: pull.report_start_ms,
            start_ms: pull.start_ms,
            end_ms: pull.end_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pair {
    pub source: Identity,
    pub target: Identity,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Provenance {
    pub source_key: Key,
    pub clock_fingerprint: [u8; 32],
    pub evidence_hash: String,
    pub expires_at: i64,
    pub source_anchor: Identity,
    pub target_anchor: Identity,
    /// Includes all input fight identities/bounds, not only accepted pairs.
    pub source_catalog_digest: [u8; 32],
    pub target_catalog_digest: [u8; 32],
    pub pairs: Vec<Pair>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Derived {
    /// Original target identity is retained, never rebased to another logger.
    pub target_key: Key,
    pub reference_ms: i64,
    pub video_seconds: f64,
    pub uncertainty_seconds: f64,
    pub expires_at: i64,
    pub provenance: Vec<Provenance>,
}

impl Derived {
    /// Cheap scope/expiry guard. The caller must separately retain fresh lookup
    /// authority and source fingerprints; this alone is not a new proof.
    pub(crate) fn matches_key(&self, key: &Key) -> bool {
        let now = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        self.target_key.same_recording_report(key)
            && i128::from(self.expires_at) > now
            && key.end_ms > key.start_ms
    }

    /// Re-prove at metadata/lookup refresh boundaries, not on every render.
    #[cfg(test)]
    pub(crate) fn valid_for(
        &self,
        key: &Key,
        target_catalog: &[Pull],
        capability: &Capability,
        target_authoritatively_absent: bool,
        sources: &[Source<'_>],
    ) -> bool {
        self.matches_key(key)
            && target_catalog.iter().any(|pull| key_identifies(key, pull))
            && derive(
                &self.target_key,
                target_catalog,
                capability,
                target_authoritatively_absent,
                sources,
            )
            .as_ref()
                == Some(self)
    }
}

/// Fingerprint the entire direct clock and its account/recording/report key.
pub(crate) fn fingerprint(key: &Key, clock: &RecordingClock) -> Option<[u8; 32]> {
    let bytes = serde_json::to_vec(&(key, clock)).ok()?;
    Some(Sha256::digest(bytes).into())
}

fn key_identifies(key: &Key, pull: &Pull) -> bool {
    key.report == pull.report
        && key.pull_id == pull.id
        && key.encounter == pull.encounter
        && key.difficulty == pull.difficulty
        && key.report_start_ms == pull.report_start_ms
        && key.start_ms == pull.start_ms
        && key.end_ms == pull.end_ms
}

fn catalog<'a>(key: &Key, pulls: &'a [Pull]) -> Option<Vec<&'a Pull>> {
    if pulls.is_empty() || pulls.len() > MAX_PULLS || !super::report_code(&key.report) {
        return None;
    }
    let mut ids = BTreeSet::new();
    let mut bounds = BTreeSet::new();
    for pull in pulls {
        let duration = pull.end_ms.checked_sub(pull.start_ms)?;
        let relative = pull.start_ms.checked_sub(pull.report_start_ms)?;
        if pull.report != key.report
            || pull.report_start_ms != key.report_start_ms
            || pull.id == 0
            || pull.encounter == 0
            || pull.difficulty == 0
            || pull.report_start_ms <= 0
            || !(1..=3_600_000).contains(&duration)
            || !(0..=604_800_000).contains(&relative)
            || !ids.insert(pull.id)
            || !bounds.insert((pull.encounter, pull.difficulty, pull.start_ms, pull.end_ms))
        {
            return None;
        }
    }
    if !pulls.iter().any(|pull| key_identifies(key, pull)) {
        return None;
    }
    let mut sorted: Vec<_> = pulls.iter().collect();
    sorted.sort_by_key(|pull| (pull.start_ms, pull.end_ms, pull.id));
    Some(sorted)
}

fn catalog_digest(pulls: &[&Pull]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"brick-clock-bridge-catalog-1\0");
    for pull in pulls {
        hash.update((pull.report.len() as u64).to_le_bytes());
        hash.update(pull.report.as_bytes());
        for value in [pull.id, pull.encounter, pull.difficulty] {
            hash.update(value.to_le_bytes());
        }
        for value in [pull.report_start_ms, pull.start_ms, pull.end_ms] {
            hash.update(value.to_le_bytes());
        }
    }
    hash.finalize().into()
}

/// Every plausible edge must be mutually unique; do not cherry-pick an
/// agreeable subset of ambiguous or contradictory nearby fight boundaries.
fn pairs<'a>(source: &[&'a Pull], target: &[&'a Pull]) -> Option<Vec<(&'a Pull, &'a Pull)>> {
    let mut result = Vec::new();
    let mut used = BTreeSet::new();
    for left in source {
        let begin = target.partition_point(|p| p.start_ms < left.start_ms.saturating_sub(3_000));
        let mut matches = target[begin..]
            .iter()
            .take_while(|p| p.start_ms <= left.start_ms.saturating_add(3_000))
            .filter(|right| {
                left.encounter == right.encounter && left.difficulty == right.difficulty
            });
        let Some(right) = matches.next() else {
            continue;
        };
        if !super::equivalent_pull(left, right)
            || matches.next().is_some()
            || !used.insert(right.id)
        {
            return None;
        }
        result.push((*left, *right));
    }
    Some(result)
}

fn anchor_key(base: &Key, anchor: &Pull) -> Key {
    let mut key = base.clone();
    key.pull_id = anchor.id;
    key.encounter = anchor.encounter;
    key.difficulty = anchor.difficulty;
    key.start_ms = anchor.start_ms;
    key.end_ms = anchor.end_ms;
    key
}

struct Proposal {
    video_seconds: f64,
    uncertainty: f64,
    provenance: Provenance,
}

/// Proof policy only: no I/O, source mutation, persistent absence, or job work.
/// Catalog completeness and fresh lookup-state ownership remain caller duties.
pub(crate) fn derive(
    target_key: &Key,
    target_catalog: &[Pull],
    capability: &Capability,
    target_authoritatively_absent: bool,
    sources: &[Source<'_>],
) -> Option<Derived> {
    if target_key.growing
        || !target_authoritatively_absent
        || !capability.valid()
        || target_key.algorithm_revision != capability.algorithm_revision
        || sources.is_empty()
        || sources.len() > MAX_SOURCES
    {
        return None;
    }
    let target = catalog(target_key, target_catalog)?;
    let target_digest = catalog_digest(&target);
    let mut seen_reports = BTreeSet::new();
    let mut proposals = BTreeMap::new();
    let mut trusted_timeline = None;
    for source in sources {
        if !target_key.same_recording(source.key)
            || source.key.report == target_key.report
            || !seen_reports.insert(source.key.report.clone())
        {
            return None;
        }
        let source_catalog = catalog(source.key, source.pulls)?;
        // Expired or otherwise invalid direct clocks never supply a proposal.
        if source.clock.alignment(source.key).is_none() {
            continue;
        }
        let identity = (&source.clock.timeline, source.clock.timeline_hash.as_str());
        if trusted_timeline.is_some_and(|old| old != identity) {
            return None;
        }
        trusted_timeline = Some(identity);
        let mut anchors = source_catalog.iter().filter(|pull| {
            let at = (pull.start_ms - pull.report_start_ms) as f64 / 1000.0;
            (at - source.clock.report_seconds).abs() <= 0.001
        });
        let Some(anchor) = anchors.next().copied() else {
            continue;
        };
        if anchors.next().is_some() {
            return None;
        }
        let matched = pairs(&source_catalog, &target)?;
        if matched.len() < 3
            || matched
                .iter()
                .map(|(p, _)| p.encounter)
                .collect::<BTreeSet<_>>()
                .len()
                < 2
        {
            continue;
        }
        let Some((_, target_anchor)) = matched.iter().find(|(p, _)| p.id == anchor.id) else {
            continue;
        };
        let anchor_alignment = source.clock.alignment(&anchor_key(source.key, anchor))?;
        let anchor_delta = anchor.start_ms.checked_sub(target_anchor.start_ms)?;
        let mut boundary_difference = 0.0_f64;
        let mut translation_deviation = 0.0_f64;
        for (left, right) in &matched {
            for delta in [
                left.start_ms.checked_sub(right.start_ms)?,
                left.end_ms.checked_sub(right.end_ms)?,
            ] {
                boundary_difference = boundary_difference.max(delta.unsigned_abs() as f64 / 1000.0);
                translation_deviation = translation_deviation
                    .max(delta.checked_sub(anchor_delta)?.unsigned_abs() as f64 / 1000.0);
            }
        }
        // Charge both systematic logger boundary disagreement and changing
        // translation, not merely the residual around a fitted clock.
        let uncertainty = source.clock.uncertainty_seconds
            + BOUNDARY_FLOOR.max(boundary_difference)
            + translation_deviation;
        if !uncertainty.is_finite() || uncertainty >= 1.0 {
            return None;
        }
        let video_seconds = anchor_alignment.result.video_seconds
            + target_key.start_ms.checked_sub(target_anchor.start_ms)? as f64 / 1000.0;
        if !video_seconds.is_finite() {
            return None;
        }
        proposals.insert(
            source.key.report.clone(),
            Proposal {
                video_seconds,
                uncertainty,
                provenance: Provenance {
                    source_key: source.key.clone(),
                    clock_fingerprint: fingerprint(source.key, source.clock)?,
                    evidence_hash: source.clock.evidence_hash.clone(),
                    expires_at: source.clock.expires_at,
                    source_anchor: anchor.into(),
                    target_anchor: (*target_anchor).into(),
                    source_catalog_digest: catalog_digest(&source_catalog),
                    target_catalog_digest: target_digest,
                    pairs: matched
                        .into_iter()
                        .map(|(source, target)| Pair {
                            source: source.into(),
                            target: target.into(),
                        })
                        .collect(),
                },
            },
        );
    }
    if proposals.is_empty() {
        return None;
    }
    let common_low = proposals
        .values()
        .map(|p| p.video_seconds - p.uncertainty)
        .fold(f64::NEG_INFINITY, f64::max);
    let common_high = proposals
        .values()
        .map(|p| p.video_seconds + p.uncertainty)
        .fold(f64::INFINITY, f64::min);
    if common_low > common_high {
        return None;
    }
    let low = proposals
        .values()
        .map(|p| p.video_seconds - p.uncertainty)
        .fold(f64::INFINITY, f64::min);
    let high = proposals
        .values()
        .map(|p| p.video_seconds + p.uncertainty)
        .fold(f64::NEG_INFINITY, f64::max);
    let video_seconds = low + (high - low) / 2.0;
    let uncertainty_seconds = (high - low) / 2.0;
    let covered_duration = trusted_timeline?
        .0
        .duration_seconds
        .min(target_key.available_seconds as f64);
    if !uncertainty_seconds.is_finite()
        || uncertainty_seconds >= 1.0
        || video_seconds >= covered_duration
        || video_seconds + target_key.duration() <= 0.0
    {
        return None;
    }
    let expires_at = proposals.values().map(|p| p.provenance.expires_at).min()?;
    Some(Derived {
        target_key: target_key.clone(),
        reference_ms: target_key.start_ms,
        video_seconds,
        uncertainty_seconds,
        expires_at,
        provenance: proposals.into_values().map(|p| p.provenance).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_alignment::{test_recording_clock, test_ticket};

    #[derive(Clone)]
    struct Fixture {
        target_key: Key,
        source_key: Key,
        target: Vec<Pull>,
        source: Vec<Pull>,
        capability: Capability,
        clock: RecordingClock,
    }
    impl Fixture {
        fn new() -> Self {
            let (mut replay, template, capability, _) = test_ticket();
            replay.available_seconds = 30_000;
            let mut target = Vec::new();
            let mut source = Vec::new();
            // Anonymous overlapping logs: three measured matches across two
            // encounters, with the requested pull before the overlapping logs.
            for (i, (seconds, delta)) in [(283, -282), (3007, -282), (7109, -188), (9283, -112)]
                .into_iter()
                .enumerate()
            {
                let mut pull = template.clone();
                pull.report = "TargetReport0001".into();
                pull.id = i as u64 + 1;
                pull.encounter = if i < 2 { 100 } else { 200 };
                pull.start_ms = pull.report_start_ms + seconds * 1000;
                pull.end_ms = pull.start_ms + 105_000;
                target.push(pull.clone());
                if i == 0 {
                    continue;
                }
                pull.report = "SourceReport0001".into();
                pull.report_start_ms += 2_896_453;
                pull.start_ms += delta;
                pull.end_ms += delta - if i == 1 { 39 } else { 0 };
                source.push(pull);
            }
            let target_key = Key::new(&replay, &target[0], &capability, 0);
            let source_key = Key::new(&replay, &source[0], &capability, 0);
            let mut clock = test_recording_clock();
            clock.timeline.duration_seconds = 30_000.0;
            clock.report_start_ms = source_key.report_start_ms;
            clock.report_seconds =
                (source_key.start_ms - source_key.report_start_ms) as f64 / 1000.0;
            clock.video_seconds = 14_395.15;
            clock.uncertainty_seconds = 0.3125;
            Self {
                target_key,
                source_key,
                target,
                source,
                capability,
                clock,
            }
        }
        fn derive(&self) -> Option<Derived> {
            derive(
                &self.target_key,
                &self.target,
                &self.capability,
                true,
                &[Source {
                    key: &self.source_key,
                    clock: &self.clock,
                    pulls: &self.source,
                }],
            )
        }
    }

    #[test]
    fn anonymous_overlap_reuses_actual_anchor_for_earlier_pull() {
        let f = Fixture::new();
        let d = f.derive().unwrap();
        assert_eq!(d.target_key, f.target_key);
        assert_eq!(d.reference_ms, f.target_key.start_ms);
        assert!((d.video_seconds - 11_671.15).abs() < 1e-8);
        assert!((d.uncertainty_seconds - 0.8035).abs() < 1e-8);
        assert_eq!(d.provenance[0].source_anchor.pull_id, 2);
        assert_eq!(d.provenance[0].target_anchor.pull_id, 2);
        assert_eq!(d.provenance[0].pairs.len(), 3);
        assert_eq!(d.expires_at, f.clock.expires_at);
    }

    #[test]
    fn fourteen_matches_and_three_direct_anchors_agree_without_affine_drift() {
        let mut f = Fixture::new();
        let template = f.target[0].clone();
        f.target.clear();
        f.source.clear();
        let mut early = template.clone();
        early.start_ms = early.report_start_ms + 282_788;
        early.end_ms = early.report_start_ms + 808_485;
        early.encounter = 103;
        f.target.push(early);
        // Anonymized report/recording/encounter identities and UTC origin;
        // boundary relations preserve an observed fourteen-fight overlap.
        let rows = [
            (3006845, 3111875, -282, -321, 103),
            (3205592, 3710590, -302, -304, 103),
            (3838054, 4471116, -282, -254, 103),
            (5009646, 5317043, -247, -244, 104),
            (5639795, 5952316, -223, -211, 105),
            (6380920, 6678196, -211, -201, 106),
            (7109682, 7216153, -188, -176, 101),
            (7316934, 7645601, -185, -176, 101),
            (7843807, 7869281, -171, -186, 102),
            (8215341, 8426808, -143, -154, 102),
            (8547960, 8974910, -145, -157, 102),
            (9143676, 9211736, -139, -149, 102),
            (9283009, 9681582, -112, -125, 102),
            (10505988, 10822614, -95, -93, 100),
        ];
        for (i, (start, end, start_delta, end_delta, encounter)) in rows.into_iter().enumerate() {
            let mut target = template.clone();
            target.id = i as u64 + 2;
            target.encounter = encounter;
            target.start_ms = target.report_start_ms + start;
            target.end_ms = target.report_start_ms + end;
            let mut source = target.clone();
            source.report = f.source_key.report.clone();
            source.report_start_ms += 2_896_453;
            source.start_ms += start_delta;
            source.end_ms += end_delta;
            f.target.push(target);
            f.source.push(source);
        }
        f.target_key = anchor_key(&f.target_key, &f.target[0]);
        f.source_key = anchor_key(&f.source_key, &f.source[0]);
        for (report_seconds, video_seconds, uncertainty, expected) in [
            (110.110, 14_395.15075, 0.3125, 11_671.09375),
            (4_213.041, 18_498.075, 0.362, 11_671.181),
            (6_386.444, 20_671.42325, 0.322, 11_671.20225),
        ] {
            f.clock.report_seconds = report_seconds;
            f.clock.video_seconds = video_seconds;
            f.clock.uncertainty_seconds = uncertainty;
            let derived = f.derive().unwrap();
            assert_eq!(derived.provenance[0].pairs.len(), 14);
            assert!((derived.video_seconds - expected).abs() < 1e-7);
            assert!(derived.uncertainty_seconds < 1.0);
            if report_seconds == 110.110 {
                assert!((derived.uncertainty_seconds - 0.8225).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn fresh_authoritative_absence_is_mandatory() {
        let f = Fixture::new();
        assert!(derive(
            &f.target_key,
            &f.target,
            &f.capability,
            false,
            &[Source {
                key: &f.source_key,
                clock: &f.clock,
                pulls: &f.source,
            }]
        )
        .is_none());
    }

    #[test]
    fn consistent_large_logger_offset_is_not_free_precision() {
        let mut f = Fixture::new();
        for (source, target) in f.source.iter_mut().zip(&f.target[1..]) {
            source.start_ms = target.start_ms + 2_000;
            source.end_ms = target.end_ms + 2_000;
        }
        f.source_key = anchor_key(&f.source_key, &f.source[0]);
        f.clock.report_seconds =
            (f.source_key.start_ms - f.source_key.report_start_ms) as f64 / 1000.0;
        assert!(f.derive().is_none());
    }

    #[test]
    fn changing_boundary_translation_is_charged() {
        let mut f = Fixture::new();
        f.source[2].end_ms += 900;
        assert!(f.derive().is_none());
    }

    #[test]
    fn ambiguous_edges_and_duplicate_identities_are_rejected() {
        for mutation in 0..4 {
            let mut f = Fixture::new();
            match mutation {
                0 => {
                    let mut p = f.target[1].clone();
                    p.id += 100;
                    p.start_ms += 1;
                    p.end_ms += 1;
                    f.target.push(p);
                }
                1 => f.target.push(f.target[1].clone()),
                2 => {
                    let mut p = f.source[1].clone();
                    p.id += 100;
                    p.start_ms += 1;
                    p.end_ms += 1;
                    f.source.push(p);
                }
                _ => {
                    let mut p = f.source[1].clone();
                    p.id += 100;
                    f.source.push(p);
                }
            }
            assert!(f.derive().is_none(), "mutation {mutation}");
        }
    }

    #[test]
    fn anchor_must_be_real_unique_and_itself_matched() {
        for mutation in 0..3 {
            let mut f = Fixture::new();
            match mutation {
                0 => f.clock.report_seconds += 0.002,
                1 => {
                    f.target.remove(1);
                }
                _ => {
                    let mut p = f.source[0].clone();
                    p.id += 100;
                    p.encounter += 1;
                    f.source.push(p);
                }
            }
            assert!(f.derive().is_none());
        }
    }

    #[test]
    fn needs_several_matches_and_encounters() {
        let mut f = Fixture::new();
        f.source.pop();
        assert!(f.derive().is_none());
        let mut f = Fixture::new();
        for p in f.source.iter_mut().chain(f.target.iter_mut()) {
            p.encounter = 100;
        }
        f.source_key = anchor_key(&f.source_key, &f.source[0]);
        assert!(f.derive().is_none());
    }

    #[test]
    fn start_only_match_with_incompatible_end_cannot_be_dropped_from_consensus() {
        let mut f = Fixture::new();
        let mut source = f.source[2].clone();
        let mut target = f.target[3].clone();
        source.id += 100;
        target.id += 100;
        source.start_ms += 400_000;
        source.end_ms += 405_000;
        target.start_ms += 400_000;
        target.end_ms += 400_000;
        f.source.push(source);
        f.target.push(target);
        assert!(f.derive().is_none());
    }

    #[test]
    fn missing_actual_anchor_cannot_be_replaced_by_another_valid_overlap() {
        let mut f = Fixture::new();
        f.clock.report_seconds += 400.0;
        assert!(f.derive().is_none());
    }

    #[test]
    fn changed_target_recording_scope_is_never_reused() {
        let f = Fixture::new();
        let derived = f.derive().unwrap();
        for mutation in 0..7 {
            let mut key = f.target_key.clone();
            match mutation {
                0 => key.video_id = "different".into(),
                1 => key.timeline_revision = Some("0".repeat(64)),
                2 => key.auth_epoch += 1,
                3 => key.guild_generation += 1,
                4 => key.report = "OtherReport00001".into(),
                5 => key.report_start_ms += 1,
                _ => key.algorithm_revision = "0".repeat(64),
            }
            assert!(!derived.matches_key(&key));
        }
    }

    #[test]
    fn reproof_requires_current_absence_metadata_and_raw_source() {
        let f = Fixture::new();
        let d = f.derive().unwrap();
        let sources = [Source {
            key: &f.source_key,
            clock: &f.clock,
            pulls: &f.source,
        }];
        assert!(d.valid_for(&f.target_key, &f.target, &f.capability, true, &sources));
        assert!(!d.valid_for(&f.target_key, &f.target, &f.capability, false, &sources));
        assert!(!d.valid_for(&f.target_key, &f.target, &f.capability, true, &[]));
        let mut changed = f.target.clone();
        changed[2].end_ms += 1;
        assert!(!d.valid_for(&f.target_key, &changed, &f.capability, true, &sources));
        // Source.clock is &RecordingClock, so &Derived cannot enter this API.
    }

    #[test]
    fn old_clock_changed_timeline_capability_and_account_never_supply_proof() {
        for mutation in 0..8 {
            let mut f = Fixture::new();
            match mutation {
                0 => f.clock.expires_at = 1,
                1 => f.clock.timeline.revision = "0".repeat(64),
                2 => f.clock.algorithm_revision = "0".repeat(64),
                3 => f.source_key.auth_epoch += 1,
                4 => f.source_key.guild_generation += 1,
                5 => f.source_key.video_id = "other".into(),
                6 => f.capability.schema = "unsupported".into(),
                _ => f.clock.uncertainty_seconds = f64::NAN,
            }
            assert!(f.derive().is_none(), "mutation {mutation}");
        }
    }

    #[test]
    fn metadata_and_direct_clock_changes_change_provenance() {
        let f = Fixture::new();
        let first = f.derive().unwrap();
        let mut clock = f.clock.clone();
        clock.evidence_hash = "1".repeat(64);
        assert_ne!(
            fingerprint(&f.source_key, &f.clock),
            fingerprint(&f.source_key, &clock)
        );
        let mut f = f;
        f.target[0].end_ms += 1;
        f.target_key.end_ms += 1;
        let second = f.derive().unwrap();
        assert_ne!(
            first.provenance[0].target_catalog_digest,
            second.provenance[0].target_catalog_digest
        );
    }

    #[test]
    fn multiple_sources_must_agree_and_keep_a_subsecond_envelope() {
        let f = Fixture::new();
        let mut second_catalog = f.source.clone();
        for p in &mut second_catalog {
            p.report = "SourceReport0002".into();
        }
        let mut second_key = f.source_key.clone();
        second_key.report = "SourceReport0002".into();
        let mut second_clock = f.clock.clone();
        for (shift, allowed) in [(0.1, true), (0.5, false), (3.0, false)] {
            second_clock.video_seconds = f.clock.video_seconds + shift;
            let result = derive(
                &f.target_key,
                &f.target,
                &f.capability,
                true,
                &[
                    Source {
                        key: &f.source_key,
                        clock: &f.clock,
                        pulls: &f.source,
                    },
                    Source {
                        key: &second_key,
                        clock: &second_clock,
                        pulls: &second_catalog,
                    },
                ],
            );
            assert_eq!(result.is_some(), allowed, "shift {shift}");
            if let Some(d) = result {
                assert_eq!(d.provenance.len(), 2);
            }
        }
    }

    #[test]
    fn growing_recording_cannot_extend_a_source_clocks_known_media() {
        let mut f = Fixture::new();
        f.target_key.growing = true;
        f.source_key.growing = true;
        f.target_key.available_seconds = 31_000;
        f.source_key.available_seconds = 31_000;
        assert_eq!(f.clock.timeline.duration_seconds, 30_000.0);
        let mut later = f.target[0].clone();
        later.id = 100;
        later.start_ms = later.report_start_ms + 19_111_850;
        later.end_ms = later.start_ms + 100_000;
        f.target.push(later.clone());
        f.target_key = anchor_key(&f.target_key, &later);
        // The current recording has media here; the source clock does not.
        assert!(f.clock.alignment(&f.source_key).is_some());
        assert!(f.derive().is_none()); // projected video start is 30,500s
        let original = f.target.iter().find(|p| p.id == 1).unwrap();
        f.target_key = anchor_key(&f.target_key, original);
        assert!(f.derive().is_none()); // even earlier live footage stays direct-only
        f.target_key.growing = false;
        assert!(f.derive().is_none()); // a live source cannot feed an archive key
    }

    #[test]
    fn agreeing_sources_still_require_one_identical_trusted_timeline() {
        let f = Fixture::new();
        let mut other_catalog = f.source.clone();
        for p in &mut other_catalog {
            p.report = "SourceReport0002".into();
        }
        let mut other_key = f.source_key.clone();
        other_key.report = "SourceReport0002".into();
        for replacement in ["1", "2"] {
            let mut other_clock = f.clock.clone();
            other_clock.timeline_hash = replacement.repeat(64);
            assert!(other_clock.alignment(&other_key).is_some());
            assert!(derive(
                &f.target_key,
                &f.target,
                &f.capability,
                true,
                &[
                    Source {
                        key: &f.source_key,
                        clock: &f.clock,
                        pulls: &f.source
                    },
                    Source {
                        key: &other_key,
                        clock: &other_clock,
                        pulls: &other_catalog
                    },
                ]
            )
            .is_none());
        }
    }

    #[test]
    fn source_order_and_catalog_order_do_not_change_the_proof() {
        let mut f = Fixture::new();
        let first = f.derive().unwrap();
        f.source.reverse();
        f.target.reverse();
        assert_eq!(first, f.derive().unwrap());
    }

    #[test]
    fn seeded_catalog_and_clock_metamorphisms_preserve_or_revoke_proof() {
        fn next(seed: &mut u64) -> u64 {
            *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            *seed
        }
        fn shuffle<T>(items: &mut [T], seed: &mut u64) {
            for i in (1..items.len()).rev() {
                items.swap(i, next(seed) as usize % (i + 1));
            }
        }
        let mut seed = 0x529c_87bd_14a3_f601;
        for case in 0..300 {
            let mut f = Fixture::new();
            for pull in &mut f.source[1..] {
                let jitter = (next(&mut seed) % 21) as i64 - 10;
                pull.start_ms += jitter;
                pull.end_ms += jitter;
            }
            let first = f.derive().unwrap();
            assert!(first.uncertainty_seconds >= f.clock.uncertainty_seconds + 0.321);
            assert!(first.uncertainty_seconds < 0.825);
            shuffle(&mut f.source, &mut seed);
            shuffle(&mut f.target, &mut seed);
            assert_eq!(first, f.derive().unwrap(), "catalog permutation {case}");

            let mut other_catalog = f.source.clone();
            for pull in &mut other_catalog {
                pull.report = "SourceReport0002".into();
            }
            let mut other_key = f.source_key.clone();
            other_key.report = "SourceReport0002".into();
            let left = Source {
                key: &f.source_key,
                clock: &f.clock,
                pulls: &f.source,
            };
            let right = Source {
                key: &other_key,
                clock: &f.clock,
                pulls: &other_catalog,
            };
            let ordered = derive(
                &f.target_key,
                &f.target,
                &f.capability,
                true,
                &[left, right],
            )
            .unwrap();
            let left = Source {
                key: &f.source_key,
                clock: &f.clock,
                pulls: &f.source,
            };
            let right = Source {
                key: &other_key,
                clock: &f.clock,
                pulls: &other_catalog,
            };
            assert_eq!(
                ordered,
                derive(
                    &f.target_key,
                    &f.target,
                    &f.capability,
                    true,
                    &[right, left]
                )
                .unwrap()
            );

            let mut translated = f.clone();
            let delta = (next(&mut seed) % 172_800_001) as i64 - 86_400_000;
            for pull in translated.source.iter_mut().chain(&mut translated.target) {
                pull.report_start_ms += delta;
                pull.start_ms += delta;
                pull.end_ms += delta;
            }
            for key in [&mut translated.source_key, &mut translated.target_key] {
                key.report_start_ms += delta;
                key.start_ms += delta;
                key.end_ms += delta;
            }
            translated.clock.report_start_ms += delta;
            let shifted = translated.derive().unwrap();
            assert!((shifted.video_seconds - first.video_seconds).abs() < 1e-9);
            assert!((shifted.uncertainty_seconds - first.uncertainty_seconds).abs() < 1e-9);

            let mut ambiguous = f.clone();
            let mut competing = ambiguous.target.iter().find(|p| p.id == 2).unwrap().clone();
            competing.id = 10_000;
            competing.start_ms += 1;
            competing.end_ms += 1;
            ambiguous.target.push(competing);
            assert!(ambiguous.derive().is_none());
            let mut duplicate = f.clone();
            duplicate.target.push(duplicate.target[0].clone());
            assert!(duplicate.derive().is_none());

            let mut changed = f.clone();
            let mut unrelated = changed.target[0].clone();
            unrelated.id = 20_000;
            unrelated.start_ms = unrelated.report_start_ms + 20_000_000;
            unrelated.end_ms = unrelated.start_ms + 100_000;
            changed.target.push(unrelated);
            let changed_proof = changed.derive().unwrap();
            assert_eq!(first.video_seconds, changed_proof.video_seconds);
            assert_ne!(
                first.provenance[0].target_catalog_digest,
                changed_proof.provenance[0].target_catalog_digest
            );
            assert!(!first.valid_for(
                &changed.target_key,
                &changed.target,
                &changed.capability,
                true,
                &[Source {
                    key: &changed.source_key,
                    clock: &changed.clock,
                    pulls: &changed.source,
                }]
            ));
        }
    }

    #[test]
    fn mutated_target_identity_and_self_report_are_rejected() {
        let mut f = Fixture::new();
        f.target_key.end_ms += 1;
        assert!(f.derive().is_none());
        let f = Fixture::new();
        assert!(derive(
            &f.source_key,
            &f.source,
            &f.capability,
            true,
            &[Source {
                key: &f.source_key,
                clock: &f.clock,
                pulls: &f.source,
            }]
        )
        .is_none());
    }
}
