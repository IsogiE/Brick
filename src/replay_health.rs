//! Estimated, local video/log association from observed boss-health changes.
//!
//! A displayed percent is uncertain by one displayed least-significant unit;
//! this covers ordinary floor/round formatting, not arbitrary OCR errors. Every
//! reading alternative is retained. Decreasing thresholds between disjoint
//! reading ranges are bracketed by actual video frames and logged HP samples.
//! Subtracting those brackets estimates `video_seconds - fight_elapsed_seconds`.
//! There is no assumed stream offset, fixed UI delay, or interpolation across a
//! missing trace. Equal-time HP observations remain ranges, never an arbitrary
//! first/last value. Healing, HP-pool changes and trace gaps divide trace segments.
//!
//! This is a visual-health association, not proof of physical event-frame timing.
//! Sampling and quantization bounds do not measure game UI or decoder latency.
//! The caller must establish exact nearby boss-name association, fresh snapshots,
//! and a stable viewport, rejecting seeks/buffering and obsolete asynchronous
//! work. `scope` must bind account/authorization, report/fight, provider/video and
//! POV. Do not apply these estimates outside the observed segment, to another
//! event type as an exact guarantee, or across unsampled archive discontinuities.

const MAX_SECONDS: f64 = 7.0 * 86_400.0;
const MAX_FRAMES: usize = 16;
const MAX_CANDIDATES: usize = 32;
const MAX_TRACES: usize = 64;
const MAX_POINTS: usize = 16_000;
const MAX_HISTORIES: usize = 128;
const MAX_MAPPINGS: usize = 128;
const MAX_WORK: usize = 4_000_000;
const MAX_CAPTURE_BRACKET: f64 = 1.0;
const MAX_FRAME_GAP: f64 = 10.0;
const MIN_FRAME_SPAN: f64 = 1.0;
const MAX_FRAME_SPAN: f64 = 120.0;
const MAX_TRACE_GAP: f64 = 0.25;
const MIN_PERCENT_SPAN: f64 = 0.2;
const MAX_MAPPING_WIDTH: f64 = 5.0;
const MIN_REGION_OVERLAP: f64 = 0.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    /// Opaque identity covering account/auth generation, report/fight and video/POV.
    pub scope: [u8; 32],
    pub capture_generation: u64,
    pub viewport_generation: u64,
    pub segment_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interval {
    pub lower_seconds: f64,
    pub upper_seconds: f64,
}

impl Interval {
    pub fn width_seconds(self) -> f64 {
        self.upper_seconds - self.lower_seconds
    }

    fn valid(self, lower: f64, upper: f64) -> bool {
        self.lower_seconds.is_finite()
            && self.upper_seconds.is_finite()
            && lower <= self.lower_seconds
            && self.lower_seconds <= self.upper_seconds
            && self.upper_seconds <= upper
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let result = Self {
            lower_seconds: self.lower_seconds.max(other.lower_seconds),
            upper_seconds: self.upper_seconds.min(other.upper_seconds),
        };
        // Do not report zero uncertainty from a single quantization boundary.
        (result.lower_seconds < result.upper_seconds).then_some(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f64::is_finite)
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.width * self.height > 0.0
            && self.x + self.width > self.x
            && self.y + self.height > self.y
            && self.x + self.width <= 1.0
            && self.y + self.height <= 1.0
    }

