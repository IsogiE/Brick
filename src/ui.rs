use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText, Stroke, TextureHandle};

use crate::{
    addon::{self, AppView, LogLevel, SyncSummary, WowClient},
    app_update::{self, PreparedAppUpdate},
    autostart,
    discord_auth::{self, AuthorizedUser, SessionStatus},
    presence::{self, Roster, RosterMember},
    single_instance, tray,
};

const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");
const APP_UPDATE_CHECK_INTERVAL_SECS: u64 = 30 * 60;
const PRESENCE_HEARTBEAT_INTERVAL_SECS: u64 = 60;
const ROSTER_REFRESH_INTERVAL_SECS: u64 = 30;

pub struct BrickApp {
    view: AppView,
    status: String,
    sync_lock: Arc<Mutex<()>>,
    auth_state: AuthUiState,
    presence_state: PresenceUiState,
    active_tab: MainTab,
    sync_rx: Option<mpsc::Receiver<Result<SyncSummary, String>>>,
    auth_rx: Option<mpsc::Receiver<Result<AuthorizedUser, String>>>,
    app_update_rx: Option<mpsc::Receiver<Result<Option<PreparedAppUpdate>, String>>>,
    presence_heartbeat_rx: Option<mpsc::Receiver<Result<(), String>>>,
    roster_rx: Option<mpsc::Receiver<Result<Roster, String>>>,
    brick_texture: Option<TextureHandle>,
    tray: Option<tray::TrayState>,
    tray_attempted: bool,
    quit_requested: bool,
    last_show_request: Option<String>,
    last_auth_check: Instant,
    last_view_refresh: Instant,
    last_app_update_check: Instant,
    last_presence_heartbeat: Instant,
    last_roster_refresh: Instant,
    roster_notice: Option<String>,
}

#[derive(Debug, Clone)]
enum AuthUiState {
    ConfigMissing(String),
    SignedOut,
    Checking,
    Authorized(AuthorizedUser),
    Denied(String),
}

