use crate::stream_player::{PlaybackCommand, PlaybackState};
use std::time::{Duration, Instant};

pub const PLAYER_COUNT: usize = 2;
const MAX_VIDEO_SECONDS: f64 = 604_800.0;
const SETTLED_TOLERANCE_MS: i64 = 500;
// Applied after accounting for the SDK observation windows, so uneven poll
// timing is not mistaken for media drift. Explicit seeks retain milliseconds.
const DRIFT_TOLERANCE_MS: i64 = 1_000;
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);
const RECOVERED_PROGRESS_MS: i64 = 2_000;

/// A recording's independently established relationship to the WCL timeline.
/// The controller does not discover or improve the accuracy of this mapping.
#[derive(Clone, Copy, Debug)]
pub struct RecordingClock {
    reference_ms: i64,
    reference_seconds: f64,
    available_seconds: f64,
}

impl RecordingClock {
    pub fn new(
        reference_ms: i64,
        reference_seconds: f64,
        available_seconds: f64,
    ) -> Result<Self, Error> {
        if !(0..=32_503_680_000_000).contains(&reference_ms)
            || !reference_seconds.is_finite()
            || !(-MAX_VIDEO_SECONDS..=MAX_VIDEO_SECONDS).contains(&reference_seconds)
            || !available_seconds.is_finite()
            || !(0.0..=MAX_VIDEO_SECONDS).contains(&available_seconds)
            || available_seconds == 0.0
        {
            return Err(Error::InvalidClock);
        }
        Ok(Self {
            reference_ms,
            reference_seconds,
            available_seconds,
        })
    }

    pub fn video_seconds(self, at_ms: i64) -> Option<f64> {
        let seconds =
            self.reference_seconds + at_ms.checked_sub(self.reference_ms)? as f64 / 1000.0;
        (seconds.is_finite() && seconds >= 0.0 && seconds < self.available_seconds)
            .then_some(seconds)
    }

