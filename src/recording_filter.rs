//! Reuse the normal per-recording metadata matcher for quiet VOD housekeeping.
//! Decisions are viewer/guild-local and never delete server history or timing.
use crate::{
    review_ui::ReviewUi,
    streams::{Snapshot, Status, Stream, Vod},
};
use eframe::egui;
use std::{
    collections::HashMap,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

pub(crate) const GRACE_MS: i64 = 12 * 60 * 60 * 1000;
const BATCH_SIZE: usize = 4;
const BATCH_PAUSE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5 * 60);
const REFRESH: Duration = Duration::from_secs(60 * 60);

fn key(vod: &Vod) -> String {
    format!(
        "{}:{}:{}:{}",
        vod.provider.key(),
        vod.id,
        vod.started_at.as_deref().unwrap_or(""),
        vod.ended_at.as_deref().unwrap_or("")
    )
}
fn eligible(vod: &Vod, now_ms: i64) -> bool {
    vod.as_stream()
        .replay_range()
        .is_some_and(|(_, end)| end <= now_ms.saturating_sub(GRACE_MS))
}
struct Checked {
    at: Instant,
    has_raid: Option<bool>,
}
impl Checked {
    fn due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at)
            >= if self.has_raid.is_some() {
                REFRESH
            } else {
                RETRY
            }
    }
}
#[derive(Default)]
pub(crate) struct Filter {
    checked: HashMap<String, Checked>,
    pending: Option<Vod>,
    pause_until: Option<Instant>,
    batch_count: usize,
    auth_epoch: Option<u64>,
    source: Weak<Vec<Vod>>,
    live_source: Weak<Snapshot>,
    live_streams: Vec<Stream>,
    visible: Option<Rc<Vec<Vod>>>,
}
impl Filter {
    fn current_broadcast(&self, vod: &Vod) -> bool {
        self.live_streams.iter().any(|stream| {
            if stream.user_id != vod.user_id || stream.provider != vod.provider {
                return false;
            }
            if let Some(id) = &stream.recording_id {
                return id == &vod.id;
            }
            if stream.provider == crate::streams::Provider::Youtube {
                return stream.channel_id == vod.id;
            }
            if let Some((vod_start, vod_end)) = vod.as_stream().replay_range() {
                if let Some((start, end)) = stream.replay_range() {
                    return start < vod_end && end > vod_start;
                }
                if let Some(start) = stream.replay_start_ms.filter(|start| *start > 0) {
                    return start < vod_end;
                }
            }
            // Twitch live snapshots can lack the archive identity/range.
            true
        })
    }
    pub(crate) fn visible(&mut self, source: &Rc<Vec<Vod>>) -> Rc<Vec<Vod>> {
        if !Weak::ptr_eq(&self.source, &Rc::downgrade(source)) || self.visible.is_none() {
            self.source = Rc::downgrade(source);
            let keys: std::collections::HashSet<_> = source.iter().map(key).collect();
            self.checked.retain(|key, _| keys.contains(key));
            let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
            self.visible = Some(Rc::new(
                source
                    .iter()
                    .filter(|vod| {
                        !eligible(vod, now_ms)
                            || self.current_broadcast(vod)
                            || self
                                .checked
                                .get(&key(vod))
                                .is_none_or(|result| result.has_raid != Some(false))
                    })
                    .cloned()
                    .collect(),
            ));
        }
        self.visible.as_ref().unwrap().clone()
    }

