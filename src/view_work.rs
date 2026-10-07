//! Serialized device settings work. The UI only consumes completed snapshots.
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{mpsc, Arc},
    thread,
};

use eframe::egui;

use crate::{
    addon::{self, AppView},
    autostart,
};

#[derive(Clone, Debug)]
pub(crate) enum Request {
    Refresh { reconcile_startup: bool, sync: bool },
    AddFolders(Vec<PathBuf>),
    RemoveFolder(String),
    StartupEnabled(bool),
    StartupMinimized(bool),
}

impl Request {
    fn mutates(&self) -> bool {
        !matches!(self, Self::Refresh { .. })
    }

    fn sync_after(&self) -> bool {
        matches!(self, Self::Refresh { sync: true, .. } | Self::AddFolders(_))
    }
}

pub(crate) struct Snapshot {
    pub(crate) view: AppView,
    pub(crate) message: Option<String>,
    pub(crate) operation_succeeded: bool,
}

pub(crate) struct Completion {
    pub(crate) result: Result<Snapshot, String>,
    pub(crate) sync: bool,
}

type Handler = dyn Fn(Request) -> Result<Snapshot, String> + Send + Sync;

pub(crate) struct ViewWork {
    handler: Arc<Handler>,
    pending: Option<(Request, mpsc::Receiver<Completion>)>,
    queue: VecDeque<Request>,
    stopped: bool,
}

impl Default for ViewWork {
    fn default() -> Self {
        Self {
            handler: Arc::new(execute),
            pending: None,
            queue: VecDeque::new(),
            stopped: false,
        }
    }
}

impl ViewWork {
    pub(crate) fn busy(&self) -> bool {
        self.pending.is_some() || !self.queue.is_empty()
    }

    pub(crate) fn mutating(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|(request, _)| request.mutates())
            || self.queue.iter().any(Request::mutates)
    }

    pub(crate) fn request(&mut self, request: Request, ctx: &egui::Context) {
        if self.stopped {
            return;
        }
        // Keep a refresh after in-flight work: that work may have read the old
        // state before an addon sync finished. Only adjacent queued refreshes
        // coalesce; settings mutations retain their order.
        if let (
            Some(Request::Refresh {
                reconcile_startup,
                sync,
            }),
            Request::Refresh {
                reconcile_startup: next_reconcile,
                sync: next_sync,
            },
        ) = (self.queue.back_mut(), &request)
        {
            *reconcile_startup |= next_reconcile;
            *sync |= next_sync;
        } else {
            self.queue.push_back(request);
        }
        self.start_next(ctx);
    }

    fn start_next(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || self.stopped {
            return;
        }
        let Some(request) = self.queue.pop_front() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let handler = self.handler.clone();
        let wake_ctx = ctx.clone();
        let work = request.clone();
        let spawn = thread::Builder::new()
            .name("brick-settings".into())
            .spawn(move || {
                let sync = work.sync_after();
                let result = catch_unwind(AssertUnwindSafe(|| handler(work)))
                    .unwrap_or_else(|_| Err("Couldn't refresh Brick settings. Try again.".into()));
                let sync = sync
                    && result
                        .as_ref()
                        .is_ok_and(|snapshot| snapshot.operation_succeeded);
                let _ = tx.send(Completion { result, sync });
                // Errors and panics complete through the same wakeup path.
                wake_ctx.request_repaint();
            });
        self.pending = Some((request, rx));
        if spawn.is_err() {
            // The disconnected receiver is consumed by the next UI pass.
            ctx.request_repaint();
        }
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Option<Completion> {
        let (_, rx) = self.pending.as_ref()?;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Completion {
                result: Err("Couldn't refresh Brick settings. Try again.".into()),
                sync: false,
            },
        };
        self.pending = None;
        self.start_next(ctx);
        Some(result)
    }

    /// Erasure discards queued commands and snapshots without waiting on disk.
    /// Existing storage write fences protect against an admitted operation
    /// recreating files or startup registration after the device reset.
    pub(crate) fn stop(&mut self) {
        self.stopped = true;
        self.queue.clear();
        self.pending = None;
    }
}

