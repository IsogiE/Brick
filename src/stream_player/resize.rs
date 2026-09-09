use wry::WebView;

pub(super) fn set_visible(view: &WebView, visible: bool, bounds: [i32; 4]) -> wry::Result<()> {
    view.set_visible(visible)?;
    // GTK restores the size remembered before the child was resized directly
    // by Wry. Reapply the current bounds after showing it, even when the app's
    // layout has not changed (for example, closing a comparison dropdown).
    #[cfg(target_os = "linux")]
    if visible {
        view.set_bounds(super::wry_bounds(bounds))?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = bounds;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "requires a native display"]
    fn native_player_viewport_shrinks_for_compare_and_grows_for_single_view() {
        struct App {
            view: Option<WebView>,
            started: Instant,
            phase: usize,
            measured: Arc<Mutex<Option<String>>>,
            waiting: bool,
            resizing: bool,
            outcome: Arc<Mutex<Result<bool, String>>>,
        }
        const SIZES: [(i32, i32); 8] = [
            (900, 500),
            (440, 500),
            (440, 500),
            (900, 500),
            (900, 500),
            (350, 300),
            (350, 300),
            (900, 500),
        ];
        impl eframe::App for App {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                super::super::pump_events();
                if self.phase == SIZES.len() {
                    return;
                }
                let ctx = ui.ctx();
                if self.started.elapsed() > Duration::from_secs(20) {
                    *self.outcome.lock().unwrap() =
                        Err(format!("Resize phase {} timed out", self.phase));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                let (width, height) = SIZES[self.phase];
                if self.view.is_none() {
                    #[cfg(target_os = "linux")]
                    super::super::initialize_gtk().unwrap();
                    let view=wry::WebViewBuilder::new()
                        .with_bounds(super::super::wry_bounds([20,40,width,height]))
                        .with_html("<!doctype html><style>html,body,iframe{margin:0;width:100%;height:100%;border:0;overflow:hidden}</style><iframe srcdoc=\"<style>html,body{margin:0;width:100%;height:100%}</style>Resize fixture\"></iframe>")
                        .build_as_child(frame).unwrap();
                    if let Ok(url) = std::env::var("BRICK_RESIZE_URL") {
                        assert!(url.starts_with("http://127.0.0.1:18083/v1/streams/player/"));
                        let mut headers = wry::http::HeaderMap::new();
                        headers.insert(
                            wry::http::header::AUTHORIZATION,
                            "Bearer local-test-101".parse().unwrap(),
                        );
                        view.load_url_with_headers(&url, headers).unwrap();
                    }
                    self.view = Some(view);
                }
                let view = self.view.as_ref().unwrap();
                if let Some(value) = self.measured.lock().unwrap().take() {
                    self.waiting = false;
                    let actual: Vec<f64> = serde_json::from_str(&value).unwrap();
                    let scale = actual[2];
                    let expected = [width as f64 / scale, height as f64 / scale];
                    eprintln!("phase {} expected={expected:?} DOM={actual:?}", self.phase);
                    let correct = actual.len() == 5
                        && (actual[0] - expected[0]).abs() <= 1.0
                        && (actual[1] - expected[1]).abs() <= 1.0
                        && (actual[3] - expected[0]).abs() <= 1.0
                        && (actual[4] - expected[1]).abs() <= 1.0;
                    if !correct {
                        *self.outcome.lock().unwrap() = Err(format!(
                            "Compare viewport cropped: expected {expected:?}, got {actual:?}"
                        ));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    self.phase += 1;
                    if self.phase == SIZES.len() {
                        *self.outcome.lock().unwrap() = Ok(true);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    super::set_visible(view, false, [20, 40, width, height]).unwrap();
                    self.resizing = true;
                    self.started = Instant::now();
                }
                if self.resizing && self.started.elapsed() > Duration::from_millis(300) {
                    super::set_visible(view, true, [20, 40, width, height]).unwrap();
                    let (w, h) = SIZES[self.phase];
                    if SIZES[self.phase] != SIZES[self.phase - 1] {
                        view.set_bounds(super::super::wry_bounds([20, 40, w, h]))
                            .unwrap();
                    }
                    self.resizing = false;
                }
                if !self.waiting
                    && self.started.elapsed()
                        > Duration::from_secs(if std::env::var_os("BRICK_RESIZE_URL").is_some() {
                            8
                        } else {
                            1
                        })
                {
                    let measured = self.measured.clone();
                    view.evaluate_script_with_callback("[innerWidth,innerHeight,devicePixelRatio,document.querySelector('iframe').clientWidth,document.querySelector('iframe').clientHeight]",move |value| *measured.lock().unwrap()=Some(value)).unwrap();
                    self.waiting = true;
                }
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }
        let outcome = Arc::new(Mutex::new(Ok(false)));
        let result = outcome.clone();
        eframe::run_native(
            "Brick player resize test",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 600.0]),
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
            },
            Box::new(move |_| {
                Ok(Box::new(App {
                    view: None,
                    started: Instant::now(),
                    phase: 0,
                    measured: Arc::new(Mutex::new(None)),
                    waiting: false,
                    resizing: false,
                    outcome,
                }))
            }),
        )
        .unwrap();
        assert_eq!(*result.lock().unwrap(), Ok(true));
    }
}
