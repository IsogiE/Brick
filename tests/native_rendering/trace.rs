//! Opt-in buffered diagnostics for the isolated rendering regression only.
use std::{
    fmt::Write as _,
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering::SeqCst},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::GetUpdateRect,
    UI::WindowsAndMessaging::{
        CallWindowProcW, GetWindowRect, SetWindowLongPtrW, GWLP_WNDPROC, WINDOWPOS, WM_ACTIVATE,
        WM_DPICHANGED, WM_ERASEBKGND, WM_GETOBJECT, WM_KILLFOCUS, WM_MOUSEMOVE, WM_NCDESTROY,
        WM_PAINT, WM_SETFOCUS, WM_SHOWWINDOW, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
    },
};

static TRACE: OnceLock<Trace> = OnceLock::new();
static PHASE: AtomicUsize = AtomicUsize::new(0);
static RENDERER_FAILED: AtomicBool = AtomicBool::new(false);
static PREVIOUS: AtomicIsize = AtomicIsize::new(0);
const PHASES: &[&str] = &[
    "startup",
    "startup_settle",
    "programmatic_move",
    "modal_drag",
    "input",
    "resize",
    "os_paint",
    "restore",
    "content",
    "surface_cycles",
    "done",
];
struct Trace {
    verbose: bool,
    start: Instant,
    records: Mutex<Vec<String>>,
}
impl log::Log for Trace {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        (metadata.level() == log::Level::Error && metadata.target().starts_with("eframe"))
            || (self.verbose
                && (metadata.target().starts_with("eframe::native")
                    || metadata.target() == "native_rendering"))
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            if record.level() == log::Level::Error && record.target().starts_with("eframe") {
                RENDERER_FAILED.store(true, SeqCst);
            }
            let micros = self.start.elapsed().as_micros();
            let phase = PHASES[PHASE.load(SeqCst)];
            if let Ok(mut records) = self.records.lock() {
                if records.len() < 100_000 {
                    records.push(format!(
                        "TRACE us={micros} phase={phase} thread={:?} {} {}",
                        std::thread::current().id(),
                        record.target(),
                        record.args()
                    ));
                }
            }
        }
    }
    fn flush(&self) {}
}

pub fn init() {
    let verbose = std::env::var("BRICK_RENDERING_TRACE").as_deref() == Ok("1");
    let trace = TRACE.get_or_init(|| Trace {
        verbose,
        start: Instant::now(),
        records: Mutex::new(Vec::with_capacity(8192)),
    });
    log::set_logger(trace).expect("Test trace logger already installed");
    log::set_max_level(if verbose {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Error
    });
    log::trace!(target: "native_rendering", "trace_begin");
}
pub fn assert_renderer_succeeded() {
    // create_native exposes the ApplicationHandler but not eframe's stored Result.
    // Preserve run_native's fail-closed behavior even if a final swap fails after
    // the application has already observed its final screenshot/input callback.
    assert!(
        !RENDERER_FAILED.load(SeqCst),
        "eframe reported a native renderer error"
    );
}
pub fn phase(phase: usize) {
    PHASE.store(phase, SeqCst);
    log::trace!(target: "native_rendering", "phase_begin");
}
pub fn ui_begin(ctx: &eframe::egui::Context, count: usize) {
    log::trace!(target: "native_rendering", "ui_begin count={count} cumulative_frame={} cumulative_pass={} pass_index={}", ctx.cumulative_frame_nr(), ctx.cumulative_pass_nr(), ctx.current_pass_index());
    if count == 1 {
        // Controlled reproduction of the old idle detector's in-flight-frame race.
        if let Ok(delay) = std::env::var("BRICK_RENDERING_FIRST_UI_DELAY_MS") {
            let delay: u64 = delay.parse().expect("Invalid diagnostic first-frame delay");
            assert!(delay <= 2000, "Diagnostic first-frame delay exceeds bound");
            log::trace!(target: "native_rendering", "first_ui_delay_begin ms={delay}");
            std::thread::sleep(Duration::from_millis(delay));
            log::trace!(target: "native_rendering", "first_ui_delay_end");
        }
    }
}
pub fn dump() {
    let Some(trace) = TRACE.get() else {
        return;
    };
    let records = trace.records.lock().expect("Trace lock poisoned");
    assert!(
        records.len() < 100_000,
        "Trace overflowed; no complete causal evidence"
    );
    let mut output = String::new();
    for record in records.iter() {
        writeln!(output, "{record}").unwrap();
    }
    eprint!("{output}");
}

pub fn observe_native(hwnd: HWND) {
    if !TRACE.get().is_some_and(|trace| trace.verbose) {
        return;
    }
    // SAFETY: Called on the owning UI thread for this process's test HWND only.
    let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, observer as *const () as isize) };
    assert_ne!(previous, 0, "Could not observe native test-window messages");
    PREVIOUS.store(previous, SeqCst);
}
unsafe extern "system" fn observer(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let label = match message {
        WM_GETOBJECT => "WM_GETOBJECT",
        WM_PAINT => "WM_PAINT",
        WM_ERASEBKGND => "WM_ERASEBKGND",
        WM_SHOWWINDOW => "WM_SHOWWINDOW",
        WM_WINDOWPOSCHANGED => "WM_WINDOWPOSCHANGED",
        WM_WINDOWPOSCHANGING => "WM_WINDOWPOSCHANGING",
        WM_DPICHANGED => "WM_DPICHANGED",
        WM_ACTIVATE => "WM_ACTIVATE",
        WM_SETFOCUS => "WM_SETFOCUS",
        WM_KILLFOCUS => "WM_KILLFOCUS",
        WM_MOUSEMOVE => "WM_MOUSEMOVE",
        WM_NCDESTROY => "WM_NCDESTROY",
        _ => "",
    };
    if !label.is_empty() {
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect);
        }
        log::trace!(target: "native_rendering", "native_begin {label} rect={},{},{},{}", rect.left, rect.top, rect.right, rect.bottom);
        if message == WM_PAINT {
            let mut update = RECT::default();
            let invalid = unsafe { GetUpdateRect(hwnd, &mut update, 0) };
            log::trace!(target: "native_rendering", "native_update_region invalid={invalid} rect={},{},{},{}", update.left, update.top, update.right, update.bottom);
        }
        if message == WM_WINDOWPOSCHANGED || message == WM_WINDOWPOSCHANGING {
            let pos = unsafe { &*(lparam as *const WINDOWPOS) };
            log::trace!(target: "native_rendering", "native_window_pos x={} y={} cx={} cy={} flags={:#x}", pos.x, pos.y, pos.cx, pos.cy, pos.flags);
        }
    }
    // SAFETY: The saved procedure was returned by Windows for this exact HWND.
    // Every message and return value is forwarded unchanged, including destruction.
    let previous = unsafe {
        std::mem::transmute::<isize, unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(
            PREVIOUS.load(SeqCst),
        )
    };
    let result = unsafe { CallWindowProcW(Some(previous), hwnd, message, wparam, lparam) };
    if !label.is_empty() {
        log::trace!(target: "native_rendering", "native_end {label}");
    }
    result
}
