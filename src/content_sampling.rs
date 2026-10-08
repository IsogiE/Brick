//! Sparse checks planned from the viewer's authenticated Warcraft Logs catalogue.
use super::*;
use crate::warcraftlogs::Review;

const LIVE_INTERVAL_MS: i64 = 2 * 60 * 60 * 1000;
const MAX_SAMPLES: usize = 64;
const MAX_FAILED_SAMPLES: usize = 3;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub tickets: Vec<Ticket>,
    pub planned: Option<Key>,
}
pub(crate) struct Plan {
    pub next: Option<Pull>,
    pub pending: Option<Ticket>,
    pub alignments: Vec<Alignment>,
}
fn same_pull(key: &Key, pull: &Pull) -> bool {
    key.report == pull.report
        && key.pull_id == pull.id
        && key.start_ms == pull.start_ms
        && key.end_ms == pull.end_ms
}
fn agrees(left: &Alignment, right: &Alignment) -> bool {
    let expected =
        left.result.video_seconds + (right.key.start_ms - left.key.start_ms) as f64 / 1000.0;
    (expected - right.result.video_seconds).abs()
        <= (left.result.uncertainty_seconds + right.result.uncertainty_seconds).max(1.0)
}
fn random_later<'a>(review: &Review, options: impl Iterator<Item = &'a Pull>) -> Option<&'a Pull> {
    options.min_by_key(|p| {
        let mut hash = Sha256::new();
        hash.update(format!(
            "{:?}:{}:{}:{}:{}",
            review.replay.provider,
            review.replay.video_id,
            review.replay.broadcast_id,
            p.report,
            p.id
        ));
        <[u8; 32]>::from(hash.finalize())
    })
}
/// Spread retries across observed lineups and raid time without guessing which
/// character owns a recording. Missing attendance can never exclude a pull.
fn diverse_retry<'a>(
    options: impl Iterator<Item = &'a Pull>,
    attempted: &[&Pull],
) -> Option<&'a Pull> {
    use std::{cmp::Reverse, collections::BTreeSet};
    let known = attempted.iter().any(|pull| pull.friendly_players.is_some());
    let seen: BTreeSet<_> = attempted
        .iter()
        .filter_map(|pull| pull.friendly_players.as_ref())
        .flatten()
        .copied()
        .collect();
    let encounters: BTreeSet<_> = attempted.iter().map(|pull| pull.encounter).collect();
    options.min_by_key(|pull| {
        let newcomers = if known {
            pull.friendly_players
                .iter()
                .flatten()
                .filter(|id| !seen.contains(*id))
                .count()
        } else {
            0
        };
        let gap = attempted
            .iter()
            .map(|old| pull.start_ms.abs_diff(old.start_ms))
            .min()
            .unwrap_or(0);
        (
            Reverse(newcomers),
            Reverse(!encounters.contains(&pull.encounter)),
            Reverse(gap),
            pull.start_ms,
            pull.report.as_str(),
            pull.id,
        )
    })
}

