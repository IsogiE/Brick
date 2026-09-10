//! Bounded, per-pull ART timestamp calibration using the owned replay player.
#[cfg(test)]
use crate::replay_marker::Marker;
use crate::{
    replay_marker::{self, Reading},
    stream_player::{FrameCapture, PlaybackCommand, StreamPlayer},
    warcraftlogs::{Pull, Replay},
};
use eframe::egui;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Key {
    provider: String,
    video: String,
    broadcast: String,
    recording_start_ms: Option<i64>,
    report: String,
    pull: u64,
    encounter: u64,
    difficulty: u64,
    start_ms: i64,
    end_ms: i64,
}
impl Key {
    pub fn new(replay: &Replay, pull: &Pull) -> Self {
        Self {
            provider: replay.provider.key().into(),
            video: replay.video_id.clone(),
            broadcast: replay.broadcast_id.clone(),
            recording_start_ms: replay.start_ms().ok(),
            report: pull.report.clone(),
            pull: pull.id,
            encounter: pull.encounter,
            difficulty: pull.difficulty,
            start_ms: pull.start_ms,
            end_ms: pull.end_ms,
        }
    }
}

pub use crate::replay_edge::Alignment;
use crate::replay_edge::{Edge, Sample};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Cache(Vec<(Key, Alignment)>);
impl Cache {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        #[derive(serde::Deserialize)]
        struct Saved {
            version: u64,
            entries: Cache,
        }
        if bytes.len() > 2 * 1024 * 1024 {
            return Self::default();
        }
        serde_json::from_slice::<Saved>(bytes)
            .ok()
            .filter(|saved| saved.version == 1 && saved.entries.0.len() <= 2048)
            .map(|saved| saved.entries)
            .unwrap_or_default()
    }
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&serde_json::json!({"version": 1, "entries": self})).ok()
    }
    pub fn get(&self, replay: &Replay, pull: &Pull) -> Option<Alignment> {
        let key = Key::new(replay, pull);
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| *value)
    }
    pub fn insert(&mut self, key: Key, value: Alignment) {
        self.0.retain(|(k, _)| *k != key);
        if self.0.len() == 2048 {
            self.0.remove(0);
        }
        self.0.push((key, value));
    }
    #[cfg(test)]
    pub fn remove_report(&mut self, replay: &Replay, report: &str) {
        self.0.retain(|(k, _)| {
            k.provider != replay.provider.key() || k.video != replay.video_id || k.report != report
        });
    }
}

