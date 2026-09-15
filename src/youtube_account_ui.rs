//! Personal YouTube grant with an explicitly selected per-guild channel.
use crate::{
    streams,
    streams_ui::action_button,
    youtube_account::{Account, Channel},
};
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
    Connect,
    Disconnect,
    Share(String),
    Discover(String, bool, bool),
}

enum Completed {
    Ready,
    Disconnected,
    Shared,
    Discovered(Duration, bool, bool, Vec<String>),
    Deferred(Duration, bool),
}
type WorkResult = (Account, Result<Completed, String>);

#[derive(Default)]
pub struct YoutubeUi {
    account: Option<Account>,
    work: Option<mpsc::Receiver<WorkResult>>,
    cancel: Option<Arc<AtomicBool>>,
    initialized: bool,
    connected: bool,
    channels: Vec<Channel>,
    selected: String,
    next_discovery: Option<Instant>,
    next_archives: Option<Instant>,
    refresh_requested: Option<Instant>,
    last_live_ids: Vec<String>,
    notice: Option<String>,
    foreground: bool,
    changed: bool,
    connecting: bool,
    disconnecting: bool,
    saved_connection: bool,
}

impl Drop for YoutubeUi {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

impl YoutubeUi {
    pub fn busy(&self) -> bool {
        self.work.is_some()
    }

    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn refresh_now(&mut self) {
        self.refresh_requested = Some(Instant::now());
    }

    fn manual_due(&self, now: Instant) -> bool {
        self.refresh_requested.is_some_and(|at| now >= at)
    }

    fn defer_discovery(&mut self, delay: Duration, manual: bool, now: Instant) {
        self.next_discovery = Some(now + delay);
        if manual {
            self.refresh_requested = Some(now + delay);
        }
    }

    /// Returns true when the guild's live/recording lists should be refreshed.
    pub fn tick(&mut self, ctx: &egui::Context, shared: Option<&Channel>) -> bool {
        if let Some(rx) = &self.work {
            let received = rx.try_recv();
            let disconnected = matches!(&received, Err(mpsc::TryRecvError::Disconnected));
            if let Ok((account, result)) = received {
                self.work = None;
                self.accept_completion(
                    account.connected() || account.needs_reconnect(),
                    result.is_ok(),
                );
                self.cancel = None;
                self.connected = account.connected();
                let reconnect_required = account.needs_reconnect();
                self.channels = account.channels().to_vec();
                self.account = Some(account);
                match result {
                    Ok(Completed::Ready) => {
                        self.notice = None;
                        self.next_discovery = None;
                        self.next_archives = None;
                        self.last_live_ids.clear();
                        if self.channels.len() == 1 {
                            self.selected = self.channels[0].channel_id.clone();
                        } else if !self
                            .channels
                            .iter()
                            .any(|channel| channel.channel_id == self.selected)
                        {
                            self.selected.clear();
                        }
                    }
                    Ok(Completed::Disconnected) => {
                        self.notice = None;
                        self.selected.clear();
                    }
                    Ok(Completed::Shared) => {
                        self.notice = None;
                        self.next_discovery = None;
                        self.next_archives = None;
                        self.last_live_ids.clear();
                        self.changed = true;
                    }
                    Ok(Completed::Deferred(delay, manual)) => {
                        self.defer_discovery(delay, manual, Instant::now());
                    }
                    Ok(Completed::Discovered(delay, changed, live_only, ids)) => {
                        self.notice = None;
                        self.next_discovery = Some(Instant::now() + delay);
                        if live_only {
                            self.last_live_ids = ids;
                        } else {
                            self.next_archives =
                                Some(Instant::now() + Duration::from_secs(15 * 60));
                        }
                        self.changed |= changed;
                    }
                    Err(error) => {
                        if self.foreground || reconnect_required {
                            self.notice = Some(error);
                        }
                        self.next_discovery = Some(Instant::now() + Duration::from_secs(15 * 60));
                    }
                }
            } else if disconnected {
                self.work = None;
                self.cancel = None;
                if self.foreground {
                    self.notice = Some("YouTube connection stopped. Try again.".into());
                }
                self.next_discovery = Some(Instant::now() + Duration::from_secs(15 * 60));
            }
        }
        if !self.initialized && Account::configured() {
            self.initialized = true;
            self.start(ctx, Action::Restore);
        } else if self.work.is_none()
            && self.connected
            && (self.manual_due(Instant::now())
                || self.next_discovery.is_none_or(|at| Instant::now() >= at))
        {
            if let Some(channel) = shared.filter(|channel| {
                self.channels
                    .iter()
                    .any(|owned| owned.channel_id == channel.channel_id)
            }) {
                let manual = self.manual_due(Instant::now());
                if manual {
                    self.refresh_requested = None;
                }
                let live_only = manual || self.next_archives.is_some_and(|at| Instant::now() < at);
                self.start(
                    ctx,
                    Action::Discover(channel.channel_id.clone(), live_only, manual),
                );
            }
        }
        std::mem::take(&mut self.changed)
    }

