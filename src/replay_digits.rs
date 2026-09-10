//! Reads only ART's fixed-position Unix timestamp, using the same bundled font.
//! No general HUD OCR, model downloads, image persistence or external OCR service.
use image::{
    imageops::{crop_imm, resize, FilterType},
    GrayImage, ImageReader, Luma,
};
use serde::Deserialize;
use std::{
    io::Cursor,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    pub unix_seconds: i64,
    pub region: Region,
    pub dimensions: (u32, u32),
    pub style: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    Present(Marker),
    Absent,
    Uncertain,
}

#[derive(Deserialize)]
struct Glyph {
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    pixels: Vec<u8>,
}
#[derive(Deserialize)]
struct Digit {
    advance: f64,
    phases: Vec<Glyph>,
}
#[derive(Deserialize)]
struct Font {
    glyphs: Vec<Digit>,
}
fn fonts() -> &'static Vec<Font> {
    static FONTS: OnceLock<Vec<Font>> = OnceLock::new();
    FONTS.get_or_init(|| {
        serde_json::from_slice(include_bytes!("assets/replay-digits.json"))
            .expect("bundled ART digit templates")
    })
}
pub(crate) fn template(timestamp: i64, style: usize) -> GrayImage {
    let font = &fonts()[style];
    let mut image = GrayImage::new(240, 64);
    let mut x = 4.0_f64;
    for digit in timestamp.to_string().bytes() {
        let digit = &font.glyphs[(digit - b'0') as usize];
        let glyph = &digit.phases[((x.fract() * 4.0).round() as usize).min(3)];
        for gy in 0..glyph.height {
            for gx in 0..glyph.width {
                let px = x.floor() as i32 + glyph.left + gx as i32;
                let py = 48 + glyph.top + gy as i32;
                if px >= 0 && py >= 0 && px < 240 && py < 64 {
                    let value = glyph.pixels[(gy * glyph.width + gx) as usize];
                    image.put_pixel(
                        px as u32,
                        py as u32,
                        Luma([value.max(image.get_pixel(px as u32, py as u32)[0])]),
                    );
                }
            }
        }
        x += digit.advance;
    }
    let mut bounds = (240, 64, 0, 0);
    for (x, y, p) in image.enumerate_pixels() {
        if p[0] > 20 {
            bounds.0 = bounds.0.min(x);
            bounds.1 = bounds.1.min(y);
            bounds.2 = bounds.2.max(x);
            bounds.3 = bounds.3.max(y);
        }
    }
    crop_imm(
        &image,
        bounds.0,
        bounds.1,
        bounds.2 - bounds.0 + 1,
        bounds.3 - bounds.1 + 1,
    )
    .to_image()
}

type Templates = Vec<(i64, usize, GrayImage)>;
fn templates(timestamp: i64) -> Arc<Templates> {
    static RECENT: OnceLock<Mutex<Vec<(i64, Arc<Templates>)>>> = OnceLock::new();
    let cache = RECENT.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(recent) = cache.lock() {
        if let Some((_, templates)) = recent.iter().find(|(key, _)| *key == timestamp) {
            return templates.clone();
        }
    }
    let templates = Arc::new(
        (-3..=3)
            .flat_map(|delta| {
                let stamp = timestamp + delta;
                (0..fonts().len()).map(move |style| (stamp, style, template(stamp, style)))
            })
            .collect(),
    );
    if let Ok(mut recent) = cache.lock() {
        recent.retain(|(key, _)| *key != timestamp);
        if recent.len() == 4 {
            recent.remove(0);
        }
        recent.push((timestamp, Arc::clone(&templates)));
    }
    templates
}