impl Sample {
    fn from_frame(frame: &FrameCapture, reading: Reading) -> Option<Self> {
        (frame.width > 0
            && frame.height > 0
            && frame.before_seconds.is_finite()
            && frame.after_seconds.is_finite()
            && frame.before_seconds >= 0.0
            && frame.after_seconds >= frame.before_seconds
            && frame.after_seconds - frame.before_seconds <= 0.15
            && frame.playing
            && frame.observed_at.elapsed() < Duration::from_secs(3))
        .then_some(Self {
            reading,
            before: frame.before_seconds,
            after: frame.after_seconds,
        })
    }
}
struct Search {
    started: Instant,
    estimate: f64,
    requested_elapsed: f64,
    autoplay: bool,
    attempt: usize,
    target: f64,
    issued: Option<Instant>,
    capture: Option<u64>,
    edge: Edge,
    last_capture: Option<Instant>,
}
impl Search {
    fn new(estimate: f64, requested_elapsed: f64, autoplay: bool, available: f64) -> Self {
        Self {
            started: Instant::now(),
            estimate,
            requested_elapsed,
            autoplay,
            attempt: 0,
            target: (estimate - 5.0).clamp(0.0, available - 0.001),
            issued: None,
            capture: None,
            edge: Edge::default(),
            last_capture: None,
        }
    }
    fn advance(&mut self, available: f64) {
        self.attempt += 1;
        let offset = match self.attempt {
            1 => -20.0,
            2 => 10.0,
            3 => -35.0,
            _ => 25.0,
        };
        self.target = (self.estimate + offset).clamp(0.0, available - 0.001);
        self.issued = None;
        self.capture = None;
        self.edge = Edge::default();
    }
    fn observe(&mut self, sample: Sample, available: f64) -> Result<Option<Alignment>, ()> {
        if let Some(alignment) = self.edge.observe(sample)? {
            return Ok(Some(alignment));
        }
        if self.edge.marker.is_none() && sample.before >= self.target + 15.0 {
            self.advance(available);
        }
        if self
            .edge
            .first_seen
            .is_some_and(|first| sample.after - first > 6.0)
        {
            return Err(());
        }
        Ok(None)
    }
}
struct ReadResult {
    serial: u64,
    generation: u64,
    sample: Option<Sample>,
}
#[derive(Default)]
pub struct Sync {
    key: Option<Key>,
    serial: u64,
    search: Option<Search>,
    work: Option<mpsc::Receiver<ReadResult>>,
    completed: Option<Alignment>,
    passive: bool,
}
impl Sync {
    pub fn busy(&self) -> bool {
        !self.passive && self.search.is_some()
    }
    pub fn intent(&self) -> Option<(f64, bool)> {
        if self.passive {
            return None;
        }
        self.search
            .as_ref()
            .map(|s| (s.requested_elapsed, s.autoplay))
    }
    pub fn cancel(&mut self, player: Option<&StreamPlayer>) {
        if !self.passive && self.search.is_some() {
            if let Some(player) = player {
                player.prepare_marker_quality(false);
            }
        }
        if self.search.as_ref().is_some_and(|s| s.capture.is_some()) {
            if let Some(player) = player {
                player.cancel_frame_capture();
            }
        }
        self.search = None;
        self.serial = self.serial.wrapping_add(1);
        self.completed = None;
    }
    pub fn reset(&mut self, player: Option<&StreamPlayer>) {
        self.cancel(player);
        self.key = None;
    }
    pub fn take_alignment(&mut self) -> Option<Alignment> {
        self.completed.take()
    }
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        player: &StreamPlayer,
        replay: &Replay,
        pull: &Pull,
        estimate: f64,
        requested_elapsed: f64,
        autoplay: bool,
        known: bool,
        passive: bool,
    ) -> Option<PlaybackCommand> {
        self.passive = passive;
        let key = Key::new(replay, pull);
        if self.key.as_ref() != Some(&key) {
            self.reset(Some(player));
            self.key = Some(key);
            if !known
                && replay_marker::supports_pull(pull)
                && estimate.is_finite()
                && requested_elapsed.is_finite()
                && replay.available_seconds > 0
            {
                if !passive {
                    player.prepare_marker_quality(true);
                }
                self.search = Some(Search::new(
                    estimate,
                    requested_elapsed,
                    autoplay,
                    replay.available_seconds as f64,
                ));
            }
        }
        let result = self.work.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(v) => Some(Some(v)),
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            self.work = None;
            if let Some(result) = result.filter(|r| {
                r.serial == self.serial && r.generation == player.frame_capture_generation()
            }) {
                if let Some(search) = &mut self.search {
                    match result
                        .sample
                        .ok_or(())
                        .and_then(|sample| search.observe(sample, replay.available_seconds as f64))
                    {
                        Ok(Some(alignment)) => {
                            self.completed = Some(alignment);
                            return self.finish(player, replay, Some(alignment));
                        }
                        Err(()) => return self.finish(player, replay, None),
                        Ok(None) => (),
                    }
                }
            }
        }
        let search = self.search.as_mut()?;
        ctx.request_repaint_after(Duration::from_millis(50));
        if search.started.elapsed() > Duration::from_secs(55) || search.attempt >= 5 {
            return self.finish(player, replay, None);
        }
        let state = player.playback_state();
        if passive {
            if !state.ready
                || state.blocked
                || state.buffering
                || !state.playing
                || state.seeking.is_some()
                || state.playback_intent.is_some()
            {
                return None;
            }
            if state.seconds < estimate - 5.0 || state.seconds > estimate + 45.0 {
                return self.finish(player, replay, None);
            }
        }
        if passive {
            // Observe normal playback. The foreground observer never seeks,
            // changes quality, pauses, resumes, or restores a previous position.
        } else if let Some(issued) = search.issued {
            let settled = state.ready
                && !state.blocked
                && !state.buffering
                && state.seeking.is_none()
                && state.playback_intent.is_none()
                && state.is_fresh_since(issued)
                && state.playing;
            if !settled {
                if issued.elapsed() > Duration::from_secs(6) && self.work.is_none() {
                    return self.finish(player, replay, None);
                }
                return None;
            }
            if issued.elapsed() < Duration::from_millis(350) {
                return None;
            }
        } else {
            if self.work.is_some()
                || !state.ready
                || state.blocked
                || state.sync_ready == Some(false)
            {
                return None;
            }
            search.issued = Some(Instant::now());
            return Some(PlaybackCommand::Seek(search.target));
        }
        if let Some(generation) = search.capture {
            if let Some(frame) = player.take_frame_capture() {
                search.capture = None;
                if let Ok(frame) = frame {
                    if frame.generation == generation
                        && generation == player.frame_capture_generation()
                    {
                        let (tx, rx) = mpsc::sync_channel(1);
                        let pull = pull.clone();
                        let serial = self.serial;
                        let ctx = ctx.clone();
                        let marker = search.edge.marker;
                        if std::thread::Builder::new()
                            .name("art-timestamp".into())
                            .spawn(move || {
                                let reading = replay_marker::read(&frame.png, &pull, marker);
                                let sample = Sample::from_frame(&frame, reading);
                                let _ = tx.send(ReadResult {
                                    serial,
                                    generation,
                                    sample,
                                });
                                ctx.request_repaint();
                            })
                            .is_ok()
                        {
                            self.work = Some(rx);
                        } else {
                            return self.finish(player, replay, None);
                        }
                        return None;
                    }
                }
                return self.finish(player, replay, None);
            } else if !player.frame_capture_pending() {
                return self.finish(player, replay, None);
            }
        } else if self.work.is_none()
            && search.last_capture.is_none_or(|at| {
                at.elapsed()
                    >= if search.edge.marker.is_some() {
                        Duration::from_millis(100)
                    } else {
                        Duration::from_millis(500)
                    }
            })
            && player.request_source_frame_capture(ctx)
        {
            search.last_capture = Some(Instant::now());
            search.capture = Some(player.frame_capture_generation());
        }
        None
    }
    fn finish(
        &mut self,
        player: &StreamPlayer,
        replay: &Replay,
        alignment: Option<Alignment>,
    ) -> Option<PlaybackCommand> {
        let search = self.search.take()?;
        if !self.passive {
            player.prepare_marker_quality(false);
        }
        player.cancel_frame_capture();
        self.serial = self.serial.wrapping_add(1);
        if self.passive {
            return None;
        }
        let seconds = (alignment.map_or(search.estimate, |a| a.video_seconds)
            + search.requested_elapsed)
            .clamp(0.0, replay.available_seconds as f64 - 0.001);
        Some(if search.autoplay {
            PlaybackCommand::Seek(seconds)
        } else {
            PlaybackCommand::SeekPaused(seconds)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn marker() -> Marker {
        Marker {
            unix_seconds: 1_788_950_123,
            region: crate::replay_marker::Region {
                x: 4,
                y: 4,
                width: 60,
                height: 10,
            },
            dimensions: (1920, 1080),
            style: 7,
        }
    }
    fn replay() -> Replay {
        Replay {
            provider: crate::streams::Provider::Youtube,
            video_id: "abcDEF_12-3".into(),
            broadcast_id: "broadcast".into(),
            started_at: "2026-09-09T10:00:00Z".into(),
            available_seconds: 7200,
        }
    }
    #[test]
    fn a_confirmed_disappearance_preserves_a_measured_pull_start_interval() {
        for start in [92.137, 100.375, 108.989] {
            let mut search = Search::new(start, 137.625, false, 7200.0);
            let mut finished = None;
            for index in 0..90 {
                let before = start - 1.0 + index as f64 * 0.10;
                let after = before + 0.02;
                let reading = if (start..start + 5.0).contains(&((before + after) / 2.0)) {
                    Reading::Present(marker())
                } else {
                    Reading::Absent
                };
                finished = search
                    .observe(
                        Sample {
                            reading,
                            before,
                            after,
                        },
                        7200.0,
                    )
                    .unwrap();
                if finished.is_some() {
                    break;
                }
            }
            let alignment = finished.expect("continuous samples must finish");
            assert!((alignment.video_seconds - start).abs() <= 0.10);
            assert!(alignment.uncertainty_seconds <= 0.15);
            assert_eq!(search.requested_elapsed, 137.625);
            assert!(!search.autoplay);
        }
    }
    #[test]
    fn late_readable_onset_uses_the_five_second_disappearance_edge() {
        for hidden_until in [0.0, 1.0, 3.0] {
            let mut search = Search::new(100.0, 0.0, true, 7200.0);
            let mut found = None;
            for index in 0..70 {
                let before = 99.0 + index as f64 * 0.1;
                let reading = if before >= 100.0 + hidden_until && before < 105.0 {
                    Reading::Present(marker())
                } else {
                    Reading::Absent
                };
                found = search
                    .observe(
                        Sample {
                            reading,
                            before,
                            after: before + 0.02,
                        },
                        7200.0,
                    )
                    .unwrap();
                if found.is_some() {
                    break;
                }
            }
            assert!((found.unwrap().video_seconds - 100.0).abs() < 0.1);
        }
    }

    #[test]
    fn ambiguous_sparse_or_interrupted_observations_cannot_align() {
        let mut search = Search::new(100.0, 0.0, true, 7200.0);
        for (reading, before) in [
            (Reading::Present(marker()), 103.0),
            (Reading::Present(marker()), 104.8),
            (Reading::Uncertain, 104.9),
            (Reading::Absent, 105.0),
            (Reading::Absent, 105.2),
            (Reading::Absent, 105.4),
        ] {
            assert!(search
                .observe(
                    Sample {
                        reading,
                        before,
                        after: before + 0.02
                    },
                    7200.0
                )
                .unwrap()
                .is_none());
        }
        let mut search = Search::new(100.0, 0.0, true, 7200.0);
        for before in [102.0, 103.0] {
            search
                .observe(
                    Sample {
                        reading: Reading::Present(marker()),
                        before,
                        after: before + 0.02,
                    },
                    7200.0,
                )
                .unwrap();
        }
        for before in [105.0, 105.2] {
            search
                .observe(
                    Sample {
                        reading: Reading::Absent,
                        before,
                        after: before + 0.02,
                    },
                    7200.0,
                )
                .unwrap();
        }
        assert!(search
            .observe(
                Sample {
                    reading: Reading::Absent,
                    before: 105.4,
                    after: 105.42
                },
                7200.0
            )
            .is_err());
    }
    #[test]
    fn saved_timestamps_survive_restart_and_reject_invalid_or_newer_files() {
        let mut cache = Cache::default();
        let replay = replay();
        let pull = crate::replay_marker::tests::pull();
        cache.insert(
            Key::new(&replay, &pull),
            Alignment {
                unix_seconds: pull.start_ms / 1000,
                video_seconds: 12.5,
                uncertainty_seconds: 0.1,
            },
        );
        let bytes = cache.to_bytes().unwrap();
        assert_eq!(
            Cache::from_bytes(&bytes)
                .get(&replay, &pull)
                .unwrap()
                .video_seconds,
            12.5
        );
        assert!(Cache::from_bytes(b"corrupt").get(&replay, &pull).is_none());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = 2.into();
        assert!(Cache::from_bytes(&serde_json::to_vec(&value).unwrap())
            .get(&replay, &pull)
            .is_none());
        let mut other = replay.clone();
        other.started_at = "2026-09-09T10:00:01Z".into();
        assert!(cache.get(&other, &pull).is_none());
    }

    #[test]
    fn cache_is_scoped_to_recording_and_exact_pull_bounds() {
        let mut cache = Cache::default();
        let mut replay = replay();
        let mut pull = crate::replay_marker::tests::pull();
        cache.insert(
            Key::new(&replay, &pull),
            Alignment {
                unix_seconds: 1_788_950_123,
                video_seconds: 123.456,
                uncertainty_seconds: 0.075,
            },
        );
        assert!(cache.get(&replay, &pull).is_some());
        let original_report = pull.report.clone();
        pull.report = "WKHnGLprBXJ862CM".into();
        assert!(
            cache.get(&replay, &pull).is_none(),
            "pull numbers restart in a new report"
        );
        pull.report = original_report;
        replay.provider = crate::streams::Provider::Twitch;
        assert!(cache.get(&replay, &pull).is_none());
        replay.provider = crate::streams::Provider::Youtube;
        pull.start_ms += 1;
        assert!(cache.get(&replay, &pull).is_none());
        pull.start_ms -= 1;
        cache.remove_report(&replay, &pull.report);
        assert!(cache.get(&replay, &pull).is_none());
    }
    #[test]
    fn discovery_uses_bounded_playing_windows_near_the_metadata_estimate() {
        let mut search = Search::new(100.0, 0.0, true, 7200.0);
        let mut targets = vec![search.target];
        for _ in 0..4 {
            search.advance(7200.0);
            targets.push(search.target);
        }
        assert_eq!(targets, vec![95.0, 80.0, 110.0, 65.0, 125.0]);
    }

    #[test]
    #[ignore = "requires an explicit local video-frame sequence and pull metadata"]
    fn recorded_art_marker_sequence() {
        let path = std::path::PathBuf::from(std::env::var_os("BRICK_ART_MARKER_SEQUENCE").unwrap());
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut pull = crate::replay_marker::tests::pull();
        pull.start_ms = fixture["startMs"].as_i64().unwrap();
        pull.difficulty = fixture["difficulty"].as_u64().unwrap();
        let estimate = fixture["videoEstimate"].as_f64().unwrap();
        let expected_unix = fixture["unixSeconds"].as_i64().unwrap();
        let mut search = Search::new(estimate, 0.0, true, 604800.0);
        let mut alignment = None;
        for frame in fixture["frames"].as_array().unwrap() {
            let bytes = std::fs::read(path.parent().unwrap().join(frame["file"].as_str().unwrap()))
                .unwrap();
            let mut reading = replay_marker::read(&bytes, &pull, search.edge.marker);
            let before = frame["seconds"].as_f64().unwrap();
            let after = frame["afterSeconds"]
                .as_f64()
                .unwrap_or(before + 1.0 / 60.0);
            if !(0.0..=0.15).contains(&(after - before)) {
                reading = Reading::Uncertain;
            }
            if let Reading::Present(marker) = reading {
                assert_eq!(marker.unix_seconds, expected_unix);
            }
            eprintln!("{before:.3}: {reading:?}");
            alignment = search
                .observe(
                    Sample {
                        reading,
                        before,
                        after,
                    },
                    604800.0,
                )
                .unwrap();
            if alignment.is_some() {
                break;
            }
        }
        let alignment =
            alignment.expect("the recorded five-second marker must produce a bounded alignment");
        eprintln!("Recorded ART alignment: {alignment:?}");
        assert!(alignment.uncertainty_seconds <= 0.225);
    }
}