fn execute(request: Request) -> Result<Snapshot, String> {
    if crate::account_erasure::requests_blocked() {
        return Err("Account deletion is pending.".into());
    }
    let recover = request.mutates();
    let result = execute_request(request);
    finish_request(
        result,
        recover && !crate::account_erasure::requests_blocked(),
        addon::load_view,
        |error| {
            let _ = addon::record_log(addon::LogLevel::Error, error);
        },
    )
}

// Settings writes can succeed before constructing AppView fails. Recover the
// latest snapshot once so the visible controls reflect any committed write,
// while preserving the original error and avoiding a sync on failed intent.
fn finish_request(
    result: Result<Snapshot, String>,
    recover: bool,
    reload: impl FnOnce() -> Result<AppView, String>,
    log_error: impl FnOnce(String),
) -> Result<Snapshot, String> {
    match result {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => {
            log_error(error.clone());
            if recover {
                if let Ok(view) = reload() {
                    return Ok(Snapshot {
                        view,
                        message: Some(error),
                        operation_succeeded: false,
                    });
                }
            }
            Err(error)
        }
    }
}

fn startup_warning(result: Result<(), String>) -> Option<String> {
    result.err().inspect(|error| {
        let _ = addon::record_log(addon::LogLevel::Warn, error.clone());
    })
}

