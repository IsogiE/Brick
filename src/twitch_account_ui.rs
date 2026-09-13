//! Home's personal Twitch connection. Guild sharing uses only channel().url.
use crate::twitch_account::{Account, Channel};
use eframe::egui::{self, RichText};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

enum Action {
    Restore,
    Check,
    Connect,
    Disconnect,
}
type WorkResult = (Account, Result<(), String>);

#[derive(Default)]
pub struct TwitchUi {
    account: Option<Account>,
    work: Option<mpsc::Receiver<WorkResult>>,
    cancel: Option<Arc<AtomicBool>>,
    generation: Option<u64>,
    initialized: bool,
    channel: Option<Channel>,
    next_check: Option<Instant>,
    notice: Option<String>,
    foreground: bool,
    connecting: bool,
}

impl Drop for TwitchUi {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

impl TwitchUi {
    pub fn busy(&self) -> bool {
        self.work.is_some()
    }
    pub fn channel(&self) -> Option<&Channel> {
        self.generation
            .filter(|g| *g == crate::guild::generation())
            .and(self.channel.as_ref())
    }
    pub fn tick(&mut self, ctx: &egui::Context) {
        // Defense in depth if a caller retains Home across a guild/account
        // transition. A cancelled refresh may finish persisting its rotation,
        // but its result can never populate the next account's panel.
        let generation = crate::guild::generation();
        if self.generation.is_some_and(|old| old != generation) {
            *self = Self::default();
        }
        self.generation = Some(generation);
        if let Some(rx) = &self.work {
            match rx.try_recv() {
                Ok((account, result)) => {
                    self.work = None;
                    self.cancel = None;
                    self.channel = if account.connected() {
                        account.channel().cloned()
                    } else {
                        None
                    };
                    match result {
                        Ok(()) => {
                            self.notice = None;
                            self.next_check =
                                account.check_after().map(|delay| Instant::now() + delay);
                        }
                        Err(error) => {
                            if self.foreground || account.needs_reconnect() {
                                self.notice = Some(error);
                            }
                            self.next_check = (!account.needs_reconnect())
                                .then(|| Instant::now() + Duration::from_secs(15 * 60));
                        }
                    }
                    self.account = Some(account);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.work = None;
                    self.cancel = None;
                    if self.foreground {
                        self.notice = Some("Twitch connection stopped. Try again.".into());
                    }
                    self.next_check = Some(Instant::now() + Duration::from_secs(15 * 60));
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        if !self.initialized && Account::configured() {
            self.initialized = true;
            self.start(ctx, Action::Restore);
        } else if !self.busy()
            && self.account.as_ref().is_some_and(|a| !a.needs_reconnect())
            && self.next_check.is_some_and(|at| Instant::now() >= at)
        {
            self.start(ctx, Action::Check);
        }
        if let Some(at) = self.next_check {
            ctx.request_repaint_after(
                at.saturating_duration_since(Instant::now())
                    .max(Duration::from_secs(1)),
            );
        }
    }
    fn start(&mut self, ctx: &egui::Context, action: Action) {
        if self.busy() {
            return;
        }
        let account = match self.account.take().map(Ok).unwrap_or_else(Account::new) {
            Ok(account) => account,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        self.foreground = matches!(action, Action::Connect | Action::Disconnect);
        self.connecting = matches!(action, Action::Connect);
        if self.foreground {
            self.notice = None;
        }
        self.next_check = None;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        crate::guild::spawn(move || {
            let mut account = account;
            let result =
                crate::discord_auth::current_access_token().and_then(|access| match action {
                    Action::Restore => account.restore(&access, &cancel),
                    // Reload the protected grant so a previously locked
                    // keyring can recover, and another panel's disconnect or
                    // rotation is respected before any provider request.
                    Action::Check => account.restore(&access, &cancel),
                    Action::Connect => account.connect(&access, &cancel),
                    Action::Disconnect => account.disconnect(&access, &cancel),
                });
            let _ = tx.send((account, result));
            ctx.request_repaint();
        });
        self.work = Some(rx);
    }
    pub fn draw_account(&mut self, ui: &mut egui::Ui, available: bool) {
        ui.label(RichText::new("Twitch").strong());
        if let Some(channel) = self.channel() {
            ui.label(&channel.title);
        }
        let enabled = available && !self.busy();
        if !Account::configured() {
            ui.label(RichText::new("Twitch account connection is being set up.").small());
        } else {
            ui.horizontal(|ui| {
                let label = if self.channel().is_some() {
                    "Change account"
                } else {
                    "Connect Twitch"
                };
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    self.start(ui.ctx(), Action::Connect);
                }
                if self.channel.is_some()
                    || self.account.as_ref().is_some_and(Account::needs_reconnect)
                {
                    if ui
                        .add_enabled(enabled, egui::Button::new("Disconnect"))
                        .clicked()
                    {
                        self.start(ui.ctx(), Action::Disconnect);
                    }
                }
            });
        }
        if self.busy() && self.foreground {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.connecting {
                    "Finish connecting in your browser."
                } else {
                    "Disconnecting…"
                });
                if ui.button("Cancel").clicked() {
                    if let Some(cancel) = &self.cancel {
                        cancel.store(true, Ordering::Release);
                    }
                }
            });
        }
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).small());
        }
        ui.label(
            RichText::new(
                "Saved on this device. Choose where to share your channel in Your streams.",
            )
            .small(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropping_account_panel_cancels_its_worker() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ui = TwitchUi::default();
        ui.cancel = Some(cancel.clone());
        drop(ui);
        assert!(cancel.load(Ordering::Acquire));
    }
    #[test]
    fn stale_panel_cannot_offer_another_accounts_channel() {
        let mut ui = TwitchUi::default();
        ui.generation = Some(crate::guild::generation().wrapping_sub(1));
        ui.channel = Some(Channel {
            user_id: "123".into(),
            login: "fixture".into(),
            title: "Fixture".into(),
            url: "https://www.twitch.tv/fixture".into(),
        });
        assert!(ui.channel().is_none());
    }
}
