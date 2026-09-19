//! Explicit interactive regression using real WebView2 and a loopback player service.
use super::*;
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetParent, IsWindow,
};

fn children(frame: &eframe::Frame) -> Vec<isize> {
    let RawWindowHandle::Win32(handle) = frame.window_handle().unwrap().as_raw() else {
        panic!("The fixture requires a Windows desktop")
    };
    let owner = handle.hwnd.get();
    unsafe extern "system" fn collect(
        hwnd: windows_sys::Win32::Foundation::HWND,
        data: isize,
    ) -> i32 {
        let (owner, found) = unsafe { &mut *(data as *mut (isize, Vec<isize>)) };
        let mut name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, name.as_mut_ptr(), 64) };
        if unsafe { GetParent(hwnd) } as isize == *owner
            && len > 0
            && String::from_utf16_lossy(&name[..len as usize]) == "WRY_WEBVIEW"
        {
            found.push(hwnd as isize);
        }
        1
    }
    let mut found = (owner, Vec::new());
    unsafe {
        EnumChildWindows(owner as _, Some(collect), &mut found as *mut _ as isize);
    }
    found.1
}

struct Driver {
    streams: StreamsUi,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    started: Instant,
    stage_at: Instant,
    stage: u8,
    cycles: u8,
    retired: Vec<isize>,
    url: String,
    baseline: Option<f64>,
}

impl Driver {
    fn prepare(&mut self) {
        let (tx, rx) = mpsc::channel();
        tx.send(Ok((self.url.clone(), "local-test-101".into(), None)))
            .unwrap();
        self.streams.player_work = Some(rx);
        self.streams.player_attempted = true;
    }

    fn next(&mut self, stage: u8) {
        self.stage = stage;
        self.stage_at = Instant::now();
        self.baseline = None;
        eprintln!(
            "Windows recovery fixture stage {stage}, cycles {}",
            self.cycles
        );
    }

    fn finish(&mut self, ctx: &egui::Context, result: Result<(), String>) {
        self.streams.stop_player();
        *self.result.lock().unwrap() = Some(result);
        self.stage = 9;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn advance(&mut self, frame: &eframe::Frame, ctx: &egui::Context) -> Result<(), String> {
        if self.started.elapsed() > Duration::from_secs(180) {
            return Err(format!(
                "Windows recovery fixture timed out in stage {}",
                self.stage
            ));
        }
        self.streams.provider_sessions.tick(ctx);
        if let Some(player) = &mut self.streams.player {
            player.poll_playback(ctx);
        }
        crate::stream_player::pump_events();
        if let Some(error) = self.streams.player.as_ref().and_then(StreamPlayer::failure) {
            if !matches!(self.stage, 3 | 5) {
                return Err(format!(
                    "Unexpected native failure in stage {}: {error}",
                    self.stage
                ));
            }
            self.streams.recover_player(error);
        }
        match self.stage {
            0 => {
                self.prepare();
                self.next(1);
            }
            1 if self.streams.player.is_some() => {
                // Close before playback readiness, as rapid navigation does.
                self.retired = children(frame);
                if self.retired.len() != 1 {
                    return Err("Expected exactly one native player child".into());
                }
                self.streams.stop_player();
                if self
                    .retired
                    .iter()
                    .any(|hwnd| unsafe { IsWindow(*hwnd as _) } != 0)
                    || !children(frame).is_empty()
                {
                    return Err("Rapid navigation retained a native player child".into());
                }
                self.cycles += 1;
                self.prepare();
                if self.cycles == 12 {
                    self.next(2);
                }
            }
            2 | 4 => {
                if let Some(player) = &self.streams.player {
                    let state = player.playback_state();
                    if state.is_fresh() && state.ready && state.playing {
                        let baseline = self.baseline.get_or_insert(state.seconds);
                        if state.seconds > *baseline + 1.0 {
                            if children(frame).len() != 1 {
                                return Err("Playback retained extra native player children".into());
                            }
                            self.retired = children(frame);
                            // Obtain the PID from this exact WebView2 controller;
                            // never select or terminate unrelated Edge processes.
                            player.diagnostic_terminate_browser()?;
                            self.next(if self.stage == 2 { 3 } else { 5 });
                        }
                    }
                }
            }
            3 if self.streams.player.is_none() => {
                if self.streams.player_retry_at.is_none() || self.streams.player_retries != 1 {
                    return Err("The first native crash did not schedule recovery".into());
                }
                if !children(frame).is_empty() {
                    return Err("The crashed player left an overlay".into());
                }
                // Only replace the credential/network preparation worker. Keep
                // the real retry deadline and native construction path intact.
                self.prepare();
                self.next(4);
            }
            5 if self.streams.player.is_none() => {
                if self.streams.player_retry_at.is_some() || self.streams.player_error.is_none() {
                    return Err("Repeated native crashes did not stop at the retry limit".into());
                }
                if !children(frame).is_empty() {
                    return Err("Repeated crash left a native overlay".into());
                }
                self.next(6);
            }
            6 if self.stage_at.elapsed() > Duration::from_secs(2) => {
                if self.streams.player.is_some() || !children(frame).is_empty() {
                    return Err("The failed player unexpectedly reopened".into());
                }
                self.finish(ctx, Ok(()));
            }
            _ => {}
        }
        if matches!(self.stage, 3 | 5) && self.stage_at.elapsed() > Duration::from_secs(10) {
            return Err("WebView2 process failure was not detected within ten seconds".into());
        }
        Ok(())
    }
}

impl eframe::App for Driver {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.stage == 9 {
            return;
        }
        let ctx = ui.ctx().clone();
        ui.heading("Brick · Windows replay recovery test");
        ui.label(format!(
            "Stage {} · rapid switches {} / 12",
            self.stage, self.cycles
        ));
        if let Some(error) = &self.streams.player_error {
            ui.label(error);
        }
        let rect = ui.available_rect_before_wrap();
        if let Err(error) = self.advance(frame, &ctx) {
            self.finish(&ctx, Err(error));
            return;
        }
        self.streams.player_rect = Some(rect);
        if self.stage < 6 {
            self.streams.update_player(frame, &ctx, true);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

#[test]
#[ignore = "requires an isolated Windows desktop and real loopback replay fixture"]
fn rapid_navigation_and_browser_crash_recover_without_orphans() {
    use winit::platform::windows::EventLoopBuilderExtWindows as _;
    assert_eq!(
        option_env!("BRICK_PRESENCE_API_URL"),
        Some("http://127.0.0.1:18083")
    );
    assert_eq!(
        std::env::var("BRICK_WINDOWS_RECOVERY_FIXTURE").as_deref(),
        Ok("1")
    );
    let result = Arc::new(Mutex::new(None));
    let app_result = result.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Brick · Windows replay recovery test")
            .with_inner_size([1000.0, 720.0])
            .with_position([20.0, 20.0]),
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_any_thread(true);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Brick Windows replay recovery",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(Driver {
                streams: StreamsUi::default(),
                result: app_result,
                started: Instant::now(),
                stage_at: Instant::now(),
                stage: 0,
                cycles: 0,
                retired: Vec::new(),
                baseline: None,
                url: "http://127.0.0.1:18083/v1/streams/player/101/youtube?at=10".into(),
            }))
        }),
    )
    .unwrap();
    let result = result.lock().unwrap();
    assert!(matches!(*result, Some(Ok(()))), "{result:?}");
}
