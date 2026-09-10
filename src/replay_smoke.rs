//! Explicit local integration check against a real provider player.
//! No Warcraft Logs session, Discord account, or persistent app profile is used.

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod native {
    use crate::{
        stream_player::{pump_events, PlaybackCommand, PlaybackState, StreamPlayer},
        streams::Provider,
        warcraftlogs::Replay,
    };
    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        io::Read,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    const LOCAL_ENDPOINT: &str = "http://127.0.0.1:18083";
    const TEST_TOKEN: &str = "local-test-101";
    const DEADLINE: Duration = Duration::from_secs(55);

    // A virtual display without a window manager never activates newly mapped
    // windows for us. Activate this test's native window, as opening Brick does;
    // this deliberately sends no input to the embedded player.
    fn activate_native_window(frame: &eframe::Frame) -> Result<(), String> {
        let handle = frame
            .window_handle()
            .map_err(|_| "The smoke window has no native handle")?;
        #[cfg(target_os = "linux")]
        {
            use x11rb::{
                connection::Connection as _,
                protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, InputFocus, StackMode},
                CURRENT_TIME,
            };
            let window = match handle.as_raw() {
                RawWindowHandle::Xlib(handle) => u32::try_from(handle.window)
                    .map_err(|_| "The smoke window has an invalid X11 handle")?,
                RawWindowHandle::Xcb(handle) => handle.window.get(),
                _ => return Err("The native replay smoke requires an X11 window".into()),
            };
            let operation = || -> Result<bool, Box<dyn std::error::Error>> {
                let (connection, _) = x11rb::connect(None)?;
                connection.map_window(window)?.check()?;
                connection
                    .configure_window(
                        window,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?
                    .check()?;
                connection
                    .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
                    .check()?;
                connection.flush()?;
                let focused = connection.get_input_focus()?.reply()?.focus == window;
                Ok(focused)
            };
            match operation() {
                Ok(true) => Ok(()),
                Ok(false) => Err("The smoke window did not receive X11 focus".into()),
                Err(_) => Err("The smoke window could not be activated on X11".into()),
            }
        }
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                BringWindowToTop, GetForegroundWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return Err("The native replay smoke requires a Win32 window".into());
            };
            let window = handle.hwnd.get() as *mut std::ffi::c_void;
            // SAFETY: the handle belongs to this live eframe test window on the
            // calling UI thread. Only the test's own top-level window is changed.
            unsafe {
                ShowWindow(window, SW_RESTORE);
                BringWindowToTop(window);
                SetForegroundWindow(window);
                if GetForegroundWindow() != window {
                    return Err("The smoke window did not receive Windows foreground focus".into());
                }
            }
            Ok(())
        }
    }

    fn primary_fixture_member(value: Option<&str>) -> Result<&'static str, &'static str> {
        match value {
            None | Some("101") => Ok("101"),
            Some("102") => Ok("102"),
            Some("103") => Ok("103"),
            _ => Err("BRICK_REPLAY_TEST_MEMBER must be local fixture member 101, 102 or 103"),
        }
    }

    #[test]
    fn primary_fixture_member_is_limited_to_known_local_members() {
        assert_eq!(primary_fixture_member(None), Ok("101"));
        for member in ["101", "102", "103"] {
            assert_eq!(primary_fixture_member(Some(member)).unwrap(), member);
        }
        for member in ["", "104", "0101", "../101", "101/youtube?at=1"] {
            assert!(primary_fixture_member(Some(member)).is_err());
        }
    }

    #[derive(Clone, Copy)]
    enum Phase {
        Autoplay,
        AutoplayAdvances,
        PauseSettles,
        PauseHolds,
        PausedSeekSettles,
        PausedSeekHolds,
        SeekSettles,
        SeekAdvances,
        Hidden,
        Shown,
        Resized,
        AlternateSettles,
        AlternateHolds,
        ReturnSettles,
        ReturnAdvances,
    }

    impl Phase {
        fn label(self) -> &'static str {
            match self {
                Self::Autoplay => "autoplay without a user click",
                Self::AutoplayAdvances => "autoplay advances",
                Self::PauseSettles => "pause takes effect",
                Self::PauseHolds => "paused position holds",
                Self::PausedSeekSettles => "paused seek reaches its precise target",
                Self::PausedSeekHolds => "paused seek remains paused",
                Self::SeekSettles => "seek reaches its target and resumes",
                Self::SeekAdvances => "playback advances after seeking",
                Self::Hidden => "hide preserves the player",
                Self::Shown => "show preserves playback",
                Self::Resized => "resize preserves playback",
                Self::AlternateSettles => "reused player prepares the alternate POV paused",
                Self::AlternateHolds => "alternate POV holds its requested paused frame",
                Self::ReturnSettles => "reused player returns to the original POV",
                Self::ReturnAdvances => "original POV resumes without replacing the native player",
            }
        }
    }

    #[derive(Default)]
    struct Outcome {
        result: Option<Result<(), String>>,
        creations: usize,
    }

    struct Driver {
        player: Option<Box<StreamPlayer>>,
        outcome: Arc<Mutex<Outcome>>,
        url: String,
        alternate: Option<(String, f64)>,
        switch_rounds: usize,
        completed_rounds: usize,
        deadline: Duration,
        player_id: Option<String>,
        capture_path: Option<std::path::PathBuf>,
        capture_requested: bool,
        capture_verified: bool,
        offset: f64,
        target: f64,
        started: Instant,
        phase_started: Instant,
        phase: Phase,
        sample: f64,
        resized: bool,
        finished: bool,
        activated: bool,
        diagnostics: bool,
        last_diagnostic: Instant,
    }

    impl Driver {
        fn next(&mut self, phase: Phase, sample: f64) {
            self.phase = phase;
            self.phase_started = Instant::now();
            self.sample = sample;
            eprintln!("Native replay smoke: {}", phase.label());
        }

        fn finish(&mut self, ctx: &egui::Context, result: Result<(), String>) {
            self.finished = true;
            self.player.take();
            self.outcome.lock().unwrap().result = Some(result);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        fn command(&mut self, command: PlaybackCommand) -> Result<(), String> {
            let player = self
                .player
                .as_mut()
                .ok_or("The native replay player disappeared")?;
            player
                .command(command)
                .map_err(|_| "The native replay command failed")?;
            let playing = matches!(command, PlaybackCommand::Play | PlaybackCommand::Seek(_));
            if player.playback_state().playback_intent != Some(playing) {
                return Err("The player lost the immediate native playback intent".into());
            }
            Ok(())
        }

        fn advance(&mut self, ctx: &egui::Context) -> Result<bool, String> {
            let Some(player) = self.player.as_mut() else {
                return Ok(false);
            };
            pump_events();
            player.poll_playback(ctx);
            if player.failure().is_some() {
                return Err(format!(
                    "The native player failed during {}",
                    self.phase.label()
                ));
            }
            let state = player.playback_state();
            if self.player_id != player.diagnostic_player_id() {
                return Err("The POV switch replaced its native WebView".into());
            }
            if self.diagnostics && self.last_diagnostic.elapsed() >= Duration::from_secs(5) {
                eprintln!(
                    "Native replay state: stage={}, window_focused={}, ready={}, playing={}, relative_seconds={:.1}",
                    self.phase.label(),
                    ctx.input(|input| input.viewport().focused.unwrap_or(false)),
                    state.ready,
                    state.playing,
                    state.seconds - self.offset
                );
                #[cfg(target_os = "linux")]
                player.diagnostic_details();
                self.last_diagnostic = Instant::now();
            }
            if self.started.elapsed() > self.deadline {
                return Err(format!(
                    "Timed out during {} (ready={}, playing={})",
                    self.phase.label(),
                    state.ready,
                    state.playing
                ));
            }
            if !matches!(
                self.phase,
                Phase::Autoplay | Phase::AlternateSettles | Phase::ReturnSettles
            ) && !state.ready
            {
                return Err(format!(
                    "The player lost readiness during {}",
                    self.phase.label()
                ));
            }
            let elapsed = self.phase_started.elapsed();
            match self.phase {
                Phase::Autoplay => {
                    // Readiness alone is insufficient: this must come from the
                    // provider's moving clock, without issuing Play or a click.
                    if state.ready && state.playing && self.near(&state, self.offset, 20.0) {
                        self.next(Phase::AutoplayAdvances, state.seconds);
                    }
                }
                Phase::AutoplayAdvances => {
                    if state.playing && state.seconds >= self.sample + 1.0 {
                        self.command(PlaybackCommand::Pause)?;
                        self.next(Phase::PauseSettles, state.seconds);
                    }
                }
                Phase::PauseSettles => {
                    if !state.playing {
                        self.next(Phase::PauseHolds, state.seconds);
                    }
                }
                Phase::PauseHolds => {
                    if state.playing || (state.seconds - self.sample).abs() > 0.4 {
                        return Err("Playback moved while paused".into());
                    }
                    if self.capture_path.is_some() && !self.capture_verified {
                        let player = self.player.as_ref().unwrap();
                        if !self.capture_requested {
                            self.capture_requested = player.request_frame_capture(ctx);
                            if self.capture_requested && player.request_frame_capture(ctx) {
                                return Err("The player accepted overlapping frame captures".into());
                            }
                        }
                        if let Some(result) = player.take_frame_capture() {
                            let frame = result
                                .map_err(|_| "The native media frame could not be captured")?;
                            if frame.playing
                                || (frame.before_seconds - self.sample).abs() > 0.4
                                || (frame.after_seconds - self.sample).abs() > 0.4
                                || frame.bracket_duration > Duration::from_secs(3)
                            {
                                return Err(
                                    "The snapshot timing did not bracket the paused player".into(),
                                );
                            }
                            let image = image::load_from_memory_with_format(
                                &frame.png,
                                image::ImageFormat::Png,
                            )
                            .map_err(|_| "The native snapshot is not a decodable PNG")?
                            .to_rgb8();
                            let colored = image
                                .pixels()
                                .filter(|pixel| {
                                    pixel.0.iter().max().unwrap() - pixel.0.iter().min().unwrap()
                                        > 20
                                })
                                .count();
                            if image.width() != frame.width
                                || image.height() != frame.height
                                || colored < image.width() as usize * image.height() as usize / 20
                            {
                                return Err(
                                    "The native snapshot contains no visible video frame".into()
                                );
                            }
                            std::fs::write(self.capture_path.as_ref().unwrap(), &frame.png)
                                .map_err(|_| {
                                    "The explicit private snapshot path could not be written"
                                })?;
                            eprintln!(
                                "Native snapshot verified: {}x{}, {} bytes, timing bracket {:.3}s",
                                frame.width,
                                frame.height,
                                frame.png.len(),
                                frame.sampling_uncertainty_seconds
                            );
                            self.capture_verified = true;
                        }
                        if !self.capture_verified {
                            return Ok(false);
                        }
                    }
                    if elapsed >= Duration::from_secs(2) {
                        self.command(PlaybackCommand::SeekPaused(self.target - 30.25))?;
                        self.next(Phase::PausedSeekSettles, state.seconds);
                    }
                }
                Phase::PausedSeekSettles => {
                    if !state.playing
                        && !state.buffering
                        && state.seeking.is_none()
                        && (state.seconds - (self.target - 30.25)).abs() <= 0.25
                    {
                        self.next(Phase::PausedSeekHolds, state.seconds);
                    }
                }
                Phase::PausedSeekHolds => {
                    if state.playing || (state.seconds - self.sample).abs() > 0.25 {
                        return Err("A paused seek resumed or changed its precise position".into());
                    }
                    if elapsed >= Duration::from_secs(2) {
                        self.command(PlaybackCommand::Seek(self.target))?;
                        self.next(Phase::SeekSettles, state.seconds);
                    }
                }
                Phase::SeekSettles => {
                    if state.playing
                        && !state.buffering
                        && state.seeking.is_none()
                        && self.near(&state, self.target, 2.0)
                    {
                        self.next(Phase::SeekAdvances, state.seconds);
                    }
                }
                Phase::SeekAdvances => {
                    if state.playing && state.seconds >= self.sample + 1.0 {
                        self.player.as_ref().unwrap().set_visible(false);
                        self.next(Phase::Hidden, state.seconds);
                    }
                }
                Phase::Hidden => {
                    if elapsed >= Duration::from_secs(1) {
                        self.player.as_ref().unwrap().set_visible(true);
                        self.next(Phase::Shown, self.sample);
                    }
                }
                Phase::Shown => {
                    self.assert_continuity(&state)?;
                    if elapsed >= Duration::from_secs(2)
                        && state.playing
                        && state.seconds >= self.sample + 1.0
                    {
                        self.resized = true;
                        self.next(Phase::Resized, state.seconds);
                    }
                }
                Phase::Resized => {
                    self.assert_continuity(&state)?;
                    if elapsed >= Duration::from_secs(2)
                        && state.playing
                        && state.seconds >= self.sample + 1.0
                    {
                        if let Some((url, _)) = &self.alternate {
                            self.player
                                .as_mut()
                                .unwrap()
                                .load_replay(ctx, url, TEST_TOKEN)
                                .map_err(|_| {
                                    "The native player could not load the alternate POV"
                                })?;
                            if self.player.as_ref().unwrap().playback_state().ready {
                                return Err(
                                    "The alternate POV retained the old document's ready state"
                                        .into(),
                                );
                            }
                            self.next(Phase::AlternateSettles, state.seconds);
                        } else {
                            return Ok(true);
                        }
                    }
                }
                Phase::AlternateSettles => {
                    let target = self.alternate.as_ref().unwrap().1;
                    if state.ready
                        && !state.playing
                        && !state.buffering
                        && state.seeking.is_none()
                        && state.playback_intent.is_none()
                        && (state.seconds - target).abs() <= 0.25
                    {
                        self.next(Phase::AlternateHolds, state.seconds);
                    }
                }
                Phase::AlternateHolds => {
                    if state.playing
                        || state.buffering
                        || (state.seconds - self.sample).abs() > 0.25
                    {
                        return Err("The alternate paused POV moved or resumed".into());
                    }
                    let hold = if self.switch_rounds > 1 { 5 } else { 2 };
                    if elapsed >= Duration::from_secs(hold) {
                        eprintln!(
                            "Native replay settled: switch={}, provider=alternate, paused=true, elapsed={:.3}",
                            self.completed_rounds * 2 + 1,
                            self.started.elapsed().as_secs_f64()
                        );
                        self.player
                            .as_mut()
                            .unwrap()
                            .load_replay(ctx, &self.url, TEST_TOKEN)
                            .map_err(|_| {
                                "The native player could not return to the original POV"
                            })?;
                        self.next(Phase::ReturnSettles, state.seconds);
                    }
                }
                Phase::ReturnSettles => {
                    if state.ready
                        && state.playing
                        && !state.buffering
                        && state.seeking.is_none()
                        && self.near(&state, self.offset, 3.0)
                    {
                        self.next(Phase::ReturnAdvances, state.seconds);
                    }
                }
                Phase::ReturnAdvances => {
                    let hold = if self.switch_rounds > 1 { 5.0 } else { 1.0 };
                    if state.playing && state.seconds >= self.sample + hold {
                        self.completed_rounds += 1;
                        eprintln!(
                            "Native replay settled: switch={}, provider=original, paused=false, elapsed={:.3}",
                            self.completed_rounds * 2,
                            self.started.elapsed().as_secs_f64()
                        );
                        if self.completed_rounds == self.switch_rounds {
                            return Ok(true);
                        }
                        self.player
                            .as_mut()
                            .unwrap()
                            .load_replay(ctx, &self.alternate.as_ref().unwrap().0, TEST_TOKEN)
                            .map_err(|_| "The repeated POV switch failed")?;
                        if self.player.as_ref().unwrap().playback_state().ready {
                            return Err("The repeated POV retained stale readiness".into());
                        }
                        self.next(Phase::AlternateSettles, state.seconds);
                    }
                }
            }
            Ok(false)
        }

        fn near(&self, state: &PlaybackState, target: f64, after: f64) -> bool {
            (target - 0.25..=target + after).contains(&state.seconds)
        }

        fn assert_continuity(&self, state: &PlaybackState) -> Result<(), String> {
            let elapsed = self.phase_started.elapsed().as_secs_f64();
            if state.seconds < self.sample - 0.5 || state.seconds > self.sample + elapsed + 4.0 {
                return Err(format!(
                    "The video position reset during {}",
                    self.phase.label()
                ));
            }
            Ok(())
        }
    }

    impl eframe::App for Driver {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            if self.finished {
                return;
            }
            // This also covers a native window that never reaches its first UI pass.
            if self.started.elapsed() > self.deadline && self.player.is_none() {
                self.finish(ctx, Err("The native replay player never opened".into()));
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
            if self.finished {
                return;
            }
            ui.heading("Brick · Native replay smoke");
            ui.label(self.phase.label());
            ui.label("One player instance · local fixture · no account sign-in");
            ui.add_space(12.0);
            let size = if self.resized {
                egui::vec2(680.0, 390.0)
            } else {
                egui::vec2(900.0, 510.0)
            };
            let (rect, _) =
                ui.allocate_exact_size(size.min(ui.available_size()), egui::Sense::hover());
            let ctx = ui.ctx().clone();
            if !self.activated {
                if let Err(error) = activate_native_window(frame) {
                    self.finish(&ctx, Err(error));
                } else {
                    self.activated = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                // Let the native focus events arrive before constructing WebKit
                // or WebView2, matching an already-open Brick window.
                return;
            }
            if let Some(player) = &mut self.player {
                if player.set_bounds(rect, ctx.pixels_per_point()).is_err() {
                    self.finish(
                        &ctx,
                        Err("The native replay player could not resize".into()),
                    );
                }
                return;
            }
            if self.outcome.lock().unwrap().creations != 0 {
                self.finish(
                    &ctx,
                    Err("The replay attempted to recreate its player".into()),
                );
                return;
            }
            match StreamPlayer::new(
                frame,
                &ctx,
                &self.url,
                TEST_TOKEN,
                rect,
                ctx.pixels_per_point(),
                None,
            ) {
                Ok(player) => {
                    self.player_id = player.diagnostic_player_id();
                    self.player = Some(Box::new(player));
                    self.outcome.lock().unwrap().creations += 1;
                }
                Err(_) => self.finish(
                    &ctx,
                    Err("The native replay player could not be created".into()),
                ),
            }
        }
    }

    #[test]
    #[ignore = "requires an explicit replay metadata fixture, localhost service and a native desktop"]
    fn real_replay_controls() {
        assert_eq!(
            option_env!("BRICK_PRESENCE_API_URL"),
            Some(LOCAL_ENDPOINT),
            "Build this smoke test only against the isolated loopback service"
        );
        let path = std::env::var_os("BRICK_REPLAY_FIXTURE")
            .expect("Set BRICK_REPLAY_FIXTURE to an explicit local replay metadata file");
        let file =
            std::fs::File::open(path).expect("The explicit replay fixture could not be opened");
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .expect("The replay fixture could not be read");
        assert!(bytes.len() <= 65536, "The replay fixture is too large");
        let replay: Replay = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("The replay fixture must contain Replay metadata JSON"));
        assert!(
            replay.provider == Provider::Youtube,
            "This smoke test requires a YouTube replay"
        );
        let offset = std::env::var("BRICK_REPLAY_TEST_OFFSET")
            .map(|value| {
                value
                    .parse::<f64>()
                    .expect("BRICK_REPLAY_TEST_OFFSET must be seconds")
            })
            .unwrap_or(10.0);
        assert!(
            replay.available_seconds <= 604800
                && offset.is_finite()
                && offset >= 0.0
                && offset + 90.0 < replay.available_seconds as f64,
            "The replay needs at least 90 seconds after the selected test offset"
        );
        let mut url = url::Url::parse(LOCAL_ENDPOINT).unwrap();
        let member = std::env::var_os("BRICK_REPLAY_TEST_MEMBER").map(|value| {
            value
                .into_string()
                .expect("The primary fixture member must be ASCII text")
        });
        let member =
            primary_fixture_member(member.as_deref()).unwrap_or_else(|error| panic!("{error}"));
        url.set_path(&format!("/v1/streams/player/{member}/youtube"));
        url.query_pairs_mut()
            .append_pair("at", &format!("{offset:.3}"))
            .append_pair("broadcast", &replay.broadcast_id);
        let saved = std::env::var("BRICK_REPLAY_SAVED_RECORDING").as_deref() == Ok("1");
        if saved {
            url.query_pairs_mut()
                .append_pair("recording", &replay.video_id);
        }
        let alternate = std::env::var_os("BRICK_REPLAY_ALT_FIXTURE").map(|path| {
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .expect("The alternate metadata fixture could not be opened")
                .take(65537)
                .read_to_end(&mut bytes)
                .expect("The alternate fixture could not be read");
            assert!(bytes.len() <= 65536, "The alternate fixture is too large");
            let replay: Replay = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                panic!("The alternate fixture must contain Replay metadata JSON")
            });
            let offset = std::env::var("BRICK_REPLAY_ALT_OFFSET")
                .map(|value| {
                    value
                        .parse::<f64>()
                        .expect("The alternate offset must be seconds")
                })
                .unwrap_or(10.125);
            assert!(
                offset.is_finite()
                    && offset >= 0.0
                    && offset + 30.0 < replay.available_seconds.min(604800) as f64,
                "Invalid alternate test offset"
            );
            let mut alternate = url::Url::parse(LOCAL_ENDPOINT).unwrap();
            alternate.set_path(&format!(
                "/v1/streams/player/{}/{}",
                if replay.provider == Provider::Youtube {
                    "102"
                } else {
                    "103"
                },
                replay.provider.key()
            ));
            alternate
                .query_pairs_mut()
                .append_pair("at", &format!("{offset:.3}"))
                .append_pair("broadcast", &replay.broadcast_id)
                .append_pair("paused", "1");
            if saved {
                alternate
                    .query_pairs_mut()
                    .append_pair("recording", &replay.video_id);
            }
            (alternate.into(), offset)
        });
        // Explicit resource/stability probe only; the ordinary regression keeps
        // its original single round trip and 55-second deadline.
        let switch_rounds = std::env::var("BRICK_REPLAY_SWITCH_ROUNDS")
            .map(|value| {
                value
                    .parse::<usize>()
                    .expect("Switch rounds must be an integer")
            })
            .unwrap_or(1);
        assert!(
            (1..=5).contains(&switch_rounds),
            "Use 1–5 switch round trips"
        );
        assert!(
            switch_rounds == 1 || alternate.is_some(),
            "Repeated switches need an alternate fixture"
        );
        let deadline = DEADLINE + Duration::from_secs((switch_rounds as u64 - 1) * 45);
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let app_outcome = outcome.clone();
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("Brick · Native replay smoke")
                .with_inner_size([960.0, 660.0])
                .with_active(true)
                .with_position([20.0, 20.0]),
            event_loop_builder: Some(Box::new(|builder| {
                #[cfg(target_os = "linux")]
                {
                    use winit::platform::x11::EventLoopBuilderExtX11 as _;
                    builder.with_x11().with_any_thread(true);
                }
                #[cfg(target_os = "windows")]
                {
                    use winit::platform::windows::EventLoopBuilderExtWindows as _;
                    builder.with_any_thread(true);
                }
            })),
            ..Default::default()
        };
        let result = eframe::run_native(
            "Brick native replay smoke",
            options,
            Box::new(move |cc| {
                cc.egui_ctx.set_visuals(egui::Visuals::dark());
                let now = Instant::now();
                Ok(Box::new(Driver {
                    player: None,
                    outcome: app_outcome,
                    url: url.into(),
                    alternate,
                    switch_rounds,
                    completed_rounds: 0,
                    deadline,
                    player_id: None,
                    capture_path: std::env::var_os("BRICK_REPLAY_CAPTURE")
                        .map(std::path::PathBuf::from),
                    capture_requested: false,
                    capture_verified: false,
                    offset,
                    target: offset + 60.375,
                    started: now,
                    phase_started: now,
                    phase: Phase::Autoplay,
                    sample: 0.0,
                    resized: false,
                    finished: false,
                    activated: false,
                    diagnostics: std::env::var_os("BRICK_REPLAY_DIAGNOSTICS").is_some(),
                    last_diagnostic: now,
                }))
            }),
        );
        assert!(
            result.is_ok(),
            "The native replay smoke window could not run"
        );
        let outcome = outcome.lock().unwrap();
        match &outcome.result {
            Some(Ok(())) => (),
            Some(Err(error)) => panic!("{error}"),
            None => panic!("The native replay smoke window closed before verification finished"),
        }
        assert_eq!(
            outcome.creations, 1,
            "Replay controls must preserve one player instance"
        );
        eprintln!("Native replay verified: autoplay, pause, precise paused seek, resumed seek, hide/show and resize; one player instance");
    }
}