impl AuthUiState {
    fn is_authorized(&self) -> bool {
        matches!(self, Self::Authorized(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTab {
    Updates,
    Roster,
}

#[derive(Debug, Clone)]
enum PresenceUiState {
    Unavailable(String),
    Idle,
    Loading,
    Ready(Roster),
    Error(String),
}

struct DisplayStatus {
    title: String,
    detail: String,
    accent: Color32,
    accent_soft: Color32,
    version: Option<String>,
}

impl BrickApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        sync_lock: Arc<Mutex<()>>,
        startup_mode: bool,
    ) -> Self {
        configure_style(&cc.egui_ctx);
        let brick_texture = load_texture(&cc.egui_ctx);
        spawn_show_request_wake(&cc.egui_ctx);

        let (view, status) = match addon::load_view() {
            Ok(view) => (view, "Ready".to_string()),
            Err(error) => {
                let _ = addon::record_log(LogLevel::Error, error.clone());
                (AppView::default(), error)
            }
        };

        let auth_state = match discord_auth::saved_session_status() {
            Ok(SessionStatus::ConfigMissing(error)) => AuthUiState::ConfigMissing(error),
            Ok(SessionStatus::SignedOut) => AuthUiState::SignedOut,
            Ok(SessionStatus::NeedsRefresh) => AuthUiState::Checking,
            Ok(SessionStatus::Authorized(user)) => AuthUiState::Authorized(user),
            Err(error) => AuthUiState::Denied(error),
        };
        let now = Instant::now();

        let mut app = Self {
            view,
            status,
            sync_lock,
            auth_state,
            presence_state: initial_presence_state(),
            active_tab: MainTab::Updates,
            sync_rx: None,
            auth_rx: None,
            app_update_rx: None,
            presence_heartbeat_rx: None,
            roster_rx: None,
            brick_texture,
            tray: None,
            tray_attempted: false,
            quit_requested: false,
            last_show_request: single_instance::read_show_request().ok().flatten(),
            last_auth_check: now,
            last_view_refresh: now,
            last_app_update_check: now,
            last_presence_heartbeat: now
                .checked_sub(Duration::from_secs(PRESENCE_HEARTBEAT_INTERVAL_SECS))
                .unwrap_or(now),
            last_roster_refresh: now
                .checked_sub(Duration::from_secs(ROSTER_REFRESH_INTERVAL_SECS))
                .unwrap_or(now),
            roster_notice: None,
        };

        if matches!(app.auth_state, AuthUiState::Checking) {
            app.start_auth_refresh();
        }
        app.start_app_update_check();
        if app.auth_state.is_authorized() {
            app.reconcile_autostart();
        }
        if app.auth_state.is_authorized()
            && !app.view.setup_required
            && app.view.settings.watcher_enabled
        {
            app.start_sync();
        }
        if app.auth_state.is_authorized() {
            app.start_presence_heartbeat();
        }
        if startup_mode {
            app.status = "Ready".to_string();
        }

        app
    }

    fn refresh_view(&mut self) {
        match addon::load_view() {
            Ok(view) => {
                self.view = view;
                self.last_view_refresh = Instant::now();
            }
            Err(error) => self.status = error,
        }
    }

    fn refresh_view_if_stale(&mut self) {
        if self.sync_rx.is_some() || self.last_view_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }

        self.refresh_view();
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.refresh_view();

        if !self.view.setup_required && self.view.settings.watcher_enabled {
            self.start_sync();
        }
    }

    fn reconcile_autostart(&mut self) {
        if self.view.setup_required {
            return;
        }

        if let Err(error) = autostart::reconcile_enabled(self.view.settings.startup_enabled) {
            let _ = addon::record_log(LogLevel::Warn, error.clone());
            self.status = error;
        }
    }

    fn add_wow_folders(&mut self, paths: Vec<PathBuf>) {
        let before = self.view.settings.clients.len();
        match addon::add_wow_paths(&paths) {
            Ok(view) => {
                self.view = view;
                match autostart::set_enabled(true) {
                    Ok(()) => {
                        let added = self.view.settings.clients.len().saturating_sub(before);
                        self.status = if added == 0 {
                            "That WoW folder is already set up.".to_string()
                        } else {
                            "WoW folder saved.".to_string()
                        };
                    }
                    Err(error) => {
                        let _ = addon::record_log(LogLevel::Warn, error.clone());
                        self.status = error;
                    }
                }
                self.start_sync();
            }
            Err(error) => {
                let _ = addon::record_log(LogLevel::Error, error.clone());
                self.status = error;
                self.refresh_view();
            }
        }
    }

    fn remove_client(&mut self, id: &str) {
        match addon::remove_client(id) {
            Ok(view) => {
                self.view = view;
                self.status = "WoW folder removed.".to_string();
            }
            Err(error) => self.status = error,
        }
    }

    fn set_automation(&mut self, enabled: bool) {
        match addon::set_automation_enabled(enabled) {
            Ok(view) => self.view = view,
            Err(error) => {
                self.status = error;
                return;
            }
        }

        match autostart::set_enabled(enabled) {
            Ok(()) => {
                self.status = if enabled {
                    "Automatic updates are on.".to_string()
                } else {
                    "Automatic updates are off.".to_string()
                };
                if enabled && !self.view.setup_required {
                    self.start_sync();
                }
            }
            Err(error) => {
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                self.status = error;
            }
        }
    }

    fn set_startup_minimized(&mut self, enabled: bool) {
        match addon::set_startup_minimized(enabled) {
            Ok(view) => {
                self.view = view;
                self.status = if enabled {
                    "Brick will start minimized.".to_string()
                } else {
                    "Brick will open at login.".to_string()
                };
            }
            Err(error) => self.status = error,
        }
    }

    fn start_discord_login(&mut self) {
        if self.auth_rx.is_some() {
            return;
        }

        self.auth_state = AuthUiState::Checking;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = discord_auth::login_with_browser();
            let _ = tx.send(result);
        });
        self.auth_rx = Some(rx);
        self.status = "Waiting for Discord.".to_string();
    }

    fn start_auth_refresh(&mut self) {
        if self.auth_rx.is_some() {
            return;
        }

        self.auth_state = AuthUiState::Checking;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = discord_auth::refresh_saved_session();
            let _ = tx.send(result);
        });
        self.auth_rx = Some(rx);
        self.status = "Checking Discord session.".to_string();
    }

    fn poll_auth(&mut self) {
        let Some(rx) = self.auth_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(user)) => {
                self.auth_state = AuthUiState::Authorized(user);
                self.auth_rx = None;
                self.status = "Discord access verified.".to_string();
                self.refresh_view();
                self.reconcile_autostart();
                if !self.view.setup_required && self.view.settings.watcher_enabled {
                    self.start_sync();
                }
                self.start_presence_heartbeat();
            }
            Ok(Err(error)) => {
                self.auth_state = AuthUiState::Denied(error.clone());
                self.auth_rx = None;
                self.status = error;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let error = "Discord login stopped unexpectedly.".to_string();
                self.auth_state = AuthUiState::Denied(error.clone());
                self.auth_rx = None;
                self.status = error;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn refresh_auth_if_expired(&mut self) {
        if self.auth_rx.is_some() || self.last_auth_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_auth_check = Instant::now();

        let should_refresh = match &self.auth_state {
            AuthUiState::Authorized(user) => discord_auth::session_expired(user.expires_at_unix),
            _ => false,
        };

        if should_refresh {
            self.start_auth_refresh();
        }
    }

    fn sign_out(&mut self) {
        match discord_auth::clear_session() {
            Ok(()) => {
                self.auth_state = AuthUiState::SignedOut;
                self.sync_rx = None;
                self.presence_heartbeat_rx = None;
                self.roster_rx = None;
                self.presence_state = initial_presence_state();
                self.roster_notice = None;
                self.status = "Signed out of Discord.".to_string();
            }
            Err(error) => {
                self.auth_state = AuthUiState::Denied(error.clone());
                self.status = error;
            }
        }
    }

    fn start_presence_heartbeat(&mut self) {
        if self.presence_heartbeat_rx.is_some()
            || !self.auth_state.is_authorized()
            || !presence::configured()
        {
            return;
        }

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| presence::send_heartbeat(&access_token));
            let _ = tx.send(result);
        });
        self.presence_heartbeat_rx = Some(rx);
        self.last_presence_heartbeat = Instant::now();
    }

    fn start_periodic_presence_heartbeat(&mut self) {
        if self.last_presence_heartbeat.elapsed()
            >= Duration::from_secs(PRESENCE_HEARTBEAT_INTERVAL_SECS)
        {
            self.start_presence_heartbeat();
        }
    }

    fn poll_presence_heartbeat(&mut self) {
        let Some(rx) = self.presence_heartbeat_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(())) => {
                self.presence_heartbeat_rx = None;
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Warn, error);
                self.presence_heartbeat_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let _ = addon::record_log(
                    LogLevel::Warn,
                    "Roster heartbeat stopped unexpectedly.".to_string(),
                );
                self.presence_heartbeat_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn start_roster_refresh(&mut self) {
        if self.roster_rx.is_some() || !self.auth_state.is_authorized() {
            return;
        }
        if !presence::configured() {
            self.presence_state = PresenceUiState::Unavailable(presence::configuration_error());
            return;
        }

        if !matches!(self.presence_state, PresenceUiState::Ready(_)) {
            self.presence_state = PresenceUiState::Loading;
        }

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| presence::fetch_roster(&access_token));
            let _ = tx.send(result);
        });
        self.roster_rx = Some(rx);
        self.last_roster_refresh = Instant::now();
    }

    fn start_roster_refresh_if_stale(&mut self) {
        if self.active_tab != MainTab::Roster {
            return;
        }
        if matches!(self.presence_state, PresenceUiState::Idle) {
            self.start_roster_refresh();
            return;
        }
        if self.last_roster_refresh.elapsed() >= Duration::from_secs(ROSTER_REFRESH_INTERVAL_SECS) {
            self.start_roster_refresh();
        }
    }

    fn poll_roster(&mut self) {
        let Some(rx) = self.roster_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(roster)) => {
                self.presence_state = PresenceUiState::Ready(roster);
                self.roster_notice = None;
                self.roster_rx = None;
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                let message = friendly_roster_problem(&error);
                if matches!(self.presence_state, PresenceUiState::Ready(_)) {
                    self.roster_notice = Some(message);
                } else {
                    self.presence_state = PresenceUiState::Error(message);
                    self.roster_notice = None;
                }
                self.roster_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let message = "Roster refresh stopped unexpectedly.".to_string();
                if matches!(self.presence_state, PresenceUiState::Ready(_)) {
                    self.roster_notice = Some(message);
                } else {
                    self.presence_state = PresenceUiState::Error(message);
                    self.roster_notice = None;
                }
                self.roster_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn start_sync(&mut self) {
        if !self.auth_state.is_authorized() {
            return;
        }

        if self.sync_rx.is_some() {
            return;
        }

        self.status = "Checking for updates.".to_string();
        let lock = self.sync_lock.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = addon::run_sync_with_lock(&lock);
            let _ = tx.send(result);
        });
        self.sync_rx = Some(rx);
    }

    fn start_app_update_check(&mut self) {
        if self.app_update_rx.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = app_update::prepare_available_update();
            let _ = tx.send(result);
        });
        self.app_update_rx = Some(rx);
        self.last_app_update_check = Instant::now();
    }

    fn start_periodic_app_update_check(&mut self) {
        if self.last_app_update_check.elapsed()
            >= Duration::from_secs(APP_UPDATE_CHECK_INTERVAL_SECS)
        {
            self.start_app_update_check();
        }
    }

    fn poll_sync(&mut self) {
        let Some(rx) = self.sync_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(summary)) => {
                self.status = summary.message;
                self.sync_rx = None;
                self.refresh_view();
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Error, error.clone());
                self.status = error;
                self.sync_rx = None;
                self.refresh_view();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status = "Update check stopped unexpectedly.".to_string();
                self.sync_rx = None;
                self.refresh_view();
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn poll_app_update(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.app_update_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(Some(update))) => {
                let message = format!("Installing Brick {}.", update.version);
                match app_update::launch_installer(&update) {
                    Ok(()) => {
                        let _ = addon::record_log(LogLevel::Info, message.clone());
                        self.status = message;
                        self.app_update_rx = None;
                        self.quit_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(error) => {
                        let _ = addon::record_log(LogLevel::Error, error.clone());
                        self.status = error;
                        self.app_update_rx = None;
                    }
                }
            }
            Ok(Ok(None)) => {
                self.app_update_rx = None;
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Warn, error);
                self.app_update_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let _ = addon::record_log(
                    LogLevel::Warn,
                    "Brick app update check stopped unexpectedly.".to_string(),
                );
                self.app_update_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn ensure_tray(&mut self) {
        if self.tray_attempted {
            return;
        }

        self.tray_attempted = true;
        match catch_unwind(AssertUnwindSafe(tray::create)) {
            Ok(Ok(tray)) => self.tray = Some(tray),
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                self.status = error;
            }
            Err(_) => {
                let error = "Brick could not add itself to the system tray.".to_string();
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                self.status = error;
            }
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = self.tray.as_ref() else {
            return;
        };

        for command in tray.drain_commands() {
            match command {
                tray::TrayCommand::Show => {
                    self.show_window(ctx);
                }
                tray::TrayCommand::Hide => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                tray::TrayCommand::Quit => {
                    self.quit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn handle_show_request(&mut self, ctx: &egui::Context) {
        let Ok(Some(token)) = single_instance::read_show_request() else {
            return;
        };

        if self.last_show_request.as_deref() == Some(token.as_str()) {
            return;
        }

        self.last_show_request = Some(token);
        self.show_window(ctx);
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.quit_requested && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn draw_content(&mut self, ui: &mut egui::Ui) {
        if !self.auth_state.is_authorized() {
            self.draw_login_screen(ui);
            return;
        }

        self.draw_header(ui);
        ui.add_space(14.0);
        self.draw_tab_bar(ui);
        ui.add_space(18.0);
        match self.active_tab {
            MainTab::Updates => self.draw_updates_tab(ui),
            MainTab::Roster => self.draw_roster_tab(ui),
        }
    }

    fn draw_updates_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("updates-tab")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - 18.0).max(0.0));
                self.draw_status_panel(ui);
                ui.add_space(18.0);
                self.draw_installs_section(ui);
                ui.add_space(18.0);
                self.draw_settings_panel(ui);
                ui.add_space(8.0);
            });
    }

    fn draw_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if tab_button(ui, "Updates", self.active_tab == MainTab::Updates).clicked() {
                self.active_tab = MainTab::Updates;
            }
            if tab_button(ui, "Roster", self.active_tab == MainTab::Roster).clicked() {
                self.active_tab = MainTab::Roster;
                self.start_roster_refresh_if_stale();
            }
        });
    }

    fn draw_roster_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            section_title(ui, "Guild Roster");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "Refresh").clicked() {
                    self.start_roster_refresh();
                }
                if self.roster_rx.is_some() {
                    ui.add(egui::Spinner::new().size(16.0).color(info_accent()));
                }
            });
        });
        ui.add_space(8.0);

        let state = self.presence_state.clone();
        panel_frame().show(ui, |ui| match state {
            PresenceUiState::Unavailable(error) => {
                empty_panel_message(ui, "Roster unavailable", &friendly_roster_problem(&error));
            }
            PresenceUiState::Idle | PresenceUiState::Loading => {
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.add(egui::Spinner::new().size(24.0).color(info_accent()));
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Loading roster")
                            .strong()
                            .color(primary_text()),
                    );
                    ui.add_space(18.0);
                });
            }
            PresenceUiState::Error(error) => {
                empty_panel_message(ui, "Roster unavailable", &error);
            }
            PresenceUiState::Ready(roster) => {
                let max_height = ui.available_height().max(260.0);
                egui::ScrollArea::vertical()
                    .id_salt("guild-roster")
                    .auto_shrink([false, true])
                    .max_height(max_height)
                    .show(ui, |ui| {
                        let content_width = (ui.available_width() - 18.0).max(260.0);
                        ui.set_width(content_width);

                        if let Some(notice) = self.roster_notice.as_deref() {
                            roster_notice(ui, notice);
                            ui.add_space(14.0);
                        }
                        draw_roster_group(ui, "Officers", &roster.officers);
                        ui.add_space(18.0);
                        draw_roster_group(ui, "Raiders", &roster.raiders);
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(roster_refresh_label(
                                &roster.generated_at,
                                roster.online_window_seconds,
                            ))
                            .small()
                            .color(muted_text()),
                        );
                    });
            }
        });
    }

    fn draw_login_screen(&mut self, ui: &mut egui::Ui) {
        let canvas = ui.max_rect();
        let state = self.auth_state.clone();
        let (title, detail, button_text, button_enabled) = login_copy(&state);
        let panel_width = canvas.width().clamp(320.0, 420.0);
        let panel_height = match state {
            AuthUiState::Denied(_) => 320.0,
            AuthUiState::Checking => 300.0,
            AuthUiState::ConfigMissing(_) => 320.0,
            _ => 284.0,
        };
        let panel_top = (canvas.center().y - panel_height * 0.55)
            .clamp(canvas.top() + 28.0, canvas.bottom() - panel_height - 28.0);
        let panel_rect = egui::Rect::from_min_size(
            egui::pos2(canvas.center().x - panel_width * 0.5, panel_top),
            egui::vec2(panel_width, panel_height),
        );

        ui.allocate_rect(panel_rect, egui::Sense::hover());
        ui.painter()
            .rect_filled(panel_rect, egui::CornerRadius::same(8), panel_background());
        ui.painter().rect_stroke(
            panel_rect,
            egui::CornerRadius::same(8),
            Stroke::new(1.0_f32, panel_stroke()),
            egui::StrokeKind::Inside,
        );

        let inner_rect = panel_rect.shrink2(egui::vec2(34.0, 32.0));
        let content_height = login_content_height(&state);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(inner_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                ui.set_clip_rect(inner_rect);
                ui.add_space(((inner_rect.height() - content_height) * 0.5).max(0.0));
                draw_icon(ui, self.brick_texture.as_ref(), 52.0);
                ui.add_space(18.0);
                ui.label(
                    RichText::new(title)
                        .size(24.0)
                        .strong()
                        .color(primary_text()),
                );
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new(detail).color(secondary_text())).wrap());
                ui.add_space(24.0);

                if matches!(state, AuthUiState::Checking) {
                    ui.add(egui::Spinner::new().size(24.0).color(info_accent()));
                    ui.add_space(12.0);
                }

                if button_enabled {
                    if ui
                        .add_sized(egui::vec2(236.0, 42.0), login_button(button_text))
                        .clicked()
                    {
                        self.start_discord_login();
                    }
                } else {
                    ui.add_enabled(false, login_button(button_text));
                }

                if matches!(state, AuthUiState::Denied(_)) {
                    ui.add_space(8.0);
                    if ui
                        .add_sized(egui::vec2(236.0, 36.0), login_secondary_button("Log out"))
                        .clicked()
                    {
                        self.sign_out();
                    }
                }
            },
        );
    }

    fn draw_header(&self, ui: &mut egui::Ui) {
        let app_version = concat!("v", env!("CARGO_PKG_VERSION"));

        ui.horizontal(|ui| {
            draw_icon(ui, self.brick_texture.as_ref(), 44.0);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Brick")
                            .heading()
                            .strong()
                            .color(Color32::from_rgb(244, 247, 251)),
                    );
                    capsule(
                        ui,
                        app_version,
                        Color32::from_rgb(215, 223, 234),
                        Color32::from_rgb(38, 42, 50),
                    );
                });
            });
        });
    }

    fn draw_status_panel(&self, ui: &mut egui::Ui) {
        let status = self.display_status();
        let row_height = if status.detail.is_empty() { 34.0 } else { 54.0 };

        panel_frame().show(ui, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), row_height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    status_dot(ui, status.accent);
                    ui.add_space(6.0);

                    let right_width = if status.version.is_some() {
                        134.0
                    } else {
                        28.0
                    };
                    let text_width = (ui.available_width() - right_width).max(180.0);

                    if !status.detail.is_empty() {
                        ui.allocate_ui_with_layout(
                            egui::vec2(text_width, row_height),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(status.title.as_str())
                                            .size(22.0)
                                            .strong()
                                            .color(primary_text()),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(status.detail.as_str())
                                                .color(secondary_text()),
                                        )
                                        .wrap(),
                                    );
                                });
                            },
                        );
                    } else {
                        ui.allocate_ui_with_layout(
                            egui::vec2(text_width, row_height),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(status.title.as_str())
                                        .size(22.0)
                                        .strong()
                                        .color(primary_text()),
                                );
                            },
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(version) = status.version.as_deref() {
                            capsule(ui, version, primary_text(), status.accent_soft);
                        }
                        if self.sync_rx.is_some() {
                            ui.add(egui::Spinner::new().size(18.0).color(status.accent));
                        }
                    });
                },
            );
        });
    }

    fn draw_installs_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            section_title(ui, "World of Warcraft");
            if !self.view.settings.clients.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, "Add Folder").clicked() {
                        self.pick_wow_folders();
                    }
                });
            }
        });
        ui.add_space(8.0);

        if self.view.settings.clients.is_empty() {
            panel_frame().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Choose your WoW folder")
                            .size(18.0)
                            .strong()
                            .color(primary_text()),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new("Select your World of Warcraft folder.")
                                .color(secondary_text()),
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    if primary_button(ui, "Choose Folder").clicked() {
                        self.pick_wow_folders();
                    }
                    ui.add_space(4.0);
                });
            });
            return;
        }

        let clients = self.view.settings.clients.clone();
        panel_frame().show(ui, |ui| {
            let max_height = (ui.available_height() - 132.0).clamp(96.0, 180.0);
            egui::ScrollArea::vertical()
                .id_salt("wow-installs")
                .auto_shrink([false, true])
                .max_height(max_height)
                .show(ui, |ui| {
                    for (index, client) in clients.iter().enumerate() {
                        if index > 0 {
                            ui.separator();
                        }
                        self.draw_install_row(ui, client);
                    }
                });
        });
    }

    fn draw_install_row(&mut self, ui: &mut egui::Ui, client: &WowClient) {
        let compact = ui.available_width() < 500.0;
        if compact {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(client.display_label())
                            .strong()
                            .color(primary_text()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if danger_button(ui, "Remove").clicked() {
                            self.remove_client(&client.id);
                        }
                    });
                });
                path_label(ui, &client.path);
            });
            return;
        }

        ui.horizontal(|ui| {
            let detail_width = (ui.available_width() - 100.0).max(180.0);
            ui.vertical(|ui| {
                ui.set_width(detail_width);
                ui.label(
                    RichText::new(client.display_label())
                        .strong()
                        .color(primary_text()),
                );
                path_label(ui, &client.path);
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if danger_button(ui, "Remove").clicked() {
                    self.remove_client(&client.id);
                }
            });
        });
    }

    fn draw_settings_panel(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Settings");
        ui.add_space(8.0);

        panel_frame().show(ui, |ui| {
            let startup_enabled =
                self.view.settings.startup_enabled && self.view.settings.watcher_enabled;
            if settings_toggle_row(ui, "Open at login", startup_enabled) {
                self.set_automation(!startup_enabled);
            }

            ui.separator();

            let startup_minimized = self.view.settings.startup_minimized;
            if settings_toggle_row(ui, "Start minimized", startup_minimized) {
                self.set_startup_minimized(!startup_minimized);
            }

            if let AuthUiState::Authorized(user) = self.auth_state.clone() {
                ui.separator();
                self.draw_discord_settings_row(ui, &user);
            }
        });
    }

    fn draw_discord_settings_row(&mut self, ui: &mut egui::Ui, user: &AuthorizedUser) {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 44.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let action_width = 94.0;
                let detail_width = (ui.available_width() - action_width).max(180.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(detail_width, 44.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Discord").strong().color(primary_text()));
                            ui.label(
                                RichText::new(user.display_name.as_str())
                                    .small()
                                    .color(secondary_text()),
                            );
                        });
                    },
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, "Log out").clicked() {
                        self.sign_out();
                    }
                });
            },
        );
    }

    fn display_status(&self) -> DisplayStatus {
        let version = current_version(&self.view.settings.clients);

        if self
            .status
            .to_ascii_lowercase()
            .starts_with("installing brick ")
        {
            return DisplayStatus {
                title: "Updating Brick".to_string(),
                detail: "Brick will restart to finish.".to_string(),
                accent: info_accent(),
                accent_soft: Color32::from_rgb(29, 48, 62),
                version,
            };
        }

        if self.view.setup_required || self.view.settings.clients.is_empty() {
            return DisplayStatus {
                title: "Setup needed".to_string(),
                detail: "Select your World of Warcraft folder.".to_string(),
                accent: warning_accent(),
                accent_soft: Color32::from_rgb(61, 47, 30),
                version,
            };
        }

        if self.sync_rx.is_some() {
            return DisplayStatus {
                title: "Checking for updates".to_string(),
                detail: String::new(),
                accent: info_accent(),
                accent_soft: Color32::from_rgb(29, 48, 62),
                version,
            };
        }

        if status_needs_attention(&self.status) {
            return DisplayStatus {
                title: "Needs attention".to_string(),
                detail: friendly_problem(&self.status),
                accent: error_accent(),
                accent_soft: Color32::from_rgb(62, 32, 36),
                version,
            };
        }

        if !(self.view.settings.startup_enabled && self.view.settings.watcher_enabled) {
            return DisplayStatus {
                title: "Automatic updates are off".to_string(),
                detail: String::new(),
                accent: warning_accent(),
                accent_soft: Color32::from_rgb(61, 47, 30),
                version,
            };
        }

        let status_lower = self.status.to_ascii_lowercase();
        if status_lower.starts_with("installed ") {
            return DisplayStatus {
                title: "Updated".to_string(),
                detail: String::new(),
                accent: success_accent(),
                accent_soft: Color32::from_rgb(26, 59, 42),
                version,
            };
        }

        DisplayStatus {
            title: "Up to date".to_string(),
            detail: String::new(),
            accent: success_accent(),
            accent_soft: Color32::from_rgb(26, 59, 42),
            version,
        }
    }

    fn pick_wow_folders(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Select World of Warcraft folder")
            .pick_folders()
        {
            self.add_wow_folders(paths);
        }
    }
}

