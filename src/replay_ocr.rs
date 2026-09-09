//! Experimental local replay-clock and boss-label/health recognition.
//! Call only from a bounded worker.
//!
//! This module supplies observations, not alignment: callers must establish a
//! stable region across several accurately paired media frames and validate the
//! clock against Warcraft Logs. Missing or ambiguous clocks remain unverified.
//!
//! Unmodified ocrs uses RTen's global pool internally. For this prototype set
//! `RTEN_NUM_THREADS=1` in the process environment BEFORE starting any threads.
//! Do not change the environment from a running GUI/worker, and do not assume an
//! outer one-thread worker limits RTen. Product integration needs that startup
//! policy or an upstream API exposing a bounded inference pool.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::{cell::OnceCell, fmt};

use image::{ImageFormat, ImageReader, RgbImage};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten_imageproc::{BoundingRect, Rect, RotatedRect};
use sha2::{Digest, Sha256};

const MAX_ENCODED_BYTES: usize = 12 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4_194_304;
const MAX_DETECTED_REGIONS: usize = 256;
const MAX_CLOCK_SECONDS: u32 = 7_199;
const MAX_BOSS_NAMES: usize = 32;
const MAX_HEALTH_CANDIDATES: usize = 32;
const MAX_HEALTH_CROP_PIXELS: u64 = 262_144;
const HEALTH_TILE_SIDE: u32 = 768;
const HEALTH_TILE_STEP: u32 = 640;
const MAX_HEALTH_TILES: usize = 12;
const MAX_HEALTH_WORDS: usize = 1024;

// Pinned default models from the official ocrs CLI's S3 URLs. Assets remain
// local-only: upstream redistribution/license clarification is still pending
// (ocrs-models issue 34). This module never downloads or commits model weights.
const DETECTION_BYTES: usize = 2_510_284;
const DETECTION_SHA256: &str = "f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca";
const RECOGNITION_BYTES: usize = 9_716_568;
const RECOGNITION_SHA256: &str = "e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl ClockRegion {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite)
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 1.0
            && self.y + self.height <= 1.0
    }

    /// Geometric similarity only; it does not prove both boxes are a pull timer.
    pub fn overlap(self, other: Self) -> f32 {
        if !self.valid() || !other.valid() {
            return 0.0;
        }
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        let intersection = width.max(0.0) * height.max(0.0);
        let union = self.width * self.height + other.width * other.height - intersection;
        if union <= 0.0 {
            0.0
        } else {
            (intersection / union).clamp(0.0, 1.0)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClockCandidate {
    pub elapsed_seconds: u32,
    pub region: ClockRegion,
    /// Whether OCR retained a colon. Colonless readings are allowed only after
    /// the caller has established this region as a clock across several frames.
    pub explicit_separator: bool,
}

/// A caller-assigned key for an exact boss label, not a unique NPC instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BossName {
    pub id: u32,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HealthRegion {
    pub boss_name_id: u32,
    pub name_region: ClockRegion,
    pub percent_region: ClockRegion,
}

/// Visible text and proximity evidence only. This does not establish which
/// Warcraft Logs actor/instance, or even which unit-frame resource, is shown.
#[derive(Clone, Debug, PartialEq)]
pub struct HealthCandidate {
    pub boss_name_id: u32,
    pub percent: f64,
    pub decimal_places: u8,
    pub name_region: ClockRegion,
    pub percent_region: ClockRegion,
}

impl HealthCandidate {
    pub fn region(&self) -> HealthRegion {
        HealthRegion {
            boss_name_id: self.boss_name_id,
            name_region: self.name_region,
            percent_region: self.percent_region,
        }
    }
}

#[derive(Clone)]
struct Word {
    text: String,
    region: ClockRegion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrError {
    InvalidImage,
    ImageTooLarge,
    InvalidRegion,
    ModelUnavailable,
    ModelMismatch,
    InferenceFailed,
    TooManyRegions,
    InvalidNames,
}

impl fmt::Display for OcrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidImage => "The replay frame could not be read.",
            Self::ImageTooLarge => "The replay frame exceeds the OCR size limit.",
            Self::InvalidRegion => "The clock region is invalid.",
            Self::ModelUnavailable => "The bundled OCR model is unavailable.",
            Self::ModelMismatch => "The bundled OCR model failed verification.",
            Self::InferenceFailed => "The replay clock could not be recognized.",
            Self::TooManyRegions => "The replay frame contains too many text regions.",
            Self::InvalidNames => "The supplied boss labels are invalid.",
        })
    }
}

impl std::error::Error for OcrError {}

pub struct ReplayOcr {
    engine: OcrEngine,
    recognition_path: PathBuf,
    // ocrs fixes its recognition alphabet at construction and owns its models.
    // Share the detector and lazily add only an unrestricted recognizer.
    text_engine: OnceCell<Result<OcrEngine, OcrError>>,
}