    pub fn encounter_ms(self, seconds: f64) -> Option<i64> {
        if !seconds.is_finite() || !(0.0..=self.available_seconds).contains(&seconds) {
            return None;
        }
        self.reference_ms
            .checked_add(((seconds - self.reference_seconds) * 1000.0).round() as i64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidClock,
    InvalidRange,
    Unavailable,
    TimedOut,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidClock => "The recording timing is unavailable.",
            Self::InvalidRange => "The selected encounter time is unavailable.",
            Self::Unavailable => "Both POVs must contain the selected moment.",
            Self::TimedOut => "The videos could not finish synchronizing. Try again.",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Preparing,
    Buffering,
    Playing,
    Paused,
    Failed(Error),
}

#[derive(Default)]
pub struct Commands {
    pub primary: Option<PlaybackCommand>,
    pub secondary: Option<PlaybackCommand>,
}

impl Commands {
    fn both(command: PlaybackCommand) -> Self {
        Self {
            primary: Some(command),
            secondary: Some(command),
        }
    }

    fn one(side: usize, command: PlaybackCommand) -> Self {
        if side == 0 {
            Self {
                primary: Some(command),
                secondary: None,
            }
        } else {
            Self {
                primary: None,
                secondary: Some(command),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Prepare,
    Resume,
    Seeking(Instant),
    Starting(Instant),
    Holding {
        issued: Option<Instant>,
    },
    CatchingUp {
        ahead: usize,
        since: Instant,
        held_at: Option<(i64, Instant)>,
    },
    Following {
        leader: usize,
        issued: Option<Instant>,
        attempts: u8,
    },
    Running,
    Paused,
    Failed {
        error: Error,
        notified: bool,
    },
}

/// Coordinates two existing native children. It owns no media, credentials,
/// workers, or timers; the caller drops its secondary child when leaving.
pub struct Controller {
    clocks: [RecordingClock; PLAYER_COUNT],
    range: [i64; 2],
    at_ms: i64,
    wants_playing: bool,
    phase: Phase,
    cause: Status,
    operation_started: Instant,
    sample_epoch: Instant,
    recovery_started: Option<Instant>,
    recovered_from_ms: Option<i64>,
    provider_leader: Option<usize>,
}

impl Controller {
    pub fn new(
        clocks: [RecordingClock; PLAYER_COUNT],
        range: [i64; 2],
        at_ms: i64,
        playing: bool,
        now: Instant,
    ) -> Result<Self, Error> {
        if range[0] < 0 || range[1] <= range[0] || range[1] - range[0] > 604_800_000 {
            return Err(Error::InvalidRange);
        }
        let mut controller = Self {
            clocks,
            range,
            at_ms,
            wants_playing: playing,
            phase: Phase::Prepare,
            cause: Status::Preparing,
            operation_started: now,
            sample_epoch: now,
            recovery_started: None,
            recovered_from_ms: None,
            provider_leader: None,
        };
        controller.seek(at_ms, playing, now)?;
        Ok(controller)
    }

    pub fn position_ms(&self) -> i64 {
        self.at_ms
    }

    pub fn wants_playing(&self) -> bool {
        self.wants_playing
    }

    pub fn status(&self) -> Status {
        match self.phase {
            Phase::Running => Status::Playing,
            Phase::Paused => Status::Paused,
            Phase::Failed { error, .. } => Status::Failed(error),
            _ => self.cause,
        }
    }

    /// Request the same WCL instant in both videos, retaining exact milliseconds.
    pub fn seek(&mut self, at_ms: i64, playing: bool, now: Instant) -> Result<(), Error> {
        if at_ms < self.range[0] || at_ms > self.range[1] {
            return Err(Error::InvalidRange);
        }
        if self
            .clocks
            .iter()
            .any(|clock| clock.video_seconds(at_ms).is_none())
        {
            return Err(Error::Unavailable);
        }
        self.provider_leader = None;
        self.at_ms = at_ms;
        self.wants_playing = playing;
        self.phase = Phase::Prepare;
        self.cause = Status::Preparing;
        self.operation_started = now;
        self.sample_epoch = now;
        self.recovery_started = None;
        self.recovered_from_ms = None;
        Ok(())
    }

    /// A provider seek already positioned one video. Synchronize only its peer;
    /// never send the controlling player back through paused preparation.
    pub fn follow_provider(
        &mut self,
        side: usize,
        at_ms: i64,
        playing: bool,
        now: Instant,
    ) -> Result<(), Error> {
        if side >= PLAYER_COUNT {
            return Err(Error::InvalidClock);
        }
        self.seek(at_ms, playing, now)?;
        self.provider_leader = Some(side);
        self.phase = Phase::Following {
            leader: side,
            issued: None,
            attempts: 0,
        };
        Ok(())
    }

    pub fn provider_leader(&self) -> Option<usize> {
        self.provider_leader
    }

    pub fn primary_seconds(&self) -> Option<f64> {
        self.clocks[0].video_seconds(self.at_ms)
    }

    /// Replace one selected POV after its metadata is ready. Caller must supply
    /// this before exposing samples from its newly navigated native child.
    pub fn replace_clock(
        &mut self,
        side: usize,
        clock: RecordingClock,
        now: Instant,
    ) -> Result<(), Error> {
        let slot = self.clocks.get_mut(side).ok_or(Error::InvalidClock)?;
        *slot = clock;
        match self.seek(self.at_ms, self.wants_playing, now) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.phase = Phase::Failed {
                    error,
                    notified: false,
                };
                Err(error)
            }
        }
    }

    /// Native Play/Pause intent wins over any automatic buffering recovery.
    pub fn set_playing(&mut self, playing: bool, now: Instant) {
        let had_provider_leader = self.provider_leader.take().is_some();
        if had_provider_leader && matches!(self.phase, Phase::Following { .. }) {
            self.phase = Phase::Prepare;
            self.operation_started = now;
            self.sample_epoch = now;
        }
        self.recovery_started = None;
        self.recovered_from_ms = None;
        let resume_pending_seek =
            playing && !self.wants_playing && matches!(self.phase, Phase::Seeking(_));
        self.wants_playing = playing;
        if !playing && had_provider_leader && matches!(self.phase, Phase::Failed { .. }) {
            // Peer timeout deliberately leaves its controlling VOD running.
            // An explicit Brick Pause must still stop both once, without seeking.
            self.hold(now, Status::Preparing);
        } else if playing {
            if matches!(self.phase, Phase::Paused) || resume_pending_seek {
                // A user's Pause then Play supersedes an unfinished seek.
                // Reissue its target once; old acknowledgments cannot release
                // the new barrier, and repeated Play must not restart it again.
                self.phase = if resume_pending_seek {
                    Phase::Prepare
                } else {
                    Phase::Resume
                };
                self.operation_started = now;
                if resume_pending_seek {
                    self.sample_epoch = now;
                }
                self.cause = Status::Preparing;
            }
        } else if matches!(self.phase, Phase::Resume) {
            self.phase = Phase::Paused;
        } else if matches!(
            self.phase,
            Phase::Running | Phase::Starting(_) | Phase::CatchingUp { .. }
        ) {
            self.hold(now, Status::Preparing);
        }
    }

    fn recovery_deadline(&mut self, now: Instant) -> Instant {
        self.recovered_from_ms = None;
        *self.recovery_started.get_or_insert(now)
    }

    fn hold(&mut self, now: Instant, cause: Status) {
        self.phase = Phase::Holding { issued: None };
        self.operation_started = if cause == Status::Buffering {
            self.recovery_deadline(now)
        } else {
            now
        };
        self.sample_epoch = now;
        self.cause = cause;
    }

    fn settled(state: &PlaybackState, since: Instant, playing: bool) -> bool {
        state.ready
            && state.is_fresh_after(since)
            && state.seconds.is_finite()
            && !state.blocked
            && !state.buffering
            && state.seeking.is_none()
            && state.playback_intent.is_none()
            && state.playing == playing
    }

    fn position(&self, side: usize, state: &PlaybackState) -> Option<i64> {
        (state.ready && state.is_fresh_after(self.sample_epoch) && state.seeking.is_none())
            .then(|| self.clocks[side].encounter_ms(state.seconds))
            .flatten()
            .map(|at| at.clamp(self.range[0], self.range[1]))
    }

    fn aligned_paused_position(
        &self,
        states: [&PlaybackState; PLAYER_COUNT],
        since: Instant,
    ) -> Option<i64> {
        if !states
            .iter()
            .all(|state| Self::settled(state, since, false))
        {
            return None;
        }
        let [Some(primary), Some(secondary)] =
            std::array::from_fn(|side| self.clocks[side].encounter_ms(states[side].seconds))
        else {
            return None;
        };
        ((self.range[0]..=self.range[1]).contains(&primary)
            && (self.range[0]..=self.range[1]).contains(&secondary)
            && (primary - secondary).abs() <= SETTLED_TOLERANCE_MS)
            .then_some(primary)
    }

    fn playback_drift(&self, states: [&PlaybackState; PLAYER_COUNT]) -> Option<[f64; 2]> {
        if !states
            .iter()
            .all(|state| Self::settled(state, self.sample_epoch, true))
        {
            return None;
        }
        let [Some(primary), Some(secondary)] =
            std::array::from_fn(|side| self.clocks[side].encounter_ms(states[side].seconds))
        else {
            return None;
        };
        let [Some([first_start, first_end]), Some([second_start, second_end])] =
            states.map(PlaybackState::observation_window)
        else {
            return None;
        };
        let milliseconds = |first: Instant, second: Instant| {
            if first >= second {
                first.duration_since(second).as_secs_f64() * 1000.0
            } else {
                -second.duration_since(first).as_secs_f64() * 1000.0
            }
        };
        // At ordinary playback speed, samples of two aligned videos differ by
        // the time between those reads. Their precise read instants lie inside
        // the request/callback brackets. Correct only a gap that cannot be
        // explained by any permitted observation skew; do not extrapolate the
        // displayed cursor or weaken the post-command acknowledgment barrier.
        let gap = (primary - secondary) as f64;
        let minimum = gap - milliseconds(first_end, second_start);
        let maximum = gap - milliseconds(first_start, second_end);
        Some([minimum, maximum])
    }

    fn provider_play(
        &mut self,
        states: [&PlaybackState; PLAYER_COUNT],
        since: Instant,
        now: Instant,
    ) -> Option<Commands> {
        let observed = states.iter().enumerate().find_map(|(side, state)| {
            let provider_gesture = state.play_intent
                && state.ready
                && state.playing
                && !state.blocked
                && !state.buffering
                && state.is_fresh_after(since)
                && state.playback_intent != Some(true);
            (Self::settled(state, since, true) || provider_gesture)
                .then(|| self.clocks[side].encounter_ms(state.seconds))
                .flatten()
                .map(|at| at.clamp(self.range[0], self.range[1]))
        })?;
        // Play during an unfinished seek changes playback intent, not the
        // moment the user selected. SDK clocks can still describe the old
        // position, or have advanced before our first useful observation.
        let position = if matches!(self.phase, Phase::Seeking(_)) {
            self.at_ms
        } else {
            observed
        };
        if let Err(error) = self.seek(position, true, now) {
            self.phase = Phase::Failed {
                error,
                notified: false,
            };
        }
        Some(self.tick(states, now))
    }

    /// Call after polling both native players. Apply each returned command once.
    /// State snapshots from before a seek/POV change cannot release the barrier.
    pub fn tick(&mut self, states: [&PlaybackState; PLAYER_COUNT], now: Instant) -> Commands {
        if matches!(self.phase, Phase::Failed { .. }) {
            if let Some(leader) = self.provider_leader {
                let state = states[leader];
                if Self::settled(state, self.sample_epoch, state.playing) {
                    if let Some(position) = self.clocks[leader].encounter_ms(state.seconds) {
                        // A failed peer stays held. Its controlling VOD and the
                        // timeline keep following real samples without retries.
                        self.at_ms = position.clamp(self.range[0], self.range[1]);
                        self.wants_playing = state.playing;
                    }
                }
            }
        }
        let user_pause = states.iter().enumerate().find_map(|(side, state)| {
            let can_pause = match self.phase {
                Phase::Running | Phase::Starting(_) => true,
                Phase::CatchingUp { ahead, .. } => side != ahead,
                Phase::Following { leader, .. } => side != leader,
                _ => false,
            };
            (can_pause
                && self.wants_playing
                && state.ready
                && !state.blocked
                && state.pause_intent
                && !state.playing
                && state.is_fresh_after(self.sample_epoch)
                && state.seeking.is_none()
                && state.playback_intent.is_none())
            .then_some(side)
        });
        if let Some(side) = user_pause {
            self.provider_leader = None;
            // A provider's Pause event can precede its cached paused state.
            // Honor the intent now, then wait for real settlement as usual.
            self.at_ms = self.position(side, states[side]).unwrap_or(self.at_ms);
            self.wants_playing = false;
            self.hold(now, Status::Preparing);
        }
        if !matches!(
            self.phase,
            Phase::Running | Phase::Paused | Phase::Failed { .. }
        ) && now.saturating_duration_since(self.operation_started) >= PREPARATION_TIMEOUT
        {
            self.phase = Phase::Failed {
                error: Error::TimedOut,
                notified: false,
            };
        }
        match self.phase {
            Phase::Failed { error, notified } => {
                self.phase = Phase::Failed {
                    error,
                    notified: true,
                };
                if notified {
                    Commands::default()
                } else if let Some(leader) = self.provider_leader {
                    Commands::one(1 - leader, PlaybackCommand::Pause)
                } else {
                    Commands::both(PlaybackCommand::Pause)
                }
            }
            Phase::Following {
                leader,
                issued,
                attempts,
            } => {
                let follower = 1 - leader;
                if Self::settled(states[leader], self.sample_epoch, states[leader].playing) {
                    if let Some(position) = self.clocks[leader].encounter_ms(states[leader].seconds)
                    {
                        self.at_ms = position.clamp(self.range[0], self.range[1]);
                    }
                    if self.wants_playing != states[leader].playing {
                        self.wants_playing = states[leader].playing;
                        self.phase = Phase::Following {
                            leader,
                            issued: None,
                            attempts: 0,
                        };
                        return self.tick(states, now);
                    }
                }
                let Some(since) = issued else {
                    let Some(seconds) = self.clocks[follower].video_seconds(self.at_ms) else {
                        self.phase = Phase::Failed {
                            error: Error::Unavailable,
                            notified: true,
                        };
                        return Commands::one(follower, PlaybackCommand::Pause);
                    };
                    self.phase = Phase::Following {
                        leader,
                        issued: Some(now),
                        attempts: attempts + 1,
                    };
                    return Commands::one(
                        follower,
                        if self.wants_playing {
                            PlaybackCommand::Seek(seconds)
                        } else {
                            PlaybackCommand::SeekPaused(seconds)
                        },
                    );
                };
                if !states
                    .iter()
                    .all(|state| Self::settled(state, since, self.wants_playing))
                {
                    return Commands::default();
                }
                let aligned = if self.wants_playing {
                    self.playback_drift(states).is_some_and(|[min, max]| {
                        min <= DRIFT_TOLERANCE_MS as f64 && max >= -(DRIFT_TOLERANCE_MS as f64)
                    })
                } else {
                    self.aligned_paused_position(states, since).is_some()
                };
                if aligned {
                    self.phase = if self.wants_playing {
                        Phase::Running
                    } else {
                        Phase::Paused
                    };
                    self.provider_leader = None;
                    self.recovery_started = None;
                } else if attempts < 3 {
                    // A playing leader can advance while its peer buffers. At
                    // most two corrections follow that live observed position.
                    self.phase = Phase::Following {
                        leader,
                        issued: None,
                        attempts,
                    };
                    return self.tick(states, now);
                }
                Commands::default()
            }
            Phase::Prepare => {
                let [Some(primary), Some(secondary)] =
                    self.clocks.map(|clock| clock.video_seconds(self.at_ms))
                else {
                    self.phase = Phase::Failed {
                        error: Error::Unavailable,
                        notified: true,
                    };
                    return Commands::both(PlaybackCommand::Pause);
                };
                self.phase = Phase::Seeking(now);
                self.sample_epoch = now;
                Commands {
                    primary: Some(PlaybackCommand::SeekPaused(primary)),
                    secondary: Some(PlaybackCommand::SeekPaused(secondary)),
                }
            }
            Phase::Resume => {
                if let Some(at_ms) = self.aligned_paused_position(states, self.sample_epoch) {
                    self.at_ms = at_ms;
                    self.phase = Phase::Starting(now);
                    self.sample_epoch = now;
                    Commands::both(PlaybackCommand::Play)
                } else {
                    self.phase = Phase::Prepare;
                    self.tick(states, now)
                }
            }
            Phase::Seeking(since) => {
                if !self.wants_playing {
                    if let Some(commands) = self.provider_play(states, since, now) {
                        return commands;
                    }
                }
                if self.wants_playing
                    && states.iter().any(|state| state.playing)
                    && states.iter().enumerate().all(|(side, state)| {
                        state.ready
                            && state.is_fresh_after(since)
                            && !state.blocked
                            && !state.buffering
                            && state.playback_intent != Some(true)
                            && state.seeking.is_none_or(|target| {
                                self.clocks[side].encounter_ms(target) == Some(self.at_ms)
                            })
                            && self.clocks[side]
                                .encounter_ms(state.seconds)
                                .is_some_and(|at| (at - self.at_ms).abs() <= SETTLED_TOLERANCE_MS)
                    })
                {
                    // A provider Play can supersede our paused seek before
                    // its Pause is sampled. Both SDK clocks already reached
                    // the target, so send the desired Play now. These commands
                    // replace the old intent; fresh acknowledgments are still
                    // required by Starting and by each native player.
                    self.phase = Phase::Starting(now);
                    self.sample_epoch = now;
                    return Commands::both(PlaybackCommand::Play);
                }
                if self.wants_playing && states.iter().any(|state| state.play_intent) {
                    // The first useful provider sample can arrive after it
                    // has advanced beyond the shared target. Its explicit
                    // newer Play still wins: re-arm the selected moment once
                    // instead of waiting for an obsolete Pause retry.
                    if let Some(commands) = self.provider_play(states, since, now) {
                        return commands;
                    }
                }
                let ready = states.iter().enumerate().all(|(side, state)| {
                    Self::settled(state, since, false)
                        && self.clocks[side]
                            .encounter_ms(state.seconds)
                            .is_some_and(|at| (at - self.at_ms).abs() <= SETTLED_TOLERANCE_MS)
                });
                if !ready {
                    return Commands::default();
                }
                if self.wants_playing {
                    self.phase = Phase::Starting(now);
                    self.sample_epoch = now;
                    Commands::both(PlaybackCommand::Play)
                } else {
                    self.phase = Phase::Paused;
                    Commands::default()
                }
            }
            Phase::Starting(since) => {
                // Buffering immediately after Play is part of starting media.
                // Pausing here and seeking both again restarts that same work,
                // so a provider that buffers on every start can never finish.
                // Keep the original command barrier and bounded deadline until
                // both providers report actual playback. An explicit Pause
                // still moves us to Holding through set_playing.
                if states.iter().all(|state| Self::settled(state, since, true)) {
                    self.phase = Phase::Running;
                }
                Commands::default()
            }
            Phase::Holding { issued } => {
                let Some(since) = issued else {
                    self.phase = Phase::Holding { issued: Some(now) };
                    self.sample_epoch = now;
                    return Commands::both(PlaybackCommand::Pause);
                };
                if !self.wants_playing {
                    if let Some(commands) = self.provider_play(states, since, now) {
                        return commands;
                    }
                }
                if states
                    .iter()
                    .all(|state| Self::settled(state, since, false))
                {
                    if let Some(primary) = self.aligned_paused_position(states, since) {
                        // Pausing an aligned pair already establishes the
                        // user's moment. Seeking back to an older sample
                        // adds latency and rewinds the video on every toggle.
                        self.at_ms = primary;
                        self.phase = if self.wants_playing {
                            self.sample_epoch = now;
                            Phase::Starting(now)
                        } else {
                            Phase::Paused
                        };
                        return if self.wants_playing {
                            Commands::both(PlaybackCommand::Play)
                        } else {
                            Commands::default()
                        };
                    }
                    self.phase = Phase::Prepare;
                }
                Commands::default()
            }
            Phase::CatchingUp {
                ahead,
                since,
                held_at,
            } => {
                let behind = 1 - ahead;
                // The held player's pause is ours. A pause from the other
                // provider is user intent and must stop the whole comparison.
                if Self::settled(states[behind], since, false) {
                    self.at_ms = self.position(behind, states[behind]).unwrap_or(self.at_ms);
                    self.wants_playing = false;
                    self.hold(now, Status::Preparing);
                    return self.tick(states, now);
                }
                if let Some((held_position, acknowledged)) = held_at {
                    if let Some(position) = self.position(ahead, states[ahead]) {
                        let playing = Self::settled(states[ahead], acknowledged, true);
                        let paused_seek = Self::settled(states[ahead], acknowledged, false)
                            && (position - held_position).abs() > DRIFT_TOLERANCE_MS;
                        if playing || paused_seek {
                            // After our Pause was acknowledged, a newer Play
                            // or deliberate paused seek is a new shared command.
                            // Do not resume against the obsolete held position.
                            if let Err(error) = self.seek(position, playing, now) {
                                self.phase = Phase::Failed {
                                    error,
                                    notified: false,
                                };
                            }
                            return self.tick(states, now);
                        }
                    }
                }
                if !Self::settled(states[ahead], since, false) {
                    return Commands::default();
                }
                let Some(position) = self.position(ahead, states[ahead]) else {
                    return Commands::default();
                };
                let held_at = held_at.unwrap_or((position, now));
                self.phase = Phase::CatchingUp {
                    ahead,
                    since,
                    held_at: Some(held_at),
                };
                if let Some(position) = self.position(0, states[0]) {
                    self.at_ms = position;
                }
                if Self::settled(states[behind], since, true)
                    && self
                        .position(behind, states[behind])
                        .is_some_and(|position| position >= held_at.0 - SETTLED_TOLERANCE_MS)
                {
                    // Keep the lagging decoder running. Re-seeking both here
                    // would recreate the provider's original startup delay.
                    self.phase = Phase::Starting(now);
                    self.sample_epoch = now;
                    return Commands::one(ahead, PlaybackCommand::Play);
                }
                Commands::default()
            }
            Phase::Running => {
                let positions = [self.position(0, states[0]), self.position(1, states[1])];
                // A provider's own Pause wins even while the other player is
                // buffering or its last poll is stale. Treating that pause as
                // automatic recovery would unexpectedly resume both videos.
                if let Some(paused) = states
                    .iter()
                    .position(|state| Self::settled(state, self.sample_epoch, false))
                {
                    self.at_ms = positions[paused].unwrap_or(self.at_ms);
                    self.wants_playing = false;
                    self.hold(now, Status::Preparing);
                    return self.tick(states, now);
                }
                if let Some(blocked) = states.iter().position(|state| {
                    !state.ready
                        || !state.is_fresh_after(self.sample_epoch)
                        || state.blocked
                        || state.buffering
                        || state.seeking.is_some()
                }) {
                    self.at_ms = positions[blocked].or(positions[0]).unwrap_or(self.at_ms);
                    self.hold(now, Status::Buffering);
                    return self.tick(states, now);
                }
                if let Some(position) = positions[0] {
                    self.at_ms = position;
                }
                if self.at_ms >= self.range[1] {
                    self.wants_playing = false;
                    self.hold(now, Status::Preparing);
                    return self.tick(states, now);
                }
                if let Some([minimum, maximum]) = self.playback_drift(states) {
                    let tolerance = DRIFT_TOLERANCE_MS as f64;
                    let ahead = if minimum > tolerance {
                        Some(0)
                    } else if maximum < -tolerance {
                        Some(1)
                    } else {
                        None
                    };
                    if let Some(ahead) = ahead {
                        self.phase = Phase::CatchingUp {
                            ahead,
                            since: now,
                            held_at: None,
                        };
                        self.operation_started = self.recovery_deadline(now);
                        self.sample_epoch = now;
                        self.cause = Status::Buffering;
                        return Commands::one(ahead, PlaybackCommand::Pause);
                    }
                    if self.recovery_started.is_some() {
                        if minimum >= -tolerance && maximum <= tolerance {
                            let start = self.recovered_from_ms.get_or_insert(self.at_ms);
                            if self.at_ms < *start {
                                *start = self.at_ms;
                            } else if self.at_ms - *start >= RECOVERED_PROGRESS_MS {
                                // Rearm only after actual aligned advancement,
                                // never after one transient Running sample.
                                self.recovery_started = None;
                                self.recovered_from_ms = None;
                            }
                        } else {
                            self.recovered_from_ms = None;
                        }
                    }
                }
                Commands::default()
            }
            Phase::Paused => {
                if let Some(commands) = self.provider_play(states, self.sample_epoch, now) {
                    return commands;
                }
                // A paused seek from either provider's own controls is also a
                // shared seek. Small SDK/keyframe variations are not new intent.
                if let Some(position) = states.iter().enumerate().find_map(|(side, state)| {
                    Self::settled(state, self.sample_epoch, false)
                        .then(|| self.position(side, state))
                        .flatten()
                        .filter(|at| (at - self.at_ms).abs() > DRIFT_TOLERANCE_MS)
                }) {
                    if let Err(error) = self.seek(position, false, now) {
                        self.phase = Phase::Failed {
                            error,
                            notified: false,
                        };
                    }
                    return self.tick(states, now);
                }
                Commands::default()
            }
        }
    }

    /// The host should immediately drop its optional secondary StreamPlayer.
    /// Primary playback continues when the comparison view is dismissed.
    #[cfg(test)]
    pub fn leave(self) -> Commands {
        Commands {
            primary: None,
            secondary: Some(PlaybackCommand::Pause),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static TEST_CLOCK: std::cell::Cell<Instant> =
            std::cell::Cell::new(Instant::now() - Duration::from_secs(1));
    }

    // Model event order explicitly; adjacent real clock reads may be equal.
    fn test_now() -> Instant {
        TEST_CLOCK.with(|clock| {
            let next = clock.get() + Duration::from_millis(1);
            clock.set(next);
            next
        })
    }

    const START: i64 = 1_800_000_000_000;
    fn clocks() -> [RecordingClock; 2] {
        [
            RecordingClock::new(START, 100.125, 5000.0).unwrap(),
            RecordingClock::new(START, 900.875, 5000.0).unwrap(),
        ]
    }
    fn sample(seconds: f64, playing: bool) -> PlaybackState {
        let mut state = PlaybackState::default();
        state.ready = true;
        state.seconds = seconds;
        state.playing = playing;
        state.mark_polled_at(test_now());
        state
    }
    fn controller(playing: bool) -> Controller {
        Controller::new(
            clocks(),
            [START, START + 300_000],
            START + 12_375,
            playing,
            test_now(),
        )
        .unwrap()
    }
    fn command_seconds(command: Option<PlaybackCommand>) -> f64 {
        match command {
            Some(PlaybackCommand::SeekPaused(seconds)) => seconds,
            _ => panic!("expected paused preparation"),
        }
    }
    fn prepared(controller: &mut Controller, playing: bool) {
        let states = [sample(112.5, false), sample(913.25, false)];
        let commands = controller.tick([&states[0], &states[1]], test_now());
        assert_eq!(
            matches!(commands.primary, Some(PlaybackCommand::Play)),
            playing
        );
        assert_eq!(
            matches!(commands.secondary, Some(PlaybackCommand::Play)),
            playing
        );
        if playing {
            let states = [sample(112.5, true), sample(913.25, true)];
            controller.tick([&states[0], &states[1]], test_now());
        }
    }

    fn observed(seconds: f64, playing: bool, window: [Instant; 2]) -> PlaybackState {
        let mut state = sample(seconds, playing);
        state.mark_polled_between(window[0], window[1]);
        state
    }

    fn running_before_observations(now: Instant) -> Controller {
        let epoch = now - Duration::from_millis(1900);
        let mut c = Controller::new(
            clocks(),
            [START, START + 300_000],
            START + 12_375,
            true,
            epoch,
        )
        .unwrap();
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], epoch);
        let at = epoch + Duration::from_millis(10);
        let paused = [
            observed(112.5, false, [at, at]),
            observed(913.25, false, [at, at]),
        ];
        c.tick([&paused[0], &paused[1]], at);
        let at = epoch + Duration::from_millis(20);
        let running = [
            observed(112.5, true, [at, at]),
            observed(913.25, true, [at, at]),
        ];
        c.tick([&running[0], &running[1]], at);
        assert_eq!(c.status(), Status::Playing);
        c
    }

    fn assert_one(commands: &Commands, side: usize, playing: bool) {
        let values = [commands.primary, commands.secondary];
        assert!(values[1 - side].is_none());
        assert!(if playing {
            matches!(values[side], Some(PlaybackCommand::Play))
        } else {
            matches!(values[side], Some(PlaybackCommand::Pause))
        });
    }

    #[test]
    fn provider_follow_only_seeks_peer_and_does_not_restart_settled_leader() {
        for leader in 0..2 {
            for playing in [false, true] {
                let mut c = controller(playing);
                let target = START + 72_345;
                c.follow_provider(leader, target, playing, test_now())
                    .unwrap();
                let mut states =
                    clocks().map(|clock| sample(clock.video_seconds(target).unwrap(), playing));
                let command = c.tick([&states[0], &states[1]], test_now());
                let commands = [command.primary, command.secondary];
                assert!(
                    commands[leader].is_none(),
                    "A native seek must never rewind its source"
                );
                assert!(matches!(commands[1 - leader], Some(PlaybackCommand::Seek(_))) == playing);
                assert!(
                    matches!(commands[1 - leader], Some(PlaybackCommand::SeekPaused(_))) != playing
                );
                assert_eq!(c.provider_leader(), Some(leader));
                let waiting = c.tick([&states[0], &states[1]], test_now());
                assert!(waiting.primary.is_none() && waiting.secondary.is_none());
                let current = target + if playing { 300 } else { 0 };
                states =
                    clocks().map(|clock| sample(clock.video_seconds(current).unwrap(), playing));
                let settled = c.tick([&states[0], &states[1]], test_now());
                assert!(settled.primary.is_none() && settled.secondary.is_none());
                assert_eq!(
                    c.status(),
                    if playing {
                        Status::Playing
                    } else {
                        Status::Paused
                    }
                );
                assert_eq!(c.position_ms(), current);
                let repeated = c.tick([&states[0], &states[1]], test_now());
                assert!(
                    repeated.primary.is_none() && repeated.secondary.is_none(),
                    "No new command barrier after alignment"
                );
            }
        }
    }

    #[test]
    fn provider_follow_has_three_peer_seeks_and_one_deadline_without_stopping_leader() {
        for leader in 0..2 {
            let mut c = controller(true);
            let target = START + 22_375;
            let started = test_now();
            c.follow_provider(leader, target, true, started).unwrap();
            let mut seeks = 0;
            for step in 0..12 {
                let mut states =
                    clocks().map(|clock| sample(clock.video_seconds(target).unwrap(), true));
                states[leader].seconds += 5.0 + step as f64;
                let command = c.tick([&states[0], &states[1]], test_now());
                let commands = [command.primary, command.secondary];
                assert!(commands[leader].is_none());
                seeks += usize::from(matches!(
                    commands[1 - leader],
                    Some(PlaybackCommand::Seek(_))
                ));
            }
            assert_eq!(seeks, 3);
            let empty = PlaybackState::default();
            let timeout = c.tick([&empty, &empty], started + Duration::from_secs(31));
            assert_one(&timeout, 1 - leader, false);
            assert_eq!(c.status(), Status::Failed(Error::TimedOut));
            assert!(
                c.wants_playing(),
                "Timeout may hold the peer but must not reverse user playback intent"
            );
            let repeated = c.tick([&empty, &empty], started + Duration::from_secs(32));
            assert!(repeated.primary.is_none() && repeated.secondary.is_none());
        }
    }

    #[test]
    fn pausing_the_provider_leader_changes_only_the_following_peer() {
        for leader in 0..2 {
            let mut c = controller(true);
            let target = START + 22_375;
            c.follow_provider(leader, target, true, test_now()).unwrap();
            let states = clocks().map(|clock| sample(clock.video_seconds(target).unwrap(), true));
            c.tick([&states[0], &states[1]], test_now());
            let mut paused =
                clocks().map(|clock| sample(clock.video_seconds(target + 400).unwrap(), true));
            paused[leader].playing = false;
            let commands = c.tick([&paused[0], &paused[1]], test_now());
            let commands = [commands.primary, commands.secondary];
            assert!(commands[leader].is_none());
            assert!(matches!(
                commands[1 - leader],
                Some(PlaybackCommand::SeekPaused(_))
            ));
            assert!(!c.wants_playing());
            assert_eq!(c.position_ms(), target + 400);
        }
    }

    #[test]
    fn failed_follow_keeps_live_position_and_explicit_peer_pause_remains_shared() {
        for leader in 0..2 {
            let target = START + 22_375;
            let mut failed = controller(true);
            let started = test_now();
            failed
                .follow_provider(leader, target, true, started)
                .unwrap();
            let empty = PlaybackState::default();
            failed.tick([&empty, &empty], started + Duration::from_secs(31));
            let current = target + 13_625;
            let states = clocks().map(|clock| sample(clock.video_seconds(current).unwrap(), true));
            let commands = failed.tick([&states[0], &states[1]], started + Duration::from_secs(32));
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_eq!(failed.position_ms(), current);
            assert_eq!(failed.primary_seconds(), clocks()[0].video_seconds(current));
            assert_eq!(failed.status(), Status::Failed(Error::TimedOut));

            let mut following = controller(true);
            following
                .follow_provider(leader, target, true, test_now())
                .unwrap();
            let states = clocks().map(|clock| sample(clock.video_seconds(target).unwrap(), true));
            following.tick([&states[0], &states[1]], test_now());
            let mut paused =
                clocks().map(|clock| sample(clock.video_seconds(target).unwrap(), true));
            paused[1 - leader].playing = false;
            paused[1 - leader].pause_intent = true;
            paused[1 - leader].playback_intent = Some(false);
            let waiting = following.tick([&paused[0], &paused[1]], test_now());
            assert!(
                waiting.primary.is_none() && waiting.secondary.is_none(),
                "A pending native pause remains internal"
            );
            paused[1 - leader].playback_intent = None;
            paused[1 - leader].mark_polled_at(test_now());
            let shared = following.tick([&paused[0], &paused[1]], test_now());
            assert!(matches!(shared.primary, Some(PlaybackCommand::Pause)));
            assert!(matches!(shared.secondary, Some(PlaybackCommand::Pause)));
            assert!(!following.wants_playing());
            assert_eq!(following.provider_leader(), None);
        }
    }

    #[test]
    fn a_blocked_provider_cannot_finish_comparison_preparation() {
        for side in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            let mut paused = [sample(112.5, false), sample(913.25, false)];
            paused[side].blocked = true;
            let result = c.tick([&paused[0], &paused[1]], test_now());
            assert!(result.primary.is_none() && result.secondary.is_none());
            assert_ne!(c.status(), Status::Paused);
            assert_ne!(c.status(), Status::Playing);
            assert!(c.wants_playing());
            paused[side].blocked = false;
            let result = c.tick([&paused[0], &paused[1]], test_now());
            assert!(matches!(result.primary, Some(PlaybackCommand::Play)));
            assert!(matches!(result.secondary, Some(PlaybackCommand::Play)));
        }
    }

    #[test]
    fn uneven_poll_ages_and_slow_callbacks_are_not_media_drift() {
        for reverse in [false, true] {
            for slow_callback in [false, true] {
                let now = Instant::now();
                let mut c = running_before_observations(now);
                let old = now - Duration::from_millis(1500);
                let recent = now - Duration::from_millis(100);
                let windows = if slow_callback {
                    [[old, recent], [recent, recent + Duration::from_millis(10)]]
                } else {
                    [
                        [old, old + Duration::from_millis(10)],
                        [recent, recent + Duration::from_millis(10)],
                    ]
                };
                let (first, second, windows) = if reverse {
                    (113.9, 913.25, [windows[1], windows[0]])
                } else {
                    (112.5, 914.65, windows)
                };
                let states = [
                    observed(first, true, windows[0]),
                    observed(second, true, windows[1]),
                ];
                let commands = c.tick([&states[0], &states[1]], now);
                assert!(commands.primary.is_none() && commands.secondary.is_none());
                assert_eq!(c.status(), Status::Playing);
                assert!(c.recovery_started.is_none());
            }
        }
    }

    #[test]
    fn actual_drift_outside_observation_uncertainty_is_corrected_immediately() {
        for ahead in 0..2 {
            for slow_callback in [false, true] {
                let now = Instant::now();
                let mut c = running_before_observations(now);
                let old = now - Duration::from_millis(1500);
                let recent = now - Duration::from_millis(100);
                let mut positions = [112.5, 914.65];
                positions[ahead] += if slow_callback { 4.0 } else { 2.0 };
                let states = [
                    observed(
                        positions[0],
                        true,
                        [old, if slow_callback { recent } else { old }],
                    ),
                    observed(positions[1], true, [recent, recent]),
                ];
                let commands = c.tick([&states[0], &states[1]], now);
                assert_one(&commands, ahead, false);
                assert_eq!(c.status(), Status::Buffering);
                assert!(c.wants_playing());
            }
        }
    }

    #[test]
    fn asymmetric_startup_catches_up_either_side_without_reseeking_both() {
        for ahead in 0..2 {
            let behind = 1 - ahead;
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let mut seconds = [112.5, 913.25];
            seconds[ahead] += 1.3;
            let states = seconds.map(|at| sample(at, true));
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert_one(&commands, ahead, false);
            // The pause acknowledgment is required; buffered/old data cannot
            // release the hold, and repeated polls must not repeat commands.
            let mut waiting = seconds.map(|at| sample(at, true));
            waiting[ahead].playback_intent = Some(false);
            for _ in 0..3 {
                let commands = c.tick([&waiting[0], &waiting[1]], test_now());
                assert!(commands.primary.is_none() && commands.secondary.is_none());
            }
            seconds[behind] += 0.4;
            let mut states = seconds.map(|at| sample(at, true));
            states[ahead].playing = false;
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert!(c.wants_playing(), "Our held Pause is not user intent");
            seconds[behind] += 0.6;
            let mut states = seconds.map(|at| sample(at, true));
            states[ahead].playing = false;
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert_one(&commands, ahead, true);
            let aligned = [sample(114.0, true), sample(914.75, true)];
            c.tick([&aligned[0], &aligned[1]], test_now());
            let commands = c.tick([&aligned[0], &aligned[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_eq!(c.status(), Status::Playing);
        }
    }

    #[test]
    fn pause_and_new_seek_cancel_the_automatic_catchup_plan() {
        for action in 0..3 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let states = [sample(112.5, true), sample(914.75, true)];
            c.tick([&states[0], &states[1]], test_now());
            let mut states = [sample(112.75, true), sample(914.75, false)];
            match action {
                0 => c.set_playing(false, test_now()),
                1 => states[0].playing = false,
                _ => c.seek(START + 22_375, false, test_now()).unwrap(),
            }
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(!c.wants_playing());
            if action == 2 {
                assert_eq!(command_seconds(commands.primary), 122.5);
                assert_eq!(command_seconds(commands.secondary), 923.25);
            } else {
                assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
                assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
            }
        }
    }

    #[test]
    fn playing_the_held_provider_after_pause_acknowledgment_is_a_new_shared_command() {
        for ahead in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let mut seconds = [112.5, 913.25];
            seconds[ahead] += 1.5;
            let states = seconds.map(|at| sample(at, true));
            c.tick([&states[0], &states[1]], test_now());
            let states = seconds.map(|at| sample(at, true));
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            let mut states = seconds.map(|at| sample(at, true));
            states[ahead].playing = false;
            let acknowledged = test_now();
            c.tick([&states[0], &states[1]], acknowledged);
            states[ahead].playing = true;
            states[ahead].seconds += 0.125;
            states[ahead].mark_polled_at(acknowledged);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            states[ahead].mark_polled_at(test_now());
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert_eq!(command_seconds(commands.primary), 114.125);
            assert_eq!(command_seconds(commands.secondary), 914.875);
            assert_eq!(c.position_ms(), START + 14_000);
            assert!(c.wants_playing());
            assert!(c.recovery_started.is_none());
        }
    }

    #[test]
    fn seeking_the_held_provider_uses_its_new_position_only_after_acknowledgment() {
        for ahead in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let mut seconds = [112.5, 913.25];
            seconds[ahead] += 1.5;
            let states = seconds.map(|at| sample(at, true));
            c.tick([&states[0], &states[1]], test_now());
            let mut states = seconds.map(|at| sample(at, true));
            states[ahead].playing = false;
            states[ahead].seconds += 5.125;
            states[ahead].playback_intent = Some(false);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            states[ahead] = sample(seconds[ahead], false);
            let acknowledged = test_now();
            c.tick([&states[0], &states[1]], acknowledged);
            states[ahead].seconds += 5.125;
            states[ahead].mark_polled_at(acknowledged);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            states[ahead] = sample(seconds[ahead] + 0.125, false);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            states[ahead] = sample(seconds[ahead] + 5.125, false);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert_eq!(command_seconds(commands.primary), 119.125);
            assert_eq!(command_seconds(commands.secondary), 919.875);
            assert_eq!(c.position_ms(), START + 19_000);
            assert!(!c.wants_playing());
        }
    }

    #[test]
    fn repeated_catchup_keeps_one_deadline_until_aligned_video_advances() {
        for recovers in [false, true] {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let states = [sample(112.5, true), sample(914.75, true)];
            c.tick([&states[0], &states[1]], test_now());
            let started = c.recovery_started.unwrap();
            let states = [sample(114.0, true), sample(914.75, false)];
            c.tick([&states[0], &states[1]], test_now());
            let states = [sample(114.0, true), sample(914.75, true)];
            c.tick([&states[0], &states[1]], test_now());
            c.tick([&states[0], &states[1]], test_now());
            assert_eq!(c.recovery_started, Some(started));
            if recovers {
                let states = [sample(116.125, true), sample(916.875, true)];
                c.tick([&states[0], &states[1]], test_now());
                assert!(c.recovery_started.is_none());
            } else {
                // Slow resume may require one opposite hold. It must not
                // create a fresh 30-second budget at each transient start.
                let states = [sample(115.5, true), sample(914.75, true)];
                let commands = c.tick([&states[0], &states[1]], test_now());
                assert_one(&commands, 0, false);
                assert_eq!(c.operation_started, started);
                let commands = c.tick([&states[0], &states[1]], started + PREPARATION_TIMEOUT);
                assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
                assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
                assert_eq!(c.status(), Status::Failed(Error::TimedOut));
            }
        }
    }

    #[test]
    fn distinct_video_origins_preserve_milliseconds_and_wait_for_both() {
        let mut c = controller(true);
        let stale = [sample(112.5, false), sample(913.25, false)];
        let commands = c.tick([&stale[0], &stale[1]], test_now());
        assert_eq!(command_seconds(commands.primary), 112.5);
        assert_eq!(command_seconds(commands.secondary), 913.25);
        let commands = c.tick([&stale[0], &stale[1]], test_now());
        assert!(commands.primary.is_none() && commands.secondary.is_none());
        let mut equal = [sample(112.5, false), sample(913.25, false)];
        for state in &mut equal {
            state.mark_polled_at(c.sample_epoch);
        }
        let commands = c.tick([&equal[0], &equal[1]], test_now());
        assert!(commands.primary.is_none() && commands.secondary.is_none());
        let ready = sample(112.5, false);
        assert!(c.tick([&ready, &stale[1]], test_now()).primary.is_none());
        prepared(&mut c, true);
        assert_eq!(c.status(), Status::Playing);
        assert_eq!(clocks()[1].encounter_ms(913.25), Some(START + 12_375));
    }

    #[test]
    fn paused_entry_and_late_pause_never_autoplay() {
        let mut c = controller(true);
        let state = PlaybackState::default();
        c.tick([&state, &state], test_now());
        c.set_playing(false, test_now());
        prepared(&mut c, false);
        assert_eq!(c.status(), Status::Paused);
    }

    #[test]
    fn explicit_pause_then_play_restarts_a_pending_seek_once_at_the_same_moment() {
        let mut c = controller(true);
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], test_now());
        let target = c.position_ms();
        let initial_epoch = c.sample_epoch;
        c.set_playing(true, test_now());
        let unchanged = c.tick([&empty, &empty], test_now());
        assert!(unchanged.primary.is_none() && unchanged.secondary.is_none());
        assert_eq!(c.sample_epoch, initial_epoch);