impl eframe::App for BrickApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_tray();
        self.handle_tray(ctx);
        self.handle_show_request(ctx);
        self.poll_auth();
        self.refresh_auth_if_expired();
        self.poll_app_update(ctx);
        self.start_periodic_app_update_check();
        self.handle_close_request(ctx);
        if self.auth_state.is_authorized() {
            self.poll_sync();
            self.poll_presence_heartbeat();
            self.start_periodic_presence_heartbeat();
            self.poll_roster();
            self.start_roster_refresh_if_stale();
            self.refresh_view_if_stale();
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(app_background())
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ctx, |ui| {
                ui.set_width(ui.available_width());
                self.draw_content(ui);
            });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

pub fn load_window_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(ICON_BYTES).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn load_texture(ctx: &egui::Context) -> Option<TextureHandle> {
    let image = image::load_from_memory(ICON_BYTES).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    let size = [width as usize, height as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &image.into_raw());
    Some(ctx.load_texture("brick-icon", color_image, egui::TextureOptions::LINEAR))
}

fn spawn_show_request_wake(ctx: &egui::Context) {
    let ctx = ctx.clone();
    thread::spawn(move || {
        let mut last_token = single_instance::read_show_request().ok().flatten();

        loop {
            thread::sleep(Duration::from_millis(250));
            let token = single_instance::read_show_request().ok().flatten();
            if token != last_token {
                last_token = token;
                ctx.request_repaint();
            }
        }
    });
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.visuals.panel_fill = app_background();
    style.visuals.window_fill = app_background();
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(35, 39, 47);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 50, 60);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(55, 61, 72);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, primary_text());
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    style.visuals.selection.bg_fill = Color32::from_rgb(65, 120, 170);
    ctx.set_style(style);
}