    fn overlap(self, other: Self) -> f64 {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        let intersection = width.max(0.0) * height.max(0.0);
        let union = self.width * self.height + other.width * other.height - intersection;
        if union > 0.0 {
            (intersection / union).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub region_id: u32,
    /// Exact normalized nearby name, not a uniquely identified NPC/instance.
    pub boss_name_id: u32,
    pub region: Rect,
    pub percent: f64,
    pub decimal_places: u8,
}

impl Candidate {
    fn unit(self) -> f64 {
        10.0_f64.powi(-i32::from(self.decimal_places))
    }

    fn bounds(self) -> (f64, f64) {
        (
            (self.percent - self.unit()).max(0.0),
            (self.percent + self.unit()).min(100.0),
        )
    }

    fn valid(self) -> bool {
        self.region.valid()
            && self.percent.is_finite()
            && (0.0..=100.0).contains(&self.percent)
            && self.decimal_places <= 2
            && (self.percent / self.unit() - (self.percent / self.unit()).round()).abs() < 1e-6
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub context: Context,
    pub sequence: u64,
    /// Fresh before/after SDK readings. These do not bound decoder/display latency.
    pub media: Interval,
    pub fresh: bool,
    pub playing: bool,
    pub buffering: bool,
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HealthPoint {
    pub elapsed_seconds: f64,
    pub hit_points: u64,
    pub max_hit_points: u64,
}

#[derive(Clone, Debug)]
pub struct Trace {
    pub context: Context,
    pub actor_id: u64,
    /// Omission is distinct from a numbered instance.
    pub instance_id: Option<u64>,
    pub boss_name_id: u32,
    /// All target-owned observations in order, retaining equal-time alternatives.
    pub points: Vec<HealthPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    EstimatedVisualHealth,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub actor_id: u64,
    pub instance_id: Option<u64>,
    pub boss_name_id: u32,
    pub region_id: u32,
    /// Estimated local intercept, not an assertion that pull zero exists in the VOD.
    pub start_interval: Interval,
    pub observed_video_interval: Interval,
    pub observed_frames: Vec<Interval>,
    pub matched_trace_interval: Interval,
    pub crossing_count: usize,
    pub max_capture_bracket_seconds: f64,
    pub max_percent_quantization_unit: f64,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assessment {
    pub context: Context,
    /// Different actors, instances, regions, reading histories or trace segments
    /// remain separate even when their estimated intercept intervals overlap.
    pub mappings: Vec<Mapping>,
}

impl Assessment {
    pub fn unique_mapping(&self) -> Option<&Mapping> {
        (self.mappings.len() == 1).then(|| &self.mappings[0])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    TooFewFrames,
    InputTooLarge,
    InvalidInput,
    ContextChanged,
    StaleCapture,
    InsufficientCoverage,
    InsufficientVariation,
    NoConsistentTrace,
    ImpreciseMapping,
}

#[derive(Clone, Copy)]
struct Bucket {
    time: f64,
    lower: f64,
    upper: f64,
    max_hp: u64,
    consistent_pool: bool,
}

#[derive(Clone, Copy)]
struct Crossing {
    percent: f64,
    video: Interval,
}

fn spend(work: &mut usize, amount: usize) -> Result<(), Rejection> {
    *work = work.checked_add(amount).ok_or(Rejection::InputTooLarge)?;
    if *work > MAX_WORK {
        return Err(Rejection::InputTooLarge);
    }
    Ok(())
}

/// Match three to sixteen fresh frames from a single short pull/POV segment.
/// At least two decreasing threshold crossings are required. No sample or
/// competing reading is silently discarded to make an estimate fit.
pub fn match_health(
    context: Context,
    frames: &[Frame],
    traces: &[Trace],
    pull_duration_seconds: f64,
) -> Result<Assessment, Rejection> {
    validate(context, frames, traces, pull_duration_seconds)?;
    let observed = Interval {
        lower_seconds: frames[0].media.lower_seconds,
        upper_seconds: frames[frames.len() - 1].media.upper_seconds,
    };
    let mut work = 0;
    let histories = histories(frames, &mut work)?;
    let mut prepared = Vec::new();
    for trace in traces {
        prepared.push((trace, buckets(&trace.points)));
    }
    let mut mappings = Vec::new();
    let mut varying = false;
    let mut imprecise = false;
    for history in &histories {
        let changes = crossings(frames, history);
        if changes.len() < 2
            || history[0].percent - history[history.len() - 1].percent < MIN_PERCENT_SPAN
        {
            continue;
        }
        varying = true;
        for (trace, points) in &prepared {
            if trace.boss_name_id != history[0].boss_name_id {
                continue;
            }
            for segment in segments(points) {
                if !supports_readings(segment, history, &mut work)? {
                    continue;
                }
                let Some((start, matched_trace)) = match_segment(segment, &changes, &mut work)?
                else {
                    continue;
                };
                // The whole observed frame envelope must stay inside this one
                // contiguous, nonhealing HP-pool segment for every possible offset.
                let log_start = observed.lower_seconds - start.upper_seconds;
                let log_end = observed.upper_seconds - start.lower_seconds;
                if log_start < segment[0].time || log_end > segment[segment.len() - 1].time {
                    continue;
                }
                if start.width_seconds() > MAX_MAPPING_WIDTH {
                    imprecise = true;
                    continue;
                }
                if mappings.len() == MAX_MAPPINGS {
                    return Err(Rejection::InputTooLarge);
                }
                mappings.push(Mapping {
                    actor_id: trace.actor_id,
                    instance_id: trace.instance_id,
                    boss_name_id: trace.boss_name_id,
                    region_id: history[0].region_id,
                    start_interval: start,
                    observed_video_interval: observed,
                    observed_frames: frames.iter().map(|frame| frame.media).collect(),
                    matched_trace_interval: matched_trace,
                    crossing_count: changes.len(),
                    max_capture_bracket_seconds: frames
                        .iter()
                        .map(|f| f.media.width_seconds())
                        .fold(0.0, f64::max),
                    max_percent_quantization_unit: history
                        .iter()
                        .map(|c| c.unit())
                        .fold(0.0, f64::max),
                    evidence: Evidence::EstimatedVisualHealth,
                });
            }
        }
    }
    if mappings.is_empty() {
        return Err(if !varying {
            Rejection::InsufficientVariation
        } else if imprecise {
            Rejection::ImpreciseMapping
        } else {
            Rejection::NoConsistentTrace
        });
    }
    Ok(Assessment { context, mappings })
}

fn validate(
    context: Context,
    frames: &[Frame],
    traces: &[Trace],
    duration: f64,
) -> Result<(), Rejection> {
    if frames.len() < 3 {
        return Err(Rejection::TooFewFrames);
    }
    if frames.len() > MAX_FRAMES
        || traces.len() > MAX_TRACES
        || frames.iter().any(|f| f.candidates.len() > MAX_CANDIDATES)
        || traces
            .iter()
            .try_fold(0usize, |sum, t| sum.checked_add(t.points.len()))
            .is_none_or(|n| n > MAX_POINTS)
    {
        return Err(Rejection::InputTooLarge);
    }
    if !duration.is_finite() || !(0.0..=MAX_SECONDS).contains(&duration) || duration == 0.0 {
        return Err(Rejection::InvalidInput);
    }
    if frames.iter().any(|f| f.context != context) || traces.iter().any(|t| t.context != context) {
        return Err(Rejection::ContextChanged);
    }
    if frames.iter().any(|f| !f.fresh || !f.playing || f.buffering)
        || frames.windows(2).any(|p| {
            p[0].sequence >= p[1].sequence || p[0].media.upper_seconds >= p[1].media.lower_seconds
        })
    {
        return Err(Rejection::StaleCapture);
    }
    if frames.iter().any(|f| {
        !f.media.valid(0.0, MAX_SECONDS)
            || f.media.width_seconds() > MAX_CAPTURE_BRACKET
            || f.candidates.iter().any(|c| !c.valid())
    }) {
        return Err(Rejection::InvalidInput);
    }
    let span = frames[frames.len() - 1].media.lower_seconds - frames[0].media.upper_seconds;
    if span < MIN_FRAME_SPAN
        || frames[frames.len() - 1].media.upper_seconds - frames[0].media.lower_seconds
            > MAX_FRAME_SPAN
        || frames
            .windows(2)
            .any(|p| p[1].media.lower_seconds - p[0].media.upper_seconds > MAX_FRAME_GAP)
    {
        return Err(Rejection::InsufficientCoverage);
    }
    for (index, trace) in traces.iter().enumerate() {
        if traces[..index]
            .iter()
            .any(|t| t.actor_id == trace.actor_id && t.instance_id == trace.instance_id)
            || trace.points.iter().any(|p| {
                !p.elapsed_seconds.is_finite()
                    || !(0.0..=duration).contains(&p.elapsed_seconds)
                    || p.max_hit_points == 0
                    || p.hit_points > p.max_hit_points
            })
            || trace
                .points
                .windows(2)
                .any(|p| p[0].elapsed_seconds > p[1].elapsed_seconds)
        {
            return Err(Rejection::InvalidInput);
        }
    }
    Ok(())
}

fn histories(frames: &[Frame], work: &mut usize) -> Result<Vec<Vec<Candidate>>, Rejection> {
    let mut histories: Vec<Vec<Candidate>> = vec![Vec::new()];
    for frame in frames {
        let mut candidates: Vec<Candidate> = Vec::new();
        for candidate in &frame.candidates {
            if let Some(existing) = candidates.iter_mut().find(|existing| {
                existing.region_id == candidate.region_id
                    && existing.boss_name_id == candidate.boss_name_id
                    && existing.percent == candidate.percent
                    && existing.decimal_places == candidate.decimal_places
                    && existing.region.overlap(candidate.region) >= MIN_REGION_OVERLAP
            }) {
                // Repeated preprocessing of the same word is one reading, not
                // independent evidence or an alternative health interpretation.
                existing.region = existing.region.union(candidate.region);
            } else {
                candidates.push(*candidate);
            }
        }
        let mut next = Vec::new();
        for history in &histories {
            for candidate in &candidates {
                spend(work, 1)?;
                if let Some(first) = history.first() {
                    let previous = history[history.len() - 1];
                    if first.region_id != candidate.region_id
                        || first.boss_name_id != candidate.boss_name_id
                        || first.region.overlap(candidate.region) < MIN_REGION_OVERLAP
                        || previous.region.overlap(candidate.region) < MIN_REGION_OVERLAP
                        || candidate.percent > previous.percent
                    {
                        continue;
                    }
                }
                if next.len() == MAX_HISTORIES {
                    return Err(Rejection::InputTooLarge);
                }
                let mut value = history.clone();
                value.push(*candidate);
                next.push(value);
            }
        }
        histories = next;
    }
    Ok(histories)
}

fn crossings(frames: &[Frame], history: &[Candidate]) -> Vec<Crossing> {
    let mut changes = Vec::new();
    let mut reference = 0;
    for index in 1..history.len() {
        let lower = history[reference].bounds().0;
        let upper = history[index].bounds().1;
        if lower - upper <= 1e-9 {
            continue;
        }
        let percent = (lower + upper) / 2.0;
        let Some(before) = (reference..index)
            .rev()
            .find(|&i| history[i].bounds().0 > percent)
        else {
            continue;
        };
        let Some(after) = (before + 1..=index).find(|&i| history[i].bounds().1 < percent) else {
            continue;
        };
        changes.push(Crossing {
            percent,
            video: Interval {
                lower_seconds: frames[before].media.lower_seconds,
                upper_seconds: frames[after].media.upper_seconds,
            },
        });
        reference = index;
    }
    changes
}

fn buckets(points: &[HealthPoint]) -> Vec<Bucket> {
    let mut result: Vec<Bucket> = Vec::new();
    for point in points {
        let percent = 100.0 * point.hit_points as f64 / point.max_hit_points as f64;
        if let Some(last) = result
            .last_mut()
            .filter(|last| last.time == point.elapsed_seconds)
        {
            last.lower = last.lower.min(percent);
            last.upper = last.upper.max(percent);
            last.consistent_pool &= last.max_hp == point.max_hit_points;
        } else {
            result.push(Bucket {
                time: point.elapsed_seconds,
                lower: percent,
                upper: percent,
                max_hp: point.max_hit_points,
                consistent_pool: true,
            });
        }
    }
    result
}

fn segments(points: &[Bucket]) -> Vec<&[Bucket]> {
    let mut result = Vec::new();
    let mut start = 0;
    for index in 0..points.len() {
        let current = points[index];
        let split = !current.consistent_pool
            || (index > start && {
                let previous = points[index - 1];
                current.time - previous.time > MAX_TRACE_GAP
                    || current.max_hp != previous.max_hp
                    || current.lower > previous.lower
                    || current.upper > previous.upper
            });
        if split {
            if index - start >= 2 {
                result.push(&points[start..index]);
            }
            start = index + usize::from(!current.consistent_pool);
        }
    }
    if points.len() - start >= 2 {
        result.push(&points[start..]);
    }
    result
}

fn match_segment(
    points: &[Bucket],
    changes: &[Crossing],
    work: &mut usize,
) -> Result<Option<(Interval, Interval)>, Rejection> {
    let mut start = Interval {
        lower_seconds: -MAX_SECONDS,
        upper_seconds: MAX_SECONDS,
    };
    let mut trace_start = f64::INFINITY;
    let mut trace_end = f64::NEG_INFINITY;
    for change in changes {
        let mut above = None;
        let mut bracket = None;
        for point in points {
            spend(work, 1)?;
            if point.lower > change.percent {
                above = Some(point.time);
            } else if point.upper < change.percent {
                if let Some(time) = above {
                    bracket = Some(Interval {
                        lower_seconds: time,
                        upper_seconds: point.time,
                    });
                }
                break;
            }
        }
        let Some(bracket) = bracket else {
            return Ok(None);
        };
        let estimate = Interval {
            lower_seconds: change.video.lower_seconds - bracket.upper_seconds,
            upper_seconds: change.video.upper_seconds - bracket.lower_seconds,
        };
        let Some(next) = start.intersect(estimate) else {
            return Ok(None);
        };
        start = next;
        trace_start = trace_start.min(bracket.lower_seconds);
        trace_end = trace_end.max(bracket.upper_seconds);
    }
    Ok(Some((
        start,
        Interval {
            lower_seconds: trace_start,
            upper_seconds: trace_end,
        },
    )))
}

fn supports_readings(
    points: &[Bucket],
    history: &[Candidate],
    work: &mut usize,
) -> Result<bool, Rejection> {
    // Threshold crossing alone must not invent intermediate health values in a
    // logged jump. Every observed range also needs ordered recorded HP support.
    // This is value/sequence support, not a zero-latency per-frame timestamp test.
    let mut index = 0;
    for reading in history {
        let (lower, upper) = reading.bounds();
        loop {
            spend(work, 1)?;
            let Some(point) = points.get(index) else {
                return Ok(false);
            };
            if point.upper >= lower && point.lower <= upper {
                break;
            }
            index += 1;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            scope: [3; 32],
            capture_generation: 5,
            viewport_generation: 2,
            segment_id: 8,
        }
    }

    fn candidate(percent: f64) -> Candidate {
        Candidate {
            region_id: 1,
            boss_name_id: 7,
            region: Rect {
                x: 0.2,
                y: 0.3,
                width: 0.1,
                height: 0.02,
            },
            percent,
            decimal_places: 1,
        }
    }

    fn frames(offset: f64) -> Vec<Frame> {
        [2.0, 3.0, 4.0, 5.0, 6.0]
            .into_iter()
            .enumerate()
            .map(|(index, time)| Frame {
                context: context(),
                sequence: index as u64,
                media: Interval {
                    lower_seconds: offset + time,
                    upper_seconds: offset + time + 0.04,
                },
                fresh: true,
                playing: true,
                buffering: false,
                candidates: vec![candidate(100.0 - time)],
            })
            .collect()
    }

    fn trace() -> Trace {
        Trace {
            context: context(),
            actor_id: 11,
            instance_id: None,
            boss_name_id: 7,
            points: (0..=100)
                .map(|i| HealthPoint {
                    elapsed_seconds: i as f64 / 10.0,
                    hit_points: 10_000 - i * 10,
                    max_hit_points: 10_000,
                })
                .collect(),
        }
    }

    fn estimate(frames: &[Frame], traces: &[Trace]) -> Result<Assessment, Rejection> {
        match_health(context(), frames, traces, 20.0)
    }

    #[test]
    fn varying_sequence_returns_observed_estimate_and_sampling_evidence() {
        let result = estimate(&frames(200.0), &[trace()]).unwrap();
        let matched = result.unique_mapping().unwrap();
        assert_eq!(matched.evidence, Evidence::EstimatedVisualHealth);
        assert!(matched.start_interval.lower_seconds <= 200.0);
        assert!(matched.start_interval.upper_seconds >= 200.0);
        assert!(matched.start_interval.width_seconds() > 0.0);
        assert_eq!(matched.observed_frames.len(), 5);
        assert_eq!(matched.crossing_count, 4);
        assert!(matched.max_capture_bracket_seconds >= 0.039);
        assert_eq!(matched.max_percent_quantization_unit, 0.1);
    }

    #[test]
    fn actor_instance_region_and_reading_alternatives_remain_ambiguous() {
        let first = trace();
        for second in [
            Trace {
                actor_id: 12,
                ..first.clone()
            },
            Trace {
                instance_id: Some(1),
                ..first.clone()
            },
        ] {
            let result = estimate(&frames(200.0), &[first.clone(), second]).unwrap();
            assert_eq!(result.mappings.len(), 2);
            assert!(result.unique_mapping().is_none());
        }
        let mut samples = frames(200.0);
        for sample in &mut samples {
            sample.candidates.push(Candidate {
                region_id: 2,
                ..sample.candidates[0]
            });
        }
        assert!(estimate(&samples, &[trace()])
            .unwrap()
            .unique_mapping()
            .is_none());
        let mut samples = frames(200.0);
        samples[2].candidates.push(candidate(96.9));
        assert!(estimate(&samples, &[trace()])
            .unwrap()
            .unique_mapping()
            .is_none());
    }

    #[test]
    fn wrong_candidate_can_fail_but_no_frame_is_dropped() {
        let mut samples = frames(200.0);
        samples[2].candidates.push(candidate(80.0));
        assert!(estimate(&samples, &[trace()])
            .unwrap()
            .unique_mapping()
            .is_some());
        samples[2].candidates = vec![candidate(80.0)];
        assert!(estimate(&samples, &[trace()]).is_err());
        samples[2].candidates.clear();
        assert!(estimate(&samples, &[trace()]).is_err());
    }

    #[test]
    fn plateau_and_too_little_coverage_reject() {
        let mut samples = frames(200.0);
        for sample in &mut samples {
            sample.candidates = vec![candidate(98.0)];
        }
        assert_eq!(
            estimate(&samples, &[trace()]),
            Err(Rejection::InsufficientVariation)
        );
        assert_eq!(
            estimate(&samples[..2], &[trace()]),
            Err(Rejection::TooFewFrames)
        );
        for (i, sample) in samples.iter_mut().enumerate() {
            sample.media = Interval {
                lower_seconds: 200.0 + i as f64 * 0.1,
                upper_seconds: 200.01 + i as f64 * 0.1,
            };
        }
        assert_eq!(
            estimate(&samples, &[trace()]),
            Err(Rejection::InsufficientCoverage)
        );
    }

    #[test]
    fn stale_context_layout_and_playback_changes_reject() {
        let mut samples = frames(200.0);
        samples[2].context.capture_generation += 1;
        assert_eq!(
            estimate(&samples, &[trace()]),
            Err(Rejection::ContextChanged)
        );
        samples = frames(200.0);
        samples[2].sequence = samples[1].sequence;
        assert_eq!(estimate(&samples, &[trace()]), Err(Rejection::StaleCapture));
        samples = frames(200.0);
        samples[2].buffering = true;
        assert_eq!(estimate(&samples, &[trace()]), Err(Rejection::StaleCapture));
        samples = frames(200.0);
        samples[2].candidates[0].region.x += 0.3;
        assert!(estimate(&samples, &[trace()]).is_err());
    }

    #[test]
    fn equal_timestamp_hp_alternatives_are_order_independent() {
        let mut first = trace();
        first.points.insert(
            36,
            HealthPoint {
                elapsed_seconds: 3.5,
                hit_points: 9640,
                max_hit_points: 10_000,
            },
        );
        let mut second = first.clone();
        second.points.swap(35, 36);
        assert_eq!(
            estimate(&frames(200.0), &[first]),
            estimate(&frames(200.0), &[second])
        );
    }

    #[test]
    fn trace_gaps_healing_and_hp_pool_changes_cannot_be_crossed() {
        let mut missing = trace();
        missing
            .points
            .retain(|p| p.elapsed_seconds < 3.4 || p.elapsed_seconds > 4.0);
        let mut reset = trace();
        for p in &mut reset.points[40..] {
            p.hit_points += 200;
        }
        let mut pool = trace();
        for p in &mut pool.points[40..] {
            p.hit_points *= 2;
            p.max_hit_points *= 2;
        }
        for value in [missing, reset, pool] {
            assert_eq!(
                estimate(&frames(200.0), &[value]),
                Err(Rejection::NoConsistentTrace)
            );
        }
    }

    #[test]
    fn independent_segments_accept_an_archive_shift_but_combined_frames_reject() {
        let mut long_trace = trace();
        long_trace.points = (0..=600)
            .map(|i| HealthPoint {
                elapsed_seconds: i as f64 / 10.0,
                hit_points: 10_000 - i * 10,
                max_hit_points: 10_000,
            })
            .collect();
        let early = match_health(context(), &frames(200.0), &[long_trace.clone()], 60.0).unwrap();
        let mut late_frames = frames(200.0);
        for frame in &mut late_frames {
            // Log elapsed advances48s while the shortened recording does not.
            frame.candidates[0].percent -= 48.0;
        }
        let late = match_health(context(), &late_frames, &[long_trace.clone()], 60.0).unwrap();
        assert!(
            (early.unique_mapping().unwrap().start_interval.lower_seconds
                - late.unique_mapping().unwrap().start_interval.lower_seconds
                - 48.0)
                .abs()
                < 1e-9
        );
        let mut mixed = frames(200.0);
        for sample in &mut mixed[3..] {
            sample.candidates[0].percent -= 48.0;
        }
        // Captures stay dense and fresh: rejection must come from incompatible
        // observed health/log mappings, not merely a frame-gap input limit.
        assert_eq!(
            match_health(context(), &mixed, &[long_trace], 60.0),
            Err(Rejection::NoConsistentTrace)
        );
    }

    #[test]
    fn crossing_thresholds_do_not_invent_unrecorded_intermediate_hp() {
        let mut value = trace();
        for point in &mut value.points {
            point.hit_points = if point.elapsed_seconds < 4.0 {
                10_000
            } else {
                0
            };
        }
        assert_eq!(
            estimate(&frames(200.0), &[value]),
            Err(Rejection::NoConsistentTrace)
        );
    }

    #[test]
    fn overlapping_preprocessing_copies_are_not_independent_reading_histories() {
        let mut samples = frames(200.0);
        for frame in &mut samples {
            let mut repeated = frame.candidates[0];
            repeated.region.x += 0.0001;
            frame.candidates.push(repeated);
        }
        assert!(estimate(&samples, &[trace()])
            .unwrap()
            .unique_mapping()
            .is_some());
    }

    #[test]
    fn touching_quantization_bounds_do_not_manufacture_crossings() {
        let mut samples = frames(200.0);
        for (frame, percent) in samples.iter_mut().zip([98.0, 97.8, 97.6, 97.4, 97.4]) {
            frame.candidates = vec![candidate(percent)];
        }
        assert_eq!(
            estimate(&samples, &[trace()]),
            Err(Rejection::InsufficientVariation)
        );
    }

    #[test]
    fn malformed_and_oversized_inputs_fail_closed() {
        let mut samples = frames(200.0);
        samples[2].media.upper_seconds = f64::NAN;
        assert_eq!(estimate(&samples, &[trace()]), Err(Rejection::InvalidInput));
        let mut samples = frames(200.0);
        samples[0].candidates = vec![candidate(98.0); MAX_CANDIDATES + 1];
        assert_eq!(
            estimate(&samples, &[trace()]),
            Err(Rejection::InputTooLarge)
        );
        let mut invalid = trace();
        invalid.points[2].max_hit_points = 0;
        assert_eq!(
            estimate(&frames(200.0), &[invalid]),
            Err(Rejection::InvalidInput)
        );
        let mut samples = frames(200.0);
        samples[1].candidates[0].region.width = f64::MIN_POSITIVE;
        assert_eq!(estimate(&samples, &[trace()]), Err(Rejection::InvalidInput));
        let mut huge = trace();
        huge.points = vec![huge.points[0]; MAX_POINTS + 1];
        assert_eq!(
            estimate(&frames(200.0), &[huge]),
            Err(Rejection::InputTooLarge)
        );
    }
}
