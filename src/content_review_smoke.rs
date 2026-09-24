//! Explicit loopback-only UI/HTTP/player integration. The accepted response is
//! an immutable synthetic fixture, not evidence of alignment accuracy.
use super::*;
use crate::{
    content_alignment::{self, Ticket},
    guild,
    stream_player::{pump_events, StreamPlayer},
    streams::Provider,
    warcraftlogs::boss_signature::{
        BossCast, BossSignature, CastType, Coverage, NamedId, PageCoverage, SCHEMA,
    },
    warcraftlogs::Replay,
};
use std::io::Read;

const ENDPOINT: &str = "http://127.0.0.1:18083";
const TOKEN: &str = "local-test-101";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    kind: String,
    replay: Replay,
    expected_video_seconds: f64,
}

struct Driver {
    viewer: ReviewUi,
    stream: Stream,
    access: guild::Access,
    pending: Ticket,
    poll: Option<mpsc::Receiver<Result<Ticket, String>>>,
    player: Option<Box<StreamPlayer>>,
    player_id: Option<String>,
    player_creations: usize,
    activated: bool,
    base_url: String,
    origin: f64,
    phase: u8,
    start: Instant,
    phase_start: Instant,
    sample: f64,
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
}

impl Driver {
    fn finish(&mut self, ctx: &egui::Context, result: Result<(), String>) {
        self.player.take();
        self.phase = 255;
        *self.outcome.lock().unwrap() = Some(result);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn advance(&mut self, ctx: &egui::Context) -> Result<bool, String> {
        if self.start.elapsed() > Duration::from_secs(90) {
            return Err(format!(
                "Content UI smoke exceeded its bound at phase {}",
                self.phase
            ));
        }
        if self.phase == 0 {
            if self.viewer.playback.is_none() || !self.viewer.content_waiting() {
                return Err("Pending alignment prevented estimated playback".into());
            }
            if self.phase_start.elapsed() >= Duration::from_secs(2) {
                let (tx, rx) = mpsc::channel();
                let access = self.access.clone();
                let pending = self.pending.clone();
                guild::spawn(move || {
                    let _ = tx.send(content_alignment::poll(
                        &access,
                        &pending,
                        &AtomicBool::new(false),
                    ));
                });
                self.poll = Some(rx);
                self.phase = 1;
            }
            return Ok(false);
        }
        if self.phase == 1 {
            match self.poll.as_ref().unwrap().try_recv() {
                Ok(result) => {
                    let ticket = result?;
                    let alignment = ticket
                        .alignment()
                        .ok_or("No valid completed fixture result")?;
                    if alignment.result.video_seconds != self.origin {
                        return Err("Fixture origin changed".into());
                    }
                    let first = self.viewer.pull.clone().unwrap();
                    let mut other = first.clone();
                    other.id += 1;
                    self.viewer
                        .review
                        .as_mut()
                        .unwrap()
                        .pulls
                        .push(other.clone());
                    self.viewer.select(other);
                    self.viewer.sync_content_selection();
                    let estimate = self.viewer.playback.as_ref().unwrap().seconds;
                    if self.viewer.accept_content(ticket.clone())
                        || self.viewer.playback.as_ref().unwrap().seconds != estimate
                    {
                        return Err("A completed job moved a different selected fight".into());
                    }
                    self.viewer.select(first);
                    self.viewer.sync_content_selection();
                    if self.viewer.content_waiting() {
                        return Err(
                            "Current completed sample did not unlock the selected fight".into()
                        );
                    }
                    let first = self.viewer.pull.clone().unwrap();
                    let mut later = first.clone();
                    later.id += 2;
                    later.start_ms += 60_000;
                    later.end_ms += 60_000;
                    self.viewer
                        .review
                        .as_mut()
                        .unwrap()
                        .pulls
                        .push(later.clone());
                    // A second scoped fixture ticket represents an independently
                    // checked later pull; no legacy server-wide clock is injected.
                    let mut later_ticket = ticket.clone();
                    later_ticket.key = content_alignment::Key::new(
                        &self.viewer.review.as_ref().unwrap().replay,
                        &later,
                        self.viewer
                            .review
                            .as_ref()
                            .unwrap()
                            .content_capability
                            .as_ref()
                            .unwrap(),
                        ticket.key.auth_epoch,
                    );
                    later_ticket.job.scope.as_mut().unwrap().pull_id = later.id;
                    let result = later_ticket.job.result.as_mut().unwrap();
                    result.video_seconds += 60.0;
                    result.seek_video_seconds += 60.0;
                    self.viewer.content.samples.remember(later_ticket);
                    self.viewer
                        .review
                        .as_mut()
                        .unwrap()
                        .pulls
                        .retain(|p| p.id != first.id + 1);
                    self.viewer.select(later);
                    self.viewer.sync_content_selection();
                    if self
                        .viewer
                        .playback
                        .as_ref()
                        .is_none_or(|p| (p.seconds - self.origin - 60.0).abs() > 1e-8)
                        || self.viewer.next_content_action().is_some()
                    {
                        return Err(
                            "Changing pulls did not reuse the shared recording clock".into()
                        );
                    }
                    self.viewer.select(first);
                    self.viewer.sync_content_selection();
                    let playback = self.viewer.playback.as_ref().unwrap();
                    if !playback.content_timing
                        || playback.seconds != self.origin
                        || !self
                            .viewer
                            .review
                            .as_ref()
                            .unwrap()
                            .marker_timing
                            .is_empty()
                        || self.viewer.marker_sync.busy()
                    {
                        return Err("Content playback used the wrong timing source".into());
                    }
                    // A different viewer sharing this account's metadata cache
                    // opens directly at the aligned pull, even while WCL is busy.
                    let prepared_review = self.viewer.review.as_ref().unwrap().clone();
                    let samples = self.viewer.content.samples.clone();
                    self.viewer.prepared.lock().unwrap().insert(
                        &self.stream,
                        prepared_review,
                        self.viewer.recording_match_status.unwrap(),
                        Vec::new(),
                    );
                    self.viewer
                        .prepared
                        .lock()
                        .unwrap()
                        .set_samples(&self.stream, samples);
                    let mut fresh = self.viewer.metadata_peer();
                    fresh.metadata_only = false;
                    let shared_client = fresh.client.clone();
                    let busy = shared_client.lock().unwrap();
                    let opened_at = Instant::now();
                    fresh.open_recording();
                    if !fresh.restore_prepared_recording(&self.stream) {
                        return Err("Prepared recording was not reusable at first open".into());
                    }
                    fresh.sync_content_selection();
                    if fresh
                        .playback
                        .as_ref()
                        .is_none_or(|playback| playback.seconds != self.origin)
                        || fresh.content_waiting()
                        || fresh.next_content_action().is_some()
                        || fresh.work.is_some()
                        || opened_at.elapsed() > Duration::from_millis(100)
                    {
                        return Err("Prepared first open waited for alignment or WCL".into());
                    }
                    drop(busy);
                    self.viewer = fresh;
                    if let Some(player) = self.player.as_mut() {
                        player
                            .command(PlaybackCommand::Seek(self.origin))
                            .map_err(|_| "Prepared video seek failed")?;
                    }
                    eprintln!("Native content UI: prepared first open ready without WCL or alignment work, elapsed_us={}", opened_at.elapsed().as_micros());
                    self.phase = 2;
                    self.phase_start = Instant::now();
                    eprintln!("Native content UI: estimate available while pending; stale selection rejected; verified clock reused");
                }
                Err(mpsc::TryRecvError::Empty) => (),
                Err(_) => return Err("Content poll worker disconnected".into()),
            }
            return Ok(false);
        }
        let Some(player) = self.player.as_mut() else {
            return Ok(false);
        };
        pump_events();
        player.poll_playback(ctx);
        if let Some(message) = player.failure() {
            return Err(format!("Content player failed: {message}"));
        }
        if player.diagnostic_player_id() != self.player_id {
            return Err("Content player was replaced".into());
        }
        let state = player.playback_state();
        match self.phase {
            2 if state.ready && state.playing && state.seconds > self.origin + 0.5 => {
                if state.seconds > self.origin + self.phase_start.elapsed().as_secs_f64() + 5.0 {
                    return Err("Content player opened at the wrong video time".into());
                }
                let at = self.viewer.pull.as_ref().unwrap().start_ms + 11_125;
                let command = self
                    .viewer
                    .seek_absolute_with_playback(at, false)
                    .ok_or("Covered seek unavailable")?;
                if !matches!(command, PlaybackCommand::SeekPaused(t) if (t-self.origin-11.125).abs()<1e-8)
                {
                    return Err("Paused log seek did not use the content origin".into());
                }
                player
                    .command(command)
                    .map_err(|_| "Paused content seek failed")?;
                self.phase = 3;
                self.phase_start = Instant::now();
            }
            3 if self.phase_start.elapsed() >= Duration::from_secs(2) && state.ready => {
                if state.playing || (state.seconds - self.origin - 11.125).abs() > 0.5 {
                    return Err("Paused content seek did not settle at its target".into());
                }
                let at = self.viewer.pull.as_ref().unwrap().start_ms + 30_375;
                let command = self
                    .viewer
                    .seek_absolute(at)
                    .ok_or("Resumed covered seek unavailable")?;
                if !matches!(command, PlaybackCommand::Seek(t) if (t-self.origin-30.375).abs()<1e-8)
                {
                    return Err("Resumed log seek did not use the content origin".into());
                }
                player
                    .command(command)
                    .map_err(|_| "Resumed content seek failed")?;
                self.phase = 4;
                self.phase_start = Instant::now();
            }
            4 if self.phase_start.elapsed() >= Duration::from_secs(2)
                && state.ready
                && state.playing =>
            {
                if state.seconds < self.origin + 30.0
                    || state.seconds
                        > self.origin + 30.375 + self.phase_start.elapsed().as_secs_f64() + 3.0
                {
                    return Err("Resumed content seek opened the wrong position".into());
                }
                self.sample = state.seconds;
                self.phase = 5;
                self.phase_start = Instant::now();
            }
            5 if self.phase_start.elapsed() >= Duration::from_secs(2) => {
                if !state.ready
                    || !state.playing
                    || state.seconds <= self.sample + 0.5
                    || self.player_creations != 1
                {
                    return Err("Content replay did not advance using one native player".into());
                }
                let pull = self.viewer.pull.as_ref().unwrap();
                if self
                    .viewer
                    .review
                    .as_ref()
                    .unwrap()
                    .video_seconds(pull, 180.001)
                    .is_some()
                {
                    return Err("Content coverage allowed an out-of-fight seek".into());
                }
                let at = pull.end_ms - 1_000;
                let command = self
                    .viewer
                    .seek_absolute(at)
                    .ok_or("End-of-pull seek unavailable")?;
                player
                    .command(command)
                    .map_err(|_| "End-of-pull seek failed")?;
                self.phase = 6;
                self.phase_start = Instant::now();
            }
            6 | 8 => {
                if let Some(command) = self.viewer.pause_at_pull_end(&state) {
                    if !matches!(command, PlaybackCommand::Pause) {
                        return Err("Pull boundary issued an unexpected command".into());
                    }
                    self.sample = state.seconds;
                    player
                        .command(command)
                        .map_err(|_| "Pull boundary pause failed")?;
                    self.phase += 1;
                    self.phase_start = Instant::now();
                }
            }
            7 | 9
                if state.is_fresh()
                    && state.playback_intent.is_none()
                    && state.observation_window().is_some_and(|window| {
                        window[0] >= self.phase_start + Duration::from_secs(2)
                    }) =>
            {
                let end = self.viewer.active_playback_range().unwrap().1;
                if !state.ready
                    || state.playing
                    || state.seeking.is_some()
                    || state.seconds < end
                    || state.seconds > end + 2.0
                    || (state.seconds - self.sample).abs() > 0.5
                {
                    return Err("Native player did not stay paused at the pull boundary".into());
                }
                if self.phase == 9 {
                    eprintln!("Native content UI verified: HTTP pending/poll, stale-selection rejection, paused and resumed log seeks, one native player, automatic end pause with verified and pending timing; synthetic timing fixture only");
                    return Ok(true);
                }
                // Reproduce the reported case: playback is available while the
                // content result is still pending. The same boundary must pause
                // the real player even without a verified alignment.
                self.viewer.content = State::default();
                self.viewer.review.as_mut().unwrap().content_timing.clear();
                let pull = self.viewer.pull.clone().unwrap();
                self.viewer.select(pull.clone());
                if !self.viewer.content_waiting() {
                    return Err("Pending-timing boundary fixture retained an alignment".into());
                }
                let command = self
                    .viewer
                    .seek_absolute(pull.end_ms - 1_000)
                    .ok_or("Estimated end-of-pull seek unavailable")?;
                player
                    .command(command)
                    .map_err(|_| "Estimated boundary seek failed")?;
                self.phase = 8;
                self.phase_start = Instant::now();
            }
            _ => (),
        }
        Ok(false)
    }
}

impl eframe::App for Driver {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.phase == 255 {
            return;
        }
        match self.advance(ctx) {
            Ok(true) => self.finish(ctx, Ok(())),
            Err(error) => self.finish(ctx, Err(error)),
            Ok(false) => (),
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.phase == 255 {
            return;
        }
        ui.heading("Brick · Content alignment UI regression");
        ui.label("Synthetic accepted timing · real native player · isolated local profile");
        self.viewer.draw_content_controls(ui, &self.stream);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(900.0, 510.0).min(ui.available_size()),
            egui::Sense::hover(),
        );
        let ctx = ui.ctx().clone();
        if !self.activated {
            match crate::replay_smoke::native::activate_native_window(frame) {
                Ok(()) => {
                    self.activated = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Err(error) => self.finish(&ctx, Err(error)),
            }
            return;
        }
        if let Some(player) = &mut self.player {
            if player.set_bounds(rect, ctx.pixels_per_point()).is_err() {
                self.finish(&ctx, Err("Content player resize failed".into()));
            }
            return;
        }
        let url = crate::streams_ui::player_url_for_playback(
            &self.base_url,
            self.viewer.playback.as_ref(),
        );
        let parsed = url::Url::parse(&url).unwrap();
        if !parsed
            .query_pairs()
            .any(|(k, v)| k == "timing" && v == "content")
        {
            self.finish(&ctx, Err("Content player URL lost its timing mode".into()));
            return;
        }
        match StreamPlayer::new(frame, &ctx, &url, TOKEN, rect, ctx.pixels_per_point(), None) {
            Ok(player) => {
                self.player_id = player.diagnostic_player_id();
                self.player = Some(Box::new(player));
                self.player_creations += 1;
            }
            Err(error) => self.finish(
                &ctx,
                Err(format!("Content native player could not open: {error}")),
            ),
        }
    }
}

#[test]
#[ignore = "requires an explicit synthetic content-job fixture, loopback service and native desktop"]
fn content_job_to_native_player_uses_relative_timing_without_marker() {
    assert_eq!(
        option_env!("BRICK_PRESENCE_API_URL"),
        Some(ENDPOINT),
        "Build only against the isolated loopback fixture"
    );
    let file = std::fs::File::open(
        std::env::var_os("BRICK_CONTENT_SMOKE_FIXTURE").expect("Set the explicit fixture path"),
    )
    .unwrap();
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() <= 65_536);
    let fixture: Fixture =
        serde_json::from_slice(&bytes).expect("Invalid explicit content fixture");
    assert_eq!(fixture.kind, "synthetic-content-ui");
    assert!(fixture.replay.started_at.is_empty() && fixture.replay.provider == Provider::Youtube);
    assert!(
        fixture.expected_video_seconds.is_finite()
            && fixture.expected_video_seconds >= 0.0
            && fixture.expected_video_seconds + 180.0 < fixture.replay.available_seconds as f64
    );
    let (_, pull, cap, _) = content_alignment::test_ticket();
    let key = content_alignment::Key::new(&fixture.replay, &pull, &cap, 0);
    let complete = PageCoverage {
        pages: 1,
        complete: true,
    };
    let signature = BossSignature {
        schema: SCHEMA.into(),
        report: pull.report.clone(),
        pull_id: pull.id,
        encounter_id: pull.encounter,
        difficulty: pull.difficulty,
        report_start_ms: pull.report_start_ms,
        fight_start_ms: pull.start_ms - pull.report_start_ms,
        fight_end_ms: pull.end_ms - pull.report_start_ms,
        complete: true,
        actors: vec![NamedId {
            id: 1,
            name: "Synthetic boss".into(),
        }],
        abilities: vec![NamedId {
            id: 1,
            name: "Synthetic cast".into(),
        }],
        casts: vec![
            BossCast {
                seconds: 1.0,
                kind: CastType::BeginCast,
                actor_id: 1,
                instance: None,
                ability_id: 1,
            },
            BossCast {
                seconds: 3.0,
                kind: CastType::Cast,
                actor_id: 1,
                instance: None,
                ability_id: 1,
            },
        ],
        health: vec![],
        coverage: Coverage {
            casts: complete.clone(),
            health: complete,
        },
    };
    let access = guild::Access::new(
        TOKEN.into(),
        guild::ADVANCE.into(),
        "101".into(),
        guild::generation(),
    );
    let pending =
        content_alignment::sampling::submit(&access, key, &signature, &AtomicBool::new(false))
            .expect("Loopback content submission failed");
    assert!(pending.pending() && pending.alignment().is_none());
    let stream = Stream {
        user_id: "101".into(),
        name: "Local content fixture".into(),
        raid_role: None,
        provider: Provider::Youtube,
        channel_id: fixture.replay.video_id.clone(),
        url: fixture.replay.public_url(0),
        status: if fixture.replay.growing {
            crate::streams::Status::Live
        } else {
            crate::streams::Status::Offline
        },
        broadcast_state: None,
        recording_id: (!fixture.replay.growing).then(|| fixture.replay.video_id.clone()),
        replay_start_ms: fixture.replay.growing.then_some(pull.report_start_ms),
        replay_end_ms: None,
    };
    let base_url = if fixture.replay.growing {
        format!("{ENDPOINT}/v1/streams/player/101/youtube")
    } else {
        format!(
            "{ENDPOINT}/v1/streams/player/101/youtube?recording={}",
            fixture.replay.video_id
        )
    };
    let mut viewer = ReviewUi::default();
    viewer.active = true;
    viewer.connected = true;
    viewer.connection_checked = true;
    viewer.recording_match_status = Some((0, false));
    viewer.review = Some(Review {
        replay: fixture.replay,
        pulls: vec![pull.clone()],
        marker_timing: Default::default(),
        marker_fallback: Default::default(),
        content_capability: Some(cap),
        content_timing: Default::default(),
    });
    viewer.select(pull);
    viewer.sync_content_selection();
    assert!(!viewer.accept_content(pending.clone()));
    let outcome = Arc::new(Mutex::new(None));
    let shared = outcome.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Brick · Content UI regression")
            .with_inner_size([960.0, 700.0])
            .with_active(true),
        event_loop_builder: Some(Box::new(|builder| {
            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::EventLoopBuilderExtWindows as _;
                builder.with_any_thread(true);
            }
            #[cfg(target_os = "linux")]
            {
                use winit::platform::x11::EventLoopBuilderExtX11 as _;
                builder.with_x11().with_any_thread(true);
            }
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Brick content UI regression",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let now = Instant::now();
            Ok(Box::new(Driver {
                viewer,
                stream,
                access,
                pending,
                poll: None,
                player: None,
                player_id: None,
                player_creations: 0,
                activated: false,
                base_url,
                origin: fixture.expected_video_seconds,
                phase: 0,
                start: now,
                phase_start: now,
                sample: 0.0,
                outcome: shared,
            }))
        }),
    )
    .expect("Content UI native window failed");
    match outcome.lock().unwrap().take() {
        Some(Ok(())) => (),
        Some(Err(error)) => panic!("{error}"),
        None => panic!("Content UI window closed before verification"),
    };
}
