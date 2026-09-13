//! Personal YouTube grant with an explicitly selected per-guild channel.
use crate::{
    streams,
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
    Discover(String),
}
enum Completed {
    Ready,
    Disconnected,
    Shared,
    Discovered(Duration, bool),
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
    notice: Option<String>,
    foreground: bool,
    changed: bool,
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

    /// Returns true when the guild's live/recording lists should be refreshed.
    pub fn tick(&mut self, ctx: &egui::Context, shared: Option<&Channel>) -> bool {
        if let Some(rx) = &self.work {
            let received = rx.try_recv();
            let disconnected = matches!(&received, Err(mpsc::TryRecvError::Disconnected));
            if let Ok((account, result)) = received {
                self.work = None;
                self.cancel = None;
                self.connected = account.connected();
                let reconnect_required = account.needs_reconnect();
                self.channels = account.channels().to_vec();
                self.account = Some(account);
                match result {
                    Ok(Completed::Ready) => {
                        self.notice = None;
                        self.next_discovery = None;
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
                        self.notice = Some("YouTube account disconnected on this device. Your guild's saved channel and VODs remain.".into());
                        self.selected.clear();
                    }
                    Ok(Completed::Shared) => {
                        self.notice = Some("Channel saved for this guild. Future broadcasts will appear automatically.".into());
                        self.next_discovery = None;
                        self.changed = true;
                    }
                    Ok(Completed::Discovered(delay, changed)) => {
                        self.next_discovery = Some(Instant::now() + delay);
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
            && self.next_discovery.is_none_or(|at| Instant::now() >= at)
        {
            if let Some(channel) = shared.filter(|channel| {
                self.channels
                    .iter()
                    .any(|owned| owned.channel_id == channel.channel_id)
            }) {
                self.start(ctx, Action::Discover(channel.channel_id.clone()));
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
        self.foreground = !matches!(action, Action::Restore | Action::Discover(_));
        if self.foreground {
            self.notice = None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
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
                    Action::Discover(channel) => {
                        let lease = streams::youtube_discovery_lease(&access, &channel)
                            .map_err(|error| error.message)?;
                        let delay = Duration::from_secs(
                            lease.retry_after_seconds.clamp(15 * 60, 24 * 60 * 60),
                        );
                        if !lease.allowed {
                            return Ok(Completed::Discovered(delay, false));
                        }
                        let ids = account.broadcasts(&channel, &access, &cancel)?;
                        if cancel.load(Ordering::Acquire) {
                            return Err("YouTube connection cancelled.".into());
                        }
                        access.check()?;
                        if !ids.is_empty() {
                            streams::publish_youtube_broadcasts(&access, &channel, &ids)
                                .map_err(|error| error.message)?;
                        }
                        Ok(Completed::Discovered(delay, !ids.is_empty()))
                    }
                });
            let _ = tx.send((account, result));
            ctx.request_repaint();
        });
        self.work = Some(rx);
    }

    pub fn draw_account(&mut self, ui: &mut egui::Ui, available: bool) {
        ui.label(RichText::new("YouTube").strong().size(15.0));
        ui.label("Connect your channel to find your live streams and VODs.");
        let enabled = available && !self.busy();
        if !Account::configured() {
            ui.label("YouTube connection is not available in this build.");
        } else if !self.connected {
            if ui
                .add_enabled(enabled, egui::Button::new("Connect YouTube"))
                .clicked()
            {
                self.start(ui.ctx(), Action::Connect);
            }
        } else {
            if self.channels.is_empty() {
                ui.label("Connected. This account has no available YouTube channels.");
            } else {
                for channel in &self.channels {
                    ui.label(RichText::new(&channel.title).strong());
                }
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("Change account"))
                    .clicked()
                {
                    self.start(ui.ctx(), Action::Connect);
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Disconnect account"))
                    .clicked()
                {
                    self.start(ui.ctx(), Action::Disconnect);
                }
            });
        }
        self.draw_progress(ui);
    }

    pub fn draw_channel(&mut self, ui: &mut egui::Ui, shared: Option<&Channel>, available: bool) {
        ui.label(RichText::new("YouTube channel").strong().size(15.0));
        if let Some(channel) = shared {
            ui.label(RichText::new(format!("Shared here: {}", channel.title)).strong());
        }
        let enabled = available && !self.busy();
        if !self.connected {
            ui.label("Connect YouTube on Home to share your channel here.");
        } else if self.channels.is_empty() {
            ui.label("Your connected account has no available YouTube channels.");
        } else {
            let title = self
                .channels
                .iter()
                .find(|channel| channel.channel_id == self.selected)
                .map(|channel| channel.title.as_str())
                .unwrap_or("Choose your channel");
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("youtube-owned-channel")
                    .selected_text(title)
                    .width(260.0)
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
            let changed = shared.is_none_or(|channel| channel.channel_id != self.selected);
            if ui
                .add_enabled(
                    enabled && changed && !self.selected.is_empty(),
                    egui::Button::new("Share channel with this guild"),
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
        ui.add_space(8.0);
    }

    fn draw_progress(&self, ui: &mut egui::Ui) {
        if self.work.is_some() && self.foreground {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
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
    }
}