    // The existing peer owns the shared Client, request cancellation, pagination,
    // report cache and provider backoff. There is no second report-query path.
    pub(crate) fn tick(
        &mut self,
        ctx: &egui::Context,
        source: Option<&Rc<Vec<Vod>>>,
        snapshot: Option<&Rc<Snapshot>>,
        active: bool,
        peer: &mut ReviewUi,
    ) -> Option<Stream> {
        if let Some(snapshot) = snapshot {
            if !Weak::ptr_eq(&self.live_source, &Rc::downgrade(snapshot)) {
                self.live_source = Rc::downgrade(snapshot);
                self.live_streams = snapshot
                    .streams
                    .iter()
                    .chain(&snapshot.own_streams)
                    .filter(|stream| stream.status == Status::Live)
                    .cloned()
                    .collect();
                self.visible = None;
            }
        }
        if let Some(epoch) = peer.recording_auth_epoch() {
            if self.auth_epoch != Some(epoch) {
                let initialized = self.auth_epoch.is_some();
                self.auth_epoch = Some(epoch);
                self.checked.clear();
                self.visible = None;
                if initialized {
                    self.pending = None;
                    peer.tick(ctx, None);
                }
            }
        }
        if !active {
            self.pending = None;
            return None;
        }
        if let Some(vod) = &self.pending {
            let stream = vod.as_stream();
            if let Some((epoch, result)) = peer.recording_match(&stream) {
                if self.auth_epoch.is_none_or(|current| current == epoch) {
                    self.checked.insert(
                        key(vod),
                        Checked {
                            at: Instant::now(),
                            has_raid: result.ok(),
                        },
                    );
                    self.visible = None;
                }
                self.pending = None;
                peer.tick(ctx, None);
            } else {
                return Some(stream);
            }
        }
        let now = Instant::now();
        if self.batch_count >= BATCH_SIZE {
            self.pause_until = Some(now + BATCH_PAUSE);
            self.batch_count = 0;
        }
        if let Some(until) = self.pause_until.filter(|until| now < *until) {
            ctx.request_repaint_after(until.saturating_duration_since(now));
            return None;
        }
        // Let an already-running live warmup finish before borrowing its peer.
        if peer.metadata_busy() {
            ctx.request_repaint_after(Duration::from_secs(1));
            return None;
        }
        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let vod = source?
            .iter()
            .filter(|vod| eligible(vod, now_ms) && !self.current_broadcast(vod))
            .filter(|vod| {
                self.checked
                    .get(&key(vod))
                    .is_none_or(|result| result.due(now))
            })
            .min_by_key(|vod| self.checked.get(&key(vod)).map(|result| result.at));
        if let Some(vod) = vod {
            self.batch_count += 1;
            self.pending = Some(vod.clone());
            Some(vod.as_stream())
        } else {
            ctx.request_repaint_after(BATCH_PAUSE);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn vod(id: &str, provider: &str, end: i64) -> Vod {
        let date = |ms| {
            time::OffsetDateTime::from_unix_timestamp(ms / 1000)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        serde_json::from_value(json!({"id":id,"userId":"11","name":"Raider","provider":provider,
            "url":"https://www.twitch.tv/videos/123","startedAt":date(end-7_200_000),"endedAt":date(end)})).unwrap()
    }
    #[test]
    fn only_completed_recordings_after_twelve_hours_are_eligible() {
        let now = 1_789_000_000_000;
        for provider in ["twitch", "youtube"] {
            assert!(!eligible(
                &vod("123", provider, now - GRACE_MS + 1_000),
                now
            ));
            assert!(eligible(&vod("123", provider, now - GRACE_MS), now));
            let mut live = vod("123", provider, now - GRACE_MS);
            live.ended_at = None;
            assert!(!eligible(&live, now));
            live.started_at = None;
            assert!(!eligible(&live, now));
        }
    }
    #[test]
    fn only_successful_no_match_hides_and_later_match_restores_original_vod() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let source = Rc::new(vec![
            vod("123", "twitch", now - GRACE_MS - 1_000),
            vod("abcDEF12345", "youtube", now - GRACE_MS - 1_000),
        ]);
        let mut filter = Filter::default();
        assert_eq!(filter.visible(&source).len(), 2);
        filter.checked.insert(
            key(&source[0]),
            Checked {
                at: Instant::now(),
                has_raid: Some(false),
            },
        );
        filter.checked.insert(
            key(&source[1]),
            Checked {
                at: Instant::now(),
                has_raid: None,
            },
        );
        filter.visible = None;
        assert_eq!(filter.visible(&source).len(), 1);
        for result in [None, Some(true)] {
            filter.checked.get_mut(&key(&source[0])).unwrap().has_raid = result;
            filter.visible = None;
            let restored = filter.visible(&source);
            assert_eq!(restored.len(), 2);
            assert_eq!(restored[0].started_at, source[0].started_at);
            assert_eq!(restored[0].ended_at, source[0].ended_at);
        }
    }
    #[test]
    fn checks_are_bounded_and_successful_and_failed_results_have_distinct_retry_times() {
        let now = Instant::now();
        assert!(!Checked {
            at: now,
            has_raid: None
        }
        .due(now + RETRY - Duration::from_secs(1)));
        assert!(Checked {
            at: now,
            has_raid: None
        }
        .due(now + RETRY));
        assert!(!Checked {
            at: now,
            has_raid: Some(false)
        }
        .due(now + RETRY));
        assert!(Checked {
            at: now,
            has_raid: Some(false)
        }
        .due(now + REFRESH));
        let mut filter = Filter {
            pause_until: Some(now + BATCH_PAUSE),
            batch_count: BATCH_SIZE,
            ..Default::default()
        };
        let mut peer = ReviewUi::default();
        let source = Rc::new(vec![vod("123", "twitch", 1_600_000_000_000)]);
        assert!(filter
            .tick(
                &egui::Context::default(),
                Some(&source),
                None,
                true,
                &mut peer
            )
            .is_none());
        assert!(filter.pending.is_none());
    }
    #[test]
    fn live_guard_uses_current_identity_or_range_and_keeps_unknown_broadcasts_safe() {
        let vod = vod("123", "twitch", 1_600_000_000_000);
        let mut live = vod.as_stream();
        live.status = Status::Live;
        let mut filter = Filter {
            live_streams: vec![live.clone()],
            ..Default::default()
        };
        assert!(filter.current_broadcast(&vod));
        filter.live_streams[0].recording_id = Some("456".into());
        assert!(!filter.current_broadcast(&vod));
        live.recording_id = None;
        live.replay_start_ms = Some(1_600_000_001_000);
        live.replay_end_ms = Some(1_600_000_002_000);
        filter.live_streams = vec![live.clone()];
        assert!(!filter.current_broadcast(&vod));
        live.replay_start_ms = None;
        live.replay_end_ms = None;
        filter.live_streams = vec![live];
        assert!(filter.current_broadcast(&vod));
    }

    #[test]
    fn changed_recording_metadata_and_removed_recordings_do_not_reuse_absence() {
        let source = Rc::new(vec![vod("123", "twitch", 1_600_000_000_000)]);
        let mut filter = Filter::default();
        filter.checked.insert(
            key(&source[0]),
            Checked {
                at: Instant::now(),
                has_raid: Some(false),
            },
        );
        assert!(filter.visible(&source).is_empty());
        let changed = Rc::new(vec![vod("123", "twitch", 1_600_000_001_000)]);
        assert_eq!(filter.visible(&changed).len(), 1);
        assert!(filter.checked.is_empty());
    }
}