fn draw_icon(ui: &mut egui::Ui, texture: Option<&TextureHandle>, size: f32) {
    if let Some(texture) = texture {
        ui.image((texture.id(), egui::vec2(size, size)));
    } else {
        ui.allocate_space(egui::vec2(size, size));
    }
}

fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(panel_background())
        .stroke(Stroke::new(1.0_f32, panel_stroke()))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(18))
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(Color32::from_rgb(164, 176, 193)),
    );
}

fn path_label(ui: &mut egui::Ui, path: &str) {
    ui.add(
        egui::Label::new(RichText::new(path).small().color(muted_text()))
            .truncate()
            .selectable(false),
    )
    .on_hover_text(path);
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(22, 18, 12)),
        )
        .corner_radius(egui::CornerRadius::same(8))
        .fill(Color32::from_rgb(236, 161, 54)),
    )
}

fn login_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).strong().color(primary_text()))
        .corner_radius(egui::CornerRadius::same(8))
        .fill(Color32::from_rgb(82, 103, 235))
        .min_size(egui::vec2(236.0, 42.0))
}

fn login_secondary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).strong().color(secondary_text()))
        .corner_radius(egui::CornerRadius::same(8))
        .fill(Color32::from_rgb(31, 34, 40))
        .stroke(Stroke::new(1.0_f32, panel_stroke()))
        .min_size(egui::vec2(236.0, 36.0))
}

fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).strong().color(primary_text()))
            .corner_radius(egui::CornerRadius::same(8))
            .fill(Color32::from_rgb(38, 42, 50)),
    )
}

fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(242, 104, 114)),
        )
        .corner_radius(egui::CornerRadius::same(8))
        .fill(Color32::from_rgb(38, 42, 50)),
    )
}

fn capsule(ui: &mut egui::Ui, text: &str, text_color: Color32, fill: Color32) {
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(text_color));
        });
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().circle_filled(rect.center(), 6.0, color);
        ui.painter().circle_stroke(
            rect.center(),
            7.0,
            Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 70),
            ),
        );
    }
}

fn toggle(ui: &mut egui::Ui, on: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(46.0, 24.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if on {
            Color32::from_rgb(40, 130, 82)
        } else {
            Color32::from_rgb(74, 79, 89)
        };
        let knob = Color32::from_rgb(245, 247, 250);
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(12), bg);
        let x = if on {
            rect.right() - 12.0
        } else {
            rect.left() + 12.0
        };
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), 8.5, knob);
    }
    response.clicked()
}

fn settings_toggle_row(ui: &mut egui::Ui, label: &str, on: bool) -> bool {
    let mut clicked = false;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 34.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let text_width = (ui.available_width() - 74.0).max(180.0);

            ui.allocate_ui_with_layout(
                egui::vec2(text_width, 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(RichText::new(label).strong().color(primary_text()));
                },
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if toggle(ui, on) {
                    clicked = true;
                }
            });
        },
    );
    clicked
}

