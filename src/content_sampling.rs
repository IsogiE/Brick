//! Sparse checks planned from the viewer's authenticated Warcraft Logs catalogue.
use super::*;
use crate::warcraftlogs::Review;

const LIVE_INTERVAL_MS: i64 = 2 * 60 * 60 * 1000;
const MAX_SAMPLES: usize = 64;

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
            let usable: Vec<_> = group
                .iter()
                .copied()
                .filter(|p| {
                    ticket_for(p)
                        .is_none_or(|t| !matches!(t.job.status, Status::Failed | Status::Canceled))
                })
                .collect();
            let Some(first) = usable.first().copied() else {
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
                    next = random_later(
                        review,
                        usable
                            .iter()
                            .copied()
                            .filter(|p| later(p) && ticket_for(p).is_none()),
                    );
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
            if plan.next.is_none() {
                plan.next = next.cloned();
            }
            let missing: Vec<_> = tickets
                .iter()
                .filter(|t| {
                    t.key.report == report
                        && t.job.status == Status::Failed
                        && t.job.error.as_deref() == Some("missing_footage")
                })
                .map(|t| (t.key.start_ms, t.key.end_ms))
                .collect();
            let expiry = points.iter().map(|p| p.expires_at).min().unwrap_or(0);
            let mut hash = Sha256::new();
            for point in &points {
                hash.update(point.version());
            }
            for gap in &missing {
                hash.update(gap.0.to_le_bytes());
                hash.update(gap.1.to_le_bytes());
            }
            let revision = format!("{:x}", hash.finalize());
            for pull in &group {
                if missing
                    .iter()
                    .any(|&(start, end)| pull.start_ms < end && pull.end_ms > start)
                {
                    continue;
                }
                // A directly verified pull retains only its actual coverage.
                if let Some(exact) = ticket_for(pull).and_then(Ticket::alignment) {
                    plan.alignments.push(exact);
                    continue;
                }
                let anchor = points
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(i, left)| {
                        if pull.start_ms < left.key.start_ms {
                            return false;
                        }
                        if let Some(right) = points.get(i + 1) {
                            pull.end_ms <= right.key.start_ms
                                && left.key.end_ms <= right.key.start_ms
                                && agrees(left, right)
                        } else if review.replay.growing {
                            pull.end_ms <= left.key.start_ms.saturating_add(LIVE_INTERVAL_MS)
                        } else {
                            checked_later
                                && *i > 0
                                && points[i - 1].key.end_ms <= left.key.start_ms
                                && agrees(&points[i - 1], left)
                        }
                    })
                    .map(|(_, point)| point);
                let Some(point) = anchor else {
                    continue;
                };
                // Do not bridge a known absence, even if offsets on either side agree.
                if missing
                    .iter()
                    .any(|&(start, end)| start < pull.end_ms && end > point.key.start_ms)
                {
                    continue;
                }
                let key = Key::new(&review.replay, pull, cap, epoch);
                let origin = point.result.video_seconds
                    + (pull.start_ms - point.key.start_ms) as f64 / 1000.0;
                let start = (-origin).max(0.0);
                let end = key
                    .duration()
                    .min(review.replay.available_seconds as f64 - origin);
                if end <= start {
                    continue;
                }
                let mut timeline = point.timeline.clone();
                timeline.duration_seconds = review.replay.available_seconds as f64;
                plan.alignments.push(Alignment {
                    key,
                    timeline,
                    timeline_hash: point.timeline_hash.clone(),
                    signature_revision: revision.clone(),
                    expires_at: expiry,
                    result: ResultData {
                        video_seconds: origin,
                        seek_video_seconds: origin.max(0.0),
                        clipped_start: origin < 0.0,
                        uncertainty_seconds: point.result.uncertainty_seconds,
                        coverage: Coverage {
                            fight_start_seconds: start,
                            fight_end_seconds: end,
                        },
                        evidence_hash: revision.clone(),
                        method_version: cap.algorithm_revision.clone(),
                    },
                });
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
            .filter(|p| ticket_for(p).is_none())
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
            marker_timing: Default::default(),
            marker_fallback: Default::default(),
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
    fn first_then_stable_later_sample_covers_a_continuous_recording() {
        let review = review();
        let mut saved = Snapshot::default();
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 1);
        saved.remember(ticket(&review, &review.pulls[0], 0.0));
        let plan = saved.plan(&review, 0);
        assert_eq!(plan.alignments.len(), 1);
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
        assert_eq!(plan.alignments.len(), 12);
        for a in plan.alignments {
            assert_eq!(
                a.seek(0.0),
                Some(15.25 + (a.key.pull_id - 1) as f64 * 600.0)
            );
        }
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
        saved.remember(ticket(&review, &review.pulls[5], 12.0));
        let plan = saved.plan(&review, 0);
        assert!(plan.next.unwrap().id < 6);
        assert!(!plan
            .alignments
            .iter()
            .any(|a| (2..6).contains(&a.key.pull_id)));
        assert!(plan.alignments.iter().any(|a| a.key.pull_id == 12));
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
        assert_eq!(saved.plan(&review, 0).next.unwrap().id, 2);
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
            .any(|a| a.key.pull_id == 1 || (6..11).contains(&a.key.pull_id)));
        assert!(plan.alignments.iter().any(|a| a.key.pull_id == 12));
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
    fn live_sampling_waits_two_hours_and_has_bounded_future_coverage() {
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
        assert!(!saved
            .plan(&review, 0)
            .alignments
            .iter()
            .any(|a| a.key.pull_id == 13));
        saved.remember(ticket(&review, &extra, 0.0));
        assert!(saved.plan(&review, 0).next.is_none());
    }
    #[test]
    fn expiry_account_and_media_changes_remove_derived_timing() {
        let review = review();
        let mut saved = Snapshot::default();
        let mut first = ticket(&review, &review.pulls[0], 0.0);
        first.job.expires_at = now_ms() + 60000;
        let expiry = first.job.expires_at;
        saved.remember(first);
        saved.remember(ticket(&review, &review.pulls[10], 0.0));
        assert!(saved
            .plan(&review, 0)
            .alignments
            .iter()
            .filter(|a| a.signature_revision == a.result.evidence_hash)
            .all(|a| a.expires_at == expiry));
        assert!(saved.plan(&review, 1).alignments.is_empty());
        let mut changed = review.clone();
        changed.replay.timeline_revision = Some("f".repeat(64));
        assert!(saved.plan(&changed, 0).alignments.is_empty());
        saved.tickets[0].job.expires_at = 1;
        assert!(saved
            .plan(&review, 0)
            .alignments
            .iter()
            .all(|a| a.key.pull_id == 11));
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