impl ReplayOcr {
    /// Blocking model loading and verification; keep off the GUI thread.
    pub fn load(detection: &Path, recognition: &Path) -> Result<Self, OcrError> {
        let detection_model = load_model(detection, DETECTION_BYTES, DETECTION_SHA256)?;
        let recognition_model = load_model(recognition, RECOGNITION_BYTES, RECOGNITION_SHA256)?;
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            allowed_chars: Some("0123456789:".into()),
            ..Default::default()
        })
        .map_err(|_| OcrError::InferenceFailed)?;
        Ok(Self {
            engine,
            recognition_path: recognition.to_path_buf(),
            text_engine: OnceCell::new(),
        })
    }

    fn text_engine(&self) -> Result<&OcrEngine, OcrError> {
        self.text_engine
            .get_or_init(|| {
                OcrEngine::new(OcrEngineParams {
                    recognition_model: Some(load_model(
                        &self.recognition_path,
                        RECOGNITION_BYTES,
                        RECOGNITION_SHA256,
                    )?),
                    ..Default::default()
                })
                .map_err(|_| OcrError::InferenceFailed)
            })
            .as_ref()
            .map_err(|error| *error)
    }

    /// Discover unrestricted text using the existing detector. Exact supplied
    /// names are associated with nearby explicit percentages; retain every
    /// qualifying association rather than choosing a nearest/likely boss.
    pub fn discover_health_rgb(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        names: &[BossName],
    ) -> Result<Vec<HealthCandidate>, OcrError> {
        validate_rgb(pixels, width, height)?;
        validate_names(names)?;
        let xs = tile_positions(width);
        let ys = tile_positions(height);
        if xs.len() * ys.len() > MAX_HEALTH_TILES {
            return Err(OcrError::ImageTooLarge);
        }
        let aspect = width as f32 / height as f32;
        let mut words = self.health_words(pixels, width, height)?;
        let found = associate_health(&words, names, aspect)?;
        if width <= HEALTH_TILE_SIDE && height <= HEALTH_TILE_SIDE {
            return Ok(found);
        }
        // Full-frame detection resizes large images and can erase small HUD
        // glyphs. A bounded overlapping grid preserves their native resolution.
        // Never stop at the first tile: all locations/alternatives survive.
        for &top in &ys {
            for &left in &xs {
                let w = HEALTH_TILE_SIDE.min(width - left);
                let h = HEALTH_TILE_SIDE.min(height - top);
                let mut crop = Vec::with_capacity((w * h * 3) as usize);
                for y in top..top + h {
                    let at = ((y * width + left) * 3) as usize;
                    crop.extend_from_slice(&pixels[at..at + (w * 3) as usize]);
                }
                for mut word in self.health_words(&crop, w, h)? {
                    word.region = ClockRegion {
                        x: (left as f32 + word.region.x * w as f32) / width as f32,
                        y: (top as f32 + word.region.y * h as f32) / height as f32,
                        width: word.region.width * w as f32 / width as f32,
                        height: word.region.height * h as f32 / height as f32,
                    };
                    if !words.iter().any(|previous| {
                        previous.text == word.text && previous.region.overlap(word.region) >= 0.8
                    }) {
                        words.push(word);
                    }
                    if words.len() > MAX_HEALTH_WORDS {
                        return Err(OcrError::TooManyRegions);
                    }
                }
            }
        }
        associate_health(&words, names, aspect)
    }

    fn health_words(&self, pixels: &[u8], width: u32, height: u32) -> Result<Vec<Word>, OcrError> {
        let input = self
            .engine
            .prepare_input(
                ImageSource::from_bytes(pixels, (width, height))
                    .map_err(|_| OcrError::InvalidImage)?,
            )
            .map_err(|_| OcrError::InferenceFailed)?;
        let boxes: Vec<_> = self
            .engine
            .detect_words(&input)
            .map_err(|_| OcrError::InferenceFailed)?
            .into_iter()
            .filter(|word| {
                let rect = word.bounding_rect();
                rect.height() >= 3.0
                    && rect.height() <= (height as f32 * 0.1).max(24.0)
                    && (0.1..=40.0).contains(&(rect.width() / rect.height()))
            })
            .collect();
        if boxes.len() > MAX_DETECTED_REGIONS {
            return Err(OcrError::TooManyRegions);
        }
        let lines: Vec<_> = boxes.iter().map(|word| vec![*word]).collect();
        let recognized = self
            .text_engine()?
            .recognize_text(&input, &lines)
            .map_err(|_| OcrError::InferenceFailed)?;
        let mut words = Vec::new();
        for line in recognized.into_iter().flatten() {
            for word in line.words() {
                let text = word.to_string();
                if text.len() > 192 {
                    continue;
                }
                if let Some(region) = normalize_rect(word.bounding_rect().to_f32(), width, height) {
                    words.push(Word { text, region });
                }
                if words.len() > MAX_DETECTED_REGIONS {
                    return Err(OcrError::TooManyRegions);
                }
            }
        }
        Ok(words)
    }

    /// Cheap tracking reads only the learned name and percent boxes, and
    /// re-establishes the exact name on this frame. All contrast alternatives
    /// are retained; missing percent signs or missing names produce no match.
    pub fn read_health_region_rgb(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        region: HealthRegion,
        names: &[BossName],
    ) -> Result<Vec<HealthCandidate>, OcrError> {
        validate_rgb(pixels, width, height)?;
        validate_names(names)?;
        if !region.name_region.valid()
            || !region.percent_region.valid()
            || !nearby(
                region.name_region,
                region.percent_region,
                width as f32 / height as f32,
            )
        {
            return Err(OcrError::InvalidRegion);
        }
        let expected: Vec<_> = names
            .iter()
            .filter(|name| name.id == region.boss_name_id)
            .map(|name| normalize_boss_name(&name.name))
            .collect();
        if expected.is_empty() {
            return Err(OcrError::InvalidNames);
        }
        let labels = self.read_text_region(pixels, width, height, region.name_region)?;
        if !labels
            .iter()
            .any(|label| expected.contains(&normalize_boss_name(label)))
        {
            return Ok(Vec::new());
        }
        let mut candidates = Vec::new();
        for text in self.read_text_region(pixels, width, height, region.percent_region)? {
            if let Some((percent, decimal_places)) = parse_percent(&text) {
                let candidate = HealthCandidate {
                    boss_name_id: region.boss_name_id,
                    percent,
                    decimal_places,
                    name_region: region.name_region,
                    percent_region: region.percent_region,
                };
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        }
        Ok(candidates)
    }

    fn read_text_region(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        region: ClockRegion,
    ) -> Result<Vec<String>, OcrError> {
        let engine = self.text_engine()?;
        let mut alternatives = Vec::new();
        // Tight crops avoid pulling unit-frame separators into a percentage;
        // padded crops tolerate detector jitter. Neither is a chosen answer.
        for padding in [0u32, 2] {
            let (left, top, right, bottom) = health_region_pixels(region, width, height, padding);
            let w = right - left;
            let h = bottom - top;
            if w == 0
                || h == 0
                || w as f32 / h as f32 > 128.0
                || u64::from(w) * u64::from(h) > MAX_HEALTH_CROP_PIXELS
            {
                return Err(OcrError::InvalidRegion);
            }
            let mut crop = Vec::with_capacity((w * h * 3) as usize);
            for y in top..bottom {
                let at = ((y * width + left) * 3) as usize;
                crop.extend_from_slice(&pixels[at..at + (w * 3) as usize]);
            }
            let line = vec![RotatedRect::from_rect(Rect::from_tlbr(
                0.0, 0.0, h as f32, w as f32,
            ))];
            for threshold in [None, Some(140u8), Some(190u8)] {
                let filtered;
                let source = if let Some(threshold) = threshold {
                    filtered = crop
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .flat_map(|rgb| {
                            let luminance =
                                (u16::from(rgb[0]) * 3 + u16::from(rgb[1]) * 6 + u16::from(rgb[2]))
                                    / 10;
                            let value = if luminance >= u16::from(threshold) {
                                255
                            } else {
                                0
                            };
                            [value; 3]
                        })
                        .collect::<Vec<_>>();
                    &filtered
                } else {
                    &crop
                };
                let input = engine
                    .prepare_input(
                        ImageSource::from_bytes(source, (w, h))
                            .map_err(|_| OcrError::InvalidImage)?,
                    )
                    .map_err(|_| OcrError::InferenceFailed)?;
                for text in engine
                    .recognize_text(&input, std::slice::from_ref(&line))
                    .map_err(|_| OcrError::InferenceFailed)?
                    .into_iter()
                    .flatten()
                {
                    let text = text.to_string();
                    if text.len() <= 192 && !alternatives.contains(&text) {
                        alternatives.push(text);
                    }
                }
            }
        }
        Ok(alternatives)
    }

    /// Discover clock candidates without configured coordinates. Full-frame
    /// detection is intentionally separate from cheap learned-region tracking.
    pub fn discover_rgb(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<ClockCandidate>, OcrError> {
        validate_rgb(pixels, width, height)?;
        let input = self
            .engine
            .prepare_input(
                ImageSource::from_bytes(pixels, (width, height))
                    .map_err(|_| OcrError::InvalidImage)?,
            )
            .map_err(|_| OcrError::InferenceFailed)?;
        let words = self
            .engine
            .detect_words(&input)
            .map_err(|_| OcrError::InferenceFailed)?;
        let mut boxes: Vec<_> = words
            .into_iter()
            .filter(|word| {
                let rect = word.bounding_rect();
                let ratio = rect.width() / rect.height();
                (1.0..=7.0).contains(&ratio)
                    && rect.height() >= 3.0
                    && rect.height() <= (height as f32 * 0.1).max(24.0)
            })
            .collect();
        // Do not silently truncate a dense frame and imply the timer was absent.
        if boxes.len() > MAX_DETECTED_REGIONS {
            return Err(OcrError::TooManyRegions);
        }
        boxes.sort_by(|a, b| {
            let a = a.bounding_rect();
            let b = b.bounding_rect();
            a.top()
                .total_cmp(&b.top())
                .then_with(|| a.left().total_cmp(&b.left()))
        });
        let lines: Vec<_> = boxes.iter().map(|word| vec![*word]).collect();
        let readings = self
            .engine
            .recognize_text(&input, &lines)
            .map_err(|_| OcrError::InferenceFailed)?;
        let mut candidates = Vec::new();
        for (word, reading) in boxes.iter().zip(readings) {
            let Some(reading) = reading else { continue };
            let Some((elapsed_seconds, explicit_separator)) =
                parse_clock(&reading.to_string(), false)
            else {
                continue;
            };
            if let Some(region) = normalize_rect(word.bounding_rect(), width, height) {
                candidates.push(ClockCandidate {
                    elapsed_seconds,
                    region,
                    explicit_separator,
                });
            }
        }
        Ok(candidates)
    }

    /// Read an already established clock region. This skips text detection and
    /// supplies the region as a single line. Results retain alternatives so the
    /// temporal validator, not an OCR confidence score, selects a reading.
    pub fn read_region_rgb(
        &self,
        pixels: &[u8],
        width: u32,
        height: u32,
        region: ClockRegion,
    ) -> Result<Vec<ClockCandidate>, OcrError> {
        validate_rgb(pixels, width, height)?;
        if !region.valid() {
            return Err(OcrError::InvalidRegion);
        }
        let (left, top, right, bottom) = region_pixels(region, width, height);
        // Only materialize the crop: preparing a full-frame float tensor for a
        // tiny timer would defeat the tracking fast path.
        let crop_width = right - left;
        let crop_height = bottom - top;
        let mut crop = Vec::with_capacity((crop_width * crop_height * 3) as usize);
        for y in top..bottom {
            let start = ((y * width + left) * 3) as usize;
            crop.extend_from_slice(&pixels[start..start + (crop_width * 3) as usize]);
        }
        let rect = Rect::from_tlbr(0.0, 0.0, crop_height as f32, crop_width as f32);
        let line = vec![RotatedRect::from_rect(rect)];
        let mut candidates = Vec::new();
        // Two generic contrast variants supplement raw recognition. They are
        // bounded and do not depend on a particular stream layout or answer.
        for threshold in [None, Some(140u8), Some(190u8)] {
            let filtered;
            let source = if let Some(threshold) = threshold {
                filtered = crop
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|rgb| {
                        let luminance =
                            (u16::from(rgb[0]) * 3 + u16::from(rgb[1]) * 6 + u16::from(rgb[2]))
                                / 10;
                        let value = if luminance >= u16::from(threshold) {
                            255
                        } else {
                            0
                        };
                        [value; 3]
                    })
                    .collect::<Vec<_>>();
                &filtered
            } else {
                &crop
            };
            let input = self
                .engine
                .prepare_input(
                    ImageSource::from_bytes(source, (crop_width, crop_height))
                        .map_err(|_| OcrError::InvalidImage)?,
                )
                .map_err(|_| OcrError::InferenceFailed)?;
            let readings = self
                .engine
                .recognize_text(&input, std::slice::from_ref(&line))
                .map_err(|_| OcrError::InferenceFailed)?;
            for reading in readings.into_iter().flatten() {
                let Some((elapsed_seconds, explicit_separator)) =
                    parse_clock(&reading.to_string(), true)
                else {
                    continue;
                };
                if let Some(existing) =
                    candidates
                        .iter_mut()
                        .find(|candidate: &&mut ClockCandidate| {
                            candidate.elapsed_seconds == elapsed_seconds
                        })
                {
                    existing.explicit_separator |= explicit_separator;
                } else {
                    candidates.push(ClockCandidate {
                        elapsed_seconds,
                        region,
                        explicit_separator,
                    });
                }
            }
        }
        candidates.sort_by_key(|candidate| candidate.elapsed_seconds);
        Ok(candidates)
    }
}