fn initial_presence_state() -> PresenceUiState {
    if presence::configured() {
        PresenceUiState::Idle
    } else {
        PresenceUiState::Unavailable(presence::configuration_error())
    }
}

fn tab_button(ui: &mut egui::Ui, text: &str, selected: bool) -> egui::Response {
    let fill = if selected {
        Color32::from_rgb(38, 42, 50)
    } else {
        Color32::from_rgb(24, 27, 33)
    };
    let stroke = if selected {
        Stroke::new(1.0_f32, Color32::from_rgb(71, 78, 91))
    } else {
        Stroke::new(1.0_f32, panel_stroke())
    };
    let color = if selected {
        primary_text()
    } else {
        secondary_text()
    };

    ui.add_sized(
        egui::vec2(106.0, 34.0),
        egui::Button::new(RichText::new(text).strong().color(color))
            .corner_radius(egui::CornerRadius::same(8))
            .fill(fill)
            .stroke(stroke),
    )
}

fn empty_panel_message(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(18.0);
        ui.label(
            RichText::new(title)
                .size(18.0)
                .strong()
                .color(primary_text()),
        );
        ui.add(egui::Label::new(RichText::new(detail).color(secondary_text())).wrap());
        ui.add_space(18.0);
    });
}

fn roster_notice(ui: &mut egui::Ui, detail: &str) {
    egui::Frame::NONE
        .fill(Color32::from_rgb(45, 39, 31))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(73, 61, 43)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(detail).color(secondary_text())).wrap());
        });
}