    fn start(&mut self, ctx: &egui::Context, action: Action) {
        if self.work.is_some() {
            return;
        }
        let account = match self.account.take().map(Ok).unwrap_or_else(Account::new) {
            Ok(account) => account,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        self.connecting = matches!(action, Action::Connect);
        self.disconnecting = matches!(action, Action::Disconnect);
        self.foreground = !matches!(action, Action::Restore | Action::Discover(_, _, false));
        if self.foreground {
            self.notice = None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        let previous_live_ids = self.last_live_ids.clone();
        crate::guild::spawn(move || {
            let mut account = account;
            let result =
                crate::discord_auth::current_access_token().and_then(|access| match action {
                    Action::Restore => account.restore(&access, &cancel).map(|()| Completed::Ready),
                    Action::Connect => account.connect(&access, &cancel).map(|()| Completed::Ready),
                    Action::Disconnect => account
                        .disconnect(&access, &cancel)
                        .map(|()| Completed::Disconnected),
                    Action::Share(channel) => {
                        if !account
                            .channels()
                            .iter()
                            .any(|owned| owned.channel_id == channel)
                        {
                            return Err("Choose one of your connected YouTube channels.".into());
                        }
                        if cancel.load(Ordering::Acquire) {
                            return Err("YouTube connection cancelled.".into());
                        }
                        streams::share_youtube_channel(&access, &channel)
                            .map_err(|error| error.message)?;
                        Ok(Completed::Shared)
                    }
                    Action::Discover(channel, live_only, manual) => {
                        let lease =
                            streams::youtube_discovery_lease(&access, &channel, live_only, manual)
                                .map_err(|error| error.message)?;
                        let delay =
                            Duration::from_secs(lease.retry_after_seconds.clamp(5, 24 * 60 * 60));
                        if !lease.allowed {
                            // Keep previous observations and do not mark an archive check complete.
                            return Ok(Completed::Deferred(delay, manual));
                        }
                        let mut ids = account.broadcasts(&channel, &access, &cancel, live_only)?;
                        ids.sort();
                        if cancel.load(Ordering::Acquire) {
                            return Err("YouTube connection cancelled.".into());
                        }
                        access.check()?;
                        let changed = !live_only || ids != previous_live_ids;
                        if !ids.is_empty() && (changed || manual) {
                            streams::publish_youtube_broadcasts(&access, &channel, &ids)
                                .map_err(|error| error.message)?;
                        }
                        let next = if live_only {
                            delay
                        } else {
                            Duration::from_secs(30)
                        };
                        Ok(Completed::Discovered(next, changed, live_only, ids))
                    }
                });
            let _ = tx.send((account, result));
            ctx.request_repaint();
        });
        self.work = Some(rx);
    }

    fn can_disconnect(&self) -> bool {
        self.saved_connection || self.connected
    }

    fn cancellation_pending(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }

    fn cancel_work(&mut self) {
        if self.busy() && self.foreground {
            if let Some(cancel) = &self.cancel {
                cancel.store(true, Ordering::Release);
            }
        }
    }

    fn accept_completion(&mut self, saved: bool, succeeded: bool) {
        // Keep a saved grant removable even if cancellation arrives after its
        // protected write, or a later channel lookup or disconnect fails.
        if succeeded {
            self.saved_connection = saved;
        } else {
            self.saved_connection |= saved || self.disconnecting;
        }
    }

    pub fn draw_channel(&mut self, ui: &mut egui::Ui, shared: Option<&Channel>, available: bool) {
        ui.label(RichText::new("YouTube channel").strong().size(15.0));
        if let Some(channel) =
            shared.filter(|channel| !self.connected || self.selected != channel.channel_id)
        {
            ui.label(RichText::new(format!("Shared here: {}", channel.title)).strong());
        }
        let enabled = available && !self.busy();
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 126.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 32.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_size(egui::vec2(width, 32.0));
                    if self.connected && !self.channels.is_empty() {
                        let title = self
                            .channels
                            .iter()
                            .find(|channel| channel.channel_id == self.selected)
                            .map(|channel| channel.title.as_str())
                            .unwrap_or("Choose your channel");
                        if self.channels.len() == 1 {
                            ui.add(egui::Label::new(title).truncate());
                        } else {
                            ui.add_enabled_ui(enabled, |ui| {
                                egui::ComboBox::from_id_salt("youtube-owned-channel")
                                    .selected_text(title)
                                    .width(ui.available_width())
                                    .show_ui(ui, |ui| {
                                        for channel in &self.channels {
                                            ui.selectable_value(
                                                &mut self.selected,
                                                channel.channel_id.clone(),
                                                &channel.title,
                                            );
                                        }
                                    });
                            });
                        }
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
                    .add_enabled(!self.busy(), action_button("Forget channel"))
                    .on_hover_text("Remove this device's saved channel connection.")
                    .clicked()
            {
                self.saved_connection = true;
                self.start(ui.ctx(), Action::Disconnect);
            }
        });
        if self.connected && self.channels.is_empty() {
            if self.notice.is_none() {
                ui.label("Your connected account has no available YouTube channels.");
            }
        } else if self.connected && shared.is_none_or(|channel| channel.channel_id != self.selected)
        {
            if ui
                .add_enabled(
                    enabled && !self.selected.is_empty(),
                    action_button("Share channel with this guild"),
                )
                .clicked()
            {
                self.start(ui.ctx(), Action::Share(self.selected.clone()));
            }
            ui.label(
                RichText::new("Sharing includes this channel's public and unlisted broadcasts.")
                    .small(),
            );
        }
        self.draw_progress(ui);
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).small());
        }
    }

    fn draw_progress(&mut self, ui: &mut egui::Ui) {
        if self.work.is_some() && self.foreground {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.cancellation_pending() {
                    "Cancelling…"
                } else if self.connecting {
                    "Choosing channel…"
                } else {
                    "Working…"
                });
                if ui
                    .add_enabled(!self.cancellation_pending(), action_button("Cancel"))
                    .clicked()
                {
                    self.cancel_work();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttled_refresh_waits_then_retries_as_a_manual_check() {
        let mut panel = YoutubeUi::default();
        let now = Instant::now();
        panel.last_live_ids = vec!["liveABC_123".into()];
        panel.next_archives = None;
        panel.refresh_now();
        panel.defer_discovery(Duration::from_secs(4), true, now);
        assert!(!panel.manual_due(now + Duration::from_secs(3)));
        assert!(panel.manual_due(now + Duration::from_secs(4)));
        assert!(panel.next_archives.is_none());
        assert_eq!(panel.last_live_ids, ["liveABC_123"]);
    }

    #[test]
    fn background_deferral_preserves_a_refresh_queued_during_work() {
        let mut panel = YoutubeUi::default();
        panel.refresh_now();
        let now = Instant::now();
        panel.defer_discovery(Duration::from_secs(30), false, now);
        assert!(panel.manual_due(now));
    }

    #[test]
    fn restored_or_partial_grants_remain_removable_without_starting_work() {
        for succeeded in [false, true] {
            let mut ui = YoutubeUi::default();
            ui.accept_completion(true, succeeded);
            assert!(ui.can_disconnect());
            assert!(!ui.busy());
        }
    }

    #[test]
    fn cancel_stops_work_without_erasing_a_saved_grant() {
        for saved in [false, true] {
            for succeeded in [false, true] {
                let mut ui = YoutubeUi::default();
                ui.connecting = true;
                ui.foreground = true;
                let (_tx, rx) = mpsc::channel();
                ui.work = Some(rx);
                let cancel = Arc::new(AtomicBool::new(false));
                ui.cancel = Some(cancel.clone());
                ui.cancel_work();
                assert!(cancel.load(Ordering::Acquire));
                ui.accept_completion(saved, succeeded);
                assert_eq!(ui.can_disconnect(), saved);
            }
        }
    }

    #[test]
    fn failed_disconnect_keeps_its_retry_until_removal_succeeds() {
        let mut ui = YoutubeUi::default();
        ui.disconnecting = true;
        ui.accept_completion(false, false);
        assert!(ui.can_disconnect());
        ui.accept_completion(false, true);
        assert!(!ui.can_disconnect());
    }

    #[test]
    fn partial_grant_exposes_removal_and_channel_retry_without_sharing() {
        let ctx = egui::Context::default();
        let mut panel = YoutubeUi::default();
        panel.accept_completion(true, false);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            panel.draw_channel(ui, None, true);
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
        assert!(labels.contains(&"Forget channel"));
        assert!(!labels.contains(&"Share channel with this guild"));
        assert!(!panel.busy());
    }

    #[test]
    fn connected_channel_still_requires_explicit_guild_sharing() {
        let ctx = egui::Context::default();
        let mut panel = YoutubeUi::default();
        panel.connected = true;
        panel.channels.push(Channel {
            channel_id: "UC1234567890123456789012".into(),
            title: "Fixture".into(),
            url: "https://www.youtube.com/channel/UC1234567890123456789012".into(),
        });
        panel.selected = panel.channels[0].channel_id.clone();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            panel.draw_channel(ui, None, true);
        });
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Share channel with this guild"));
        assert!(labels.contains(&"Sharing includes this channel's public and unlisted broadcasts."));
        assert!(labels.contains(&"Forget channel"));
        assert!(!panel.busy());
        assert!(!panel.changed);
    }
}
