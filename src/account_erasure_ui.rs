use crate::account_erasure::{self, Identity, Receipt};
use eframe::egui::{self, RichText};
use std::sync::mpsc;

enum Finished {
    Server(Receipt),
    LocalRetry,
}

#[derive(Default)]
pub(crate) struct ErasureUi {
    open: bool,
    pending: bool,
    identity: Option<Identity>,
    identity_rx: Option<mpsc::Receiver<Result<Identity, String>>>,
    deletion_rx: Option<mpsc::Receiver<Result<Finished, String>>>,
    error: Option<String>,
    receipt: Option<Receipt>,
    finished: bool,
}

impl ErasureUi {
    pub(crate) fn new() -> Self {
        Self {
            pending: account_erasure::pending()
                || crate::local_erasure::is_pending().unwrap_or(true),
            ..Self::default()
        }
    }

    pub(crate) fn blocks_normal_use(&self) -> bool {
        self.pending || self.finished
    }
    pub(crate) fn modal_open(&self) -> bool {
        self.open
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context) {
        match crate::local_erasure::is_pending() {
            Ok(true) => {
                self.pending = true;
                self.open = false;
                self.error = None;
                let (tx, rx) = mpsc::channel();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(crate::local_erasure::reset().map(|()| Finished::LocalRetry));
                    ctx.request_repaint();
                });
                self.deletion_rx = Some(rx);
                return;
            }
            Err(error) => {
                self.error = Some(error);
                return;
            }
            Ok(false) => (),
        }
        self.open = true;
        if self.identity.is_none() && self.identity_rx.is_none() && self.deletion_rx.is_none() {
            self.verify(ctx);
        }
    }

    fn verify(&mut self, ctx: &egui::Context) {
        self.error = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(account_erasure::prepare());
            ctx.request_repaint();
        });
        self.identity_rx = Some(rx);
    }

    pub(crate) fn poll(&mut self) {
        if let Some(rx) = &self.identity_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.identity_rx = None;
                    match result {
                        Ok(identity) => self.identity = Some(identity),
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.identity_rx = None;
                    self.error = Some("Couldn't verify your account. Try again.".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if let Some(rx) = &self.deletion_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.deletion_rx = None;
                    self.open = false;
                    match result {
                        Ok(done) => {
                            self.finished = true;
                            self.receipt = match done {
                                Finished::Server(receipt) => Some(receipt),
                                Finished::LocalRetry => None,
                            };
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.deletion_rx = None;
                    self.open = false;
                    self.error = Some("Deletion is pending. Try again.".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
    }

    /// Returns the confirmed operation only after journaling intent. The owner
    /// must stop native views and normal workers before starting this identity.
    pub(crate) fn draw_confirmation(&mut self, ctx: &egui::Context) -> Option<Identity> {
        if !self.open || self.deletion_rx.is_some() {
            return None;
        }
        let mut cancel = false;
        let mut confirm = false;
        let mut retry = false;
        let modal = egui::Modal::new(egui::Id::new("erase-account-confirmation"))
            .frame(crate::ui::panel_frame())
            .show(ctx, |ui| {
                ui.set_width(420.0_f32.min((ctx.content_rect().width() - 64.0).max(280.0)));
                ui.heading("Delete my data");
                ui.add_space(10.0);
                if let Some(identity) = &self.identity {
                    ui.label(RichText::new(&identity.name).strong());
                    ui.label("Delete your Brick data from every guild and reset Brick on this device. This cannot be undone.");
                    ui.add_space(8.0);
                    ui.label("A minimal exclusion record prevents old devices and backups from recreating your account. Your account will remain excluded from Brick.");
                    ui.add_space(8.0);
                    ui.label("Google channel access is revoked across the same app project. Your provider accounts and WoW installation remain.");
                } else if self.identity_rx.is_some() {
                    ui.label("Verifying your Discord account…");
                }
                if let Some(error) = &self.error {
                    ui.add_space(8.0);
                    ui.label(error);
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    cancel = crate::ui::secondary_button(ui, "Cancel").clicked();
                    if self.identity.is_some() {
                        confirm = crate::ui::danger_button(ui, "Delete my data").clicked();
                    } else if self.identity_rx.is_none() {
                        retry = crate::ui::secondary_button(ui, "Try again").clicked();
                    }
                });
            });
        if retry {
            self.verify(ctx);
        }
        if cancel || modal.should_close() {
            self.open = false;
            self.identity = None;
            self.identity_rx = None;
            self.error = None;
        } else if confirm {
            let identity = self.identity.as_ref()?;
            match account_erasure::remember(identity) {
                Ok(()) => {
                    self.pending = true;
                    self.open = false;
                    self.error = None;
                    return self.identity.take();
                }
                Err(error) => self.error = Some(error),
            }
        }
        None
    }

    pub(crate) fn start(&mut self, ctx: &egui::Context, identity: Identity) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(account_erasure::finish(&identity).map(Finished::Server));
            ctx.request_repaint();
        });
        self.deletion_rx = Some(rx);
    }

    pub(crate) fn draw_pending(&mut self, ui: &mut egui::Ui) -> bool {
        let mut close = false;
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            if self.finished {
                ui.heading(
                    if self.receipt.as_ref().is_some_and(|receipt| {
                        receipt.active_data_deleted && !receipt.backups_pending
                    }) {
                        "Data deleted"
                    } else if self.receipt.is_none() {
                        "Local data removed"
                    } else {
                        "Deletion requested"
                    },
                );
                ui.label("Brick’s data on this device has been removed.");
                if self
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.backups_pending)
                {
                    ui.label("Server cleanup is still pending.");
                } else if self.receipt.is_none() {
                    ui.label("Server deletion status is unavailable.");
                }
                ui.add_space(16.0);
                close = crate::ui::secondary_button(ui, "Close Brick").clicked();
            } else {
                ui.heading("Deletion pending");
                if self.deletion_rx.is_some() {
                    ui.label("Removing your data…");
                } else {
                    if let Some(error) = &self.error {
                        ui.label(error);
                    }
                    ui.add_space(16.0);
                    if crate::ui::secondary_button(ui, "Continue deletion").clicked() {
                        self.show(ui.ctx());
                    }
                }
            }
        });
        close
    }
}