/// Decode a native snapshot in memory with dimensions checked before pixel
/// allocation. PNG is the only accepted encoded format.
pub fn decode_snapshot(png: &[u8]) -> Result<RgbImage, OcrError> {
    if png.len() > MAX_ENCODED_BYTES {
        return Err(OcrError::ImageTooLarge);
    }
    let reader = || ImageReader::with_format(Cursor::new(png), ImageFormat::Png);
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|_| OcrError::InvalidImage)?;
    validate_dimensions(width, height)?;
    let mut decoder = reader();
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    decoder.limits(limits);
    decoder
        .decode()
        .map(|image| image.into_rgb8())
        .map_err(|_| OcrError::InvalidImage)
}

fn load_model(path: &Path, size: usize, digest: &str) -> Result<rten::Model, OcrError> {
    let file = std::fs::File::open(path).map_err(|_| OcrError::ModelUnavailable)?;
    let mut data = Vec::with_capacity(size);
    file.take(size as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| OcrError::ModelUnavailable)?;
    if data.len() != size || hex::encode(Sha256::digest(&data)) != digest {
        return Err(OcrError::ModelMismatch);
    }
    rten::Model::load(data).map_err(|_| OcrError::ModelMismatch)
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), OcrError> {
    if width == 0 || height == 0 {
        return Err(OcrError::InvalidImage);
    }
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(OcrError::ImageTooLarge);
    }
    Ok(())
}

