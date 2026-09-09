//! Opt-in local clock observations. Nothing here changes replay alignment.

use crate::{
    replay_ocr::{
        decode_snapshot, BossName, ClockCandidate, ClockRegion, HealthCandidate, HealthRegion,
        OcrError, ReplayOcr,
    },
    stream_player::{FrameCapture, PlaybackState, StreamPlayer},
    warcraftlogs::{Pull, Replay},
};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
const DISCOVERY_RETRY: Duration = Duration::from_secs(15);
const MAX_DISCOVERIES: u8 = 3;
const MAX_MISSES: u8 = 3;
const MAX_REGIONS: usize = 8;
const MAX_CANDIDATES: usize = 32;
const MAX_RESULT_AGE: Duration = Duration::from_secs(10);
static MODEL_DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Call as the first operation in main, before application threads exist.
/// Models are local, explicitly selected and hash-verified by ReplayOcr::load.
pub fn configure_at_startup() {
    let directory = std::env::var_os("BRICK_REPLAY_OCR_MODEL_DIR")
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    if directory.is_some() {
        // SAFETY: main calls this before starting the GUI, watchers or workers.
        // RTen uses a global pool, so setting a limit inside the worker is late.
        unsafe { std::env::set_var("RTEN_NUM_THREADS", "1") };
    }
    let _ = MODEL_DIRECTORY.set(directory);
}

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub provider: &'static str,
    pub video_id: String,
    pub broadcast_id: String,
    pub report_code: String,
    pub pull_id: u64,
    pub pull_start_ms: i64,
    pub pull_end_ms: i64,
}