/// Normalized correlation is independent of the video's brightness/contrast.
/// Low-information crops never count as evidence that a marker is present.
fn correlation(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.len() < 24 {
        return 0.0;
    }
    let n = a.len() as f64;
    let ma = a.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mb = b.iter().map(|&v| v as f64).sum::<f64>() / n;
    let (mut cross, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (&a, &b) in a.iter().zip(b) {
        let a = a as f64 - ma;
        let b = b as f64 - mb;
        cross += a * b;
        va += a * a;
        vb += b * b;
    }
    if va < n * 100.0 || vb < n * 100.0 {
        return 0.0;
    }
    cross / (va * vb).sqrt()
}

fn score(image: &GrayImage, region: Region, template: &GrayImage) -> f64 {
    if region.width < 20
        || region.height < 4
        || region.width > 180
        || region.height > 32
        || region.x + region.width > image.width()
        || region.y + region.height > image.height()
    {
        return 0.0;
    }
    let actual = crop_imm(image, region.x, region.y, region.width, region.height).to_image();
    let expected = resize(template, region.width, region.height, FilterType::Triangle);
    let whole = correlation(actual.as_raw(), expected.as_raw());
    let left = region.width * 7 / 10;
    let tail_a = crop_imm(&actual, left, 0, region.width - left, region.height).to_image();
    let tail_b = crop_imm(&expected, left, 0, region.width - left, region.height).to_image();
    let tail = correlation(tail_a.as_raw(), tail_b.as_raw());
    whole * 0.4 + tail * 0.6
}

fn fit(
    image: &GrayImage,
    region: Region,
    stamps: &[(i64, usize, GrayImage)],
) -> (Region, i64, usize, f64) {
    let best = |region| {
        stamps
            .iter()
            .map(|(stamp, style, template)| {
                (region, *stamp, *style, score(image, region, template))
            })
            .max_by(|a, b| a.3.total_cmp(&b.3))
            .unwrap()
    };
    let mut found = best(region);
    if found.3 < 0.50 || found.3 >= 0.92 {
        return found;
    }
    // Fit both horizontal edges together: fitting one at a time can make a
    // clipped 5 look like a 3 and settle on the wrong width/font combination.
    for left in -2_i32..=2 {
        for right in -2_i32..=2 {
            let x = region.x as i32 + left;
            let width = region.width as i32 + right - left;
            if x < 0 || width <= 0 {
                continue;
            }
            let candidate = best(Region {
                x: x as u32,
                width: width as u32,
                ..region
            });
            if candidate.3 > found.3 {
                found = candidate;
            }
        }
    }
    let base = found.0;
    for top in -1_i32..=1 {
        for bottom in -1_i32..=1 {
            let y = base.y as i32 + top;
            let height = base.height as i32 + bottom - top;
            if y < 0 || height <= 0 {
                continue;
            }
            let candidate = best(Region {
                y: y as u32,
                height: height as u32,
                ..base
            });
            if candidate.3 > found.3 {
                found = candidate;
            }
        }
    }
    found
}

fn has_better_other_number(
    image: &GrayImage,
    region: Region,
    stamp: i64,
    confidence: f64,
    start_ms: i64,
    style: usize,
) -> bool {
    // Challenge the proposed reading with every single-digit alternative,
    // including numbers outside the expected pull window. Metadata must never
    // coerce a visually different timestamp into the selected pull.
    let mut digits = stamp.to_string().into_bytes();
    for position in 0..digits.len() {
        let original = digits[position];
        for digit in b'0'..=b'9' {
            if digit == original || (position == 0 && digit == b'0') {
                continue;
            }
            digits[position] = digit;
            let other = std::str::from_utf8(&digits)
                .unwrap()
                .parse::<i64>()
                .unwrap();
            if (other * 1000 - start_ms).abs() > 3500
                && score(image, region, &template(other, style)) > confidence + 0.015
            {
                #[cfg(test)]
                if std::env::var_os("BRICK_MARKER_DIAGNOSTIC").is_some() {
                    eprintln!("rejected {stamp} for alternative {other}");
                }
                return true;
            }
        }
        digits[position] = original;
    }
    false
}

fn candidates(image: &GrayImage) -> Vec<Region> {
    let mut regions = candidates_at(image, 70);
    regions.extend(candidates_at(image, 35));
    regions.sort_by_key(|r| (r.y, r.x, r.width, r.height));
    regions.dedup();
    regions
}

fn candidates_at(image: &GrayImage, threshold: u8) -> Vec<Region> {
    let width = (image.width() / 2).min(2048) as usize;
    let height = (image.height() / 3).min(1024) as usize;
    let mut seen = vec![false; width * height];
    let mut stack = Vec::new();
    let mut parts = Vec::<Region>::new();
    for y in 0..height {
        for x in 0..width {
            if seen[y * width + x] || image.get_pixel(x as u32, y as u32)[0] < threshold {
                continue;
            }
            seen[y * width + x] = true;
            stack.push((x, y));
            let (mut left, mut top, mut right, mut bottom) = (x, y, x, y);
            while let Some((x, y)) = stack.pop() {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
                for dy in -1_i32..=1 {
                    for dx in -1_i32..=1 {
                        let nx = x as i32 + dx;
                        let ny = y as i32 + dy;
                        if nx >= 0 && ny >= 0 && (nx as usize) < width && (ny as usize) < height {
                            let (nx, ny) = (nx as usize, ny as usize);
                            if !seen[ny * width + nx]
                                && image.get_pixel(nx as u32, ny as u32)[0] >= threshold
                            {
                                seen[ny * width + nx] = true;
                                stack.push((nx, ny));
                            }
                        }
                    }
                }
            }
            if (2..=32).contains(&(bottom - top + 1)) && right - left < 180 {
                parts.push(Region {
                    x: left as u32,
                    y: top as u32,
                    width: (right - left + 1) as u32,
                    height: (bottom - top + 1) as u32,
                });
                if parts.len() > 2048 {
                    return Vec::new();
                }
            }
        }
    }
    parts.sort_by_key(|r| (r.x, r.y));
    let mut words = Vec::new();
    for (i, &first) in parts.iter().enumerate() {
        let mut word = first;
        let mut joined = 1;
        for next in &parts[i + 1..] {
            let overlap = (word.y + word.height)
                .min(next.y + next.height)
                .saturating_sub(word.y.max(next.y));
            let gap = next.x.saturating_sub(word.x + word.width);
            if gap > word.height.max(4) {
                break;
            }
            if overlap * 2 >= word.height.min(next.height)
                && gap <= word.height.max(next.height) * 3 / 4
            {
                let bottom = (word.y + word.height).max(next.y + next.height);
                word.y = word.y.min(next.y);
                word.height = bottom - word.y;
                word.width = (next.x + next.width).max(word.x + word.width) - word.x;
                joined += 1;
                // Keep complete numeric runs even if gray scenery follows them.
                // Waiting until the end of the row folds that scenery into the text.
                if joined >= 5
                    && (4..=32).contains(&word.height)
                    && (20..=180).contains(&word.width)
                    && word.width >= word.height * 3
                    && word.width <= word.height * 9
                {
                    words.push(word);
                }
            }
        }
        if words.len() > 4096 {
            break;
        }
    }
    // A top-left UI icon can split the first digit from an otherwise intact
    // numeric run. Test one leading digit's width as well; the same full-number
    // confidence and alternative-number checks still apply.
    let extended: Vec<_> = words
        .iter()
        .filter_map(|word| {
            let extra = word.width / 9;
            (word.width >= word.height * 4 && extra > 0 && word.x >= extra).then_some(Region {
                x: word.x.saturating_sub(extra),
                width: word.width + extra,
                ..*word
            })
        })
        .collect();
    words.extend(extended);
    words.sort_by_key(|r| (r.y, r.x, r.width, r.height));
    words.dedup();
    words.truncate(128);
    words
}

pub(crate) fn recognize(image: &GrayImage, start_ms: i64, locked: Option<Marker>) -> Reading {
    if let Some(marker) = locked {
        if marker.dimensions != image.dimensions() {
            return Reading::Uncertain;
        }
        let expected = template(marker.unix_seconds, marker.style);
        let confidence = score(image, marker.region, &expected);
        // Discovery establishes all ten digits at 0.80. Tracking only asks
        // whether that already-verified pattern remains visible through color
        // bleed; it must not reinterpret weaker frames as different numbers.
        if confidence >= 0.60 {
            return Reading::Present(marker);
        }
        // Adaptive playback can change the text geometry while the capture's
        // dimensions stay the same. Reacquire before treating a lost crop as
        // disappearance of the five-second marker.
        return match recognize(image, start_ms, None) {
            Reading::Present(found) if found.unix_seconds == marker.unix_seconds => {
                Reading::Present(found)
            }
            Reading::Present(_) | Reading::Uncertain => Reading::Uncertain,
            Reading::Absent if confidence < 0.35 => Reading::Absent,
            Reading::Absent => Reading::Uncertain,
        };
    }
    let stamps = templates(start_ms / 1000);
    let mut found = None;
    #[cfg(test)]
    if std::env::var_os("BRICK_MARKER_DIAGNOSTIC").is_some() {
        eprintln!("candidate regions: {:?}", candidates(image));
    }
    let coarse: Vec<_> = stamps
        .iter()
        .filter(|(stamp, style, _)| {
            *stamp == start_ms / 1000 && [0, 4, 8, 12, 18, 26].contains(style)
        })
        .collect();
    let mut regions: Vec<_> = candidates(image)
        .into_iter()
        .map(|region| {
            let confidence = coarse
                .iter()
                .map(|(_, _, template)| score(image, region, template))
                .fold(0.0_f64, f64::max);
            (region, confidence)
        })
        .filter(|(_, confidence)| *confidence >= 0.35)
        .collect();
    regions.sort_by(|a, b| b.1.total_cmp(&a.1));
    // Only promising numeric runs receive the more expensive edge fitting.
    for (region, _) in regions.into_iter().take(8) {
        let (region, stamp, style, confidence) = fit(image, region, &stamps);
        #[cfg(test)]
        if std::env::var_os("BRICK_MARKER_DIAGNOSTIC").is_some() {
            eprintln!("region={region:?} stamp={stamp} style={style} confidence={confidence}");
        }
        if confidence >= 0.80
            && !has_better_other_number(image, region, stamp, confidence, start_ms, style)
        {
            let marker = Marker {
                unix_seconds: stamp,
                region,
                dimensions: image.dimensions(),
                style,
            };
            if found.is_some_and(|(old, _): (Marker, f64)| {
                let a = old.region;
                let b = marker.region;
                let overlap = (a.x + a.width)
                    .min(b.x + b.width)
                    .saturating_sub(a.x.max(b.x))
                    * (a.y + a.height)
                        .min(b.y + b.height)
                        .saturating_sub(a.y.max(b.y));
                old.unix_seconds != marker.unix_seconds
                    || overlap * 5 < (a.width * a.height).min(b.width * b.height) * 4
            }) {
                return Reading::Uncertain;
            }
            if found.is_none_or(|(_, old_confidence)| confidence > old_confidence) {
                found = Some((marker, confidence));
            }
        }
    }
    found.map_or(Reading::Absent, |(marker, _)| Reading::Present(marker))
}

pub fn read(png: &[u8], start_ms: i64, locked: Option<Marker>) -> Reading {
    if png.len() > 24 * 1024 * 1024 {
        return Reading::Uncertain;
    }
    let reader = ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
    let Ok((width, height)) = reader.into_dimensions() else {
        return Reading::Uncertain;
    };
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || u64::from(width) * u64::from(height) > 8_847_360
    {
        return Reading::Uncertain;
    }
    let Ok(decoded) = image::load_from_memory_with_format(png, image::ImageFormat::Png) else {
        return Reading::Uncertain;
    };
    let rgb = decoded.into_rgb8();
    let image = GrayImage::from_fn(width, height, |x, y| {
        let p = rgb.get_pixel(x, y).0;
        // Keep the neutral component continuously. YUV compression bleeds
        // bright scenery into tiny white letters; a hard saturation cutoff
        // would erase those still-visible digits as the camera moves.
        Luma([*p.iter().min().unwrap()])
    });
    recognize(&image, start_ms, locked)
}