fn draw_roster_group(ui: &mut egui::Ui, title: &str, members: &[RosterMember]) {
    let online_count = members.iter().filter(|member| member.online).count();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .size(18.0)
                .strong()
                .color(primary_text()),
        );
        capsule(
            ui,
            &format!("{online_count}/{} online", members.len()),
            secondary_text(),
            Color32::from_rgb(38, 42, 50),
        );
    });
    ui.add_space(8.0);

    if members.is_empty() {
        ui.label(
            RichText::new(format!("No {} found.", title.to_ascii_lowercase())).color(muted_text()),
        );
        return;
    }

    for (index, member) in members.iter().enumerate() {
        if index > 0 {
            ui.separator();
        }
        draw_roster_member_row(ui, member);
    }
}

fn draw_roster_member_row(ui: &mut egui::Ui, member: &RosterMember) {
    let row_height = 36.0;
    let row_width = ui.available_width().max(260.0);
    let (row_rect, _) =
        ui.allocate_exact_size(egui::vec2(row_width, row_height), egui::Sense::hover());

    let dot_color = if member.online {
        success_accent()
    } else {
        Color32::from_rgb(87, 95, 108)
    };
    let dot_center = egui::pos2(row_rect.left() + 8.0, row_rect.center().y);
    if ui.is_rect_visible(row_rect) {
        ui.painter().circle_filled(dot_center, 6.0, dot_color);
        ui.painter().circle_stroke(
            dot_center,
            7.0,
            Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(dot_color.r(), dot_color.g(), dot_color.b(), 70),
            ),
        );
    }

    let name_left = row_rect.left() + 34.0;
    let status_width = if row_width < 520.0 { 148.0 } else { 220.0 };
    let status_rect = egui::Rect::from_min_max(
        egui::pos2(row_rect.right() - status_width, row_rect.top()),
        row_rect.right_bottom(),
    );
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(name_left, row_rect.top()),
        egui::pos2(
            (status_rect.left() - 16.0).max(name_left),
            row_rect.bottom(),
        ),
    );

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(name_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.set_clip_rect(name_rect);
            ui.add(
                egui::Label::new(
                    RichText::new(member.name.as_str())
                        .strong()
                        .color(primary_text()),
                )
                .truncate(),
            )
            .on_hover_text(format!("{} - {}", member.role, member.user_id));
        },
    );

    let status = roster_member_status(member);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(status_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        |ui| {
            ui.set_clip_rect(status_rect);
            ui.add(
                egui::Label::new(
                    RichText::new(status.as_str())
                        .small()
                        .color(if member.online {
                            secondary_text()
                        } else {
                            muted_text()
                        }),
                )
                .truncate(),
            )
            .on_hover_text(status);
        },
    );
}

fn roster_member_status(member: &RosterMember) -> String {
    if member.online {
        let version = member
            .app_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("v{value}"));
        let platform = member
            .platform
            .as_deref()
            .filter(|value| !value.trim().is_empty());

        match (version, platform) {
            (Some(version), Some(platform)) => format!("Online - {version} {platform}"),
            (Some(version), None) => format!("Online - {version}"),
            _ => "Online".to_string(),
        }
    } else if member.last_seen_at.is_some() {
        "Offline".to_string()
    } else {
        "Not reporting".to_string()
    }
}

fn roster_refresh_label(generated_at: &str, online_window_seconds: u64) -> String {
    format!(
        "Updated {generated_at}. Online means seen in the last {}.",
        duration_label(online_window_seconds)
    )
}