impl Identity {
    pub fn new(replay: &Replay, pull: &Pull) -> Self {
        Self {
            provider: replay.provider.key(),
            video_id: replay.video_id.clone(),
            broadcast_id: replay.broadcast_id.clone(),
            report_code: pull.report.clone(),
            pull_id: pull.id,
            pull_start_ms: pull.start_ms,
            pull_end_ms: pull.end_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Discovery,
    Tracking,
}

#[derive(Clone)]
pub struct Reading {
    pub region_id: u32,
    pub elapsed_seconds: u32,
}

#[derive(Clone)]
pub struct HealthReading {
    pub region_id: u32,
    pub candidate: HealthCandidate,
}

/// Candidate clocks from one frame, never a verified encounter time or offset.
pub struct Observation {
    pub identity: Identity,
    pub width: u32,
    pub height: u32,
    pub observed_at: Instant,
    pub before_seconds: f64,
    pub after_seconds: f64,
    #[cfg(test)]
    pub sampling_uncertainty_seconds: f64,
    pub readings: Vec<Reading>,
    pub health_readings: Vec<HealthReading>,
    pub health_assessment: Option<crate::replay_health::Assessment>,
    pub phase: Phase,
    /// Wall time spent decoding/recognizing this image, excluding model loading.
    pub processing_duration: Duration,
    pub model_load_duration: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Disabled,
    Waiting,
    Observing,
    NoClock,
    Ambiguous,
    Exhausted,
    Unavailable,
}

#[derive(Default, Clone, Copy)]
pub struct Statistics {
    pub captured_frames: u32,
    pub discoveries: u32,
    pub tracked_frames: u32,
    pub discarded_results: u32,
    pub processing_duration: Duration,
    pub model_load_duration: Duration,
}

#[derive(Clone, PartialEq, Eq)]
struct Epoch {
    identity: Identity,
    generation: u64,
    session: u64,
}

#[derive(Clone)]
struct Region {
    id: u32,
    bounds: ClockRegion,
    previous: Option<(f64, u32)>,
    increasing_samples: u8,
}

#[derive(Clone)]
struct TrackedHealth {
    id: u32,
    region: HealthRegion,
    misses: u8,
}

struct Job {
    epoch: Epoch,
    frame: FrameCapture,
    regions: Vec<Region>,
    health_regions: Vec<TrackedHealth>,
    boss_names: Vec<BossName>,
    discover_clocks: bool,
    discover_health: bool,
    health_region_base: u32,
    health_window: Option<Arc<crate::warcraftlogs::HealthWindow>>,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
}

struct Completed {
    key: CompletionKey,
    result: Result<(Observation, Vec<Region>, Vec<TrackedHealth>), Failure>,
}

/// A cancelled completion contains no report, recording, player or actor data.
/// The session changes on every identity/authorization reset, so these scalar
/// generations suffice for rejecting obsolete replies.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CompletionKey {
    generation: u64,
    session: u64,
}

impl From<&Epoch> for CompletionKey {
    fn from(epoch: &Epoch) -> Self {
        Self {
            generation: epoch.generation,
            session: epoch.session,
        }
    }
}

fn publish(
    responses: &SyncSender<Completed>,
    publication: &Mutex<()>,
    cancel: &AtomicBool,
    mut completed: Completed,
) -> bool {
    let _guard = publication
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cancel.load(Ordering::Relaxed) {
        completed.result = Err(Failure::Cancelled);
    }
    responses.try_send(completed).is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Cancelled,
    NoClock,
    Ambiguous,
    Image,
    Model,
    Inference,
}

struct Worker {
    jobs: SyncSender<Job>,
    results: Receiver<Completed>,
    clear: Arc<AtomicBool>,
    publication: Arc<Mutex<()>>,
}

impl Worker {
    fn start(directory: PathBuf) -> Result<Self, ()> {
        let (jobs, requests) = mpsc::sync_channel::<Job>(1);
        let (responses, results) = mpsc::sync_channel(1);
        let clear = Arc::new(AtomicBool::new(false));
        let worker_clear = clear.clone();
        let publication = Arc::new(Mutex::new(()));
        let worker_publication = publication.clone();
        std::thread::Builder::new()
            .name("replay-observer".into())
            .spawn(move || {
                let mut engine: Option<Result<ReplayOcr, OcrError>> = None;
                let mut health = HealthSession::default();
                loop {
                    let request = requests.recv_timeout(Duration::from_secs(1));
                    if worker_clear.swap(false, Ordering::Relaxed) {
                        health = HealthSession::default();
                    }
                    let job = match request {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    #[cfg(test)]
                    let diagnostic_started = Instant::now();
                    let key = CompletionKey::from(&job.epoch);
                    let ctx = job.ctx.clone();
                    let result = (|| {
                        current(&job.cancel)?;
                        let loading = Instant::now();
                        let newly_loaded = engine.is_none();
                        let engine = engine.get_or_insert_with(|| {
                            ReplayOcr::load(
                                &directory.join("text-detection.rten"),
                                &directory.join("text-recognition.rten"),
                            )
                        });
                        let load_duration = if newly_loaded {
                            loading.elapsed()
                        } else {
                            Duration::ZERO
                        };
                        current(&job.cancel)?;
                        let engine = engine.as_ref().map_err(|_| Failure::Model)?;
                        let (mut observation, regions, health_regions) =
                            recognize(engine, &job, load_duration)?;
                        current(&job.cancel)?;
                        observation.health_assessment = health.observe(&job, &observation);
                        current(&job.cancel)?;
                        Ok((observation, regions, health_regions))
                    })();
                    if result.is_err() {
                        // A failed capture cannot silently disappear from a
                        // supposedly continuous visual sequence.
                        health.frames.clear();
                    }
                    #[cfg(test)]
                    eprintln!(
                        "Native OCR job: discovery={}, elapsed_ms={}, outcome={:?}",
                        job.regions.is_empty(),
                        diagnostic_started.elapsed().as_millis(),
                        result
                            .as_ref()
                            .map(|(observation, _, _)| observation.readings.len()
                                + observation.health_readings.len())
                            .map_err(|error| *error)
                    );
                    // The caller submits one job only after consuming the prior
                    // result. A full/disconnected slot must never block shutdown.
                    if !publish(
                        &responses,
                        &worker_publication,
                        &job.cancel,
                        Completed { key, result },
                    ) {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .map_err(|_| ())?;
        Ok(Self {
            jobs,
            results,
            clear,
            publication,
        })
    }
}

pub struct Observer {
    directory: Option<PathBuf>,
    worker: Option<Worker>,
    in_flight: Option<Arc<AtomicBool>>,
    capture_epoch: Option<Epoch>,
    epoch: Option<Epoch>,
    dimensions: Option<(u32, u32)>,
    regions: Vec<Region>,
    health_regions: Vec<TrackedHealth>,
    boss_names: Vec<BossName>,
    health_discoveries: u8,
    health_retry_after: Option<Instant>,
    health_window: Option<Arc<crate::warcraftlogs::HealthWindow>>,
    session: u64,
    discoveries: u8,
    misses: u8,
    last_capture: Option<Instant>,
    retry_after: Option<Instant>,
    observation: Option<Observation>,
    status: Status,
    statistics: Statistics,
}

impl Default for Observer {
    fn default() -> Self {
        Self::with_directory(MODEL_DIRECTORY.get().cloned().flatten())
    }
}

impl Observer {
    #[cfg(test)]
    pub(crate) fn for_native_test() -> Self {
        // The ignored harness is launched as a separate process with this set
        // externally, before the Rust test runner creates any threads.
        assert_eq!(std::env::var("RTEN_NUM_THREADS").as_deref(), Ok("1"));
        let path = PathBuf::from(std::env::var_os("BRICK_REPLAY_OCR_MODEL_DIR").unwrap());
        assert!(path.is_absolute());
        Self::with_directory(Some(path))
    }

    fn with_directory(directory: Option<PathBuf>) -> Self {
        Self {
            status: if directory.is_some() {
                Status::Waiting
            } else {
                Status::Disabled
            },
            directory,
            worker: None,
            in_flight: None,
            capture_epoch: None,
            epoch: None,
            dimensions: None,
            regions: Vec::new(),
            health_regions: Vec::new(),
            boss_names: Vec::new(),
            health_discoveries: 0,
            health_retry_after: None,
            health_window: None,
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            discoveries: 0,
            misses: 0,
            last_capture: None,
            retry_after: None,
            observation: None,
            statistics: Statistics::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.directory.is_some()
    }

    #[cfg(test)]
    pub fn status(&self) -> Status {
        self.status
    }
    #[cfg(test)]
    pub fn statistics(&self) -> Statistics {
        self.statistics
    }
    pub fn observation(&self) -> Option<&Observation> {
        self.observation.as_ref()
    }

    /// Keep the one worker/model allocation across tab changes and sign-out.
    /// Outstanding inference can finish, but its result cannot be published.
    pub fn reset(&mut self) {
        // Serialize cancellation/draining with the worker's final cancellation
        // check and publication. A result cannot race into the idle queue after
        // reset has purged it; ongoing work can publish only a scalar cancel reply.
        let publication = self
            .worker
            .as_ref()
            .map(|worker| worker.publication.clone());
        let _guard = publication.as_ref().map(|publication| {
            publication
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        });
        if let Some(worker) = &self.worker {
            worker.clear.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = &self.in_flight {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = &self.worker {
            while worker.results.try_recv().is_ok() {
                self.in_flight = None;
                self.statistics.discarded_results += 1;
            }
        }
        self.capture_epoch = None;
        self.epoch = None;
        self.dimensions = None;
        self.regions.clear();
        self.health_regions.clear();
        self.boss_names.clear();
        self.health_discoveries = 0;
        self.health_retry_after = None;
        self.health_window = None;
        self.session = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        self.discoveries = 0;
        self.misses = 0;
        self.retry_after = None;
        self.observation = None;
        if self.status != Status::Unavailable {
            self.status = if self.enabled() {
                Status::Waiting
            } else {
                Status::Disabled
            };
        }
    }

    #[cfg(test)]
    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        player: Option<&StreamPlayer>,
        identity: Option<Identity>,
    ) {
        self.tick_with_health(ctx, player, identity, &[], None);
    }

    pub fn tick_with_health(
        &mut self,
        ctx: &egui::Context,
        player: Option<&StreamPlayer>,
        identity: Option<Identity>,
        boss_names: &[BossName],
        health_window: Option<&Arc<crate::warcraftlogs::HealthWindow>>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut epoch = identity.zip(player).map(|(identity, player)| Epoch {
            identity,
            generation: player.frame_capture_generation(),
            session: self.session,
        });
        if self.epoch != epoch || self.boss_names != boss_names {
            if self.capture_epoch.is_some() {
                if let Some(player) = player {
                    player.cancel_frame_capture();
                    if let Some(epoch) = &mut epoch {
                        epoch.generation = player.frame_capture_generation();
                    }
                }
            }
            self.reset();
            if let Some(epoch) = &mut epoch {
                epoch.session = self.session;
            }
            self.epoch = epoch;
            // Names come from this viewer's protected WCL query, and remain in
            // native memory. They never enter the provider page or a network OCR API.
            if boss_names.len() <= 32 {
                self.boss_names = boss_names.to_vec();
            }
        }
        self.health_window = health_window.cloned();
        let settled = player.is_some_and(|player| eligible(&player.playback_state()))
            && self.epoch.is_some()
            && ctx.input(|input| input.viewport().visible().unwrap_or(true));
        if !settled {
            if let Some(cancel) = &self.in_flight {
                cancel.store(true, Ordering::Relaxed);
            }
            if self.capture_epoch.take().is_some() {
                self.discard_capture();
                if let Some(player) = player {
                    player.cancel_frame_capture();
                }
            }
            self.observation = None;
        }
        self.receive(settled);
        if !settled || self.status == Status::Unavailable {
            return;
        }
        let player = player.unwrap();
        if let Some(captured_for) = self.capture_epoch.clone() {
            if let Some(result) = player.take_frame_capture() {
                self.capture_epoch = None;
                match result {
                    Ok(frame)
                        if captured_for == *self.epoch.as_ref().unwrap()
                            && frame.generation == captured_for.generation
                            && frame.playing
                            && frame.observed_at.elapsed() < MAX_RESULT_AGE =>
                    {
                        self.submit(ctx, captured_for, frame);
                    }
                    _ => self.discard_capture(),
                }
            } else if !player.frame_capture_pending() {
                self.capture_epoch = None;
                self.discard_capture();
            }
            return;
        }
        if self.in_flight.is_some() || !self.can_sample(Instant::now()) {
            return;
        }
        if player.request_frame_capture(ctx) {
            self.last_capture = Some(Instant::now());
            self.capture_epoch = self.epoch.clone();
            self.statistics.captured_frames += 1;
        }
    }

    fn can_sample(&self, now: Instant) -> bool {
        self.last_capture
            .is_none_or(|at| now.duration_since(at) >= SAMPLE_INTERVAL)
            && self.retry_after.is_none_or(|at| now >= at)
            && !(self.regions.is_empty()
                && self.discoveries >= MAX_DISCOVERIES
                && (self.boss_names.is_empty()
                    || (self.health_regions.is_empty()
                        && self.health_discoveries >= MAX_DISCOVERIES)))
    }

    fn discard_capture(&mut self) {
        self.statistics.discarded_results += 1;
        self.observation = None;
        if let Some(worker) = &self.worker {
            worker.clear.store(true, Ordering::Relaxed);
        }
    }

    fn submit(&mut self, ctx: &egui::Context, epoch: Epoch, frame: FrameCapture) {
        if self.in_flight.is_some() {
            return;
        }
        if self.dimensions != Some((frame.width, frame.height)) {
            self.regions.clear();
            self.health_regions.clear();
            self.dimensions = Some((frame.width, frame.height));
        }
        if self.worker.is_none() {
            self.worker = Worker::start(self.directory.as_ref().unwrap().clone()).ok();
        }
        let Some(worker) = &self.worker else {
            self.status = Status::Unavailable;
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let discover_clocks = self.regions.is_empty() && self.discoveries < MAX_DISCOVERIES;
        let discover_health = !self.boss_names.is_empty()
            && self.health_regions.is_empty()
            && self.health_discoveries < MAX_DISCOVERIES
            && self
                .health_retry_after
                .is_none_or(|at| Instant::now() >= at);
        let job = Job {
            epoch,
            frame,
            regions: self.regions.clone(),
            health_regions: self.health_regions.clone(),
            boss_names: self.boss_names.clone(),
            discover_clocks,
            discover_health,
            health_region_base: 1_000 + u32::from(self.health_discoveries) * MAX_REGIONS as u32,
            health_window: self.health_window.clone(),
            cancel: cancel.clone(),
            ctx: ctx.clone(),
        };
        if worker.jobs.try_send(job).is_ok() {
            self.in_flight = Some(cancel);
            self.status = Status::Observing;
            if discover_clocks {
                self.discoveries += 1;
            }
            if discover_health {
                self.health_discoveries += 1;
                self.health_retry_after = Some(Instant::now() + DISCOVERY_RETRY);
            }
            if discover_clocks || discover_health {
                self.statistics.discoveries += 1;
            } else {
                self.statistics.tracked_frames += 1;
            }
        } else {
            self.status = Status::Unavailable;
        }
    }

    fn receive(&mut self, settled: bool) {
        let Some(worker) = &self.worker else {
            return;
        };
        let completed = match worker.results.try_recv() {
            Ok(completed) => completed,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.status = Status::Unavailable;
                return;
            }
        };
        let cancelled = self
            .in_flight
            .take()
            .is_none_or(|cancel| cancel.load(Ordering::Relaxed));
        if cancelled
            || !settled
            || self.epoch.as_ref().map(CompletionKey::from) != Some(completed.key)
        {
            self.statistics.discarded_results += 1;
            return;
        }
        match completed.result {
            Ok((mut observation, mut regions, health_regions))
                if observation.observed_at.elapsed() < MAX_RESULT_AGE =>
            {
                // Rediscovery may locate a different clock after a layout
                // change. Never reuse the previous clock region's identity.
                if observation.phase == Phase::Discovery {
                    let base = u32::from(self.discoveries.saturating_sub(1)) * MAX_REGIONS as u32;
                    for region in &mut regions {
                        region.id += base;
                    }
                    for reading in &mut observation.readings {
                        reading.region_id += base;
                    }
                }
                self.statistics.processing_duration += observation.processing_duration;
                self.statistics.model_load_duration += observation.model_load_duration;
                self.misses = 0;
                self.regions = regions;
                self.health_regions = health_regions;
                self.observation = Some(observation);
                self.status = Status::Observing;
            }
            Err(Failure::Model) => self.status = Status::Unavailable,
            Err(Failure::Cancelled) | Ok(_) => self.statistics.discarded_results += 1,
            Err(failure) => {
                self.observation = None;
                self.misses += 1;
                self.status = if failure == Failure::Ambiguous {
                    Status::Ambiguous
                } else {
                    Status::NoClock
                };
                if self.regions.is_empty() || self.misses >= MAX_MISSES {
                    self.regions.clear();
                    self.health_regions.clear();
                    self.misses = 0;
                    self.retry_after = Some(Instant::now() + DISCOVERY_RETRY);
                    if self.discoveries >= MAX_DISCOVERIES {
                        self.status = Status::Exhausted;
                    }
                }
            }
        }
    }
}

/// Lives on the single OCR worker. Only scalar observations and a bounded WCL
/// trace are retained; frames are released after recognition. Reset clears this
/// state even while the worker is idle, without reloading model weights.
#[derive(Default)]
struct HealthSession {
    epoch: Option<Epoch>,
    dimensions: Option<(u32, u32)>,
    window: Option<Arc<crate::warcraftlogs::HealthWindow>>,
    frames: Vec<crate::replay_health::Frame>,
    traces: Vec<crate::replay_health::Trace>,
    sequence: u64,
}

impl HealthSession {
    fn observe(
        &mut self,
        job: &Job,
        observation: &Observation,
    ) -> Option<crate::replay_health::Assessment> {
        use crate::replay_health::{Candidate, Context, Frame, HealthPoint, Interval, Rect, Trace};
        use sha2::{Digest, Sha256};
        let window = job.health_window.as_ref()?;
        let dimensions = (observation.width, observation.height);
        let changed = self.epoch.as_ref() != Some(&job.epoch)
            || self.dimensions != Some(dimensions)
            || self
                .window
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, window));
        if changed {
            *self = Self::default();
            self.epoch = Some(job.epoch.clone());
            self.dimensions = Some(dimensions);
            self.window = Some(window.clone());
        }
        let mut hash = Sha256::new();
        let identity = &job.epoch.identity;
        for value in [
            identity.provider,
            &identity.video_id,
            &identity.broadcast_id,
            &identity.report_code,
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        for value in [
            identity.pull_id,
            identity.pull_start_ms as u64,
            identity.pull_end_ms as u64,
            job.epoch.session,
        ] {
            hash.update(value.to_be_bytes());
        }
        let context = Context {
            scope: hash.finalize().into(),
            capture_generation: job.epoch.generation,
            viewport_generation: (u64::from(dimensions.0) << 32) | u64::from(dimensions.1),
            segment_id: job.epoch.session,
        };
        if changed {
            self.traces = window
                .traces
                .iter()
                .filter_map(|trace| {
                    let normalized = crate::replay_ocr::normalize_boss_name(&trace.name);
                    let name = job.boss_names.iter().find(|name| {
                        crate::replay_ocr::normalize_boss_name(&name.name) == normalized
                    })?;
                    Some(Trace {
                        context,
                        actor_id: trace.actor,
                        instance_id: trace.instance,
                        boss_name_id: name.id,
                        points: trace
                            .points
                            .iter()
                            .map(|point| HealthPoint {
                                elapsed_seconds: point.elapsed_seconds,
                                hit_points: point.hit_points,
                                max_hit_points: point.max_hit_points,
                            })
                            .collect(),
                    })
                })
                .collect();
        }
        if observation.health_readings.is_empty() {
            self.frames.clear();
            return None;
        }
        let media = Interval {
            lower_seconds: observation.before_seconds,
            upper_seconds: observation.after_seconds,
        };
        if self.frames.last().is_some_and(|last| {
            last.media.upper_seconds >= media.lower_seconds
                || media.lower_seconds - last.media.upper_seconds > 10.0
        }) {
            self.frames.clear();
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.frames.push(Frame {
            context,
            sequence: self.sequence,
            media,
            fresh: observation.observed_at.elapsed() < MAX_RESULT_AGE,
            playing: job.frame.playing,
            buffering: false,
            candidates: observation
                .health_readings
                .iter()
                .map(|reading| {
                    let candidate = &reading.candidate;
                    let region = candidate.percent_region;
                    Candidate {
                        region_id: reading.region_id,
                        boss_name_id: candidate.boss_name_id,
                        region: Rect {
                            x: region.x as f64,
                            y: region.y as f64,
                            width: region.width as f64,
                            height: region.height as f64,
                        },
                        percent: candidate.percent,
                        decimal_places: candidate.decimal_places,
                    }
                })
                .collect(),
        });
        if self.frames.len() > 16 {
            self.frames.remove(0);
        }
        let duration = (identity.pull_end_ms - identity.pull_start_ms) as f64 / 1000.0;
        crate::replay_health::match_health(context, &self.frames, &self.traces, duration).ok()
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.reset();
    }
}

fn eligible(state: &PlaybackState) -> bool {
    state.ready
        && state.playing
        && state.is_fresh()
        && !state.buffering
        && state.seeking.is_none()
        && state.playback_intent.is_none()
}

fn current(cancel: &AtomicBool) -> Result<(), Failure> {
    if cancel.load(Ordering::Relaxed) {
        Err(Failure::Cancelled)
    } else {
        Ok(())
    }
}

fn recognize(
    engine: &ReplayOcr,
    job: &Job,
    model_load_duration: Duration,
) -> Result<(Observation, Vec<Region>, Vec<TrackedHealth>), Failure> {
    current(&job.cancel)?;
    let started = Instant::now();
    let image = decode_snapshot(&job.frame.png).map_err(|_| Failure::Image)?;
    if image.width() != job.frame.width || image.height() != job.frame.height {
        return Err(Failure::Image);
    }
    current(&job.cancel)?;
    let phase = if job.discover_clocks {
        Phase::Discovery
    } else {
        Phase::Tracking
    };
    let mut regions = job.regions.clone();
    let mut readings = Vec::new();
    let video_seconds = job.frame.estimated_seconds();
    if job.discover_clocks {
        let candidates = engine
            .discover_rgb(image.as_raw(), image.width(), image.height())
            .map_err(ocr_failure)?;
        current(&job.cancel)?;
        match discover_regions(candidates, video_seconds) {
            Ok(found) => (regions, readings) = found,
            Err(Failure::NoClock) => (),
            Err(error) => return Err(error),
        }
    } else {
        for region in &mut regions {
            current(&job.cancel)?;
            let candidates = engine
                .read_region_rgb(image.as_raw(), image.width(), image.height(), region.bounds)
                .map_err(ocr_failure)?;
            current(&job.cancel)?;
            readings.extend(track_region(region, candidates, video_seconds));
            if readings.len() > MAX_CANDIDATES {
                return Err(Failure::Ambiguous);
            }
        }
    }
    let mut health_regions = job.health_regions.clone();
    let mut health_readings = Vec::new();
    if job.discover_health {
        current(&job.cancel)?;
        let candidates = engine
            .discover_health_rgb(
                image.as_raw(),
                image.width(),
                image.height(),
                &job.boss_names,
            )
            .map_err(ocr_failure)?;
        current(&job.cancel)?;
        for candidate in candidates {
            let index = health_regions.iter().position(|tracked| {
                tracked.region.boss_name_id == candidate.boss_name_id
                    && tracked
                        .region
                        .percent_region
                        .overlap(candidate.percent_region)
                        >= 0.5
                    && tracked.region.name_region.overlap(candidate.name_region) >= 0.5
            });
            let index = match index {
                Some(index) => index,
                None => {
                    if health_regions.len() == MAX_REGIONS {
                        return Err(Failure::Ambiguous);
                    }
                    health_regions.push(TrackedHealth {
                        id: job.health_region_base + health_regions.len() as u32 + 1,
                        region: HealthRegion {
                            boss_name_id: candidate.boss_name_id,
                            name_region: candidate.name_region,
                            percent_region: candidate.percent_region,
                        },
                        misses: 0,
                    });
                    health_regions.len() - 1
                }
            };
            health_readings.push(HealthReading {
                region_id: health_regions[index].id,
                candidate,
            });
            if health_readings.len() > MAX_CANDIDATES {
                return Err(Failure::Ambiguous);
            }
        }
    } else {
        for tracked in &mut health_regions {
            current(&job.cancel)?;
            let candidates = engine
                .read_health_region_rgb(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    tracked.region,
                    &job.boss_names,
                )
                .map_err(ocr_failure)?;
            current(&job.cancel)?;
            if candidates.is_empty() {
                tracked.misses = tracked.misses.saturating_add(1);
            } else {
                tracked.misses = 0;
            }
            health_readings.extend(candidates.into_iter().map(|candidate| HealthReading {
                region_id: tracked.id,
                candidate,
            }));
            if health_readings.len() > MAX_CANDIDATES {
                return Err(Failure::Ambiguous);
            }
        }
        health_regions.retain(|tracked| tracked.misses < MAX_MISSES);
    }
    current(&job.cancel)?;
    if readings.is_empty() && health_readings.is_empty() {
        return Err(Failure::NoClock);
    }
    Ok((
        Observation {
            identity: job.epoch.identity.clone(),
            width: image.width(),
            height: image.height(),
            observed_at: job.frame.observed_at,
            before_seconds: job.frame.before_seconds,
            after_seconds: job.frame.after_seconds,
            #[cfg(test)]
            sampling_uncertainty_seconds: job.frame.sampling_uncertainty_seconds,
            readings,
            health_readings,
            health_assessment: None,
            phase,
            processing_duration: started.elapsed(),
            model_load_duration,
        },
        regions,
        health_regions,
    ))
}

fn ocr_failure(error: OcrError) -> Failure {
    match error {
        OcrError::TooManyRegions => Failure::Ambiguous,
        OcrError::ModelMismatch | OcrError::ModelUnavailable => Failure::Model,
        _ => Failure::Inference,
    }
}

fn discover_regions(
    candidates: Vec<ClockCandidate>,
    video_seconds: f64,
) -> Result<(Vec<Region>, Vec<Reading>), Failure> {
    if candidates.is_empty() {
        return Err(Failure::NoClock);
    }
    if candidates.len() > MAX_CANDIDATES {
        return Err(Failure::Ambiguous);
    }
    let mut regions: Vec<Region> = Vec::new();
    let mut readings = Vec::new();
    for candidate in candidates
        .into_iter()
        .filter(|candidate| candidate.explicit_separator)
    {
        let index = regions
            .iter()
            .position(|region| region.bounds.overlap(candidate.region) >= 0.5);
        let index = match index {
            Some(index) => index,
            None => {
                if regions.len() == MAX_REGIONS {
                    return Err(Failure::Ambiguous);
                }
                regions.push(Region {
                    id: regions.len() as u32 + 1,
                    bounds: candidate.region,
                    previous: Some((video_seconds, candidate.elapsed_seconds)),
                    increasing_samples: 1,
                });
                regions.len() - 1
            }
        };
        readings.push(Reading {
            region_id: regions[index].id,
            elapsed_seconds: candidate.elapsed_seconds,
        });
    }
    if readings.is_empty() {
        return Err(Failure::NoClock);
    }
    for region in &mut regions {
        if readings
            .iter()
            .filter(|reading| reading.region_id == region.id)
            .count()
            != 1
        {
            region.previous = None;
            region.increasing_samples = 0;
        }
    }
    Ok((regions, readings))
}

fn track_region(
    region: &mut Region,
    candidates: Vec<ClockCandidate>,
    video_seconds: f64,
) -> Vec<Reading> {
    let readings: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| candidate.explicit_separator || region.increasing_samples >= 3)
        .map(|candidate| Reading {
            region_id: region.id,
            elapsed_seconds: candidate.elapsed_seconds,
        })
        .collect();
    if let [reading] = readings.as_slice() {
        let increasing = region
            .previous
            .is_some_and(|(previous_video, previous_elapsed)| {
                let elapsed = f64::from(reading.elapsed_seconds) - f64::from(previous_elapsed);
                elapsed > 0.0 && ((video_seconds - previous_video) - elapsed).abs() <= 2.0
            });
        region.increasing_samples = if increasing {
            region.increasing_samples.saturating_add(1)
        } else {
            1
        };
        region.previous = Some((video_seconds, reading.elapsed_seconds));
    } else {
        region.increasing_samples = 0;
        region.previous = None;
    }
    readings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            provider: "youtube",
            video_id: "fixture-video".into(),
            broadcast_id: "fixture-broadcast".into(),
            report_code: "fixture-report".into(),
            pull_id: 1,
            pull_start_ms: 1,
            pull_end_ms: 180001,
        }
    }
    fn epoch(generation: u64) -> Epoch {
        Epoch {
            identity: identity(),
            generation,
            session: 1,
        }
    }
    fn frame(generation: u64) -> FrameCapture {
        FrameCapture {
            generation,
            png: vec![1],
            width: 1,
            height: 1,
            before_seconds: 100.0,
            after_seconds: 100.1,
            playing: true,
            observed_at: Instant::now(),
            bracket_duration: Duration::from_millis(100),
            capture_duration: Duration::from_millis(50),
            sampling_uncertainty_seconds: 0.1,
        }
    }
    fn candidate(seconds: u32, explicit: bool) -> ClockCandidate {
        ClockCandidate {
            elapsed_seconds: seconds,
            explicit_separator: explicit,
            region: ClockRegion {
                x: 0.1,
                y: 0.1,
                width: 0.1,
                height: 0.03,
            },
        }
    }
    fn observation() -> Observation {
        Observation {
            identity: identity(),
            width: 1,
            height: 1,
            observed_at: Instant::now(),
            before_seconds: 100.0,
            after_seconds: 100.1,
            sampling_uncertainty_seconds: 0.1,
            readings: Vec::new(),
            health_readings: Vec::new(),
            health_assessment: None,
            phase: Phase::Discovery,
            processing_duration: Duration::ZERO,
            model_load_duration: Duration::ZERO,
        }
    }
    fn queued_observer() -> (Observer, Receiver<Job>, SyncSender<Completed>) {
        let (jobs, requests) = mpsc::sync_channel(1);
        let (responses, results) = mpsc::sync_channel(1);
        let mut observer = Observer::with_directory(Some(PathBuf::from("/unused-models")));
        observer.worker = Some(Worker {
            jobs,
            results,
            clear: Arc::new(AtomicBool::new(false)),
            publication: Arc::new(Mutex::new(())),
        });
        observer.epoch = Some(epoch(1));
        (observer, requests, responses)
    }

    fn health_job() -> Job {
        Job {
            epoch: epoch(1),
            frame: frame(1),
            regions: Vec::new(),
            health_regions: Vec::new(),
            boss_names: vec![BossName {
                id: 1,
                name: "Fixture Boss".into(),
            }],
            discover_clocks: false,
            discover_health: false,
            health_region_base: 1000,
            health_window: Some(Arc::new(crate::warcraftlogs::HealthWindow {
                start_ms: 0,
                end_ms: 180_000,
                traces: vec![crate::warcraftlogs::HealthTrace {
                    actor: 1,
                    instance: None,
                    game_id: 1,
                    name: "Fixture Boss".into(),
                    points: Vec::new(),
                }],
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            ctx: egui::Context::default(),
        }
    }
    fn health_observation(seconds: f64) -> Observation {
        let mut result = observation();
        result.before_seconds = seconds;
        result.after_seconds = seconds + 0.1;
        result.health_readings = vec![HealthReading {
            region_id: 1001,
            candidate: HealthCandidate {
                boss_name_id: 1,
                percent: 90.0,
                decimal_places: 1,
                name_region: ClockRegion {
                    x: 0.5,
                    y: 0.4,
                    width: 0.2,
                    height: 0.05,
                },
                percent_region: ClockRegion {
                    x: 0.7,
                    y: 0.4,
                    width: 0.1,
                    height: 0.05,
                },
            },
        }];
        result
    }

    #[test]
    fn health_evidence_does_not_cross_authorization_or_capture_sessions() {
        let mut session = HealthSession::default();
        let mut job = health_job();
        session.observe(&job, &health_observation(100.0));
        session.observe(&job, &health_observation(103.0));
        let scope = session.frames[0].context.scope;
        assert_eq!(session.frames.len(), 2);
        job.epoch.session += 1;
        session.observe(&job, &health_observation(106.0));
        assert_eq!(session.frames.len(), 1);
        assert_ne!(session.frames[0].context.scope, scope);
        job.epoch.generation += 1;
        session.observe(&job, &health_observation(109.0));
        assert_eq!(session.frames.len(), 1);
        assert_eq!(session.frames[0].context.capture_generation, 2);
    }

    #[test]
    fn missing_or_rewound_health_frames_break_the_observed_sequence() {
        let mut session = HealthSession::default();
        let job = health_job();
        session.observe(&job, &health_observation(100.0));
        session.observe(&job, &health_observation(103.0));
        let mut missing = health_observation(106.0);
        missing.health_readings.clear();
        session.observe(&job, &missing);
        assert!(session.frames.is_empty());
        session.observe(&job, &health_observation(109.0));
        session.observe(&job, &health_observation(100.0));
        assert_eq!(session.frames.len(), 1);
        assert_eq!(session.frames[0].media.lower_seconds, 100.0);
    }

    #[test]
    fn reset_requests_worker_private_cache_cleanup_without_a_new_capture() {
        let (mut observer, _, _) = queued_observer();
        assert!(!observer
            .worker
            .as_ref()
            .unwrap()
            .clear
            .load(Ordering::Relaxed));
        observer.reset();
        assert!(observer
            .worker
            .as_ref()
            .unwrap()
            .clear
            .load(Ordering::Relaxed));
        assert!(observer.health_window.is_none());
        assert!(observer.boss_names.is_empty());
    }

    #[test]
    fn disabled_or_unsettled_playback_never_starts_observation() {
        let mut observer = Observer::with_directory(None);
        observer.tick(&egui::Context::default(), None, None);
        assert!(observer.worker.is_none());
        assert_eq!(observer.status(), Status::Disabled);
        let mut state = PlaybackState::default();
        state.ready = true;
        state.playing = true;
        state.mark_polled_now();
        assert!(eligible(&state));
        state.buffering = true;
        assert!(!eligible(&state));
        state.buffering = false;
        state.seeking = Some(20.0);
        assert!(!eligible(&state));
        state.seeking = None;
        state.playback_intent = Some(true);
        assert!(!eligible(&state));
        state.playback_intent = None;
        state.playing = false;
        assert!(!eligible(&state));
    }

    #[test]
    fn reset_purges_a_completed_private_observation_without_another_tick() {
        let (mut observer, _, responses) = queued_observer();
        let cancel = Arc::new(AtomicBool::new(false));
        observer.in_flight = Some(cancel.clone());
        assert!(publish(
            &responses,
            &observer.worker.as_ref().unwrap().publication,
            &cancel,
            Completed {
                key: CompletionKey::from(&epoch(1)),
                result: Ok((health_observation(100.0), Vec::new(), Vec::new())),
            },
        ));
        observer.reset();
        assert!(matches!(
            observer.worker.as_ref().unwrap().results.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(observer.in_flight.is_none());
        assert!(observer.observation().is_none());
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn a_worker_finishing_after_reset_can_queue_only_a_scalar_cancel_notice() {
        let (mut observer, _, responses) = queued_observer();
        let cancel = Arc::new(AtomicBool::new(false));
        observer.in_flight = Some(cancel.clone());
        let publication = observer.worker.as_ref().unwrap().publication.clone();
        let (ready, prepared) = mpsc::sync_channel(1);
        let (resume, wait) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = Completed {
                key: CompletionKey::from(&epoch(1)),
                result: Ok((health_observation(100.0), Vec::new(), Vec::new())),
            };
            ready.send(()).unwrap();
            wait.recv().unwrap();
            assert!(publish(&responses, &publication, &cancel, result));
        });
        prepared.recv().unwrap();
        observer.reset();
        assert!(observer.in_flight.is_some());
        resume.send(()).unwrap();
        worker.join().unwrap();
        let reply = observer
            .worker
            .as_ref()
            .unwrap()
            .results
            .try_recv()
            .unwrap();
        assert!(reply.key == CompletionKey::from(&epoch(1)));
        assert!(matches!(reply.result, Err(Failure::Cancelled)));
    }

    #[test]
    fn a_discarded_native_capture_clears_cached_health_evidence() {
        let (mut observer, _, _) = queued_observer();
        observer.observation = Some(health_observation(100.0));
        observer.discard_capture();
        assert!(observer.observation().is_none());
        assert!(observer
            .worker
            .as_ref()
            .unwrap()
            .clear
            .load(Ordering::Relaxed));
        assert_eq!(observer.statistics.discarded_results, 1);
    }

    #[test]
    fn capture_rate_and_discovery_retries_remain_bounded() {
        let mut observer = Observer::with_directory(Some(PathBuf::from("/unused-models")));
        let now = Instant::now();
        assert!(observer.can_sample(now));
        observer.last_capture = Some(now);
        assert!(!observer.can_sample(now + Duration::from_millis(2999)));
        assert!(observer.can_sample(now + SAMPLE_INTERVAL));
        observer.discoveries = MAX_DISCOVERIES;
        assert!(!observer.can_sample(now + Duration::from_secs(60)));
        observer.reset();
        // Rapid POV changes must not bypass the global capture rate limit.
        assert!(!observer.can_sample(now + Duration::from_secs(1)));
        observer.retry_after = Some(now + DISCOVERY_RETRY);
        assert!(!observer.can_sample(now + Duration::from_secs(10)));
    }

    #[test]
    fn one_job_survives_reset_only_as_cancelled_work() {
        let (mut observer, requests, responses) = queued_observer();
        let ctx = egui::Context::default();
        observer.submit(&ctx, epoch(1), frame(1));
        observer.submit(&ctx, epoch(1), frame(1));
        let first = requests.try_recv().unwrap();
        assert!(requests.try_recv().is_err());
        assert_eq!(observer.statistics.discoveries, 1);
        observer.reset();
        observer.epoch = Some(epoch(2));
        assert_eq!(current(&first.cancel), Err(Failure::Cancelled));
        // No second worker/job is allowed while the old inference is finishing.
        observer.submit(&ctx, epoch(2), frame(2));
        assert!(requests.try_recv().is_err());
        responses
            .send(Completed {
                key: CompletionKey::from(&epoch(1)),
                result: Ok((observation(), Vec::new(), Vec::new())),
            })
            .unwrap();
        observer.receive(true);
        assert!(observer.observation().is_none());
        assert!(observer.in_flight.is_none());
        assert!(observer.worker.is_some());
        observer.submit(&ctx, epoch(2), frame(2));
        assert_eq!(requests.try_recv().unwrap().epoch.generation, 2);
    }

    #[test]
    fn old_generation_and_old_frame_results_are_rejected() {
        let (mut observer, _requests, responses) = queued_observer();
        observer.epoch = Some(epoch(2));
        observer.in_flight = Some(Arc::new(AtomicBool::new(false)));
        responses
            .send(Completed {
                key: CompletionKey::from(&epoch(1)),
                result: Ok((observation(), Vec::new(), Vec::new())),
            })
            .unwrap();
        observer.receive(true);
        assert!(observer.observation().is_none());
        let mut stale = observation();
        stale.observed_at = Instant::now() - MAX_RESULT_AGE - Duration::from_secs(1);
        observer.in_flight = Some(Arc::new(AtomicBool::new(false)));
        responses
            .send(Completed {
                key: CompletionKey::from(&epoch(2)),
                result: Ok((stale, Vec::new(), Vec::new())),
            })
            .unwrap();
        observer.receive(true);
        assert!(observer.observation().is_none());
        assert_eq!(observer.statistics.discarded_results, 2);
    }

    #[test]
    fn discovery_rejects_excess_regions_instead_of_selecting_a_clock() {
        let candidates = (0..=MAX_REGIONS)
            .map(|index| {
                let mut candidate = candidate(index as u32, true);
                candidate.region.x = 0.01 + index as f32 * 0.1;
                candidate.region.width = 0.05;
                candidate
            })
            .collect();
        assert!(matches!(
            discover_regions(candidates, 100.0),
            Err(Failure::Ambiguous)
        ));
        assert!(matches!(
            discover_regions(vec![candidate(42, false)], 100.0),
            Err(Failure::NoClock)
        ));
    }

    #[test]
    fn rediscovery_cannot_reuse_a_previous_clock_identity() {
        let (mut observer, _requests, responses) = queued_observer();
        observer.discoveries = 2;
        observer.in_flight = Some(Arc::new(AtomicBool::new(false)));
        let (regions, readings) = discover_regions(vec![candidate(30, true)], 100.0).unwrap();
        let mut observation = observation();
        observation.readings = readings;
        responses
            .send(Completed {
                key: CompletionKey::from(&epoch(1)),
                result: Ok((observation, regions, Vec::new())),
            })
            .unwrap();
        observer.receive(true);
        assert_eq!(observer.observation().unwrap().readings[0].region_id, 9);
        assert_eq!(observer.regions[0].id, 9);
    }

    #[test]
    fn colonless_tracking_requires_multiple_increasing_clock_samples() {
        let (mut regions, _) = discover_regions(vec![candidate(30, true)], 100.0).unwrap();
        let region = &mut regions[0];
        assert_eq!(
            track_region(region, vec![candidate(33, true)], 103.0).len(),
            1
        );
        assert_eq!(
            track_region(region, vec![candidate(36, true)], 106.0).len(),
            1
        );
        assert_eq!(region.increasing_samples, 3);
        assert_eq!(
            track_region(region, vec![candidate(39, false)], 109.0).len(),
            1
        );
        // Static timers and ambiguous alternatives cannot establish a clock.
        track_region(region, vec![candidate(39, true)], 112.0);
        assert!(track_region(region, vec![candidate(42, false)], 115.0).is_empty());
        track_region(
            region,
            vec![candidate(45, true), candidate(145, true)],
            118.0,
        );
        assert_eq!(region.increasing_samples, 0);
    }
}