impl Snapshot {
    pub fn remember(&mut self, ticket: Ticket) {
        self.tickets.retain(|old| {
            !old.expired()
                && old.key.same_recording(&ticket.key)
                && !(old.key.report == ticket.key.report && old.key.pull_id == ticket.key.pull_id)
        });
        if self.tickets.len() >= MAX_SAMPLES {
            return;
        }
        if self
            .planned
            .as_ref()
            .is_some_and(|key| key.report == ticket.key.report && key.pull_id == ticket.key.pull_id)
            && !ticket.pending()
        {
            self.planned = None;
        }
        self.tickets.push(ticket);
    }
    /// Called only after the encrypted vault's guild, Discord account and WCL
    /// session checks. Persisted process epochs are not reusable credentials.
    pub fn rebound(&self, review: &Review, epoch: u64, access: &guild::Access) -> Self {
        let Some(cap) = review.content_capability.as_ref() else {
            return Self::default();
        };
        let rebind = |key: &Key| {
            let pull = review.pulls.iter().find(|pull| same_pull(key, pull))?;
            let mut old = key.clone();
            old.guild_generation = guild::request_generation();
            old.auth_epoch = epoch;
            old.matches(&review.replay, pull, cap).then_some(old)
        };
        let member = member_hash(access);
        let tickets = self
            .tickets
            .iter()
            .take(MAX_SAMPLES)
            .filter_map(|ticket| {
                if ticket.guild_id != access.guild_id || ticket.member_hash != member {
                    return None;
                }
                let mut ticket = ticket.clone();
                ticket.key = rebind(&ticket.key)?;
                ticket
                    .job
                    .validate(&ticket.key, &access.guild_id, &member, None)
                    .ok()?;
                Some(ticket)
            })
            .collect();
        Self {
            tickets,
            planned: self.planned.as_ref().and_then(rebind),
        }
    }
    /// Keep server clocks independent of this viewer's sparse sample tickets.
    /// A fresh conflicting measurement or confirmed missing range still takes precedence.
    pub fn apply_to(&self, review: &mut Review, epoch: u64) {
        let plan = self.plan(review, epoch);
        let measured: Vec<_> = self.tickets.iter().filter_map(Ticket::alignment).collect();
        let replay = &review.replay;
        let pulls: std::collections::HashMap<_, _> = review
            .pulls
            .iter()
            .map(|pull| ((pull.report.as_str(), pull.id), pull))
            .collect();
        let cap = review.content_capability.as_ref();
        review.content_timing.retain(|_, shared| {
            shared.shared_clock
                && shared.key.auth_epoch == epoch
                && cap.is_some_and(|cap| {
                    pulls
                        .get(&(shared.key.report.as_str(), shared.key.pull_id))
                        .is_some_and(|pull| shared.matches(replay, pull, cap))
                })
                && !measured.iter().any(|point| {
                    point.key.same_recording_report(&shared.key)
                        && ((shared.result.video_seconds
                            + (point.key.start_ms - shared.key.start_ms) as f64 / 1000.0)
                            - point.result.video_seconds)
                            .abs()
                            > (shared.result.uncertainty_seconds + point.result.uncertainty_seconds)
                                .max(0.1)
                })
                && !self.tickets.iter().any(|ticket| {
                    !ticket.expired()
                        && ticket.key.same_recording_report(&shared.key)
                        && ticket.job.status == Status::Failed
                        && ticket
                            .job
                            .validate(&ticket.key, &ticket.guild_id, &ticket.member_hash, None)
                            .is_ok()
                        && ticket.job.error.as_deref() == Some("missing_footage")
                        && ticket.key.start_ms < shared.key.end_ms
                        && ticket.key.end_ms > shared.key.start_ms
                })
        });
        for alignment in plan.alignments {
            review
                .content_timing
                .entry((alignment.key.report.clone(), alignment.key.pull_id))
                .or_insert(alignment);
        }
    }