        c.set_playing(false, test_now());
        // A delayed acknowledgment must not satisfy the user's new Play.
        let old = [sample(112.5, false), sample(913.25, false)];
        c.set_playing(true, test_now());
        let commands = c.tick([&old[0], &old[1]], test_now());
        assert_eq!(command_seconds(commands.primary), 112.5);
        assert_eq!(command_seconds(commands.secondary), 913.25);
        assert_eq!(c.position_ms(), target);
        assert!(c.wants_playing());
        assert!(c.sample_epoch > initial_epoch);
        let restarted = c.sample_epoch;
        for _ in 0..3 {
            c.set_playing(true, test_now());
            let commands = c.tick([&old[0], &old[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_eq!(c.sample_epoch, restarted);
        }
        prepared(&mut c, true);
        assert_eq!(c.status(), Status::Playing);
    }

    #[test]
    fn an_aligned_pause_keeps_the_actual_moment_without_seeking_on_rapid_resume() {
        for resume in [false, true] {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            c.set_playing(false, test_now());
            let commands = c.tick([&empty, &empty], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
            // These are where the decoders really stopped, later than the
            // controller's last playing sample and within its existing tolerance.
            let paused = [sample(114.625, false), sample(915.5, false)];
            if resume {
                c.set_playing(true, test_now());
            }
            let commands = c.tick([&paused[0], &paused[1]], test_now());
            assert_eq!(c.position_ms(), START + 14_500);
            if resume {
                assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
                assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
                let playing = [sample(114.75, true), sample(915.625, true)];
                let commands = c.tick([&playing[0], &playing[1]], test_now());
                assert!(commands.primary.is_none() && commands.secondary.is_none());
                assert_eq!(c.status(), Status::Playing);
            } else {
                assert!(commands.primary.is_none() && commands.secondary.is_none());
                assert_eq!(c.status(), Status::Paused);
            }
        }
    }

    #[test]
    fn pause_fast_path_requires_both_fresh_settled_and_aligned_positions() {
        for invalid in 0..5 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            c.set_playing(false, test_now());
            c.tick([&empty, &empty], test_now());
            let mut paused = [sample(114.625, false), sample(915.375, false)];
            match invalid {
                0 => paused[1].mark_polled_at(c.sample_epoch),
                1 => paused[1].buffering = true,
                2 => paused[1].seeking = Some(915.375),
                3 => paused[1].seconds += 0.501,
                _ => {
                    paused[0].seconds = 99.0;
                    paused[1].seconds = 899.75;
                }
            }
            let commands = c.tick([&paused[0], &paused[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_ne!(c.status(), Status::Paused);
            assert_eq!(c.position_ms(), START + 12_375);
        }
    }

    #[test]
    fn resuming_a_confirmed_pause_plays_once_and_rechecks_alignment() {
        for action in 0..3 {
            let mut c = controller(false);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, false);
            let mut states = [sample(112.5, false), sample(913.25, false)];
            c.set_playing(true, test_now());
            c.set_playing(true, test_now());
            match action {
                1 => c.set_playing(false, test_now()),
                2 => states[1].seconds += 3.0,
                _ => (),
            }
            let commands = c.tick([&states[0], &states[1]], test_now());
            match action {
                0 => {
                    assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
                    assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
                    let commands = c.tick([&states[0], &states[1]], test_now());
                    assert!(commands.primary.is_none() && commands.secondary.is_none());
                    assert_ne!(
                        c.status(),
                        Status::Playing,
                        "Old paused samples cannot acknowledge Play"
                    );
                }
                1 => {
                    assert!(commands.primary.is_none() && commands.secondary.is_none());
                    assert_eq!(c.status(), Status::Paused);
                }
                _ => {
                    assert_eq!(command_seconds(commands.primary), 112.5);
                    assert_eq!(command_seconds(commands.secondary), 913.25);
                }
            }
        }
    }

    #[test]
    fn provider_pause_intent_wins_before_its_cached_state_finishes_changing() {
        for side in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let mut states = [sample(112.5, true), sample(913.25, true)];
            states[side].playing = false;
            states[side].buffering = true;
            states[side].pause_intent = true;
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
            assert!(!c.wants_playing());
            let states = [sample(112.5, false), sample(913.25, false)];
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_eq!(c.status(), Status::Paused);
        }
    }

    #[test]
    fn a_playing_provider_at_the_shared_seek_target_releases_its_ready_peer() {
        for side in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            let mut states = [sample(112.5, false), sample(913.25, false)];
            states[side].playing = true;
            states[side].seeking = Some(states[side].seconds);
            states[side].playback_intent = Some(false);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
            assert_ne!(c.status(), Status::Playing);
            assert_eq!(c.position_ms(), START + 12_375);
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_ne!(
                c.status(),
                Status::Playing,
                "Old intent is not a Play acknowledgment"
            );
            let playing = [sample(112.625, true), sample(913.375, true)];
            c.tick([&playing[0], &playing[1]], test_now());
            assert_eq!(c.status(), Status::Playing);
        }
    }

    #[test]
    fn an_explicit_provider_play_supersedes_a_pending_paused_seek() {
        for wants_playing in [false, true] {
            for side in 0..2 {
                for offset in [-56.0, 1.589] {
                    let mut c = controller(wants_playing);
                    let empty = PlaybackState::default();
                    c.tick([&empty, &empty], test_now());
                    let mut states = [sample(112.5, false), sample(913.25, false)];
                    states[side].playing = true;
                    states[side].seeking = Some(states[side].seconds);
                    states[side].seconds += offset;
                    states[side].playback_intent = Some(false);
                    states[side].play_intent = true;
                    let commands = c.tick([&states[0], &states[1]], test_now());
                    assert!(c.wants_playing());
                    assert_eq!(command_seconds(commands.primary), 112.5);
                    assert_eq!(command_seconds(commands.secondary), 913.25);
                    assert_eq!(c.position_ms(), START + 12_375);
                    assert_ne!(c.status(), Status::Playing);
                    let commands = c.tick([&states[0], &states[1]], test_now());
                    assert!(
                        commands.primary.is_none() && commands.secondary.is_none(),
                        "The same old event cannot restart alignment"
                    );
                }
            }
        }
    }

    #[test]
    fn early_seek_resume_rejects_stale_unready_or_wrong_target_samples() {
        for invalid in 0..7 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            let mut states = [sample(112.5, true), sample(913.25, false)];
            states[0].seeking = Some(112.5);
            states[0].playback_intent = Some(false);
            match invalid {
                0 => states[1].mark_polled_at(c.sample_epoch),
                1 => states[1].buffering = true,
                2 => states[1].seconds += 0.501,
                3 => states[0].seeking = Some(111.0),
                4 => states[0].playback_intent = Some(true),
                5 => states[0].playing = false,
                _ => states[1].ready = false,
            }
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(commands.primary.is_none() && commands.secondary.is_none());
            assert_ne!(c.status(), Status::Playing);
        }
    }

    #[test]
    fn native_pending_or_held_pause_intent_is_not_a_new_user_pause() {
        let mut c = controller(true);
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], test_now());
        prepared(&mut c, true);
        let mut states = [sample(112.5, true), sample(913.25, true)];
        states[0].playing = false;
        states[0].pause_intent = true;
        states[0].playback_intent = Some(false);
        c.tick([&states[0], &states[1]], test_now());
        assert!(c.wants_playing());

