//! Opt-in snapshots of the owned media child. No desktop capture or persistence.

use super::PlaybackState;
use eframe::egui;
#[cfg(any(target_os = "linux", test))]
use std::io::{self, Write};
use std::{
    cell::{Cell, RefCell},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use wry::WebView;

const MAX_BYTES: usize = 12 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4_194_304;
const TIMEOUT: Duration = Duration::from_secs(3);
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(500);
const MIN_INTERVAL: Duration = Duration::from_millis(500);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The browser APIs provide an image, but no decoded-frame presentation time.
/// These SDK readings bracket the capture; they are not an exact frame timestamp.
pub struct FrameCapture {
    pub generation: u64,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub before_seconds: f64,
    pub after_seconds: f64,
    pub playing: bool,
    pub observed_at: Instant,
    #[cfg(test)]
    pub bracket_duration: Duration,
    #[cfg(test)]
    pub capture_duration: Duration,
    /// Measured sampling window, excluding any unobservable SDK/decoder latency.
    #[cfg(test)]
    pub sampling_uncertainty_seconds: f64,
}

impl FrameCapture {
    pub fn estimated_seconds(&self) -> f64 {
        (self.before_seconds + self.after_seconds) / 2.0
    }
}

pub(super) struct Controller {
    job: RefCell<Option<Arc<Mutex<Job>>>>,
    native_pending: Arc<AtomicBool>,
    last_requested: Cell<Option<Instant>>,
    generation: Cell<u64>,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            job: RefCell::new(None),
            native_pending: Arc::new(AtomicBool::new(false)),
            last_requested: Cell::new(None),
            generation: Cell::new(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

struct Sample {
    state: PlaybackState,
    requested: Instant,
    received: Instant,
}

enum Pixels {
    #[cfg(any(target_os = "linux", test))]
    Rgb {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    #[cfg(target_os = "windows")]
    Png(Vec<u8>),
}

struct Job {
    generation: u64,
    started: Instant,
    before: Option<Sample>,
    native_started: Option<Instant>,
    native_finished: Option<Instant>,
    pixels: Option<Pixels>,
    after_requested: bool,
    after: Option<Sample>,
    encoding: bool,
    result: Option<Result<FrameCapture, String>>,
}

impl Controller {
    pub fn cancel(&self) {
        // Every asynchronous callback has only a Weak reference. Cancellation,
        // navigation and player drop release any unconsumed image immediately.
        self.job.borrow_mut().take();
        self.generation
            .set(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed));
    }

    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    pub fn pending(&self) -> bool {
        self.job.borrow().is_some() || self.native_pending.load(Ordering::Relaxed)
    }

    pub fn take(&self) -> Option<Result<FrameCapture, String>> {
        let result = self.job.borrow().as_ref()?.lock().ok()?.result.take();
        if result.is_some() {
            self.job.borrow_mut().take();
        }
        result
    }

    pub fn request(
        &self,
        view: &WebView,
        state: &PlaybackState,
        visible: bool,
        bounds: [i32; 4],
        ctx: &egui::Context,
    ) -> bool {
        if self.pending()
            || !eligible(state, visible)
            || dimensions(bounds[2] as u32, bounds[3] as u32).is_err()
            || self
                .last_requested
                .get()
                .is_some_and(|at| at.elapsed() < MIN_INTERVAL)
        {
            return false;
        }
        let now = Instant::now();
        self.last_requested.set(Some(now));
        let job = Arc::new(Mutex::new(Job {
            generation: self.generation.get(),
            started: now,
            before: None,
            native_started: None,
            native_finished: None,
            pixels: None,
            after_requested: false,
            after: None,
            encoding: false,
            result: None,
        }));
        self.job.replace(Some(job.clone()));
        request_clock(view, Arc::downgrade(&job), false, ctx);
        true
    }

    pub fn tick(&self, view: &WebView, state: &PlaybackState, visible: bool, ctx: &egui::Context) {
        let Some(job) = self.job.borrow().clone() else {
            return;
        };
        if !eligible(state, visible) {
            self.cancel();
            return;
        }
        enum Next {
            Wait,
            Snapshot,
            After,
            Encode(Pixels, Sample, Sample, Instant, Instant),
        }
        let next = {
            let Ok(mut current) = job.lock() else {
                self.cancel();
                return;
            };
            if current.result.is_some() {
                return;
            }
            if current.started.elapsed() > TIMEOUT {
                current.result = Some(Err("The media snapshot took too long.".into()));
                return;
            }
            if current.before.is_none() {
                Next::Wait
            } else if current.native_started.is_none() {
                if current.before.as_ref().unwrap().received.elapsed() > SAMPLE_TIMEOUT {
                    current.result = Some(Err("The player timing sample became stale.".into()));
                    return;
                }
                current.native_started = Some(Instant::now());
                Next::Snapshot
            } else if current.pixels.is_some() && !current.after_requested {
                current.after_requested = true;
                Next::After
            } else if current.after.is_some() && !current.encoding {
                current.encoding = true;
                Next::Encode(
                    current.pixels.take().unwrap(),
                    current.before.take().unwrap(),
                    current.after.take().unwrap(),
                    current.native_started.unwrap(),
                    current.native_finished.unwrap(),
                )
            } else {
                Next::Wait
            }
        };
        match next {
            Next::Wait => (),
            Next::Snapshot => {
                self.native_pending.store(true, Ordering::Relaxed);
                snapshot(
                    view,
                    Arc::downgrade(&job),
                    self.native_pending.clone(),
                    ctx.clone(),
                );
            }
            Next::After => request_clock(view, Arc::downgrade(&job), true, ctx),
            Next::Encode(pixels, before, after, started, finished) => {
                let weak = Arc::downgrade(&job);
                let ctx = ctx.clone();
                let spawned = std::thread::Builder::new()
                    .name("replay-frame".into())
                    .spawn(move || {
                        if weak.strong_count() == 0 {
                            return;
                        }
                        let result = frame(pixels, before, after, started, finished, &weak);
                        complete(&weak, result);
                        ctx.request_repaint();
                    });
                if spawned.is_err() {
                    complete(
                        &Arc::downgrade(&job),
                        Err("The snapshot worker could not start.".into()),
                    );
                }
            }
        }
    }
}

fn eligible(state: &PlaybackState, visible: bool) -> bool {
    visible
        && state.ready
        && state.is_fresh()
        && !state.buffering
        && state.seeking.is_none()
        && state.playback_intent.is_none()
}

fn dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        Err("The media snapshot dimensions exceed the capture limit.".into())
    } else {
        Ok(())
    }
}

fn request_clock(view: &WebView, job: Weak<Mutex<Job>>, after: bool, ctx: &egui::Context) {
    let requested = Instant::now();
    let failed = job.clone();
    let ctx = ctx.clone();
    if view
        .evaluate_script_with_callback(
            "JSON.stringify(window.brickMedia ? window.brickMedia.state() : null)",
            move |value| {
                let received = Instant::now();
                let parsed = if value.len() <= 4096 {
                    let decoded = serde_json::from_str::<String>(&value).unwrap_or(value);
                    serde_json::from_str::<PlaybackState>(&decoded).ok()
                } else {
                    None
                };
                if let Some(mut state) = parsed.filter(|state| {
                    state.ready
                        && !state.buffering
                        && state.seconds.is_finite()
                        && (0.0..=604800.0).contains(&state.seconds)
                        && received.duration_since(requested) <= SAMPLE_TIMEOUT
                }) {
                    state.polled_at = Some(requested);
                    if let Some(job) = job.upgrade() {
                        if let Ok(mut current) = job.lock() {
                            let sample = Sample {
                                state,
                                requested,
                                received,
                            };
                            if after {
                                current.after = Some(sample);
                            } else {
                                current.before = Some(sample);
                            }
                        }
                    }
                } else {
                    complete(
                        &job,
                        Err("The player timing changed during capture.".into()),
                    );
                }
                ctx.request_repaint();
            },
        )
        .is_err()
    {
        complete(
            &failed,
            Err("The player timing could not be sampled.".into()),
        );
    }
}

fn complete(job: &Weak<Mutex<Job>>, result: Result<FrameCapture, String>) {
    if let Some(job) = job.upgrade() {
        if let Ok(mut current) = job.lock() {
            if current.result.is_none() {
                current.result = Some(result);
            }
        }
    }
}

fn received_pixels(job: &Weak<Mutex<Job>>, pixels: Result<Pixels, String>, ctx: &egui::Context) {
    match pixels {
        Ok(pixels) => {
            if let Some(job) = job.upgrade() {
                if let Ok(mut current) = job.lock() {
                    current.native_finished = Some(Instant::now());
                    current.pixels = Some(pixels);
                }
            }
        }
        Err(error) => complete(job, Err(error)),
    }
    ctx.request_repaint();
}

#[cfg(target_os = "linux")]
fn snapshot(view: &WebView, job: Weak<Mutex<Job>>, pending: Arc<AtomicBool>, ctx: egui::Context) {
    use webkit2gtk::WebViewExt;
    use wry::WebViewExtUnix;
    view.webview().snapshot(
        webkit2gtk::SnapshotRegion::Visible,
        webkit2gtk::SnapshotOptions::NONE,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            pending.store(false, Ordering::Relaxed);
            if job.strong_count() == 0 {
                return;
            }
            let pixels = result
                .map_err(|_| "The media frame could not be captured.".into())
                .and_then(|surface| {
                    let image = gtk::cairo::ImageSurface::try_from(surface).map_err(|_| {
                        "The media snapshot has an unsupported surface.".to_string()
                    })?;
                    let width = image.width() as u32;
                    let height = image.height() as u32;
                    dimensions(width, height)?;
                    if !matches!(
                        image.format(),
                        gtk::cairo::Format::ARgb32 | gtk::cairo::Format::Rgb24
                    ) {
                        return Err("The media snapshot has an unsupported pixel format.".into());
                    }
                    let mut bytes = vec![0; width as usize * height as usize * 3];
                    let stride = image.stride() as usize;
                    image
                        .with_data(|source| {
                            for y in 0..height as usize {
                                for x in 0..width as usize {
                                    let at = y * stride + x * 4;
                                    let pixel =
                                        u32::from_ne_bytes(source[at..at + 4].try_into().unwrap());
                                    let out = (y * width as usize + x) * 3;
                                    // Cairo's native-endian RGB is composited on the
                                    // media child's opaque background, not the desktop.
                                    bytes[out..out + 3].copy_from_slice(&[
                                        (pixel >> 16) as u8,
                                        (pixel >> 8) as u8,
                                        pixel as u8,
                                    ]);
                                }
                            }
                        })
                        .map_err(|_| "The media snapshot pixels could not be read.".to_string())?;
                    Ok(Pixels::Rgb {
                        width,
                        height,
                        bytes,
                    })
                });
            received_pixels(&job, pixels, &ctx);
        },
    );
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn snapshot(_: &WebView, job: Weak<Mutex<Job>>, pending: Arc<AtomicBool>, _: egui::Context) {
    pending.store(false, Ordering::Relaxed);
    complete(
        &job,
        Err("Native media capture is unavailable on this platform.".into()),
    );
}

#[cfg(target_os = "windows")]
fn snapshot(view: &WebView, job: Weak<Mutex<Job>>, pending: Arc<AtomicBool>, ctx: egui::Context) {
    use webview2_com::{
        CapturePreviewCompletedHandler,
        Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
    };
    use windows::Win32::{
        System::Com::{STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET},
        UI::Shell::SHCreateMemStream,
    };
    use wry::WebViewExtWindows;
    // The stream belongs only to this UI-thread capture. No file or shared
    // clipboard is created, and the image dimensions were bounded beforehand.
    let Some(stream) = (unsafe { SHCreateMemStream(None) }) else {
        pending.store(false, Ordering::Relaxed);
        complete(
            &job,
            Err("The media snapshot buffer could not be created.".into()),
        );
        return;
    };
    let reader = stream.clone();
    let failed = job.clone();
    let finished = pending.clone();
    let callback = CapturePreviewCompletedHandler::create(Box::new(move |status| {
        finished.store(false, Ordering::Relaxed);
        if job.strong_count() == 0 {
            return Ok(());
        }
        let pixels = (|| -> Result<Pixels, String> {
            status.map_err(|_| "The media frame could not be captured.".to_string())?;
            let mut stat = STATSTG::default();
            unsafe { reader.Stat(&mut stat, STATFLAG_NONAME) }
                .map_err(|_| "The media snapshot size is unavailable.".to_string())?;
            let size = usize::try_from(stat.cbSize)
                .ok()
                .filter(|size| *size > 0 && *size <= MAX_BYTES)
                .ok_or("The media snapshot exceeds the capture limit.")?;
            let mut bytes = vec![0; size];
            let mut read = 0u32;
            unsafe {
                reader
                    .Seek(0, STREAM_SEEK_SET, None)
                    .map_err(|_| "The media snapshot could not be read.".to_string())?;
                reader
                    .Read(bytes.as_mut_ptr().cast(), size as u32, Some(&mut read))
                    .ok()
                    .map_err(|_| "The media snapshot could not be read.".to_string())?;
            }
            if read as usize != size {
                return Err("The media snapshot was incomplete.".into());
            }
            Ok(Pixels::Png(bytes))
        })();
        received_pixels(&job, pixels, &ctx);
        Ok(())
    }));
    if unsafe {
        view.webview().CapturePreview(
            COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
            &stream,
            &callback,
        )
    }
    .is_err()
    {
        pending.store(false, Ordering::Relaxed);
        complete(
            &failed,
            Err("The media frame could not be captured.".into()),
        );
    }
}

fn frame(
    pixels: Pixels,
    before: Sample,
    after: Sample,
    _started: Instant,
    _finished: Instant,
    job: &Weak<Mutex<Job>>,
) -> Result<FrameCapture, String> {
    let generation = job
        .upgrade()
        .and_then(|job| job.lock().ok().map(|job| job.generation))
        .ok_or("The media snapshot was cancelled.")?;
    let elapsed = after.received.duration_since(before.requested);
    let movement = after.state.seconds - before.state.seconds;
    if before.state.playing != after.state.playing
        || movement < -0.05
        || (before.state.playing && movement > elapsed.as_secs_f64() * 2.0 + 0.5)
        || (!before.state.playing && movement.abs() > 0.05)
    {
        return Err("Playback changed during the media snapshot.".into());
    }
    let (png, width, height) = match pixels {
        #[cfg(any(target_os = "linux", test))]
        Pixels::Rgb {
            width,
            height,
            bytes,
        } => {
            use image::ImageEncoder;
            let mut output = BoundedWriter {
                bytes: Vec::new(),
                job: job.clone(),
            };
            image::codecs::png::PngEncoder::new_with_quality(
                &mut output,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Sub,
            )
            .write_image(&bytes, width, height, image::ExtendedColorType::Rgb8)
            .map_err(|_| "The media snapshot could not be encoded within its limit.".to_string())?;
            (output.bytes, width, height)
        }
        #[cfg(target_os = "windows")]
        Pixels::Png(bytes) => {
            if bytes.len() < 24
                || bytes.len() > MAX_BYTES
                || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
                || &bytes[12..16] != b"IHDR"
            {
                return Err("The media snapshot has an invalid image header.".into());
            }
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            dimensions(width, height)?;
            (bytes, width, height)
        }
    };
    Ok(FrameCapture {
        generation,
        png,
        width,
        height,
        before_seconds: before.state.seconds,
        after_seconds: after.state.seconds,
        playing: before.state.playing,
        observed_at: before.requested + elapsed / 2,
        #[cfg(test)]
        bracket_duration: elapsed,
        #[cfg(test)]
        capture_duration: _finished.duration_since(_started),
        #[cfg(test)]
        sampling_uncertainty_seconds: elapsed.as_secs_f64().max(movement.abs()),
    })
}

#[cfg(any(target_os = "linux", test))]
struct BoundedWriter {
    bytes: Vec<u8>,
    job: Weak<Mutex<Job>>,
}
#[cfg(any(target_os = "linux", test))]
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.job.strong_count() == 0 || bytes.len() > MAX_BYTES.saturating_sub(self.bytes.len())
        {
            return Err(io::Error::other("Snapshot cancelled or too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Arc<Mutex<Job>> {
        Arc::new(Mutex::new(Job {
            generation: 1,
            started: Instant::now(),
            before: None,
            native_started: None,
            native_finished: None,
            pixels: None,
            after_requested: false,
            after: None,
            encoding: false,
            result: None,
        }))
    }

    fn sample(at: Instant, seconds: f64, playing: bool) -> Sample {
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = seconds;
        state.playing = playing;
        state.polled_at = Some(at);
        Sample {
            state,
            requested: at,
            received: at + Duration::from_millis(10),
        }
    }

    #[test]
    fn capture_skips_hidden_stale_buffering_or_changing_playback() {
        let mut state = sample(Instant::now(), 100.0, false).state;
        assert!(eligible(&state, true));
        assert!(!eligible(&state, false));
        state.seeking = Some(100.0);
        assert!(!eligible(&state, true));
        state.seeking = None;
        state.playback_intent = Some(false);
        assert!(!eligible(&state, true));
        state.playback_intent = None;
        state.buffering = true;
        assert!(!eligible(&state, true));
        state.buffering = false;
        state.polled_at = Some(Instant::now() - Duration::from_secs(3));
        assert!(!eligible(&state, true));
    }

    #[test]
    fn snapshot_limits_and_cancellation_bound_retained_data() {
        assert!(dimensions(1920, 1080).is_ok());
        assert!(dimensions(4096, 4096).is_err());
        assert!(dimensions(0, 1).is_err());
        let controller = Controller::default();
        let owned = job();
        let weak = Arc::downgrade(&owned);
        controller.job.replace(Some(owned));
        controller.cancel();
        complete(&weak, Err("late callback".into()));
        assert!(weak.upgrade().is_none());
        assert!(controller.take().is_none());
        let mut output = BoundedWriter {
            bytes: Vec::new(),
            job: weak,
        };
        assert!(output.write(b"cancelled image").is_err());
        assert!(output.bytes.is_empty());
    }

    #[test]
    fn timing_bracket_rejects_a_seek_or_pause_during_capture() {
        let now = Instant::now();
        let owned = job();
        let weak = Arc::downgrade(&owned);
        let pixels = || Pixels::Rgb {
            width: 1,
            height: 1,
            bytes: vec![0, 128, 255],
        };
        let end = now + Duration::from_millis(100);
        assert!(frame(
            pixels(),
            sample(now, 100.0, true),
            sample(end, 120.0, true),
            now,
            end,
            &weak
        )
        .is_err());
        assert!(frame(
            pixels(),
            sample(now, 100.0, true),
            sample(end, 100.1, false),
            now,
            end,
            &weak
        )
        .is_err());
        let valid = frame(
            pixels(),
            sample(now, 100.0, false),
            sample(end, 100.0, false),
            now,
            end,
            &weak,
        )
        .unwrap();
        assert_eq!(valid.estimated_seconds(), 100.0);
        assert!(valid.sampling_uncertainty_seconds >= 0.1);
        assert_eq!(valid.capture_duration, Duration::from_millis(100));
        assert!(image::load_from_memory_with_format(&valid.png, image::ImageFormat::Png).is_ok());
    }
}