fn validate_rgb(pixels: &[u8], width: u32, height: u32) -> Result<(), OcrError> {
    validate_dimensions(width, height)?;
    if pixels.len() != (u64::from(width) * u64::from(height) * 3) as usize {
        return Err(OcrError::InvalidImage);
    }
    Ok(())
}

fn normalize_rect(rect: Rect<f32>, width: u32, height: u32) -> Option<ClockRegion> {
    if ![rect.left(), rect.top(), rect.right(), rect.bottom()]
        .into_iter()
        .all(f32::is_finite)
    {
        return None;
    }
    let left = rect.left().clamp(0.0, width as f32);
    let top = rect.top().clamp(0.0, height as f32);
    let right = rect.right().clamp(left, width as f32);
    let bottom = rect.bottom().clamp(top, height as f32);
    let region = ClockRegion {
        x: left / width as f32,
        y: top / height as f32,
        width: (right - left) / width as f32,
        height: (bottom - top) / height as f32,
    };
    region.valid().then_some(region)
}

fn region_pixels(region: ClockRegion, width: u32, height: u32) -> (u32, u32, u32, u32) {
    // Generic padding absorbs detector jitter and a missing digit at a box edge.
    let left = (region.x * width as f32).floor().max(2.0) as u32 - 2;
    let top = (region.y * height as f32).floor().max(2.0) as u32 - 2;
    let right = ((region.x + region.width) * width as f32).ceil() as u32;
    let bottom = ((region.y + region.height) * height as f32).ceil() as u32;
    (
        left,
        top,
        right.saturating_add(2).min(width),
        bottom.saturating_add(2).min(height),
    )
}