fn execute_request(request: Request) -> Result<Snapshot, String> {
    let (view, message) = match request {
        Request::Refresh {
            reconcile_startup, ..
        } => {
            let view = addon::load_view()?;
            let message = if reconcile_startup && !view.setup_required {
                startup_warning(autostart::reconcile_enabled(view.settings.startup_enabled))
            } else {
                None
            };
            (view, message)
        }
        Request::AddFolders(paths) => {
            let view = addon::add_wow_paths(&paths)?;
            let message = startup_warning(autostart::set_enabled(view.settings.startup_enabled))
                .unwrap_or_else(|| "WoW folders saved.".into());
            (view, Some(message))
        }
        Request::RemoveFolder(id) => (
            addon::remove_client(&id)?,
            Some("WoW folder removed.".into()),
        ),
        Request::StartupEnabled(enabled) => {
            let view = addon::set_startup_enabled(enabled)?;
            let message = startup_warning(autostart::set_enabled(enabled)).unwrap_or_else(|| {
                if enabled {
                    "Brick will open at login."
                } else {
                    "Brick will stay closed at login."
                }
                .into()
            });
            (view, Some(message))
        }
        Request::StartupMinimized(enabled) => (
            addon::set_startup_minimized(enabled)?,
            Some(
                if enabled {
                    "Brick will start minimized."
                } else {
                    "Brick will open at login."
                }
                .into(),
            ),
        ),
    };
    Ok(Snapshot {
        view,
        message,
        operation_succeeded: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn refresh() -> Request {
        Request::Refresh {
            reconcile_startup: false,
            sync: false,
        }
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            view: AppView::default(),
            message: None,
            operation_succeeded: true,
        }
    }
    fn receive(work: &mut ViewWork, ctx: &egui::Context) -> Completion {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(done) = work.poll(ctx) {
                return done;
            }
            assert!(Instant::now() < deadline, "worker failed to complete");
            thread::yield_now();
        }
    }

    #[test]
    fn slow_io_does_not_block_ui_and_mutations_preserve_snapshot_order() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let mut work = ViewWork {
            handler: Arc::new(move |request| {
                entered_tx.send(request.clone()).unwrap();
                if matches!(request, Request::Refresh { .. }) {
                    release_rx.lock().unwrap().recv().unwrap();
                }
                let mut result = snapshot();
                if let Request::StartupEnabled(enabled) = request {
                    result.view.settings.startup_enabled = enabled;
                }
                Ok(result)
            }),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        work.request(refresh(), &ctx);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        work.request(Request::StartupEnabled(false), &ctx);
        for _ in 0..100 {
            assert!(work.poll(&ctx).is_none());
        }
        assert!(work.mutating());
        assert!(
            entered_rx.try_recv().is_err(),
            "commands must not race a snapshot read"
        );
        release_tx.send(()).unwrap();
        assert!(
            receive(&mut work, &ctx)
                .result
                .unwrap()
                .view
                .settings
                .startup_enabled
        );
        assert!(
            !receive(&mut work, &ctx)
                .result
                .unwrap()
                .view
                .settings
                .startup_enabled
        );
        assert!(!work.busy());
    }

    #[test]
    fn queued_refreshes_coalesce_without_losing_sync_or_startup_intent() {
        let mut work = ViewWork::default();
        let (_tx, rx) = mpsc::channel();
        work.pending = Some((refresh(), rx));
        let ctx = egui::Context::default();
        for _ in 0..100 {
            work.request(refresh(), &ctx);
        }
        work.request(
            Request::Refresh {
                reconcile_startup: true,
                sync: true,
            },
            &ctx,
        );
        assert_eq!(work.queue.len(), 1);
        assert!(matches!(
            work.queue.front(),
            Some(Request::Refresh {
                reconcile_startup: true,
                sync: true
            })
        ));
        work.request(Request::StartupEnabled(false), &ctx);
        work.request(refresh(), &ctx);
        assert_eq!(
            work.queue.len(),
            3,
            "refreshes must not overtake settings edits"
        );
    }

    #[test]
    fn erasure_discards_results_and_queued_work_without_joining_a_worker() {
        let mut work = ViewWork::default();
        let (tx, rx) = mpsc::channel();
        work.pending = Some((refresh(), rx));
        let ctx = egui::Context::default();
        work.request(Request::StartupEnabled(true), &ctx);
        work.stop();
        assert!(tx
            .send(Completion {
                result: Ok(snapshot()),
                sync: true
            })
            .is_err());
        work.request(refresh(), &ctx);
        assert!(!work.busy());
        assert!(work.poll(&ctx).is_none());
    }

    #[test]
    fn partial_mutation_failure_recovers_committed_state_without_starting_sync() {
        let ctx = egui::Context::default();
        let mut work = ViewWork {
            handler: Arc::new(|_| {
                finish_request(
                    Err("The folder was saved, but its display metadata could not be read.".into()),
                    true,
                    || {
                        let mut view = AppView::default();
                        view.settings.startup_enabled = false;
                        Ok(view)
                    },
                    |error| assert!(error.starts_with("The folder was saved")),
                )
            }),
            ..Default::default()
        };
        work.request(Request::AddFolders(vec![PathBuf::from("fixture")]), &ctx);
        let completion = receive(&mut work, &ctx);
        assert!(
            !completion.sync,
            "a recovered view must not turn a failed mutation into success"
        );
        let snapshot = completion.result.unwrap();
        assert!(!snapshot.view.settings.startup_enabled);
        assert!(snapshot
            .message
            .unwrap()
            .starts_with("The folder was saved"));
        assert!(!work.busy());
    }

    #[test]
    fn failed_recovery_is_bounded_and_keeps_original_error() {
        let mut reloads = 0;
        let mut logged = None;
        let result = finish_request(
            Err("Original settings error".into()),
            true,
            || {
                reloads += 1;
                Err("Recovery also failed".into())
            },
            |error| logged = Some(error),
        );
        assert_eq!(reloads, 1);
        assert_eq!(logged.as_deref(), Some("Original settings error"));
        assert!(matches!(result, Err(error) if error == "Original settings error"));
    }

    #[test]
    fn refresh_failure_does_not_retry_or_replace_previous_view() {
        let result = finish_request(
            Err("Snapshot read failed".into()),
            false,
            || panic!("read failures must wait for the normal next refresh"),
            |_| {},
        );
        assert!(matches!(result, Err(error) if error == "Snapshot read failed"));
    }

    #[test]
    fn worker_panic_completes_and_wakes_without_stranding_pending_state() {
        let mut work = ViewWork {
            handler: Arc::new(|_| panic!("test failure")),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let (wake_tx, wake_rx) = mpsc::channel();
        ctx.set_request_repaint_callback(move |_| {
            let _ = wake_tx.send(());
        });
        work.request(refresh(), &ctx);
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(receive(&mut work, &ctx).result.is_err());
        assert!(!work.busy());
    }
}
