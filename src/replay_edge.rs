//! The same measured five-second edge is used by passive playback and the worker.
use crate::replay_digits::{Marker, Reading};
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alignment {
    pub unix_seconds: i64,
    pub video_seconds: f64,
    pub uncertainty_seconds: f64,
}

#[derive(Clone, Copy)]
pub struct Sample {
    pub reading: Reading,
    pub before: f64,
    pub after: f64,
}
#[derive(Clone, Copy, Debug)]
enum Stage {
    Locate,
    Tracking,
}
pub struct Edge {
    pub marker: Option<Marker>,
    stage: Stage,
    pub first_seen: Option<f64>,
    last_absent: Option<Sample>,
    onset: Option<Alignment>,
    present_count: u8,
    last_seen: Option<Sample>,
    absent_since: Option<f64>,
    absent_count: u8,
}
impl Default for Edge {
    fn default() -> Self {
        Self {
            marker: None,
            stage: Stage::Locate,
            first_seen: None,
            last_absent: None,
            onset: None,
            present_count: 0,
            last_seen: None,
            absent_since: None,
            absent_count: 0,
        }
    }
}
impl Edge {
    pub fn observe(&mut self, sample: Sample) -> Result<Option<Alignment>, ()> {
        match sample.reading {
            Reading::Present(marker) => {
                if self
                    .marker
                    .is_some_and(|old| old.unix_seconds != marker.unix_seconds)
                {
                    return Err(());
                }
                self.marker = Some(marker);
                if matches!(self.stage, Stage::Locate) {
                    if let Some(absent) = self.last_absent {
                        let width = sample.after - absent.before;
                        if sample.before > absent.after && (0.0..=0.35).contains(&width) {
                            self.onset = Some(Alignment {
                                unix_seconds: marker.unix_seconds,
                                video_seconds: (absent.before + sample.after) / 2.0,
                                uncertainty_seconds: width / 2.0 + 0.05,
                            });
                        }
                    }
                }
                self.stage = Stage::Tracking;
                self.first_seen.get_or_insert(sample.before);
                self.last_seen = Some(sample);
                self.absent_since = None;
                self.absent_count = 0;
                self.present_count = self.present_count.saturating_add(1);
            }
            Reading::Uncertain => {
                // An ambiguous frame is not negative evidence. Require a fresh
                // visible sample before accepting a subsequent disappearance.
                self.absent_since = None;
                self.absent_count = 0;
                self.last_absent = None;
                self.present_count = 0;
                if matches!(self.stage, Stage::Tracking) {
                    self.last_seen = None;
                }
            }
            Reading::Absent => {
                if matches!(self.stage, Stage::Locate) {
                    self.last_absent = Some(sample);
                }
                self.present_count = 0;
                if let Some(last) = self.last_seen {
                    if sample.before <= last.after {
                        return Err(());
                    }
                    let first = *self.absent_since.get_or_insert(sample.after);
                    self.absent_count = self.absent_count.saturating_add(1);
                    if self.absent_count >= 3 && sample.before - first >= 0.15 {
                        let width = first - last.before;
                        let observed = last.after - self.first_seen.ok_or(())?;
                        if !(0.0..=0.35).contains(&width) || observed < 0.25 {
                            return Err(());
                        }
                        // ART holds a static timestamp for exactly five seconds.
                        // Use its verified disappearance so provider chrome at
                        // the initial seek does not hide the onset. Keep a real
                        // media-time interval; never invent millisecond accuracy.
                        let video_seconds = (last.before + first) / 2.0 - 5.0;
                        if video_seconds < 0.0 {
                            return Err(());
                        }
                        if let Some(onset) = self.onset {
                            if (video_seconds - onset.video_seconds).abs() <= 0.35 {
                                return Ok(Some(onset));
                            }
                        }
                        return Ok(Some(Alignment {
                            unix_seconds: self.marker.ok_or(())?.unix_seconds,
                            video_seconds,
                            uncertainty_seconds: width / 2.0 + 0.05,
                        }));
                    }
                }
            }
        }
        if self
            .first_seen
            .is_some_and(|first| sample.after - first > 6.0)
        {
            return Err(());
        }
        Ok(None)
    }
}