fn health_region_pixels(
    region: ClockRegion,
    width: u32,
    height: u32,
    padding: u32,
) -> (u32, u32, u32, u32) {
    // Normalized integer character boxes can round just below an integer after
    // multiplication. Absorb only that arithmetic error, not a glyph pixel.
    let left = (region.x * width as f32 + 0.0001).floor() as u32;
    let top = (region.y * height as f32 + 0.0001).floor() as u32;
    let right = ((region.x + region.width) * width as f32 - 0.0001).ceil() as u32;
    let bottom = ((region.y + region.height) * height as f32 - 0.0001).ceil() as u32;
    (
        left.saturating_sub(padding),
        top.saturating_sub(padding),
        right.max(left + 1).saturating_add(padding).min(width),
        bottom.max(top + 1).saturating_add(padding).min(height),
    )
}

/// Case, whitespace and apostrophes only; no fuzzy spelling or
/// substring equivalence. Callers must preserve collisions between actor names.
pub fn normalize_boss_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !matches!(ch, '\'' | '\u{2018}' | '\u{2019}'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn validate_names(names: &[BossName]) -> Result<(), OcrError> {
    if names.is_empty()
        || names.len() > MAX_BOSS_NAMES
        || names.iter().any(|name| {
            name.name.len() > 192
                || name.name.chars().any(char::is_control)
                || !(2..=96).contains(&normalize_boss_name(&name.name).chars().count())
        })
        || names.iter().enumerate().any(|(index, name)| {
            names[..index].iter().any(|previous| {
                previous.id == name.id
                    && normalize_boss_name(&previous.name) != normalize_boss_name(&name.name)
            })
        })
    {
        Err(OcrError::InvalidNames)
    } else {
        Ok(())
    }
}

fn tile_positions(length: u32) -> Vec<u32> {
    if length <= HEALTH_TILE_SIDE {
        return vec![0];
    }
    let mut positions = vec![0];
    while positions.last().copied().unwrap() + HEALTH_TILE_SIDE < length {
        let next =
            (positions.last().copied().unwrap() + HEALTH_TILE_STEP).min(length - HEALTH_TILE_SIDE);
        positions.push(next);
    }
    positions
}

fn parse_percent(text: &str) -> Option<(f64, u8)> {
    let compact: String = text
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    if compact.len() > 9 || !compact.is_ascii() {
        return None;
    }
    let number = compact.strip_suffix('%')?;
    let parts: Vec<_> = number.split(['.', ',']).collect();
    if parts.is_empty()
        || parts.len() > 2
        || !(1..=3).contains(&parts[0].len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let decimals = if parts.len() == 2 { parts[1].len() } else { 0 };
    if decimals > 3 {
        return None;
    }
    let scale = 10u32.pow(decimals as u32);
    let units = parts[0]
        .parse::<u32>()
        .ok()?
        .checked_mul(scale)?
        .checked_add(if decimals == 0 {
            0
        } else {
            parts[1].parse().ok()?
        })?;
    (units <= 100 * scale).then_some((f64::from(units) / f64::from(scale), decimals as u8))
}

fn union_region(a: ClockRegion, b: ClockRegion) -> ClockRegion {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    ClockRegion {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

fn same_line(a: ClockRegion, b: ClockRegion) -> bool {
    let overlap = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    overlap >= a.height.min(b.height) * 0.4
}

fn nearby(name: ClockRegion, percent: ClockRegion, aspect: f32) -> bool {
    if !name.valid() || !percent.valid() {
        return false;
    }
    let h = name.height.max(percent.height);
    let dx = (name.x - (percent.x + percent.width))
        .max(percent.x - (name.x + name.width))
        .max(0.0);
    let dy = ((name.y + name.height / 2.0) - (percent.y + percent.height / 2.0)).abs();
    (same_line(name, percent) && dx * aspect <= h * 24.0)
        || (dy <= h * 2.5 && dx * aspect <= h * 12.0)
}

fn associate_health(
    words: &[Word],
    names: &[BossName],
    aspect: f32,
) -> Result<Vec<HealthCandidate>, OcrError> {
    validate_names(names)?;
    if words.len() > MAX_HEALTH_WORDS {
        return Err(OcrError::TooManyRegions);
    }
    let mut labels: Vec<(u32, ClockRegion)> = Vec::new();
    for name in names {
        let expected = normalize_boss_name(&name.name);
        for (start, word) in words.iter().enumerate() {
            let first = normalize_boss_name(&word.text);
            if first.is_empty()
                || (first != expected && !expected.starts_with(&(first.clone() + " ")))
            {
                continue;
            }
            let mut paths = vec![(first, word.region, start, 1)];
            let mut expanded = 0;
            while let Some((text, bounds, last, count)) = paths.pop() {
                expanded += 1;
                if expanded > MAX_DETECTED_REGIONS {
                    return Err(OcrError::TooManyRegions);
                }
                if text == expected {
                    if !labels.contains(&(name.id, bounds)) {
                        labels.push((name.id, bounds));
                    }
                    if labels.len() > MAX_HEALTH_CANDIDATES {
                        return Err(OcrError::TooManyRegions);
                    }
                    continue;
                }
                if count == 8 {
                    continue;
                }
                for (index, next) in words.iter().enumerate() {
                    let previous = words[last].region;
                    let gap = next.region.x - (previous.x + previous.width);
                    if index == last
                        || next.region.x <= previous.x
                        || !same_line(previous, next.region)
                        || gap * aspect < -previous.height * 0.2
                        || gap * aspect > previous.height.max(next.region.height) * 3.0
                    {
                        continue;
                    }
                    let joined = format!("{text} {}", normalize_boss_name(&next.text));
                    if joined == expected || expected.starts_with(&(joined.clone() + " ")) {
                        if paths.len() >= MAX_DETECTED_REGIONS {
                            return Err(OcrError::TooManyRegions);
                        }
                        paths.push((joined, union_region(bounds, next.region), index, count + 1));
                    }
                }
            }
        }
    }
    let mut percentages = Vec::new();
    for word in words {
        if let Some((percent, decimals)) = parse_percent(&word.text) {
            percentages.push((percent, decimals, word.region));
        } else if !word.text.contains('%') {
            for sign in words.iter().filter(|sign| sign.text == "%") {
                let gap = sign.region.x - (word.region.x + word.region.width);
                if same_line(word.region, sign.region)
                    && (0.0..=word.region.height * 1.5).contains(&(gap * aspect))
                {
                    if let Some((percent, decimals)) = parse_percent(&(word.text.clone() + "%")) {
                        percentages.push((
                            percent,
                            decimals,
                            union_region(word.region, sign.region),
                        ));
                    }
                }
            }
        }
        if percentages.len() > MAX_HEALTH_CANDIDATES {
            return Err(OcrError::TooManyRegions);
        }
    }
    let mut candidates = Vec::new();
    for (boss_name_id, name_region) in labels {
        for &(percent, decimal_places, percent_region) in &percentages {
            if nearby(name_region, percent_region, aspect) {
                let candidate = HealthCandidate {
                    boss_name_id,
                    percent,
                    decimal_places,
                    name_region,
                    percent_region,
                };
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
                if candidates.len() > MAX_HEALTH_CANDIDATES {
                    return Err(OcrError::TooManyRegions);
                }
            }
        }
    }
    Ok(candidates)
}

fn parse_clock(text: &str, established_region: bool) -> Option<(u32, bool)> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.len() > 6 || compact.is_empty() || !compact.is_ascii() {
        return None;
    }
    let (minutes, seconds, explicit_separator) =
        if let Some((minutes, seconds)) = compact.split_once(':') {
            if minutes.is_empty() || minutes.len() > 3 || seconds.len() != 2 {
                return None;
            }
            (minutes, seconds, true)
        } else if established_region && (3..=4).contains(&compact.len()) {
            let (minutes, seconds) = compact.split_at(compact.len() - 2);
            (minutes, seconds, false)
        } else {
            return None;
        };
    if !minutes.bytes().all(|byte| byte.is_ascii_digit())
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let minutes: u32 = minutes.parse().ok()?;
    let seconds: u32 = seconds.parse().ok()?;
    let elapsed = minutes.checked_mul(60)?.checked_add(seconds)?;
    (seconds < 60 && elapsed <= MAX_CLOCK_SECONDS).then_some((elapsed, explicit_separator))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f32, y: f32, width: f32) -> Word {
        Word {
            text: text.into(),
            region: ClockRegion {
                x,
                y,
                width,
                height: 0.02,
            },
        }
    }

    #[test]
    fn health_tight_crops_exclude_neighbors_and_padded_crops_stay_inside_frame() {
        let r = ClockRegion {
            x: 101.0 / 1920.0,
            y: 91.0 / 1080.0,
            width: 33.0 / 1920.0,
            height: 13.0 / 1080.0,
        };
        assert_eq!(health_region_pixels(r, 1920, 1080, 0), (101, 91, 134, 104));
        assert_eq!(health_region_pixels(r, 1920, 1080, 2), (99, 89, 136, 106));
        let edge = ClockRegion {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        assert_eq!(health_region_pixels(edge, 640, 360, 2), (0, 0, 640, 360));
    }

    #[test]
    fn health_tiling_is_bounded_and_covers_small_and_large_frames() {
        assert_eq!(tile_positions(500), vec![0]);
        for length in [769, 1080, 1920, 4096] {
            let positions = tile_positions(length);
            assert_eq!(positions.first(), Some(&0));
            assert_eq!(
                positions.last().copied().unwrap() + HEALTH_TILE_SIDE,
                length
            );
            assert!(positions
                .windows(2)
                .all(|pair| pair[1] > pair[0] && pair[1] - pair[0] <= HEALTH_TILE_STEP));
        }
        assert!(tile_positions(1920).len() * tile_positions(1080).len() <= MAX_HEALTH_TILES);
        assert!(tile_positions(4096).len() * tile_positions(1024).len() > MAX_HEALTH_TILES);
    }

    #[test]
    fn health_association_allows_an_adjacent_health_row_but_not_remote_rows() {
        let names = [BossName {
            id: 1,
            name: "Amber's King".into(),
        }];
        let words = [
            word("Ambers King", 0.1, 0.1, 0.10),
            word("79.0%", 0.23, 0.13, 0.04),
            word("10%", 0.23, 0.20, 0.04),
        ];
        let found = associate_health(&words, &names, 16.0 / 9.0).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].percent, 79.0);
        assert_eq!(found[0].decimal_places, 1);
        let wrong = [BossName {
            id: 2,
            name: "Amers King".into(),
        }];
        assert!(associate_health(&words, &wrong, 16.0 / 9.0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn health_percent_preserves_display_precision_and_rejects_guesses() {
        assert_eq!(parse_percent("73%"), Some((73.0, 0)));
        assert_eq!(parse_percent("73.50%"), Some((73.5, 2)));
        assert_eq!(parse_percent(" 73,5 % "), Some((73.5, 1)));
        assert_eq!(parse_percent("0.001%"), Some((0.001, 3)));
        assert_eq!(parse_percent("100.0%"), Some((100.0, 1)));
        for value in [
            "73",
            "O%",
            "-1%",
            "101%",
            "100.001%",
            "NaN%",
            "1e1%",
            "1.2.3%",
            "12/100%",
            "12%%",
            ".5%",
            "12.%",
            "0.0001%",
            "１２%",
            "12% health",
        ] {
            assert_eq!(parse_percent(value), None, "{value}");
        }
    }

    #[test]
    fn health_requires_exact_nearby_names_and_retains_ambiguity() {
        let names = vec![BossName {
            id: 10,
            name: "The Amber King".into(),
        }];
        let words = vec![
            word("The", 0.10, 0.10, 0.03),
            word("Amber", 0.14, 0.10, 0.06),
            word("King", 0.21, 0.10, 0.04),
            word("73.5%", 0.30, 0.10, 0.05),
            word("24%", 0.91, 0.80, 0.05),
        ];
        let found = associate_health(&words, &names, 16.0 / 9.0).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].percent, 73.5);
        assert_eq!(found[0].boss_name_id, 10);
        let wrong = vec![BossName {
            id: 10,
            name: "The Amber Kings".into(),
        }];
        assert!(associate_health(&words, &wrong, 16.0 / 9.0)
            .unwrap()
            .is_empty());
        let mut ambiguous = names.clone();
        ambiguous.push(BossName {
            id: 11,
            name: "the amber king".into(),
        });
        assert_eq!(
            associate_health(&words, &ambiguous, 16.0 / 9.0)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn health_label_normalization_and_name_bounds_do_not_merge_different_names() {
        assert_eq!(normalize_boss_name("  Amber’S   King  "), "ambers king");
        assert_ne!(
            normalize_boss_name("Amber King"),
            normalize_boss_name("Amberking")
        );
        assert_eq!(validate_names(&[]), Err(OcrError::InvalidNames));
        assert_eq!(
            validate_names(&[BossName {
                id: 1,
                name: "A".into()
            }]),
            Err(OcrError::InvalidNames)
        );
        let conflict = [
            BossName {
                id: 1,
                name: "Amber King".into(),
            },
            BossName {
                id: 1,
                name: "Ivory Queen".into(),
            },
        ];
        assert_eq!(validate_names(&conflict), Err(OcrError::InvalidNames));
        let too_many: Vec<_> = (0..=MAX_BOSS_NAMES)
            .map(|id| BossName {
                id: id as u32,
                name: "Amber King".into(),
            })
            .collect();
        assert_eq!(validate_names(&too_many), Err(OcrError::InvalidNames));
    }

    #[test]
    fn health_percent_sign_and_name_association_remain_spatial() {
        let names = vec![BossName {
            id: 1,
            name: "Amber".into(),
        }];
        let mut words = vec![
            word("Amber", 0.10, 0.10, 0.08),
            word("73.50", 0.25, 0.10, 0.04),
            word("%", 0.295, 0.10, 0.01),
        ];
        let found = associate_health(&words, &names, 16.0 / 9.0).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].decimal_places, 2);
        words[2].region.y = 0.4;
        assert!(associate_health(&words, &names, 16.0 / 9.0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn clock_parser_rejects_non_clocks_and_requires_colon_for_discovery() {
        assert_eq!(parse_clock("0:55", false), Some((55, true)));
        assert_eq!(parse_clock("1 : 43", false), Some((103, true)));
        assert_eq!(parse_clock("143", false), None);
        assert_eq!(parse_clock("143", true), Some((103, false)));
        for invalid in [
            "55",
            "1:99",
            "1:2",
            "1:02:03",
            "20.5",
            "3:34%",
            "NaN",
            "",
            "éa",
            "１２:３４",
            "999:59",
        ] {
            assert_eq!(parse_clock(invalid, true), None, "{invalid}");
        }
    }

    #[test]
    fn untrusted_image_and_region_bounds_are_checked() {
        assert_eq!(validate_rgb(&[], 0, 1), Err(OcrError::InvalidImage));
        assert_eq!(
            validate_rgb(&[], u32::MAX, u32::MAX),
            Err(OcrError::ImageTooLarge)
        );
        assert_eq!(validate_rgb(&[0; 3], 2, 1), Err(OcrError::InvalidImage));
        assert_eq!(decode_snapshot(b"not a PNG"), Err(OcrError::InvalidImage));
        let invalid = ClockRegion {
            x: f32::NAN,
            y: 0.0,
            width: 0.1,
            height: 0.1,
        };
        assert!(!invalid.valid());
        let edge = ClockRegion {
            x: 0.9,
            y: 0.0,
            width: 0.1,
            height: 0.1,
        };
        assert_eq!(region_pixels(edge, 100, 100), (88, 0, 100, 12));
        assert_eq!(edge.overlap(edge), 1.0);
        assert_eq!(edge.overlap(invalid), 0.0);
    }

    #[test]
    fn png_dimensions_are_rejected_before_pixel_decode() {
        let mut encoded = Cursor::new(Vec::new());
        RgbImage::new(MAX_SIDE + 1, 1)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        assert_eq!(
            decode_snapshot(encoded.get_ref()),
            Err(OcrError::ImageTooLarge)
        );
        let pixels = RgbImage::from_raw(2, 1, vec![20, 30, 40, 80, 90, 100]).unwrap();
        let mut encoded = Cursor::new(Vec::new());
        pixels.write_to(&mut encoded, ImageFormat::Png).unwrap();
        assert_eq!(decode_snapshot(encoded.get_ref()).unwrap(), pixels);
    }

    /// Local-only model/fixture test. The manifest is a JSON array of objects
    /// with `path`, `group` (same video layout), and `expected_seconds` fields.
    /// Private file names, video IDs, and images stay outside the repository.
    /// Set RTEN_NUM_THREADS=1 before starting the test executable.
    #[test]
    #[ignore = "requires explicit local model and image-fixture paths"]
    fn local_frames_discover_then_track_without_configured_crop() {
        use std::time::Instant;

        assert_eq!(std::env::var("RTEN_NUM_THREADS").as_deref(), Ok("1"));
        let model_dir = std::path::PathBuf::from(
            std::env::var_os("BRICK_OCR_MODEL_DIR").expect("BRICK_OCR_MODEL_DIR is required"),
        );
        let manifest =
            std::env::var_os("BRICK_OCR_FIXTURES").expect("BRICK_OCR_FIXTURES is required");
        let fixtures: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let fixtures = fixtures.as_array().unwrap();
        assert!(!fixtures.is_empty() && fixtures.len() <= 24);
        let started = Instant::now();
        let engine = ReplayOcr::load(
            &model_dir.join("text-detection.rten"),
            &model_dir.join("text-recognition.rten"),
        )
        .unwrap();
        println!(
            "model_load_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        let mut learned: std::collections::BTreeMap<String, Vec<ClockRegion>> =
            std::collections::BTreeMap::new();
        let mut frames = Vec::new();
        for (index, fixture) in fixtures.iter().enumerate() {
            let group = fixture["group"].as_str().unwrap().to_owned();
            let frame = decode_snapshot(&std::fs::read(fixture["path"].as_str().unwrap()).unwrap())
                .unwrap();
            let started = Instant::now();
            let candidates = engine
                .discover_rgb(frame.as_raw(), frame.width(), frame.height())
                .unwrap();
            println!(
                "frame={index} discovery_ms={:.3} candidates={candidates:?}",
                started.elapsed().as_secs_f64() * 1000.0
            );
            let regions = learned.entry(group.clone()).or_default();
            for candidate in candidates {
                if !regions
                    .iter()
                    .any(|region| region.overlap(candidate.region) >= 0.3)
                {
                    regions.push(candidate.region);
                }
            }
            assert!(regions.len() <= 24, "too many candidate clock regions");
            frames.push((group, frame));
        }
        // Regions are learned solely from discovered clock syntax and geometry.
        // Expected answers are used only in the final assertion, never to pick
        // a crop, reading, preprocessing variant, or model parameter.
        let mut missed = Vec::new();
        for (index, (group, frame)) in frames.iter().enumerate() {
            let expected = fixtures[index]["expected_seconds"].as_u64().unwrap() as u32;
            let mut found = false;
            let mut observations = Vec::new();
            for (region_index, region) in learned[group].iter().copied().enumerate() {
                let started = Instant::now();
                let readings = engine
                    .read_region_rgb(frame.as_raw(), frame.width(), frame.height(), region)
                    .unwrap();
                println!("frame={index} region={region_index} recognition_ms={:.3} candidates={readings:?}",
                    started.elapsed().as_secs_f64() * 1000.0);
                found |= readings
                    .iter()
                    .any(|reading| reading.elapsed_seconds == expected);
                observations.extend(readings.iter().map(|reading| {
                    serde_json::json!({
                        "region_id": region_index,
                        "elapsed_seconds": reading.elapsed_seconds,
                        "explicit_separator": reading.explicit_separator,
                        "region": [region.x, region.y, region.width, region.height]
                    })
                }));
            }
            println!(
                "OCR_FIXTURE_RESULT {}",
                serde_json::json!({
                    "frame": index,
                    "group": group,
                    "video_seconds": fixtures[index].get("video_seconds"),
                    "candidates": observations
                })
            );
            if !found {
                missed.push(index);
            }
        }
        assert!(
            missed.is_empty(),
            "expected clock absent from candidate readings in frames {missed:?}"
        );
    }
}
