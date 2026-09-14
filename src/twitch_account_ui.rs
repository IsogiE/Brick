//! Optional personal Twitch grant. Guild sharing uses only channel().url.
use crate::{
    streams_ui::action_button,
    twitch_account::{Account, Channel},
};
use eframe::egui;
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
    disconnecting: bool,
    saved_connection: bool,
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
        // Defense in depth if a caller retains this panel across a guild/account
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
                    self.accept_completion(
                        account.needs_reconnect() || account.check_after().is_some(),
                        result.is_ok(),
                    );
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
        let configured = Account::configured();
        // A failed store load may still need a bounded Restore retry. A
        // successful empty restore clears its deadline through check_after().
        // Lost workers and revoked grants must never retain a repaint timer.
        let can_check = configured && self.account.as_ref().is_some_and(|a| !a.needs_reconnect());
        if !self.initialized && configured {
            self.initialized = true;
            self.start(ctx, Action::Restore);
        } else if !self.busy()
            && can_check
            && self.next_check.is_some_and(|at| Instant::now() >= at)
        {
            self.start(ctx, Action::Check);
        }
        self.schedule_check_repaint(ctx, can_check);
    }
    fn schedule_check_repaint(&mut self, ctx: &egui::Context, can_check: bool) {
        if !can_check {
            self.next_check = None;
        } else if let Some(at) = self.next_check {
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
        self.disconnecting = matches!(action, Action::Disconnect);
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
    fn can_disconnect(&self) -> bool {
        self.saved_connection || self.channel().is_some()
    }

    fn cancellation_pending(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }

    fn cancel_connect(&mut self) {
        if self.busy() && self.connecting {
            if let Some(cancel) = &self.cancel {
                cancel.store(true, Ordering::Release);
            }
        }
    }

    fn accept_completion(&mut self, saved: bool, succeeded: bool) {
        // Cancellation may arrive after a protected write. Keep that grant
        // removable, including when the channel lookup or disconnect failed.
        if succeeded {
            self.saved_connection = saved;
        } else {
            self.saved_connection |= saved || self.disconnecting;
        }
    }

    pub fn draw_connection_control(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 126.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 32.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_size(egui::vec2(width, 32.0));
                    if let Some(channel) = self.channel() {
                        ui.add(egui::Label::new(&channel.title).truncate());
                    } else if ui
                        .add_enabled(
                            !self.busy() && Account::configured(),
                            action_button("Choose channel"),
                        )
                        .clicked()
                    {
                        self.start(ui.ctx(), Action::Connect);
                    }
                },
            );
            if self.can_disconnect()
                && ui
                    .add_enabled(!self.busy(), action_button("Forget account"))
                    .on_hover_text("Remove this device's saved channel connection.")
                    .clicked()
            {
                self.saved_connection = true;
                self.start(ui.ctx(), Action::Disconnect);
            }
        });
        if self.busy() && self.foreground {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.cancellation_pending() {
                    "Cancelling…"
                } else if self.connecting {
                    "Choosing channel…"
                } else {
                    "Forgetting account…"
                });
                if self.connecting
                    && ui
                        .add_enabled(!self.cancellation_pending(), action_button("Cancel"))
                        .clicked()
                {
                    self.cancel_connect();
                }
            });
        }
        if let Some(notice) = &self.notice {
            ui.label(egui::RichText::new(notice).small());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restored_or_partial_grants_remain_removable_without_starting_work() {
        for succeeded in [false, true] {
            let mut ui = TwitchUi::default();
            ui.accept_completion(true, succeeded);
            assert!(ui.can_disconnect());
            assert!(!ui.busy());
        }
    }

    #[test]
    fn cancel_stops_work_without_erasing_a_saved_grant() {
        for saved in [false, true] {
            for succeeded in [false, true] {
                let mut ui = TwitchUi::default();
                ui.connecting = true;
                let (_tx, rx) = mpsc::channel();
                ui.work = Some(rx);
                let cancel = Arc::new(AtomicBool::new(false));
                ui.cancel = Some(cancel.clone());
                ui.cancel_connect();
                assert!(cancel.load(Ordering::Acquire));
                // Covers a successful result already queued when Cancel wins
                // the UI race, and a failure after the protected save.
                ui.accept_completion(saved, succeeded);
                assert_eq!(ui.can_disconnect(), saved);
            }
        }
    }

    #[test]
    fn failed_disconnect_keeps_its_retry_until_removal_succeeds() {
        let mut ui = TwitchUi::default();
        ui.disconnecting = true;
        ui.accept_completion(false, false);
        assert!(ui.can_disconnect());
        ui.accept_completion(false, true);
        assert!(!ui.can_disconnect());
    }

    #[test]
    fn partial_grant_exposes_removal_and_channel_retry_without_sharing() {
        let ctx = egui::Context::default();
        let mut panel = TwitchUi::default();
        panel.accept_completion(true, false);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            panel.draw_connection_control(ui);
        });
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Choose channel"));
        assert!(labels.contains(&"Forget account"));
        assert!(!panel.busy());
        assert!(panel.channel().is_none());
    }

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
    #[test]
    fn paused_connection_drops_expired_deadlines_without_repainting() {
        let ctx = egui::Context::default();
        let mut ui = TwitchUi::default();
        ui.next_check = Some(Instant::now() - Duration::from_secs(1));
        let mut delay = Duration::ZERO;
        for _ in 0..5 {
            let output = ctx.run_ui(egui::RawInput::default(), |_| {
                // The common paused path for revoked or unconfigured accounts.
                ui.schedule_check_repaint(&ctx, false);
            });
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        assert!(ui.next_check.is_none());
        assert_eq!(delay, Duration::MAX);
    }
    #[test]
    fn disconnected_worker_cannot_leave_an_endless_repaint_timer() {
        let ctx = egui::Context::default();
        let mut ui = TwitchUi::default();
        ui.initialized = true;
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        drop(tx);
        let mut delay = Duration::ZERO;
        for _ in 0..5 {
            let output = ctx.run_ui(egui::RawInput::default(), |_| ui.tick(&ctx));
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        assert!(ui.work.is_none());
        assert!(ui.next_check.is_none());
        assert_eq!(delay, Duration::MAX);
    }
    #[test]
    fn transient_restore_failure_retains_its_fifteen_minute_retry() {
        let ctx = egui::Context::default();
        let mut ui = TwitchUi::default();
        ui.next_check = Some(Instant::now() + Duration::from_secs(15 * 60));
        let mut delay = Duration::ZERO;
        for _ in 0..5 {
            let output = ctx.run_ui(egui::RawInput::default(), |_| {
                // A present Account with a temporarily locked store remains
                // retryable; no credential store or provider is used here.
                ui.schedule_check_repaint(&ctx, true);
            });
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        assert!(ui.next_check.is_some());
        assert!(delay > Duration::from_secs(14 * 60));
        assert!(delay <= Duration::from_secs(15 * 60));
    }
}