    pub fn plan(&self, review: &Review, epoch: u64) -> Plan {
        let mut plan = Plan {
            next: None,
            pending: None,
            alignments: vec![],
        };
        let Some(cap) = review.content_capability.as_ref() else {
            return plan;
        };
        if review.pulls.len() > 4096 || self.tickets.len() > MAX_SAMPLES {
            return plan;
        }
        let now = now_ms();
        let mut pulls: Vec<_> = review
            .pulls
            .iter()
            .filter(|p| p.end_ms > p.start_ms && (!review.replay.growing || p.end_ms <= now))
            .collect();
        pulls.sort_by_key(|p| (p.start_ms, p.report.as_str(), p.id));
        let tickets: Vec<_> = self
            .tickets
            .iter()
            .filter(|ticket| {
                ticket.key.auth_epoch == epoch
                    && ticket.key.guild_generation == guild::request_generation()
                    && pulls
                        .iter()
                        .any(|pull| ticket.key.matches(&review.replay, pull, cap))
                    && ticket
                        .job
                        .validate(&ticket.key, &ticket.guild_id, &ticket.member_hash, None)
                        .is_ok()
            })
            .collect();
        let ticket_for = |pull: &Pull| {
            tickets
                .iter()
                .find(|ticket| same_pull(&ticket.key, pull))
                .copied()
        };
        // Failed searches are not evidence of an offset. Bound automatic
        // fallback per report; a growing stream may try a fresh two-hour window.
        let can_sample = |pull: &Pull| {
            tickets
                .iter()
                .filter(|t| {
                    t.key.report == pull.report
                        && matches!(t.job.status, Status::Failed | Status::Canceled)
                        && (!review.replay.growing
                            || t.key.start_ms > pull.start_ms.saturating_sub(LIVE_INTERVAL_MS))
                })
                .count()
                < MAX_FAILED_SAMPLES
        };
        plan.pending = tickets
            .iter()
            .find(|ticket| ticket.pending())
            .map(|t| (*t).clone());
        // Process every report independently: different logger clocks must never
        // inherit each other's offset merely because their raid times overlap.
        let mut reports = Vec::new();
        for pull in &pulls {
            if !reports.contains(&pull.report.as_str()) {
                reports.push(pull.report.as_str());
            }
        }
        for report in reports {
            let group: Vec<_> = pulls
                .iter()
                .copied()
                .filter(|p| p.report == report)
                .collect();
            // A saved server clock already supplies this archive's timing.
            // Opening it on another client must not repeat verification work.
            if !review.replay.growing
                && group.iter().all(|pull| {
                    review
                        .content_alignment(pull)
                        .is_some_and(|alignment| alignment.shared_clock)
                })
            {
                continue;
            }
            let usable: Vec<_> = group
                .iter()
                .copied()
                .filter(|p| {
                    ticket_for(p).map_or_else(
                        || can_sample(p),
                        |t| !matches!(t.job.status, Status::Failed | Status::Canceled),
                    )
                })
                .collect();
            let attempted: Vec<_> = group
                .iter()
                .copied()
                .filter(|pull| ticket_for(pull).is_some())
                .collect();
            let retry = attempted.iter().any(|pull| {
                ticket_for(pull).is_some_and(|ticket| {
                    matches!(ticket.job.status, Status::Failed | Status::Canceled)
                })
            });
            let first = if retry {
                // A successful diversified retry is now the anchor; do not go
                // back and scan every earlier pull while its shared clock loads.
                usable
                    .iter()
                    .copied()
                    .find(|pull| ticket_for(pull).and_then(Ticket::alignment).is_some())
                    .or_else(|| diverse_retry(usable.iter().copied(), &attempted))
            } else {
                usable.first().copied()
            };
            let Some(first) = first else {
                continue;
            };
            let last = group.last().unwrap();
            let mut points: Vec<_> = group
                .iter()
                .filter_map(|p| ticket_for(p).and_then(Ticket::alignment))
                .collect();
            points.sort_by_key(|p| (p.key.start_ms, p.key.pull_id));
            let halfway = first.start_ms + (last.start_ms - first.start_ms) / 2;
            let later = |p: &Pull| p.start_ms >= halfway && p.start_ms >= first.end_ms;
            let checked_later = usable
                .iter()
                .any(|p| later(p) && ticket_for(p).and_then(Ticket::alignment).is_some());
            let mut next = if ticket_for(first).and_then(Ticket::alignment).is_none() {
                Some(first)
            } else {
                None
            };
            if next.is_none() {
                if review.replay.growing {
                    let due = points
                        .last()
                        .unwrap()
                        .key
                        .start_ms
                        .saturating_add(LIVE_INTERVAL_MS);
                    next = usable
                        .iter()
                        .copied()
                        .find(|p| p.start_ms >= due && ticket_for(p).is_none());
                } else if !checked_later {
                    let options = usable
                        .iter()
                        .copied()
                        .filter(|p| later(p) && ticket_for(p).is_none());
                    next = if retry {
                        diverse_retry(options, &attempted)
                    } else {
                        random_later(review, options)
                    };
                }
            }
            // Conflicting checks leave a gap. Additional checks narrow that gap,
            // rather than averaging a discontinuity into every subsequent pull.
            for pair in points.windows(2) {
                let (left, right) = (&pair[0], &pair[1]);
                if agrees(left, right) {
                    continue;
                }
                let middle = left.key.start_ms + (right.key.start_ms - left.key.start_ms) / 2;
                if let Some(extra) = usable
                    .iter()
                    .copied()
                    .filter(|p| {
                        p.start_ms > left.key.start_ms
                            && p.start_ms < right.key.start_ms
                            && ticket_for(p).is_none()
                    })
                    .min_by_key(|p| (p.start_ms - middle).abs())
                {
                    next = Some(extra);
                    break;
                }
            }
            if next.is_none() && !review.replay.growing && points.len() >= 2 {
                let (left, right) = (&points[points.len() - 2], points.last().unwrap());
                if left.key.end_ms > right.key.start_ms || !agrees(left, right) {
                    next = random_later(
                        review,
                        usable
                            .iter()
                            .copied()
                            .filter(|p| p.start_ms >= right.key.end_ms && ticket_for(p).is_none()),
                    );
                }
            }
            let missing: Vec<_> = tickets
                .iter()
                .filter(|t| {
                    t.key.report == report
                        && t.job.status == Status::Failed
                        && matches!(t.job.error.as_deref(), Some("missing_footage"))
                })
                .map(|t| (t.key.start_ms, t.key.end_ms))
                .collect();
            if next.is_none() {
                // An unverified pull is a gap, not evidence that every
                // subsequent pull before the later check is also absent.
                for &(_, end) in &missing {
                    if let Some(after) = usable.iter().copied().find(|p| p.start_ms >= end) {
                        if ticket_for(after).is_none() {
                            next = Some(after);
                            break;
                        }
                    }
                }
            }
            if plan.next.is_none() {
                plan.next = next.cloned();
            }
            // Only the server extrapolates recording/report offsets. The client
            // may retain this viewer's exact GPU measurements while offline.
            for pull in &group {
                if let Some(exact) = ticket_for(pull).and_then(Ticket::alignment) {
                    plan.alignments.push(exact);
                }
            }
        }
        // Freeze a still-valid choice across refreshes, other viewers' busy work,
        // and restarts. Never evict first samples and then submit them repeatedly.
        if let Some(pull) = self
            .planned
            .as_ref()
            .filter(|key| key.auth_epoch == epoch)
            .and_then(|key| {
                pulls
                    .iter()
                    .find(|p| key.matches(&review.replay, p, cap))
                    .copied()
            })
            .filter(|p| {
                ticket_for(p).is_none()
                    && can_sample(p)
                    // An old unsubmitted chronological retry must not override
                    // the better choice learned from a failed sample.
                    && (!tickets.iter().any(|ticket| {
                        ticket.key.report == p.report
                            && matches!(ticket.job.status, Status::Failed | Status::Canceled)
                    }) || plan.next.as_ref().is_some_and(|next| {
                        next.report == p.report && next.id == p.id
                    }))
                    && (review.replay.growing
                        || !review.content_alignment(p).is_some_and(|a| a.shared_clock))
            })
        {
            plan.next = Some(pull.clone());
        }
        if plan.pending.is_some() || tickets.len() >= MAX_SAMPLES {
            plan.next = None;
        }
        plan
    }
}
pub(crate) fn submit(
    access: &guild::Access,
    key: Key,
    signature: &BossSignature,
    cancel: &AtomicBool,
) -> Result<Ticket, String> {
    super::submit_to(access, key, signature, cancel, &format!("{PATH}/sample"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn review() -> Review {
        let (mut replay, pull, cap, _) = test_ticket();
        replay.available_seconds = 20_000;
        Review {
            replay,
            pulls: (0..12)
                .map(|i| {
                    let mut p = pull.clone();
                    p.id = i + 1;
                    p.start_ms += i as i64 * 600_000;
                    p.end_ms += i as i64 * 600_000;
                    p
                })
                .collect(),

            content_timing: Default::default(),
            content_capability: Some(cap),
        }
    }
    fn ticket(review: &Review, pull: &Pull, shift: f64) -> Ticket {
        let (_, _, _, mut ticket) = test_ticket();
        let first = review.pulls.iter().map(|p| p.start_ms).min().unwrap();
        ticket.key = Key::new(
            &review.replay,
            pull,
            review.content_capability.as_ref().unwrap(),
            0,
        );
        let scope = ticket.job.scope.as_mut().unwrap();
        scope.report = pull.report.clone();
        scope.pull_id = pull.id;
        scope.duration_seconds = ticket.key.duration();
        scope.timeline.duration_seconds = review.replay.available_seconds as f64;
        scope.timeline.raw_started_at_ms = review.replay.start_ms().ok();
        let result = ticket.job.result.as_mut().unwrap();
        result.video_seconds = 15.25 + (pull.start_ms - first) as f64 / 1000.0 + shift;
        result.seek_video_seconds = result.video_seconds;
        result.coverage.fight_end_seconds = ticket.key.duration();
        ticket
            .job
            .validate(&ticket.key, &ticket.guild_id, &ticket.member_hash, None)
            .unwrap();
        ticket
    }
    #[test]
    fn shared_clock_survives_empty_or_unsuccessful_samples_but_excludes_confirmed_gaps() {
        for outcome in [None, Some("alignment_not_found"), Some("missing_footage")] {
            let mut review = review();
            for pull in &review.pulls {
                let mut alignment = ticket(&review, pull, 0.0).alignment().unwrap();
                alignment.shared_clock = true;
                review
                    .content_timing
                    .insert((pull.report.clone(), pull.id), alignment);
            }
            let mut saved = Snapshot::default();
            if let Some(error) = outcome {
                let mut failed = ticket(&review, &review.pulls[5], 0.0);
                failed.job.status = Status::Failed;
                failed.job.result = None;
                failed.job.error = Some(error.into());
                saved.remember(failed);
            }
            saved.apply_to(&mut review, 0);
            assert_eq!(
                review.content_timing.len(),
                if outcome == Some("missing_footage") {
                    11
                } else {
                    12
                }
            );
            assert!(review.content_alignment(&review.pulls[0]).is_some());
            assert_eq!(
                review.content_alignment(&review.pulls[5]).is_none(),
                outcome == Some("missing_footage")
            );
            assert!(review.content_alignment(&review.pulls[11]).is_some());
        }
    }

    #[test]
    fn a_known_server_clock_opens_an_archive_without_new_viewer_jobs_or_extrapolation() {
        let mut review = review();
        for pull in review.pulls.clone() {
            crate::content_alignment::test_set_timing(
                &mut review,
                &pull,
                15.25 + (pull.id - 1) as f64 * 600.0,
            );
        }
        let saved = Snapshot::default();
        let plan = saved.plan(&review, 0);
        assert!(plan.next.is_none());
        assert!(plan.pending.is_none());
        assert!(plan.alignments.is_empty());
        saved.apply_to(&mut review, 0);
        assert_eq!(review.content_timing.len(), 12);
    }

    #[test]
    fn first_then_stable_later_sample_checks_a_continuous_recording() {
        let review = review();
        let mut saved = Snapshot::default();
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 1);
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.alignments.len(), 1);
        assert_eq!(plan.alignments[0].seek(0.0), Some(15.25));
        let later = plan.next.unwrap();
        assert!(later.id >= 7);
        saved.planned = Some(Key::new(
            &review.replay,
            &later,
            review.content_capability.as_ref().unwrap(),
            0,
        ));
        let serialized = serde_json::to_vec(&saved).unwrap();
        let restored: Snapshot = serde_json::from_slice(&serialized).unwrap();
        let mut growing_catalogue = review.clone();
        let mut newest = review.pulls.last().unwrap().clone();
        newest.id = 99;
        newest.start_ms += 3600000;
        newest.end_ms += 3600000;
        growing_catalogue.pulls.push(newest);
        assert_eq!(
            restored.plan(&growing_catalogue, 0).next.unwrap().id,
            later.id
        );
        saved.remember(ticket(&review, &later, 0.0));
        let plan = saved.plan(&review, 0);
        assert!(plan.next.is_none());
        assert_eq!(plan.alignments.len(), 2);
        for a in plan.alignments {
            assert_eq!(
                a.seek(0.0),
                Some(15.25 + (a.key.pull_id - 1) as f64 * 600.0)
            );
        }
    }
    #[test]
    fn a_single_later_measurement_only_certifies_its_exact_pull_on_the_client() {
        let review = review();
        let mut saved = Snapshot::default();
        saved.remember(ticket(&review, &review.pulls[5], 0.0));
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.alignments.len(), 1);
        for alignment in plan.alignments {
            assert_eq!(
                alignment.seek(0.0),
                Some(15.25 + (alignment.key.pull_id - 1) as f64 * 600.0)
            );
        }
        assert_eq!(plan.next.unwrap().id, 1);
    }
    #[test]
    fn drift_requests_more_evidence_and_never_interpolates_a_jump() {
        let review = review();
        let mut saved = Snapshot::default();
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        saved.remember(ticket(&review, &review.pulls[10], 12.0));
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.next.unwrap().id, 6);
        assert_eq!(plan.alignments.len(), 2);
        assert_eq!(plan.alignments.last().unwrap().seek(0.0), Some(6027.25));
        saved.remember(ticket(&review, &review.pulls[5], 12.0));
        let plan = saved.plan(&review, 0);
        assert!(plan.next.unwrap().id < 6);
        assert!(!plan
            .alignments
            .iter()
            .any(|a| (2..6).contains(&a.key.pull_id)));
        assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 12));
    }
    #[test]
    fn missing_footage_is_a_gap_and_terminal_failures_advance_first_sample() {
        let review = review();
        let mut saved = Snapshot::default();
        let mut absent = ticket(&review, &review.pulls[0], 0.0);
        absent.job.status = Status::Failed;
        absent.job.result = None;
        absent.job.error = Some("missing_footage".into());
        saved.remember(absent);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 12);
        saved.remember(ticket(&review, &review.pulls[1], 0.0));
        saved.remember(ticket(&review, &review.pulls[10], 0.0));
        let mut gap = ticket(&review, &review.pulls[5], 0.0);
        gap.job.status = Status::Failed;
        gap.job.result = None;
        gap.job.error = Some("missing_footage".into());
        saved.remember(gap);
        let plan = saved.plan(&review, 0);
        assert!(!plan
            .alignments
            .iter()
            .any(|a| a.key.pull_id == 1 || a.key.pull_id == 6));
        assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 7));
        assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 12));
        assert_eq!(plan.next.unwrap().id, 7);
        saved.remember(ticket(&review, &review.pulls[6], 0.0));
        let resumed = saved.plan(&review, 0);
        assert!(resumed.next.is_none());
        assert!(!resumed.alignments.iter().any(|a| a.key.pull_id == 6));
        assert!(resumed.alignments.iter().any(|a| a.key.pull_id == 7));
        assert!(resumed.alignments.iter().any(|a| a.key.pull_id == 11));
        assert!(!resumed.alignments.iter().any(|a| a.key.pull_id == 12));
    }
    fn fail(saved: &mut Snapshot, review: &Review, pull: &Pull) {
        let mut failed = ticket(review, pull, 0.0);
        failed.job.status = Status::Failed;
        failed.job.result = None;
        failed.job.error = Some("alignment_not_found".into());
        saved.remember(failed);
    }

    #[test]
    fn retries_cover_changed_lineups_instead_of_three_adjacent_pulls() {
        let mut review = review();
        for (index, pull) in review.pulls.iter_mut().enumerate() {
            pull.friendly_players = Some(match index {
                0..=3 => vec![1, 2],
                4..=7 => vec![1, 3],
                _ => vec![4, 5],
            });
        }
        let mut saved = Snapshot::default();
        let mut tried = Vec::new();
        for _ in 0..MAX_FAILED_SAMPLES {
            let next = saved.plan(&review, 0).next.unwrap();
            tried.push(next.id);
            fail(&mut saved, &review, &next);
        }
        assert_eq!(tried, [1, 12, 6]);
        assert!(saved.plan(&review, 0).next.is_none());
        assert_eq!(saved.tickets.len(), 3);
        assert!(saved.plan(&review, 0).alignments.is_empty());
    }

    #[test]
    fn retry_lineup_novelty_uses_all_attempts_before_encounter_and_time() {
        let mut review = review();
        for pull in &mut review.pulls {
            pull.friendly_players = Some(vec![1, 2]);
        }
        review.pulls[11].friendly_players = Some(vec![3, 4]);
        review.pulls[5].friendly_players = Some(vec![4, 5]);
        review.pulls[1].encounter += 1;
        let mut saved = Snapshot::default();
        fail(&mut saved, &review, &review.pulls[0]);
        fail(&mut saved, &review, &review.pulls[11]);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 6);
        review.pulls.reverse();
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 6);
    }

    #[test]
    fn unknown_attendance_falls_back_to_encounter_then_time_without_exclusions() {
        let mut review = review();
        let mut saved = Snapshot::default();
        fail(&mut saved, &review, &review.pulls[0]);
        // No known attempted lineup means that a candidate's known roster
        // cannot supply evidence that its participants are new.
        review.pulls[1].friendly_players = Some(vec![20, 21, 22]);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 12);
        review.pulls[3].encounter += 1;
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 4);
        fail(&mut saved, &review, &review.pulls[3]);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 12);
        assert_eq!(review.pulls.len(), 12);
    }

    #[test]
    fn same_lineup_and_encounter_retries_spread_over_time_with_earliest_ties() {
        let mut review = review();
        for pull in &mut review.pulls {
            pull.friendly_players = Some(vec![1, 2]);
        }
        let mut saved = Snapshot::default();
        fail(&mut saved, &review, &review.pulls[0]);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 12);
        fail(&mut saved, &review, &review.pulls[11]);
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 6);
    }

    #[test]
    fn restored_unsubmitted_retry_cannot_override_new_diversity_evidence() {
        let review = review();
        let mut saved = Snapshot::default();
        fail(&mut saved, &review, &review.pulls[0]);
        saved.planned = Some(Key::new(
            &review.replay,
            &review.pulls[1],
            review.content_capability.as_ref().unwrap(),
            0,
        ));
        let mut saved: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 12);
        saved.remember(ticket(&review, &review.pulls[11], 0.0));
        let plan = saved.plan(&review, 0);
        assert!(plan.next.is_none(), "Keep a successful retry as the anchor");
        assert_eq!(plan.alignments.len(), 1);
        assert_eq!(plan.alignments[0].key.pull_id, 12);
        assert_eq!(saved.tickets.len(), 2, "Retain the old failure receipt");
    }

    #[test]
    fn pending_retry_survives_a_more_diverse_catalogue() {
        let mut review = review();
        let mut saved = Snapshot::default();
        fail(&mut saved, &review, &review.pulls[0]);
        let mut pending = ticket(&review, &review.pulls[1], 0.0);
        pending.job.status = Status::Running;
        pending.job.result = None;
        saved.remember(pending.clone());
        review.pulls[11].friendly_players = Some(vec![30, 31]);
        review.pulls[11].encounter += 1;
        let plan = saved.plan(&review, 0);
        assert!(plan.next.is_none());
        assert_eq!(plan.pending, Some(pending));
        assert_eq!(saved.tickets.len(), 2);
    }

    #[test]
    fn repeated_no_matches_stop_archives_and_resume_only_in_a_new_live_window() {
        let mut review = review();
        let mut saved = Snapshot::default();
        for pull in review.pulls.iter().take(MAX_FAILED_SAMPLES) {
            let mut failed = ticket(&review, pull, 0.0);
            failed.job.status = Status::Failed;
            failed.job.result = None;
            failed.job.error = Some("alignment_not_found".into());
            saved.remember(failed);
        }
        saved.planned = Some(Key::new(
            &review.replay,
            &review.pulls[3],
            review.content_capability.as_ref().unwrap(),
            0,
        ));
        assert!(saved.plan(&review, 0).next.is_none());
        assert!(saved.plan(&review, 0).alignments.is_empty());
        // Repeated refreshes and serialization cannot restart the exhausted search.
        let saved: Snapshot =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert!(saved.plan(&review, 0).next.is_none());
        review.replay.growing = true;
        // Tickets must refer to the same recording identity (including growing).
        let mut live = Snapshot::default();
        for pull in review.pulls.iter().take(MAX_FAILED_SAMPLES) {
            let mut failed = ticket(&review, pull, 0.0);
            failed.job.status = Status::Failed;
            failed.job.result = None;
            failed.job.error = Some("alignment_not_found".into());
            live.remember(failed);
        }
        assert!(live.plan(&review, 0).next.is_none());
        let mut later = review.pulls[0].clone();
        later.id = 99;
        later.start_ms += LIVE_INTERVAL_MS;
        later.end_ms += LIVE_INTERVAL_MS;
        review.pulls.push(later);
        assert_eq!(live.plan(&review, 0).next.unwrap().id, 99);
    }

    #[test]
    fn unsuccessful_search_does_not_discard_a_known_offset() {
        for error in ["alignment_not_found", "budget_exhausted"] {
            let review = review();
            let mut saved = Snapshot::default();
            saved.remember(ticket(&review, &review.pulls[0], 0.0));
            let mut failed = ticket(&review, &review.pulls[5], 0.0);
            failed.job.status = Status::Failed;
            failed.job.result = None;
            failed.job.error = Some(error.into());
            saved.remember(failed);
            let plan = saved.plan(&review, 0);
            assert_eq!(plan.alignments.len(), 1);
            assert_eq!(plan.alignments[0].seek(0.0), Some(15.25));
            assert!(plan.alignments.iter().any(|a| a.key.pull_id == 1));
            assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 11));
        }
    }

    #[test]
    fn reports_cannot_borrow_offsets_or_count_overlapping_samples_as_later() {
        let mut review = review();
        let mut saved = Snapshot::default();
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        saved.remember(ticket(&review, &review.pulls[10], 0.0));
        let mut other = review.pulls[1].clone();
        other.report = "QrStUvWxYz123456".into();
        review.pulls.push(other.clone());
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.next.unwrap().report, other.report);
        assert!(!plan.alignments.iter().any(|a| a.key.report == other.report));
        let mut duplicate = review.pulls[0].clone();
        duplicate.id = 99;
        review.pulls.push(duplicate.clone());
        let mut one = Snapshot::default();
        one.remember(ticket(&review, &review.pulls[0], 0.0));
        one.remember(ticket(&review, &duplicate, 0.0));
        let plan = one.plan(&review, 0);
        assert!(plan.next.unwrap().start_ms >= review.pulls[0].end_ms);
        assert_eq!(plan.alignments.len(), 2);
    }
    #[test]
    fn pending_live_drift_check_does_not_create_a_viewer_offset() {
        let mut review = review();
        review.replay.growing = true;
        let mut later = review.pulls[0].clone();
        later.id = 42;
        later.start_ms += 2 * LIVE_INTERVAL_MS;
        later.end_ms = later.start_ms + 143_000;
        review.pulls.push(later.clone());
        let mut saved = Snapshot::default();
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        let mut pending = ticket(&review, &later, 0.0);
        pending.job.status = Status::Running;
        pending.job.result = None;
        saved.remember(pending.clone());
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.pending, Some(pending));
        assert!(plan.next.is_none());
        assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 42));
    }
    #[test]
    fn live_sampling_checks_after_two_hours_without_client_extrapolation() {
        let mut review = review();
        review.replay.growing = true;
        let mut extra = review.pulls.last().unwrap().clone();
        extra.id = 13;
        extra.start_ms = review.pulls[0].start_ms + LIVE_INTERVAL_MS;
        extra.end_ms = extra.start_ms + 180000;
        let mut saved = Snapshot::default();
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        assert!(saved.plan(&review, 0).next.is_none());
        review.pulls.push(extra.clone());
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 13);
        let plan = saved.plan(&review, 0);
        assert!(!plan.alignments.iter().any(|a| a.key.pull_id == 13));
        saved.remember(ticket(&review, &extra, 0.0));
        assert!(saved.plan(&review, 0).next.is_none());
    }
    #[test]
    fn expiry_account_and_media_changes_remove_exact_viewer_timing() {
        let review = review();
        let mut saved = Snapshot::default();
        let mut first = ticket(&review, &review.pulls[0], 0.0);
        first.job.expires_at = now_ms() + 60000;
        let expiry = first.job.expires_at;
        saved.remember(first);
        saved.remember(ticket(&review, &review.pulls[10], 0.0));
        assert_eq!(saved.plan(&review, 0).alignments[0].expires_at, expiry);
        assert!(saved.plan(&review, 1).alignments.is_empty());
        let mut changed = review.clone();
        changed.replay.timeline_revision = Some("f".repeat(64));
        assert!(saved.plan(&changed, 0).alignments.is_empty());
        saved.tickets[0].job.expires_at = 1;
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.alignments.len(), 1);
        assert!(plan.alignments.iter().all(|a| a.expires_at > now_ms()));
        assert_eq!(plan.alignments[0].seek(0.0), Some(6015.25));
    }
    #[test]
    fn pending_samples_survive_catalogue_refresh_and_do_not_queue_other_pulls() {
        let review = review();
        let mut saved = Snapshot::default();
        let mut job = ticket(&review, &review.pulls[0], 0.0);
        job.job.status = Status::Pending;
        job.job.result = None;
        saved.remember(job.clone());
        let plan = saved.plan(&review, 0);
        assert!(plan.next.is_none());
        assert_eq!(plan.pending, Some(job));
    }
}

