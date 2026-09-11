//! Cut native media out of egui's floating areas without hiding its document.
//! Menus and tooltips must neither unmap the player nor restart playback.

use eframe::egui;
use std::cell::RefCell;
use wry::WebView;

#[cfg(target_os = "windows")]
mod windows_focus;

#[derive(Default)]
pub(super) struct Controller {
    applied: RefCell<Option<([i32; 4], Vec<[i32; 4]>)>>,
    fullscreen_cover: std::cell::Cell<bool>,
    #[cfg(test)]
    updates: std::cell::Cell<usize>,
}

impl Controller {
    pub fn cover_for_fullscreen(&self, covered: bool) {
        self.fullscreen_cover.set(covered);
    }

    fn cuts(&self, ctx: &egui::Context, bounds: [i32; 4]) -> Vec<[i32; 4]> {
        if self.fullscreen_cover.get() {
            // The existing comparison peer keeps its document and media clock
            // visible to the provider, while none of its pixels accept input.
            vec![[0, 0, bounds[2], bounds[3]]]
        } else {
            overlay_rects(ctx, bounds)
        }
    }

    pub fn update(&self, view: &WebView, ctx: &egui::Context, bounds: [i32; 4]) {
        #[cfg(target_os = "windows")]
        windows_focus::update(view, ctx);
        let cuts = self.cuts(ctx, bounds);
        let mut applied = self.applied.borrow_mut();
        if applied
            .as_ref()
            .is_some_and(|last| last.0 == bounds && last.1 == cuts)
        {
            return;
        }
        if apply(view, bounds, &cuts).is_ok() {
            *applied = Some((bounds, cuts));
            #[cfg(test)]
            self.updates.set(self.updates.get() + 1);
            // Area visibility also includes the previous frame. One settled
            // frame clears a closed tooltip; stable regions never animate.
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

fn overlay_rects(ctx: &egui::Context, bounds: [i32; 4]) -> Vec<[i32; 4]> {
    let scale = ctx.pixels_per_point();
    let player = egui::Rect::from_min_size(
        egui::pos2(bounds[0] as f32, bounds[1] as f32),
        egui::vec2(bounds[2] as f32, bounds[3] as f32),
    );
    let areas = ctx.memory(|memory| {
        memory
            .areas()
            .visible_layer_ids()
            .into_iter()
            .filter(|layer| layer.order >= egui::Order::Middle)
            .filter_map(|layer| memory.area_rect(layer.id).map(|rect| (layer, rect)))
            .collect::<Vec<_>>()
    });
    let mut cuts = Vec::with_capacity(areas.len());
    for (layer, rect) in areas {
        let rect = ctx
            .layer_transform_to_global(layer)
            .map_or(rect, |t| t * rect);
        // Reveal only the floating UI itself. Cutting extra space for shadows
        // exposes the opaque app background around the video as a black halo.
        let cut = (rect * scale).intersect(player);
        if cut.is_positive() && cut.is_finite() {
            cuts.push([
                (cut.left() - player.left()).floor() as i32,
                (cut.top() - player.top()).floor() as i32,
                (cut.right() - player.left()).ceil() as i32,
                (cut.bottom() - player.top()).ceil() as i32,
            ]);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

#[cfg(target_os = "linux")]
fn apply(view: &WebView, bounds: [i32; 4], cuts: &[[i32; 4]]) -> Result<(), ()> {
    xshape::apply(view, bounds, cuts)
}

#[cfg(target_os = "linux")]
mod xshape {
    use gtk::{glib::translate::ToGlibPtr, prelude::*};
    #[cfg(test)]
    use std::ffi::c_uint;
    use std::ffi::{c_int, c_ulong, c_void};
    use wry::{WebView, WebViewExtUnix};

    const BOUNDING: c_int = 0;
    const SET: c_int = 0;
    const SUBTRACT: c_int = 3;
    const UNSORTED: c_int = 0;

    #[repr(C)]
    struct Rectangle {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    }

    #[link(name = "gdk-3")]
    unsafe extern "C" {
        fn gdk_x11_window_get_xid(window: *mut gtk::gdk::ffi::GdkWindow) -> c_ulong;
        fn gdk_x11_display_get_xdisplay(display: *mut gtk::gdk::ffi::GdkDisplay) -> *mut c_void;
    }
    #[link(name = "Xext")]
    unsafe extern "C" {
        fn XShapeQueryExtension(
            display: *mut c_void,
            event: *mut c_int,
            error: *mut c_int,
        ) -> c_int;
        fn XShapeCombineMask(
            display: *mut c_void,
            window: c_ulong,
            kind: c_int,
            x: c_int,
            y: c_int,
            source: c_ulong,
            op: c_int,
        );
        fn XShapeCombineRectangles(
            display: *mut c_void,
            window: c_ulong,
            kind: c_int,
            x: c_int,
            y: c_int,
            rectangles: *const Rectangle,
            count: c_int,
            op: c_int,
            ordering: c_int,
        );
        #[cfg(test)]
        fn XShapeQueryExtents(
            display: *mut c_void,
            window: c_ulong,
            bounding: *mut c_int,
            bx: *mut c_int,
            by: *mut c_int,
            bw: *mut c_uint,
            bh: *mut c_uint,
            clip: *mut c_int,
            cx: *mut c_int,
            cy: *mut c_int,
            cw: *mut c_uint,
            ch: *mut c_uint,
        ) -> c_int;
    }
    #[link(name = "X11")]
    unsafe extern "C" {
        fn XFlush(display: *mut c_void) -> c_int;
    }

    fn handles(view: &WebView) -> Result<(*mut c_void, c_ulong), ()> {
        let window = view
            .webview()
            .toplevel()
            .and_then(|widget| widget.window())
            .ok_or(())?;
        // SAFETY: Wry is pinned to its X11 child backend and both GDK objects
        // remain owned by this WebView on the GTK/UI thread.
        let (display, xid) = unsafe {
            (
                gdk_x11_display_get_xdisplay(window.display().to_glib_none().0),
                gdk_x11_window_get_xid(window.to_glib_none().0),
            )
        };
        if display.is_null() || xid == 0 {
            return Err(());
        }
        Ok((display, xid))
    }

    pub(super) fn apply(view: &WebView, bounds: [i32; 4], cuts: &[[i32; 4]]) -> Result<(), ()> {
        let (display, xid) = handles(view)?;
        let rect = |cut: [i32; 4]| -> Result<Rectangle, ()> {
            Ok(Rectangle {
                x: cut[0].try_into().map_err(|_| ())?,
                y: cut[1].try_into().map_err(|_| ())?,
                width: (cut[2] - cut[0]).try_into().map_err(|_| ())?,
                height: (cut[3] - cut[1]).try_into().map_err(|_| ())?,
            })
        };
        let full = rect([0, 0, bounds[2], bounds[3]])?;
        let holes = cuts
            .iter()
            .copied()
            .map(rect)
            .collect::<Result<Vec<_>, _>>()?;
        // GDK records shape state but does not apply it to Wry's foreign X11
        // window. Shape the actual child through XShape, on its existing display.
        // Bounding regions also bound input, so clicks reach the floating egui UI.
        // Coordinates here are native pixels; GDK logical scaling is bypassed.
        // SAFETY: all pointers live through these synchronous Xlib calls, and the
        // child XID belongs to the live WebView. No map, resize or media command.
        unsafe {
            let (mut event, mut error) = (0, 0);
            if XShapeQueryExtension(display, &mut event, &mut error) == 0 {
                return Err(());
            }
            if holes.is_empty() {
                XShapeCombineMask(display, xid, BOUNDING, 0, 0, 0, SET);
            } else {
                XShapeCombineRectangles(display, xid, BOUNDING, 0, 0, &full, 1, SET, UNSORTED);
                for hole in &holes {
                    XShapeCombineRectangles(
                        display, xid, BOUNDING, 0, 0, hole, 1, SUBTRACT, UNSORTED,
                    );
                }
            }
            XFlush(display);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn shaped(view: &WebView) -> bool {
        let (display, xid) = handles(view).unwrap();
        let (
            mut bounding,
            mut bx,
            mut by,
            mut bw,
            mut bh,
            mut clip,
            mut cx,
            mut cy,
            mut cw,
            mut ch,
        ) = (0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        unsafe {
            assert_ne!(
                XShapeQueryExtents(
                    display,
                    xid,
                    &mut bounding,
                    &mut bx,
                    &mut by,
                    &mut bw,
                    &mut bh,
                    &mut clip,
                    &mut cx,
                    &mut cy,
                    &mut cw,
                    &mut ch
                ),
                0
            );
        }
        bounding != 0
    }
}

#[cfg(target_os = "windows")]
fn apply(view: &WebView, bounds: [i32; 4], cuts: &[[i32; 4]]) -> Result<(), ()> {
    use windows_sys::Win32::{
        Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, RGN_DIFF},
        UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_STYLE, WS_CHILD},
    };
    use wry::WebViewExtWindows;
    let mut parent = windows::Win32::Foundation::HWND::default();
    // SAFETY: the controller and its Wry child are accessed only on the UI
    // thread. Never shape a top-level application window.
    unsafe {
        view.controller()
            .ParentWindow(&mut parent)
            .map_err(|_| ())?;
        if parent.0.is_null() || GetWindowLongPtrW(parent.0, GWL_STYLE) & WS_CHILD as isize == 0 {
            return Err(());
        }
        if cuts.is_empty() {
            return (SetWindowRgn(parent.0, std::ptr::null_mut(), 1) != 0)
                .then_some(())
                .ok_or(());
        }
        let region = CreateRectRgn(0, 0, bounds[2], bounds[3]);
        if region.is_null() {
            return Err(());
        }
        for cut in cuts {
            let hole = CreateRectRgn(cut[0], cut[1], cut[2], cut[3]);
            if hole.is_null() {
                DeleteObject(region);
                return Err(());
            }
            let result = CombineRgn(region, region, hole, RGN_DIFF);
            DeleteObject(hole);
            if result == 0 {
                DeleteObject(region);
                return Err(());
            }
        }
        // On success Windows owns the region; on failure the caller still does.
        if SetWindowRgn(parent.0, region, 1) == 0 {
            DeleteObject(region);
            return Err(());
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn apply(_: &WebView, _: [i32; 4], _: &[[i32; 4]]) -> Result<(), ()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_floating_areas_cut_the_player_and_closing_restores_it() {
        let ctx = egui::Context::default();
        let bounds = [100, 100, 600, 400];
        let mut cuts = Vec::new();
        for show in [true, true, false, false] {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show_inside(ui, |_| {});
                if show {
                    egui::Area::new(egui::Id::new("large-buff-tooltip"))
                        .order(egui::Order::Tooltip)
                        .fixed_pos(egui::pos2(150.0, 150.0))
                        .show(&ctx, |ui| {
                            ui.allocate_space(egui::vec2(200.0, 240.0));
                        });
                }
                cuts = overlay_rects(&ctx, bounds);
            });
            if show {
                assert!(!cuts.is_empty());
                assert!(
                    cuts.iter().all(|cut| {
                        cut[0] >= 50 && cut[1] >= 50 && cut[2] <= 250 && cut[3] <= 290
                    }),
                    "Player must remain visible outside the floating UI bounds"
                );
            }
        }
        assert!(
            cuts.is_empty(),
            "Closed tooltips must restore the full player"
        );
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::{Duration, Instant},
    };

    // Exercise real native clipping and the media clock without network, login,
    // or provider availability. The silent WAV stays entirely in browser RAM.
    const HTML: &str = r#"<!doctype html><style>body{margin:0;background:#21664e;color:white;font:24px sans-serif}</style>
      Player continues beneath menus<audio id="media" autoplay loop></audio>
      <script>
      window.changes=0;document.addEventListener('visibilitychange',()=>changes++);
      const b=new ArrayBuffer(44+320000),d=new DataView(b),s=(o,t)=>[...t].forEach((c,i)=>d.setUint8(o+i,c.charCodeAt(0)));
      s(0,'RIFF');d.setUint32(4,b.byteLength-8,true);s(8,'WAVE');s(12,'fmt ');d.setUint32(16,16,true);d.setUint16(20,1,true);d.setUint16(22,1,true);d.setUint32(24,8000,true);d.setUint32(28,16000,true);d.setUint16(32,2,true);d.setUint16(34,16,true);s(36,'data');d.setUint32(40,320000,true);
      media.src=URL.createObjectURL(new Blob([b],{type:'audio/wav'}));media.play().catch(()=>{});
      </script>"#;

    #[test]
    #[ignore = "requires a native display and media runtime"]
    fn native_overlays_preserve_media_visibility_size_and_playback() {
        struct App {
            view: Option<WebView>,
            loaded: Arc<AtomicBool>,
            controller: Controller,
            started: Instant,
            phase_at: Instant,
            phase: usize,
            previous: f64,
            cycles: usize,
            completed_cycles: usize,
            frames: usize,
            measured: Arc<Mutex<Option<String>>>,
            pending: bool,
            result: Arc<Mutex<Result<bool, String>>>,
        }
        const BOUNDS: [i32; 4] = [20, 40, 600, 360];
        impl eframe::App for App {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                crate::stream_player::pump_events();
                let ctx = ui.ctx();
                self.frames += 1;
                if self.started.elapsed() > Duration::from_secs(25 + 12 * (self.cycles as u64 - 1))
                {
                    *self.result.lock().unwrap() =
                        Err(format!("Media overlay phase {} timed out", self.phase));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                if self.phase == 6 {
                    return;
                }
                if self.view.is_none() {
                    #[cfg(target_os = "linux")]
                    crate::stream_player::initialize_gtk().unwrap();
                    let loaded = self.loaded.clone();
                    let page_ctx = ctx.clone();
                    self.view = Some(
                        wry::WebViewBuilder::new()
                            .with_on_page_load_handler(move |event, _| {
                                loaded.store(
                                    matches!(event, wry::PageLoadEvent::Finished),
                                    Ordering::Release,
                                );
                                page_ctx.request_repaint();
                            })
                            .with_bounds(crate::stream_player::wry_bounds(BOUNDS))
                            .with_autoplay(true)
                            .with_html(HTML)
                            .build_as_child(frame)
                            .unwrap(),
                    );
                    self.phase_at = Instant::now();
                }
                match self.phase {
                    1 => {
                        egui::Window::new("Settings over video")
                            .fixed_pos(egui::pos2(60.0, 80.0))
                            .resizable(false)
                            .show(ctx, |ui| {
                                ui.set_min_size(egui::vec2(180.0, 100.0));
                                ui.label("The same video keeps playing.");
                            });
                    }
                    2 => {
                        egui::Area::new(egui::Id::new("buff-tooltip"))
                            .order(egui::Order::Tooltip)
                            .fixed_pos(egui::pos2(40.0, 60.0))
                            .show(ctx, |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    ui.set_min_width(300.0);
                                    for i in 0..12 {
                                        ui.label(format!(
                                            "Buff {i}: Apotheosis / healing cooldown"
                                        ));
                                    }
                                });
                            });
                    }
                    3 => {
                        egui::Area::new(egui::Id::new("cover-player"))
                            .order(egui::Order::Foreground)
                            .fixed_pos(egui::pos2(20.0, 40.0))
                            .show(ctx, |ui| {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(600.0, 360.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter()
                                    .rect_filled(rect, 0.0, egui::Color32::DARK_BLUE);
                            });
                    }
                    _ => {}
                }
                let view = self.view.as_ref().unwrap();
                self.controller.cover_for_fullscreen(self.phase == 4);
                self.controller.update(view, ctx, BOUNDS);
                if let Some(value) = self.measured.lock().unwrap().take() {
                    self.pending = false;
                    let actual: serde_json::Value =
                        serde_json::from_str(&value).unwrap_or_default();
                    let seconds = actual["seconds"].as_f64().unwrap_or(0.0);
                    // A cold media process may need longer than the first sample.
                    // The global deadline still fails an unavailable runtime.
                    if self.phase == 0 && (seconds < 0.3 || actual["paused"] != false) {
                        self.phase_at = Instant::now();
                        ctx.request_repaint_after(Duration::from_millis(100));
                        return;
                    }
                    eprintln!("Overlay phase {}: {actual}", self.phase);
                    if std::env::var_os("BRICK_NATIVE_OVERLAY_DIAGNOSTICS").is_some() {
                        eprintln!(
                            "Overlay scheduling rendered={} passes={} causes={:?}",
                            ctx.cumulative_frame_nr(),
                            ctx.cumulative_pass_nr(),
                            ctx.repaint_causes()
                        );
                    }
                    if actual["visible"] != "visible"
                        || actual["changes"] != 0
                        || actual["paused"] != false
                        || {
                            let delta = if seconds >= self.previous {
                                seconds - self.previous
                            } else {
                                seconds + actual["duration"].as_f64().unwrap_or(0.0) - self.previous
                            };
                            delta <= 0.2
                        }
                        || (actual["width"].as_f64().unwrap_or(0.0)
                            * actual["scale"].as_f64().unwrap_or(1.0)
                            - BOUNDS[2] as f64)
                            .abs()
                            > 1.0
                        || (actual["height"].as_f64().unwrap_or(0.0)
                            * actual["scale"].as_f64().unwrap_or(1.0)
                            - BOUNDS[3] as f64)
                            .abs()
                            > 1.0
                    {
                        *self.result.lock().unwrap() = Err(format!(
                            "Menu interrupted media at phase {}: {actual}",
                            self.phase
                        ));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                    let cuts = self.controller.cuts(ctx, BOUNDS);
                    assert_eq!(
                        cuts.is_empty(),
                        self.phase == 0 || self.phase == 5,
                        "Native player must only be cut beneath floating UI areas"
                    );
                    #[cfg(target_os = "linux")]
                    {
                        assert_eq!(xshape::shaped(view), !cuts.is_empty());
                    }
                    #[cfg(target_os = "windows")]
                    unsafe {
                        use windows_sys::Win32::Graphics::Gdi::{
                            CreateRectRgn, DeleteObject, GetWindowRgn, PtInRegion,
                        };
                        use wry::WebViewExtWindows;
                        let mut parent = windows::Win32::Foundation::HWND::default();
                        view.controller().ParentWindow(&mut parent).unwrap();
                        let region = CreateRectRgn(0, 0, 0, 0);
                        let kind = GetWindowRgn(parent.0, region);
                        if !cuts.is_empty() {
                            assert_ne!(kind, 0);
                            assert_eq!(PtInRegion(region, cuts[0][0] + 1, cuts[0][1] + 1), 0);
                        }
                        DeleteObject(region);
                    }
                    self.previous = seconds;
                    self.phase += 1;
                    if self.phase == 6 {
                        self.completed_cycles += 1;
                        if self.completed_cycles == self.cycles {
                            let elapsed = self.started.elapsed().as_secs_f64();
                            eprintln!("Overlay stress cycles={} passes={} seconds={elapsed:.2} region_updates={}",
                                self.cycles, self.frames, self.controller.updates.get());
                            assert!(
                                self.controller.updates.get() <= 16 * self.cycles,
                                "Stable overlay regions were reapplied repeatedly"
                            );
                            *self.result.lock().unwrap() = Ok(true);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            return;
                        }
                        self.phase = 0;
                    }
                    self.phase_at = Instant::now();
                }
                // Do not submit callback-bearing scripts before Wry's initial
                // page commit; its Linux pending-script queue drops callbacks.
                if self.loaded.load(Ordering::Acquire)
                    && !self.pending
                    && self.phase_at.elapsed() > Duration::from_secs(2)
                {
                    self.pending = true;
                    let measured = self.measured.clone();
                    view.evaluate_script_with_callback("JSON.stringify({visible:document.visibilityState,changes:window.changes,seconds:media.currentTime,duration:media.duration,paused:media.paused,width:innerWidth,height:innerHeight,scale:devicePixelRatio})", move |value| {
                        let value = serde_json::from_str::<String>(&value).unwrap_or(value);
                        *measured.lock().unwrap() = Some(value);
                    }).unwrap();
                }
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        let result = Arc::new(Mutex::new(Ok(false)));
        let out = result.clone();
        eframe::run_native(
            "Brick native overlay test",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 480.0]),
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
                    loaded: Arc::new(AtomicBool::new(false)),
                    controller: Controller::default(),
                    started: Instant::now(),
                    phase_at: Instant::now(),
                    phase: 0,
                    previous: 0.0,
                    cycles: std::env::var("BRICK_NATIVE_OVERLAY_CYCLES")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(1)
                        .clamp(1, 12),
                    completed_cycles: 0,
                    frames: 0,
                    measured: Arc::new(Mutex::new(None)),
                    pending: false,
                    result,
                }))
            }),
        )
        .unwrap();
        assert_eq!(*out.lock().unwrap(), Ok(true));
    }
}
