//! Observe completion of the same eframe callbacks used by `run_native`.
//! UI entry alone is not frame completion: rendering and swap happen afterward.
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Arc,
};
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, StartCause, WindowEvent},
    event_loop::ActiveEventLoop,
    window::WindowId,
};

#[derive(Default)]
pub struct Completion {
    in_flight: AtomicUsize,
    callbacks: AtomicUsize,
    completed_ui_passes: AtomicUsize,
}
impl Completion {
    /// Only accept a snapshot outside a callback, with all observed UI work done.
    pub fn idle_snapshot(&self, ui_passes: usize) -> Option<(usize, usize)> {
        let generation = self.callbacks.load(SeqCst);
        if self.in_flight.load(SeqCst) != 0 {
            return None;
        }
        let completed = self.completed_ui_passes.load(SeqCst);
        if completed != ui_passes || completed == 0 {
            return None;
        }
        if self.in_flight.load(SeqCst) != 0 || self.callbacks.load(SeqCst) != generation {
            return None;
        }
        Some((generation, completed))
    }
}

struct CallbackGuard<'a> {
    completion: &'a Completion,
    ui_passes: &'a dyn Fn() -> usize,
    kind: &'a str,
}
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        let passes = (self.ui_passes)();
        self.completion.completed_ui_passes.store(passes, SeqCst);
        self.completion.callbacks.fetch_add(1, SeqCst);
        self.completion.in_flight.fetch_sub(1, SeqCst);
        log::trace!(target: "native_rendering", "callback_end kind={} completed_ui_passes={passes}", self.kind);
    }
}

pub struct ObservedApplication<'a> {
    inner: eframe::EframeWinitApplication<'a>,
    completion: Arc<Completion>,
    ui_passes: Box<dyn Fn() -> usize>,
}
impl<'a> ObservedApplication<'a> {
    pub fn new(
        inner: eframe::EframeWinitApplication<'a>,
        completion: Arc<Completion>,
        ui_passes: impl Fn() -> usize + 'static,
    ) -> Self {
        Self {
            inner,
            completion,
            ui_passes: Box::new(ui_passes),
        }
    }
    fn forward(&mut self, kind: &str, call: impl FnOnce(&mut eframe::EframeWinitApplication<'a>)) {
        self.completion.in_flight.fetch_add(1, SeqCst);
        log::trace!(target: "native_rendering", "callback_begin kind={kind}");
        let completion = CallbackGuard {
            completion: &self.completion,
            ui_passes: self.ui_passes.as_ref(),
            kind,
        };
        call(&mut self.inner);
        drop(completion);
    }
}
impl ApplicationHandler<eframe::UserEvent> for ObservedApplication<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.forward("resumed", |inner| inner.resumed(event_loop));
    }
    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.forward("suspended", |inner| inner.suspended(event_loop));
    }
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        self.forward("new_events", |inner| inner.new_events(event_loop, cause));
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: eframe::UserEvent) {
        self.forward("user_event", |inner| inner.user_event(event_loop, event));
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.forward("window_event", |inner| {
            inner.window_event(event_loop, id, event)
        });
    }
    fn device_event(&mut self, event_loop: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
        self.forward("device_event", |inner| {
            inner.device_event(event_loop, id, event)
        });
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.forward("about_to_wait", |inner| inner.about_to_wait(event_loop));
    }
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.forward("exiting", |inner| inner.exiting(event_loop));
    }
    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.forward("memory_warning", |inner| inner.memory_warning(event_loop));
    }
}
