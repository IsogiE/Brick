//! Submit bounded, viewer-specific log matches without changing the visible list.
//! The server applies saved decisions on the next explicit catalog load.
use crate::{
    review_ui::ReviewUi,
    streams::{RecordingCheck, Snapshot, Status, Stream, Vod},
};
use eframe::egui;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

pub(crate) const GRACE_MS: i64 = 12 * 60 * 60 * 1000;
const BATCH: usize = 4;
const PAUSE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(5 * 60);
const REFRESH: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
pub(crate) struct Filter {
    queued: VecDeque<RecordingCheck>,
    pending: Option<RecordingCheck>,
    submission: Option<mpsc::Receiver<Result<(), String>>>,
    cancel: Arc<AtomicBool>,
    epoch: Option<u64>,
    next_catalog: Option<Instant>,
    pause_until: Option<Instant>,
    batch: usize,
}

impl Filter {
    pub(crate) fn catalog_received(&mut self, checks: Vec<RecordingCheck>) {
        self.queued = checks.into_iter().take(32).collect();
        self.next_catalog = Some(Instant::now() + REFRESH);
    }

    pub(crate) fn catalog_started(&mut self) {
        self.next_catalog = Some(Instant::now() + RETRY);
    }

    pub(crate) fn needs_catalog(&self) -> bool {
        self.queued.is_empty()
            && self.pending.is_none()
            && self.submission.is_none()
            && self.next_catalog.is_none_or(|at| Instant::now() >= at)
    }

    pub(crate) fn tick(
        &mut self,
        ctx: &egui::Context,
        snapshot: Option<&Snapshot>,
        active: bool,
        peer: &mut ReviewUi,
    ) {
        if let Some(epoch) = peer.recording_auth_epoch() {
            if self.epoch.is_some_and(|previous| previous != epoch) {
                self.cancel.store(true, Ordering::SeqCst);
                self.cancel = Arc::default();
                self.queued.clear();
                self.pending = None;
                self.submission = None;
                self.next_catalog = None;
                peer.tick(ctx, None);
            }
            self.epoch = Some(epoch);
        }
        if let Some(receiver) = &self.submission {
            match receiver.try_recv() {
                Ok(Ok(())) => self.submission = None,
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.submission = None;
                    self.queued.clear();
                    self.next_catalog = Some(Instant::now() + RETRY);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !active {
            if let Some(check) = self.pending.take() {
                self.queued.push_front(check);
            }
            peer.tick(ctx, None);
            return;
        }
        if let Some(check) = &self.pending {
            let stream = check.vod.as_stream();
            peer.tick_recording_match(ctx, &stream);
            if let Some((epoch, result)) = peer.recording_match(&stream) {
                let check = self.pending.take().unwrap();
                if let Ok(has_raid) = result {
                    if self.epoch.is_none_or(|current| current == epoch)
                        && eligible(&check.vod, snapshot)
                    {
                        self.submission = Some(peer.publish_recording_match(
                            ctx,
                            epoch,
                            check.lease,
                            has_raid,
                            self.cancel.clone(),
                        ));
                    }
                } else {
                    self.next_catalog = Some(Instant::now() + RETRY);
                }
                peer.tick(ctx, None);
            }
        }
        if self.pending.is_some() || self.submission.is_some() {
            return;
        }
        if peer.metadata_busy() {
            ctx.request_repaint_after(Duration::from_secs(1));
            return;
        }
        let now = Instant::now();
        if self.batch >= BATCH {
            self.batch = 0;
            self.pause_until = Some(now + PAUSE);
        }
        if let Some(until) = self.pause_until.filter(|until| now < *until) {
            ctx.request_repaint_after(until.saturating_duration_since(now));
            return;
        }
        while let Some(check) = self.queued.pop_front() {
            if !eligible(&check.vod, snapshot) {
                continue;
            }
            self.batch += 1;
            peer.tick_recording_match(ctx, &check.vod.as_stream());
            self.pending = Some(check);
            break;
        }
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

fn eligible(vod: &Vod, snapshot: Option<&Snapshot>) -> bool {
    let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
    vod.as_stream()
        .replay_range()
        .is_some_and(|(_, end)| end <= now_ms.saturating_sub(GRACE_MS))
        && !snapshot.is_some_and(|snapshot| {
            snapshot
                .streams
                .iter()
                .chain(&snapshot.own_streams)
                .filter(|stream| stream.status == Status::Live)
                .any(|stream| current_broadcast(vod, stream))
        })
}

fn current_broadcast(vod: &Vod, stream: &Stream) -> bool {
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vod() -> Vod {
        serde_json::from_value(serde_json::json!({"id":"123","userId":"11","name":"Raider","provider":"twitch",
            "url":"https://www.twitch.tv/videos/123","startedAt":"2026-01-01T12:00:00Z","endedAt":"2026-01-01T14:00:00Z"})).unwrap()
    }
    #[test]
    fn missing_recent_and_current_live_recordings_are_not_submitted() {
        let mut recording = vod();
        assert!(eligible(&recording, None));
        let mut live = recording.as_stream();
        live.status = Status::Live;
        let snapshot = Snapshot {
            streams: vec![live],
            own_streams: vec![],
            unverified_count: 0,
            own_youtube_channel: None,
        };
        assert!(!eligible(&recording, Some(&snapshot)));
        recording.ended_at = None;
        assert!(!eligible(&recording, None));
        recording.ended_at = Some(
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        assert!(!eligible(&recording, None));
    }
    #[test]
    fn server_tickets_stay_bounded_and_dropping_the_filter_cancels_submissions() {
        let mut filter = Filter::default();
        filter.catalog_received(
            (0..40)
                .map(|_| RecordingCheck {
                    vod: vod(),
                    lease: "synthetic".into(),
                })
                .collect(),
        );
        assert_eq!(filter.queued.len(), 32);
        assert!(!filter.needs_catalog());
        let (sender, receiver) = mpsc::channel();
        filter.submission = Some(receiver);
        sender.send(Ok(())).unwrap();
        filter.tick(
            &egui::Context::default(),
            None,
            false,
            &mut ReviewUi::default(),
        );
        assert!(filter.submission.is_none());
        let cancellation = filter.cancel.clone();
        drop(filter);
        assert!(cancellation.load(Ordering::SeqCst));
    }
}