#[cfg(test)]
pub(crate) fn test_samples(review: &Review, epoch: u64) -> Snapshot {
    let mut saved = Snapshot::default();
    let first = review.pulls.iter().map(|p| p.start_ms).min().unwrap();
    for pull in &review.pulls {
        let (_, _, _, mut ticket) = test_ticket();
        ticket.key = Key::new(
            &review.replay,
            pull,
            review.content_capability.as_ref().unwrap(),
            epoch,
        );
        let scope = ticket.job.scope.as_mut().unwrap();
        scope.provider = review.replay.provider.clone();
        scope.video_id = review.replay.video_id.clone();
        scope.report = pull.report.clone();
        scope.pull_id = pull.id;
        scope.duration_seconds = ticket.key.duration();
        scope.timeline.provider = review.replay.provider.clone();
        scope.timeline.video_id = review.replay.video_id.clone();
        scope.timeline.revision = review.replay.timeline_revision.clone().unwrap();
        scope.timeline_revision = scope.timeline.revision.clone();
        scope.timeline.duration_seconds = review.replay.available_seconds as f64;
        scope.timeline.raw_started_at_ms = review.replay.start_ms().ok();
        let result = ticket.job.result.as_mut().unwrap();
        result.video_seconds = 15.25 + (pull.start_ms - first) as f64 / 1000.0;
        result.seek_video_seconds = result.video_seconds;
        result.coverage.fight_end_seconds = ticket.key.duration();
        ticket
            .job
            .validate(&ticket.key, &ticket.guild_id, &ticket.member_hash, None)
            .unwrap();
        saved.remember(ticket);
    }
    saved
}