        let mut c = controller(true);
        c.tick([&empty, &empty], test_now());
        prepared(&mut c, true);
        let states = [sample(112.5, true), sample(914.75, true)];
        c.tick([&states[0], &states[1]], test_now());
        let mut held = [sample(112.5, true), sample(914.75, false)];
        held[1].pause_intent = true;
        c.tick([&held[0], &held[1]], test_now());
        assert!(
            c.wants_playing(),
            "The ahead POV was paused by our catch-up plan"
        );
    }

    #[test]
    fn startup_buffering_on_either_provider_does_not_restart_the_same_seek() {
        for side in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            let paused = [sample(112.5, false), sample(913.25, false)];
            let commands = c.tick([&paused[0], &paused[1]], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
            let barrier = c.sample_epoch;
            let deadline = c.operation_started;
            for _ in 0..6 {
                let mut states = [sample(112.75, true), sample(913.5, true)];
                states[side].playing = false;
                states[side].buffering = true;
                states[side].playback_intent = Some(true);
                let commands = c.tick([&states[0], &states[1]], test_now());
                assert!(commands.primary.is_none() && commands.secondary.is_none());
                assert!(c.wants_playing());
                assert_eq!(c.sample_epoch, barrier);
                assert_eq!(c.operation_started, deadline);
            }
            let running = [sample(113.0, true), sample(913.75, true)];
            c.tick([&running[0], &running[1]], test_now());
            assert_eq!(c.status(), Status::Playing);
            c.tick([&running[0], &running[1]], test_now());
            assert_eq!(c.position_ms(), START + 12_875);
        }
    }

    #[test]
    fn startup_buffering_keeps_pause_intent_and_the_original_timeout() {
        for pause in [false, true] {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            let paused = [sample(112.5, false), sample(913.25, false)];
            c.tick([&paused[0], &paused[1]], test_now());
            let mut states = [sample(112.5, true), sample(913.25, false)];
            states[1].buffering = true;
            c.tick([&states[0], &states[1]], test_now());
            let now = if pause {
                let now = test_now();
                c.set_playing(false, now);
                now
            } else {
                c.operation_started + PREPARATION_TIMEOUT
            };
            let commands = c.tick([&states[0], &states[1]], now);
            assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
            assert!(c.tick([&states[0], &states[1]], now).primary.is_none());
            if pause {
                assert!(!c.wants_playing());
            } else {
                assert_eq!(c.status(), Status::Failed(Error::TimedOut));
            }
        }
    }

    #[test]
    fn buffering_pauses_once_then_resumes_already_aligned_players() {
        let mut c = controller(true);
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], test_now());
        prepared(&mut c, true);
        let primary = sample(115.0, true);
        let mut secondary = sample(915.5, true);
        secondary.buffering = true;
        let commands = c.tick([&primary, &secondary], test_now());
        assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
        assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
        assert_eq!(c.status(), Status::Buffering);
        assert_eq!(c.position_ms(), START + 14_625);
        assert!(c.tick([&primary, &secondary], test_now()).primary.is_none());
        let states = [sample(115.0, false), sample(915.5, false)];
        let commands = c.tick([&states[0], &states[1]], test_now());
        assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
        assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
        assert_eq!(c.position_ms(), START + 14_875);
        let commands = c.tick([&states[0], &states[1]], test_now());
        assert!(commands.primary.is_none() && commands.secondary.is_none());
    }

    #[test]
    fn pause_during_buffer_recovery_wins() {
        let mut c = controller(true);
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], test_now());
        prepared(&mut c, true);
        let primary = sample(112.5, true);
        let mut secondary = sample(913.25, true);
        secondary.buffering = true;
        c.tick([&primary, &secondary], test_now());
        c.set_playing(false, test_now());
        let states = [sample(112.5, false), sample(913.25, false)];
        c.tick([&states[0], &states[1]], test_now());
        c.tick([&states[0], &states[1]], test_now());
        prepared(&mut c, false);
        assert_eq!(c.status(), Status::Paused);
    }

    #[test]
    fn provider_pause_on_either_side_wins_over_the_other_sides_buffering() {
        for side in 0..2 {
            let mut c = controller(true);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, true);
            let mut states = [sample(112.5, true), sample(913.25, true)];
            states[side].playing = false;
            states[1 - side].buffering = true;
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
            assert!(!c.wants_playing());
            let paused = [sample(112.5, false), sample(913.25, false)];
            c.tick([&paused[0], &paused[1]], test_now());
            c.tick([&paused[0], &paused[1]], test_now());
            prepared(&mut c, false);
            assert_eq!(c.status(), Status::Paused);
        }
    }

    #[test]
    fn provider_play_on_either_paused_side_resumes_both_through_a_shared_seek() {
        for side in 0..2 {
            let mut c = controller(false);
            let empty = PlaybackState::default();
            c.tick([&empty, &empty], test_now());
            prepared(&mut c, false);
            let mut states = [sample(112.5, false), sample(913.25, false)];
            states[side].playing = true;
            states[side].seconds += 0.375;
            let commands = c.tick([&states[0], &states[1]], test_now());
            assert_eq!(command_seconds(commands.primary), 112.875);
            assert_eq!(command_seconds(commands.secondary), 913.625);
            assert_eq!(c.position_ms(), START + 12_750);
            assert!(c.wants_playing());
            let paused = [sample(112.875, false), sample(913.625, false)];
            let commands = c.tick([&paused[0], &paused[1]], test_now());
            assert!(matches!(commands.primary, Some(PlaybackCommand::Play)));
            assert!(matches!(commands.secondary, Some(PlaybackCommand::Play)));
        }
    }

    #[test]
    fn provider_play_after_one_pause_acknowledgement_does_not_wait_for_the_peer() {
        for holding in [false, true] {
            for side in 0..2 {
                let mut c = controller(holding);
                let empty = PlaybackState::default();
                let mut stale_playing = [sample(112.5, true), sample(913.25, true)];
                c.tick([&empty, &empty], test_now());
                if holding {
                    prepared(&mut c, true);
                    c.set_playing(false, test_now());
                    c.tick([&stale_playing[0], &stale_playing[1]], test_now());
                }
                // Equal clock ticks cannot establish a later provider gesture.
                // Set that boundary explicitly; Instant calls can coincide on
                // Windows even when made in separate statements.
                for state in &mut stale_playing {
                    state.mark_polled_at(c.sample_epoch);
                }
                c.tick([&stale_playing[0], &stale_playing[1]], test_now());
                assert!(!c.wants_playing());
                let mut states = [sample(112.5, false), sample(913.25, false)];
                states[1 - side].buffering = true;
                c.tick([&states[0], &states[1]], test_now());
                // A resumed state still carrying an unacknowledged native
                // Pause is not yet evidence of a subsequent provider action.
                states[side] = sample(states[side].seconds, true);
                states[side].playback_intent = Some(false);
                c.tick([&states[0], &states[1]], test_now());
                assert!(!c.wants_playing());
                states[side].playback_intent = None;
                states[side].mark_polled_at(c.sample_epoch + Duration::from_millis(1));
                let commands = c.tick([&states[0], &states[1]], test_now());
                assert!(c.wants_playing());
                assert_eq!(command_seconds(commands.primary), 112.5);
                assert_eq!(command_seconds(commands.secondary), 913.25);
            }
        }
    }

    #[test]
    fn replacing_pov_rejects_old_samples_and_uncovered_target() {
        let mut c = controller(false);
        let empty = PlaybackState::default();
        c.tick([&empty, &empty], test_now());
        prepared(&mut c, false);
        let old = [sample(112.5, false), sample(913.25, false)];
        let replacement = RecordingClock::new(START, 1200.125, 5000.0).unwrap();
        c.replace_clock(1, replacement, test_now()).unwrap();
        let commands = c.tick([&old[0], &old[1]], test_now());
        assert_eq!(command_seconds(commands.secondary), 1212.5);
        assert!(c.tick([&old[0], &old[1]], test_now()).primary.is_none());
        let uncovered = RecordingClock::new(START, 4999.0, 5000.0).unwrap();
        assert_eq!(
            c.replace_clock(1, uncovered, test_now()),
            Err(Error::Unavailable)
        );
        assert!(matches!(
            c.tick([&old[0], &old[1]], test_now()).primary,
            Some(PlaybackCommand::Pause)
        ));
    }

    #[test]
    fn startup_timeout_stops_both_without_repeated_commands() {
        let mut c = controller(true);
        let empty = PlaybackState::default();
        let now = test_now() + PREPARATION_TIMEOUT;
        let commands = c.tick([&empty, &empty], now);
        assert!(matches!(commands.primary, Some(PlaybackCommand::Pause)));
        assert_eq!(c.status(), Status::Failed(Error::TimedOut));
        assert!(c.tick([&empty, &empty], now).primary.is_none());
    }

    #[test]
    fn invalid_clocks_ranges_and_exit_are_bounded() {
        for seconds in [f64::NAN, f64::INFINITY, 604_801.0] {
            assert!(RecordingClock::new(START, seconds, 5000.0).is_err());
        }
        let mut c = controller(false);
        assert_eq!(
            c.seek(START - 1, false, test_now()),
            Err(Error::InvalidRange)
        );
        assert_eq!(
            c.replace_clock(2, clocks()[0], test_now()),
            Err(Error::InvalidClock)
        );
        assert!(RecordingClock::new(START, 100.0, 100.0)
            .unwrap()
            .video_seconds(START)
            .is_none());
        let commands = c.leave();
        assert!(commands.primary.is_none());
        assert!(matches!(commands.secondary, Some(PlaybackCommand::Pause)));
    }
}