fn duration_label(seconds: u64) -> String {
    if seconds >= 60 && seconds % 60 == 0 {
        let minutes = seconds / 60;
        if minutes == 1 {
            "1 minute".to_string()
        } else {
            format!("{minutes} minutes")
        }
    } else if seconds == 1 {
        "1 second".to_string()
    } else {
        format!("{seconds} seconds")
    }
}

fn login_content_height(state: &AuthUiState) -> f32 {
    let base = 52.0 + 18.0 + 30.0 + 6.0 + 20.0 + 24.0 + 42.0;
    if matches!(state, AuthUiState::Checking) {
        base + 36.0
    } else if matches!(state, AuthUiState::Denied(_)) {
        base + 44.0
    } else {
        base
    }
}

fn login_copy(state: &AuthUiState) -> (&'static str, String, &'static str, bool) {
    match state {
        AuthUiState::ConfigMissing(error) => ("Setup needed", error.clone(), "Unavailable", false),
        AuthUiState::SignedOut => (
            "Sign in",
            "Use Discord to continue.".to_string(),
            "Continue with Discord",
            true,
        ),
        AuthUiState::Checking => (
            "Waiting for Discord",
            "Complete the prompt in your browser.".to_string(),
            "Waiting...",
            false,
        ),
        AuthUiState::Authorized(user) => (
            "Ready",
            format!("Signed in as {}.", user.display_name),
            "Continue",
            false,
        ),
        AuthUiState::Denied(error) => (
            "Could not verify access",
            friendly_auth_problem(error),
            "Try again",
            true,
        ),
    }
}

fn friendly_auth_problem(status: &str) -> String {
    let lower = status.to_ascii_lowercase();
    if lower.contains("does not have") || lower.contains("role") {
        format!(
            "This Discord account needs {} in {}.",
            discord_auth::role_label(),
            discord_auth::guild_name()
        )
    } else if lower.contains("timed out") {
        "Discord login timed out. Try again when the browser prompt is ready.".to_string()
    } else if lower.contains("invalid_client") || lower.contains("configured") {
        "Brick could not use its Discord app configuration.".to_string()
    } else if lower.contains("401") || lower.contains("unauthorized") || lower.contains("revoked") {
        "The saved Discord session expired or was revoked.".to_string()
    } else {
        "Discord could not verify access right now.".to_string()
    }
}

fn friendly_roster_problem(status: &str) -> String {
    let lower = status.to_ascii_lowercase();
    if lower.contains("built without") || lower.contains("configured") {
        "Roster service is not configured for this Brick build.".to_string()
    } else if lower.contains("rate limit") || lower.contains("429") {
        "Discord is rate limiting roster checks. Brick will retry automatically.".to_string()
    } else if lower.contains("session") || lower.contains("authorization") || lower.contains("401")
    {
        "Sign in again so Brick can refresh roster access.".to_string()
    } else if lower.contains("bot")
        || lower.contains("discord request failed")
        || lower.contains("server members")
        || lower.contains("403")
    {
        "The roster server cannot read Discord yet. Check the bot token and Server Members Intent."
            .to_string()
    } else {
        "Brick could not reach the roster service. It will try again automatically.".to_string()
    }
}

fn current_version(clients: &[WowClient]) -> Option<String> {
    let mut versions = clients
        .iter()
        .filter_map(|client| client.last_installed_version.as_deref())
        .filter(|version| !version.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();

    match versions.len() {
        0 => None,
        1 => versions.into_iter().next(),
        _ => Some("Mixed versions".to_string()),
    }
}

fn status_needs_attention(status: &str) -> bool {
    let status = status.to_ascii_lowercase();
    status.contains("failed")
        || status.contains("error")
        || status.contains("invalid")
        || status.contains("mismatch")
        || status.contains("not available")
        || status.contains("not published")
        || status.contains("stopped unexpectedly")
        || status.contains("could not")
        || status.contains("permission")
        || status.contains("refused")
}

fn friendly_problem(status: &str) -> String {
    let lower = status.to_ascii_lowercase();
    if lower.contains("download") || lower.contains("request") || lower.contains("not available") {
        "Brick could not reach the update service. It will try again automatically.".to_string()
    } else if lower.contains("signature") || lower.contains("sha") || lower.contains("mismatch") {
        "Brick rejected an update because it could not verify it.".to_string()
    } else if lower.contains("permission") || lower.contains("access") || lower.contains("denied") {
        "Brick could not write to one of the selected WoW folders.".to_string()
    } else if lower.contains("tray") {
        "Brick is running, but the system tray is not available on this desktop.".to_string()
    } else {
        "Brick could not update Advance Raid Tools. It will try again automatically.".to_string()
    }
}

fn app_background() -> Color32 {
    Color32::from_rgb(21, 23, 28)
}

fn panel_background() -> Color32 {
    Color32::from_rgb(29, 32, 38)
}

fn panel_stroke() -> Color32 {
    Color32::from_rgb(47, 52, 61)
}

fn primary_text() -> Color32 {
    Color32::from_rgb(241, 244, 248)
}

fn secondary_text() -> Color32 {
    Color32::from_rgb(183, 194, 208)
}

fn muted_text() -> Color32 {
    Color32::from_rgb(130, 142, 158)
}

fn success_accent() -> Color32 {
    Color32::from_rgb(69, 211, 127)
}

fn warning_accent() -> Color32 {
    Color32::from_rgb(236, 178, 80)
}

fn error_accent() -> Color32 {
    Color32::from_rgb(244, 91, 102)
}

fn info_accent() -> Color32 {
    Color32::from_rgb(94, 168, 224)
}
