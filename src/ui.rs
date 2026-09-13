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
    app_update::{self, AvailableAppUpdate},
    autostart,
    discord_auth::{self, AuthorizedUser, RefreshError, SessionStatus},
    presence::{self, Roster, RosterMember},
    profile::{self, ProfileUi, RaidRole},
    single_instance,
    streams_ui::StreamsUi,
    tray,
};

const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");
const APP_UPDATE_CHECK_INTERVAL_SECS: u64 = 60;
const ROSTER_REFRESH_INTERVAL_SECS: u64 = 30;
const AUTH_REFRESH_CHECK_INTERVAL_SECS: u64 = 60;
const VIEW_REFRESH_INTERVAL_SECS: u64 = 60;
#[cfg(not(target_os = "windows"))]
const SHOW_REQUEST_POLL_INTERVAL_SECS: u64 = 5;
const IDLE_REPAINT_MAX_SECS: u64 = 60;

pub struct BrickApp {
    view: AppView,
    status: String,
    view_error: Option<String>,
    egui_ctx: egui::Context,
    sync_lock: Arc<Mutex<()>>,
    auth_state: AuthUiState,
    presence_state: PresenceUiState,
    streams: StreamsUi,
    profile: ProfileUi,
    active_tab: MainTab,
    sync_rx: Option<mpsc::Receiver<Result<SyncSummary, String>>>,
    auth_rx: Option<mpsc::Receiver<Result<AuthorizedUser, RefreshError>>>,
    guild_rx: Option<mpsc::Receiver<Result<AuthorizedUser, RefreshError>>>,
    guild_switching: bool,
    guild_access_lost: bool,
    last_guild_check: Instant,
    app_update_rx: Option<mpsc::Receiver<Result<Option<AvailableAppUpdate>, String>>>,
    app_update_install_rx: Option<mpsc::Receiver<Result<Option<String>, String>>>,
    roster_rx: Option<mpsc::Receiver<Result<Roster, String>>>,
    app_update_state: AppUpdateUiState,
    brick_texture: Option<TextureHandle>,
    tray: Option<tray::TrayState>,
    tray_attempted: bool,
    window_visible: bool,
    initial_visibility_applied: bool,
    #[cfg(target_os = "linux")]
    native_wayland: bool,
    quit_requested: bool,
    confirm_logout: bool,
    show_request_rx: mpsc::Receiver<String>,
    last_show_request: Option<String>,
    last_auth_check: Instant,
    last_view_refresh: Instant,
    last_app_update_check: Instant,
    last_roster_refresh: Instant,
    roster_notice: Option<String>,
}

#[derive(Debug, Clone)]
enum AuthUiState {
    ConfigMissing(String),
    SignedOut,
    Checking,
    Refreshing,
    Retrying,
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
    Home,
    Roster,
    Streams,
}

#[derive(Debug, Clone)]
enum PresenceUiState {
    Unavailable(String),
    Idle,
    Loading,
    Ready(Roster),
    Error(String),
}

#[derive(Debug, Clone)]
enum AppUpdateUiState {
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Installing,
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
        let show_request_rx = spawn_show_request_wake(&cc.egui_ctx);

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
            Ok(SessionStatus::NeedsRefresh) => AuthUiState::Refreshing,
            Ok(SessionStatus::Authorized(user)) => AuthUiState::Authorized(user),
            Err(error) => AuthUiState::Denied(error),
        };
        let now = Instant::now();
        if let AuthUiState::Authorized(user) = &auth_state {
            crate::guild::activate(&user.guild_id, &user.user_id);
        }

        let window_visible = !(startup_mode && view.settings.startup_minimized);
        let view_error = status_needs_attention(&status).then(|| status.clone());
        let mut app = Self {
            view,
            status,
            view_error,
            egui_ctx: cc.egui_ctx.clone(),
            sync_lock,
            auth_state,
            presence_state: initial_presence_state(),
            streams: StreamsUi::default(),
            profile: ProfileUi::default(),
            active_tab: MainTab::Home,
            sync_rx: None,
            auth_rx: None,
            guild_rx: None,
            guild_switching: false,
            guild_access_lost: false,
            last_guild_check: now.checked_sub(Duration::from_secs(300)).unwrap_or(now),
            app_update_rx: None,
            app_update_install_rx: None,
            roster_rx: None,
            app_update_state: AppUpdateUiState::Idle,
            brick_texture,
            tray: None,
            tray_attempted: false,
            window_visible,
            initial_visibility_applied: false,
            #[cfg(target_os = "linux")]
            native_wayland: false,
            quit_requested: false,
            confirm_logout: false,
            show_request_rx,
            last_show_request: single_instance::read_show_request().ok().flatten(),
            last_auth_check: now,
            last_view_refresh: now,
            last_app_update_check: now,
            last_roster_refresh: now
                .checked_sub(Duration::from_secs(ROSTER_REFRESH_INTERVAL_SECS))
                .unwrap_or(now),
            roster_notice: None,
        };

        if matches!(app.auth_state, AuthUiState::Refreshing) {
            app.start_auth_refresh();
        }
        app.start_app_update_check();
        if app.auth_state.is_authorized() {
            app.reconcile_autostart();
        }
        if app.auth_state.is_authorized() && !app.view.setup_required {
            app.start_sync();
        }
        app
    }

    fn refresh_view(&mut self) {
        self.apply_view_refresh(addon::load_view());
    }

    fn apply_view_refresh(&mut self, result: Result<AppView, String>) {
        // Failed reads must wait for the next interval too.
        self.last_view_refresh = Instant::now();
        match result {
            Ok(view) => {
                if self.view_error.as_deref() == Some(self.status.as_str()) {
                    self.status = "Ready".to_string();
                }
                self.view_error = None;
                self.view = view;
            }
            Err(error) => {
                self.view_error = Some(error.clone());
                self.status = error;
            }
        }
    }

    fn refresh_view_if_stale(&mut self) {
        if !self.window_visible
            || self.sync_rx.is_some()
            || self.last_view_refresh.elapsed() < Duration::from_secs(VIEW_REFRESH_INTERVAL_SECS)
        {
            return;
        }

        self.refresh_view();
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.window_visible = false;
        // Native Wayland cannot hide windows, but can minimize them. Keep a
        // taskbar entry there so the user can restore Brick without the tray.
        #[cfg(target_os = "linux")]
        if self.native_wayland {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn update_window_visibility(
        &mut self,
        visible: Option<bool>,
        minimized: Option<bool>,
        focused: bool,
    ) {
        if visible == Some(false) || minimized == Some(true) {
            self.window_visible = false;
        } else if (visible == Some(true) && minimized == Some(false)) || focused {
            self.window_visible = true;
        }
        // Wayland returns None for both queries. Preserve our requested state
        // until focus confirms that the user restored the window.
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.window_visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.refresh_view();

        if !self.view.setup_required {
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
                match autostart::set_enabled(self.view.settings.startup_enabled) {
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

    fn set_startup_enabled(&mut self, enabled: bool) {
        match addon::set_startup_enabled(enabled) {
            Ok(view) => self.view = view,
            Err(error) => {
                self.status = error;
                return;
            }
        }

        match autostart::set_enabled(enabled) {
            Ok(()) => {
                self.status = if enabled {
                    "Brick will open at login.".to_string()
                } else {
                    "Brick will stay closed at login.".to_string()
                };
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
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = discord_auth::login_with_browser().map_err(RefreshError::rejected);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.auth_rx = Some(rx);
        self.status = "Waiting for Discord.".to_string();
    }

    fn start_auth_refresh(&mut self) {
        if self.auth_rx.is_some() {
            return;
        }

        self.auth_state = AuthUiState::Refreshing;
        let (tx, rx) = mpsc::channel();
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = discord_auth::refresh_saved_session();
            let _ = tx.send(result);
            ctx.request_repaint();
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
                self.reset_changed_guild(&user);
                self.auth_state = AuthUiState::Authorized(user);
                self.auth_rx = None;
                self.status = "Discord access verified.".to_string();
                self.refresh_view();
                self.reconcile_autostart();
                if !self.view.setup_required {
                    self.start_sync();
                }
            }
            Ok(Err(error)) => {
                crate::guild::invalidate();
                self.reset_guild_panel();
                self.auth_state = if error.retryable {
                    AuthUiState::Retrying
                } else {
                    AuthUiState::Denied(error.message.clone())
                };
                self.auth_rx = None;
                self.last_auth_check = Instant::now();
                self.status = error.message;
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
        if self.auth_rx.is_some()
            || self.last_auth_check.elapsed()
                < Duration::from_secs(AUTH_REFRESH_CHECK_INTERVAL_SECS)
        {
            return;
        }
        self.last_auth_check = Instant::now();

        let should_refresh = match &self.auth_state {
            AuthUiState::Authorized(user) => {
                discord_auth::session_expired(user.expires_at_unix)
                    || discord_auth::session_renewal_due(user.created_at_unix)
            }
            AuthUiState::Retrying => true,
            _ => false,
        };

        if should_refresh {
            self.start_auth_refresh();
        }
    }

    fn reset_guild_panel(&mut self) {
        self.streams.clear();
        self.profile = ProfileUi::default();
        self.roster_rx = None;
        self.presence_state = initial_presence_state();
        self.roster_notice = None;
        self.last_roster_refresh = Instant::now()
            .checked_sub(Duration::from_secs(ROSTER_REFRESH_INTERVAL_SECS))
            .unwrap_or_else(Instant::now);
    }

    fn reset_changed_guild(&mut self, next: &AuthorizedUser) {
        if matches!(&self.auth_state, AuthUiState::Authorized(previous)
            if previous.guild_id != next.guild_id || previous.user_id != next.user_id)
        {
            crate::guild::invalidate();
            self.reset_guild_panel();
        }
        crate::guild::activate(&next.guild_id, &next.user_id);
    }

    fn handle_guild_access_loss(&mut self) {
        // A denied player or panel request removes this workspace while the
        // account's other memberships are checked through the same discovery.
        crate::guild::invalidate();
        self.reset_guild_panel();
        self.guild_access_lost = true;
        self.start_guild_lookup(None);
    }

    fn start_guild_lookup(&mut self, selected: Option<String>) {
        if self.guild_rx.is_some() || self.auth_rx.is_some() || !self.auth_state.is_authorized() {
            return;
        }
        self.guild_switching = selected.is_some();
        if self.guild_switching {
            crate::guild::invalidate();
            self.reset_guild_panel();
        }
        self.last_guild_check = Instant::now();
        let (tx, rx) = mpsc::channel();
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = match selected {
                Some(guild) => discord_auth::select_guild(&guild),
                None => discord_auth::discover_guilds(),
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.guild_rx = Some(rx);
    }

    fn poll_guilds(&mut self) {
        let Some(rx) = &self.guild_rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Guild lookup stopped. Brick will retry.".to_string().into())
            }
        };
        let switching = self.guild_switching;
        self.guild_rx = None;
        self.guild_switching = false;
        match result {
            Ok(user) => {
                self.guild_access_lost = false;
                self.reset_changed_guild(&user);
                self.auth_state = AuthUiState::Authorized(user);
            }
            Err(error) => {
                if switching {
                    self.status = error.message.clone();
                }
                if !error.retryable && (!switching || self.guild_access_lost) {
                    crate::guild::invalidate();
                    self.reset_guild_panel();
                    self.guild_access_lost = false;
                    self.auth_state = AuthUiState::Denied(error.message);
                }
                if error.retryable {
                    self.last_guild_check = Instant::now()
                        .checked_sub(Duration::from_secs(240))
                        .unwrap_or_else(Instant::now);
                }
            }
        }
    }

    fn draw_guild_selector(&mut self, ui: &mut egui::Ui) {
        let AuthUiState::Authorized(user) = &self.auth_state else {
            return;
        };
        if user.guilds.len() <= 1 {
            return;
        }
        let mut selected = user.guild_id.clone();
        ui.add_enabled_ui(self.guild_rx.is_none() && self.auth_rx.is_none(), |ui| {
            ui.set_max_width(135.0);
            ui.spacing_mut().button_padding.y = 4.0;
            egui::ComboBox::from_id_salt("guild-selector")
                .selected_text(&user.guild_name)
                .width(135.0)
                .truncate()
                .show_ui(ui, |ui| {
                    for guild in &user.guilds {
                        ui.selectable_value(
                            &mut selected,
                            guild.guild_id.clone(),
                            &guild.guild_name,
                        );
                    }
                })
                .response
                .on_hover_text(&user.guild_name);
        });
        if selected != user.guild_id {
            self.start_guild_lookup(Some(selected));
        }
    }

    fn sign_out(&mut self) {
        match discord_auth::clear_session() {
            Ok(()) => {
                self.confirm_logout = false;
                self.auth_state = AuthUiState::SignedOut;
                self.auth_rx = None;
                self.guild_rx = None;
                self.guild_switching = false;
                self.guild_access_lost = false;
                self.streams.clear();
                self.sync_rx = None;
                self.roster_rx = None;
                self.presence_state = initial_presence_state();
                self.roster_notice = None;
                self.profile = ProfileUi::default();
                self.status = "Signed out of Discord.".to_string();
            }
            Err(error) => {
                self.auth_state = AuthUiState::Denied(error.clone());
                self.status = error;
            }
        }
    }

    fn request_sign_out(&mut self) {
        self.confirm_logout = true;
    }

    fn apply_roster_role(&mut self, id: &str, role: Option<RaidRole>) {
        // An older in-flight roster response must not undo the confirmed write.
        self.roster_rx = None;
        if let PresenceUiState::Ready(roster) = &mut self.presence_state {
            if let Some(member) = roster
                .officers
                .iter_mut()
                .chain(&mut roster.raiders)
                .find(|member| member.user_id == id)
            {
                member.raid_role = role;
                roster.sort_members();
            }
        }
        self.last_roster_refresh = Instant::now();
        self.streams.profiles_changed();
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
        let ctx = self.egui_ctx.clone();
        crate::guild::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| presence::fetch_roster(&access_token));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.roster_rx = Some(rx);
        self.last_roster_refresh = Instant::now();
    }

    fn roster_refresh_enabled(&self) -> bool {
        self.window_visible
            && self.auth_state.is_authorized()
            && self.active_tab == MainTab::Roster
            && self.roster_rx.is_none()
            && !matches!(self.presence_state, PresenceUiState::Unavailable(_))
    }

    fn start_roster_refresh_if_stale(&mut self) {
        if !self.roster_refresh_enabled() {
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
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = addon::run_sync_with_lock(&lock);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.sync_rx = Some(rx);
    }

    fn start_app_update_check(&mut self) {
        if self.app_update_rx.is_some() || self.app_update_install_rx.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            let result = app_update::check_available_update();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.app_update_rx = Some(rx);
        self.app_update_state = AppUpdateUiState::Checking;
        self.last_app_update_check = Instant::now();
    }

    fn start_app_update_install(&mut self) {
        if self.app_update_rx.is_some() || self.app_update_install_rx.is_some() {
            return;
        }

        let version = match &self.app_update_state {
            AppUpdateUiState::Available(version) => version.clone(),
            _ => return,
        };

        let (tx, rx) = mpsc::channel();
        let ctx = self.egui_ctx.clone();
        thread::spawn(move || {
            // Rechecking the installer hash and replacing an AppImage can read
            // hundreds of MiB. Keep that work off the native UI thread too.
            // launch_installer still verifies the saved package before launch.
            let result = app_update::prepare_available_update().and_then(|update| {
                update
                    .map(|update| {
                        app_update::launch_installer(&update)?;
                        Ok(update.version)
                    })
                    .transpose()
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.app_update_install_rx = Some(rx);
        self.app_update_state = AppUpdateUiState::Installing;
        self.status = format!("Preparing Brick {version}.");
    }

    fn periodic_app_update_enabled(&self) -> bool {
        self.app_update_rx.is_none()
            && self.app_update_install_rx.is_none()
            && !matches!(
                self.app_update_state,
                AppUpdateUiState::Available(_) | AppUpdateUiState::Installing
            )
    }

    fn start_periodic_app_update_check(&mut self) {
        if !self.periodic_app_update_enabled() {
            return;
        }

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
            self.poll_app_update_install(ctx);
            return;
        };

        match rx.try_recv() {
            Ok(Ok(Some(update))) => {
                self.app_update_state = AppUpdateUiState::Available(update.version.clone());
                self.status = format!("Brick {} is available.", update.version);
                self.app_update_rx = None;
            }
            Ok(Ok(None)) => {
                self.app_update_state = AppUpdateUiState::UpToDate;
                self.app_update_rx = None;
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                self.app_update_state =
                    AppUpdateUiState::Error(friendly_app_update_problem(&error));
                self.app_update_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let message = "Brick app update check stopped unexpectedly.".to_string();
                let _ = addon::record_log(LogLevel::Warn, message.clone());
                self.app_update_state =
                    AppUpdateUiState::Error(friendly_app_update_problem(&message));
                self.app_update_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        self.poll_app_update_install(ctx);
    }

    fn poll_app_update_install(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.app_update_install_rx.as_ref() else {
            return;
        };

        match rx.try_recv() {
            Ok(Ok(Some(version))) => {
                let message = format!("Installing Brick {version}.");
                let _ = addon::record_log(LogLevel::Info, message.clone());
                self.status = message;
                self.app_update_state = AppUpdateUiState::Installing;
                self.app_update_install_rx = None;
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(Ok(None)) => {
                self.status = "Brick is up to date.".to_string();
                self.app_update_state = AppUpdateUiState::UpToDate;
                self.app_update_install_rx = None;
            }
            Ok(Err(error)) => {
                let _ = addon::record_log(LogLevel::Error, error.clone());
                self.status = error;
                self.app_update_state = AppUpdateUiState::Error("Update failed".to_string());
                self.app_update_install_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let message = "Brick app update stopped unexpectedly.".to_string();
                let _ = addon::record_log(LogLevel::Error, message.clone());
                self.status = message;
                self.app_update_state = AppUpdateUiState::Error("Update failed".to_string());
                self.app_update_install_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn ensure_tray(&mut self, ctx: &egui::Context) {
        if self.tray_attempted {
            return;
        }

        self.tray_attempted = true;
        match catch_unwind(AssertUnwindSafe(|| tray::create(ctx.clone()))) {
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
                #[cfg(not(target_os = "windows"))]
                tray::TrayCommand::Quit => {
                    self.quit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn handle_show_request(&mut self, ctx: &egui::Context) {
        let mut should_show = false;
        while let Ok(token) = self.show_request_rx.try_recv() {
            if self.last_show_request.as_deref() == Some(token.as_str()) {
                continue;
            }

            self.last_show_request = Some(token);
            should_show = true;
        }

        if should_show {
            self.show_window(ctx);
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.quit_requested && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }
    }

    fn draw_content(&mut self, ui: &mut egui::Ui) {
        if !self.auth_state.is_authorized() {
            self.draw_login_screen(ui);
            return;
        }

        if self.guild_switching || self.guild_access_lost {
            self.draw_header(ui);
            ui.add_space(18.0);
            empty_panel_message(ui, "Opening guild", "Checking your Discord access…");
            return;
        }

        if self.review_workspace_open() {
            self.draw_review_header(ui);
            ui.add_space(8.0);
        } else {
            self.draw_header(ui);
            let compact_streams =
                self.active_tab == MainTab::Streams && ui.ctx().content_rect().height() < 640.0;
            ui.add_space(if compact_streams { 4.0 } else { 14.0 });
            self.draw_tab_bar(ui);
            ui.add_space(if compact_streams { 4.0 } else { 18.0 });
        }
        match self.active_tab {
            MainTab::Home => self.draw_scrollable_updates_tab(ui),
            MainTab::Roster => self.draw_roster_tab(ui),
            MainTab::Streams => self.streams.draw(ui),
        }
    }

    fn draw_scrollable_updates_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("updates-tab")
            .auto_shrink([false, false])
            .max_height(ui.available_height().max(0.0))
            .min_scrolled_height(0.0)
            .show(ui, |ui| {
                let content_width = (ui.available_width() - 18.0).max(260.0);
                ui.set_width(content_width);
                self.draw_updates_tab(ui);
            });
    }

    fn draw_updates_tab(&mut self, ui: &mut egui::Ui) {
        self.draw_status_panel(ui);
        ui.add_space(18.0);
        self.draw_installs_section(ui);
        ui.add_space(18.0);
        self.draw_settings_panel(ui);
    }

    fn draw_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            self.draw_tab_buttons(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.draw_guild_selector(ui);
            });
        });
    }

    fn draw_tab_buttons(&mut self, ui: &mut egui::Ui) {
        if tab_button(ui, "Home", self.active_tab == MainTab::Home).clicked() {
            self.active_tab = MainTab::Home;
        }
        if tab_button(ui, "Roster", self.active_tab == MainTab::Roster).clicked() {
            self.active_tab = MainTab::Roster;
            self.start_roster_refresh_if_stale();
        }
        if tab_button(ui, "Streams", self.active_tab == MainTab::Streams).clicked() {
            self.active_tab = MainTab::Streams;
        }
    }

    fn review_workspace_open(&self) -> bool {
        self.auth_state.is_authorized()
            && self.active_tab == MainTab::Streams
            && self.streams.reviewing()
    }

    fn draw_review_header(&mut self, ui: &mut egui::Ui) {
        let compact = ui.available_width() < 850.0;
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 34.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                draw_icon(ui, self.brick_texture.as_ref(), 28.0);
                ui.label(
                    RichText::new("Brick")
                        .size(18.0)
                        .strong()
                        .color(primary_text()),
                );
                if !compact {
                    ui.label(
                        RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(muted_text()),
                    );
                }
                ui.add_space(10.0);
                self.draw_tab_buttons(ui);
                self.draw_guild_selector(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let available = matches!(self.app_update_state, AppUpdateUiState::Available(_));
                    let busy = self.app_update_rx.is_some() || self.app_update_install_rx.is_some();
                    let text = if available {
                        "Update now"
                    } else if compact {
                        "Updates"
                    } else {
                        "Check updates"
                    };
                    if ui
                        .add_enabled(!busy, compact_update_button(text, available))
                        .clicked()
                    {
                        if available {
                            self.start_app_update_install();
                        } else {
                            self.start_app_update_check();
                        }
                    }
                    if busy {
                        busy_indicator(ui, 12.0, info_accent());
                    }
                    let status = app_update_status_text(&self.app_update_state);
                    let attention =
                        available || matches!(self.app_update_state, AppUpdateUiState::Error(_));
                    ui.add(
                        egui::Label::new(RichText::new(&status).small().color(if attention {
                            warning_accent()
                        } else {
                            muted_text()
                        }))
                        .truncate(),
                    )
                    .on_hover_text(status);
                });
            },
        );
    }

    fn draw_roster_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, "Refresh").clicked() {
                    self.start_roster_refresh();
                }
                if self.roster_rx.is_some() {
                    busy_indicator(ui, 16.0, info_accent());
                }
            });
        });
        ui.add_space(8.0);

        let state = &self.presence_state;
        let mut role_change = None;
        panel_frame().show(ui, |ui| match state {
            PresenceUiState::Unavailable(error) => {
                empty_panel_message(ui, "Roster unavailable", &friendly_roster_problem(&error));
            }
            PresenceUiState::Idle | PresenceUiState::Loading => {
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    busy_indicator(ui, 24.0, info_accent());
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
                let max_height = ui.available_height().max(0.0);
                egui::ScrollArea::vertical()
                    .id_salt("guild-roster")
                    .auto_shrink([false, true])
                    .max_height(max_height)
                    .min_scrolled_height(0.0)
                    .show(ui, |ui| {
                        let content_width = (ui.available_width() - 18.0).max(260.0);
                        ui.set_width(content_width);

                        if let Some(notice) = self.roster_notice.as_deref() {
                            roster_notice(ui, notice);
                            ui.add_space(14.0);
                        }
                        let can_edit = roster.can_edit_roles;
                        draw_roster_group(
                            ui,
                            "Officers",
                            &roster.officers,
                            can_edit,
                            &self.profile,
                            &mut role_change,
                        );
                        ui.add_space(18.0);
                        draw_roster_group(
                            ui,
                            "Raiders",
                            &roster.raiders,
                            can_edit,
                            &self.profile,
                            &mut role_change,
                        );
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
        if let Some((id, role)) = role_change {
            self.profile.set_member_role(ui.ctx(), id, role);
        }
    }

    fn draw_login_screen(&mut self, ui: &mut egui::Ui) {
        let canvas = ui.max_rect();
        let state = self.auth_state.clone();
        let (title, detail, button_text, button_enabled) = login_copy(&state);
        let panel_width = canvas.width().clamp(320.0, 420.0);
        let panel_height = match state {
            AuthUiState::Denied(_) | AuthUiState::Retrying => 350.0,
            AuthUiState::Checking | AuthUiState::Refreshing => 340.0,
            AuthUiState::ConfigMissing(_) => 340.0,
            _ => 300.0,
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

                if matches!(state, AuthUiState::Checking | AuthUiState::Refreshing) {
                    busy_indicator(ui, 24.0, info_accent());
                    ui.add_space(12.0);
                }

                if button_enabled {
                    if ui
                        .add_sized(egui::vec2(236.0, 46.0), login_button(button_text))
                        .clicked()
                    {
                        if matches!(state, AuthUiState::Retrying) {
                            self.start_auth_refresh();
                        } else {
                            self.start_discord_login();
                        }
                    }
                } else {
                    ui.add_enabled_ui(false, |ui| {
                        ui.add_sized(egui::vec2(236.0, 46.0), login_button(button_text));
                    });
                }

                if matches!(state, AuthUiState::Denied(_) | AuthUiState::Retrying) {
                    ui.add_space(8.0);
                    if ui
                        .add_sized(egui::vec2(236.0, 36.0), login_secondary_button("Log out"))
                        .clicked()
                    {
                        self.request_sign_out();
                    }
                }
            },
        );
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        let app_version = concat!("v", env!("CARGO_PKG_VERSION"));

        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 46.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                draw_icon(ui, self.brick_texture.as_ref(), 44.0);
                ui.add_space(10.0);
                ui.label(
                    RichText::new("Brick")
                        .size(25.0)
                        .strong()
                        .color(Color32::from_rgb(244, 247, 251)),
                );
                ui.add_space(12.0);
                header_chip(
                    ui,
                    app_version,
                    56.0,
                    Color32::from_rgb(215, 223, 234),
                    Color32::from_rgb(38, 42, 50),
                );
                ui.add_space(10.0);
                self.draw_app_update_control(ui);
            },
        );
    }

    fn draw_app_update_control(&mut self, ui: &mut egui::Ui) {
        let available = matches!(self.app_update_state, AppUpdateUiState::Available(_));
        let busy = self.app_update_rx.is_some() || self.app_update_install_rx.is_some();
        let button_text = if available {
            "Update now"
        } else {
            "Check for updates"
        };
        let button_width = if available { 104.0 } else { 144.0 };

        let status = app_update_status_text(&self.app_update_state);
        if !status.is_empty() {
            let color = if available {
                warning_accent()
            } else {
                muted_text()
            };
            header_status_text(ui, status.as_str(), color);
            ui.add_space(10.0);
        }

        if busy {
            busy_indicator(ui, 12.0, info_accent());
            ui.add_space(10.0);
        }

        let response = ui
            .add_enabled_ui(!busy, |ui| {
                ui.add_sized(
                    egui::vec2(button_width, 26.0),
                    compact_update_button(button_text, available),
                )
            })
            .inner;

        if response.clicked() {
            if available {
                self.start_app_update_install();
            } else {
                self.start_app_update_check();
            }
        }
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
                            busy_indicator(ui, 18.0, status.accent);
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
            let startup_enabled = self.view.settings.startup_enabled;
            if settings_toggle_row(ui, "Open at login", startup_enabled) {
                self.set_startup_enabled(!startup_enabled);
            }

            ui.separator();

            let startup_minimized = self.view.settings.startup_minimized;
            if settings_toggle_row(ui, "Start minimized", startup_minimized) {
                self.set_startup_minimized(!startup_minimized);
            }

            if let AuthUiState::Authorized(user) = self.auth_state.clone() {
                ui.separator();
                self.draw_discord_settings_row(ui, &user);
                ui.separator();
                self.profile.draw(ui, &user.display_name);
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
                        self.request_sign_out();
                    }
                });
            },
        );
    }

    fn draw_logout_confirmation(&mut self, ctx: &egui::Context) {
        if !self.confirm_logout {
            return;
        }

        let mut open = self.confirm_logout;
        let mut confirm = false;
        let mut cancel = false;

        egui::Window::new("Log out")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .fixed_size(egui::vec2(320.0, 138.0))
            .frame(panel_frame())
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Are you sure you want to log out?")
                        .strong()
                        .color(primary_text()),
                );
                ui.add_space(18.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if danger_button(ui, "Yes").clicked() {
                        confirm = true;
                    }
                    if secondary_button(ui, "No").clicked() {
                        cancel = true;
                    }
                });
            });

        self.confirm_logout = open && !cancel;
        if confirm {
            self.sign_out();
        }
    }

    fn display_status(&self) -> DisplayStatus {
        let version = current_version(&self.view.settings.clients);

        if settings_problem(&self.status) && status_needs_attention(&self.status) {
            return DisplayStatus {
                title: "Settings need attention".to_string(),
                detail: friendly_problem(&self.status),
                accent: error_accent(),
                accent_soft: Color32::from_rgb(62, 32, 36),
                // A failed settings save can leave the cached version behind
                // the actual addon installation. Do not present it as current.
                version: None,
            };
        }

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

    fn next_repaint_after(&self) -> Duration {
        // Workers wake egui when they finish. Only schedule work that can run;
        // paused or in-flight checks must not leave expired deadlines spinning.
        let mut next = Duration::from_secs(IDLE_REPAINT_MAX_SECS);
        if self.periodic_app_update_enabled() {
            next = next.min(time_until(
                self.last_app_update_check,
                APP_UPDATE_CHECK_INTERVAL_SECS,
            ));
        }

        if (self.auth_state.is_authorized() || matches!(self.auth_state, AuthUiState::Retrying))
            && self.auth_rx.is_none()
        {
            next = next.min(time_until(
                self.last_auth_check,
                AUTH_REFRESH_CHECK_INTERVAL_SECS,
            ));
        }
        if self.auth_state.is_authorized() {
            if self.window_visible && self.sync_rx.is_none() {
                next = next.min(time_until(
                    self.last_view_refresh,
                    VIEW_REFRESH_INTERVAL_SECS,
                ));
            }
            if self.roster_refresh_enabled() {
                next = next.min(time_until(
                    self.last_roster_refresh,
                    ROSTER_REFRESH_INTERVAL_SECS,
                ));
            }
        }

        next = next.min(self.profile.repaint_after(
            self.auth_state.is_authorized()
                && self.window_visible
                && self.active_tab == MainTab::Home,
        ));
        next.min(self.streams.repaint_after(
            self.auth_state.is_authorized()
                && self.window_visible
                && self.active_tab == MainTab::Streams,
        ))
    }
}

impl eframe::App for BrickApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        if let Some(window) = frame.winit_window() {
            use winit::platform::wayland::WindowExtWayland as _;
            self.native_wayland = window.xdg_toplevel().is_some();
        }
        if self.initial_visibility_applied {
            if let Some(window) = frame.winit_window() {
                let visible = window.is_visible();
                let minimized = window.is_minimized();
                #[cfg(target_os = "windows")]
                if !self.window_visible && visible == Some(true) && minimized == Some(false) {
                    // A second instance restores via ShowWindowAsync. Sync
                    // winit's cached VISIBLE flag with that already-visible
                    // native window, otherwise its next hide becomes a no-op.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                // Minimize belongs to the window manager: retain the taskbar
                // entry. Only a close request (or startup) hides to the tray.
                self.update_window_visibility(visible, minimized, window.has_focus());
            }
        }
        tray::remember_main_window(frame, ctx);
        self.ensure_tray(ctx);
        if !self.initial_visibility_applied {
            self.initial_visibility_applied = true;
            if !self.window_visible {
                // Eframe makes the window visible after its first paint. Apply
                // startup visibility afterward, once a working tray is known.
                if self.tray.is_some() {
                    self.hide_window(ctx);
                } else {
                    self.show_window(ctx);
                }
            }
        }
        self.handle_tray(ctx);
        self.handle_show_request(ctx);
        self.poll_auth();
        self.poll_guilds();
        if self.auth_state.is_authorized()
            && self.last_guild_check.elapsed() >= Duration::from_secs(300)
        {
            self.start_guild_lookup(None);
        }
        self.refresh_auth_if_expired();
        self.poll_app_update(ctx);
        self.start_periodic_app_update_check();
        self.handle_close_request(ctx);
        if self.streams.tick(
            ctx,
            self.auth_state.is_authorized() && !self.guild_switching && !self.guild_access_lost,
            self.window_visible && self.active_tab == MainTab::Streams,
        ) {
            self.handle_guild_access_loss();
        }
        if self.auth_state.is_authorized() && !self.guild_switching && !self.guild_access_lost {
            let user = match &self.auth_state {
                AuthUiState::Authorized(user) => Some(user),
                _ => None,
            };
            if self.profile.tick(
                ctx,
                user,
                self.window_visible && self.active_tab == MainTab::Home,
            ) {
                self.start_roster_refresh();
                self.streams.profiles_changed();
            }
            if let Some((id, role)) = self.profile.take_role_change() {
                self.apply_roster_role(&id, role);
            }
            self.poll_sync();
            self.poll_roster();
            self.start_roster_refresh_if_stale();
            self.refresh_view_if_stale();
        }

        ctx.request_repaint_after(self.next_repaint_after());
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let fullscreen = self.auth_state.is_authorized()
            && self.window_visible
            && self.active_tab == MainTab::Streams
            && !self.confirm_logout
            && self.streams.fullscreen();
        let reviewing = self.review_workspace_open();
        let vertical_margin = if reviewing {
            12
        } else if self.auth_state.is_authorized()
            && self.active_tab == MainTab::Streams
            && ctx.content_rect().height() < 640.0
        {
            8
        } else {
            24
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(app_background())
                    .inner_margin(if fullscreen {
                        egui::Margin::ZERO
                    } else {
                        egui::Margin::symmetric(if reviewing { 20 } else { 28 }, vertical_margin)
                    }),
            )
            .show_inside(ui, |ui| {
                ui.set_width(ui.available_width());
                if fullscreen {
                    self.streams.draw_fullscreen(ui);
                } else {
                    self.draw_content(ui);
                }
            });
        self.draw_logout_confirmation(&ctx);
        if self.streams.update_player(
            frame,
            &ctx,
            self.auth_state.is_authorized()
                && !self.guild_switching
                && !self.guild_access_lost
                && self.window_visible
                && self.active_tab == MainTab::Streams
                && !self.confirm_logout,
        ) {
            self.handle_guild_access_loss();
        }

        if let Err(error) = crate::browser::open_pending_urls(&ctx) {
            let _ = addon::record_log(LogLevel::Error, error.clone());
            self.status = error;
        }
        ctx.request_repaint_after(self.next_repaint_after());
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

fn spawn_show_request_wake(ctx: &egui::Context) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();

    #[cfg(not(target_os = "windows"))]
    {
        let tx = tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let mut last_token = single_instance::read_show_request().ok().flatten();

            loop {
                thread::sleep(Duration::from_secs(SHOW_REQUEST_POLL_INTERVAL_SECS));
                let token = single_instance::read_show_request().ok().flatten();
                if token != last_token {
                    last_token = token.clone();
                    if let Some(token) = token {
                        let _ = tx.send(token);
                    }
                    ctx.request_repaint();
                }
            }
        });
    }

    #[cfg(target_os = "windows")]
    {
        let _ = ctx;
        let _ = tx;
    }

    rx
}

fn time_until(last: Instant, interval_secs: u64) -> Duration {
    Duration::from_secs(interval_secs).saturating_sub(last.elapsed())
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(egui::Visuals::dark());
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.visuals.panel_fill = app_background();
    style.visuals.window_fill = app_background();
    // egui computes button padding from the theme stroke before applying a
    // Button::stroke override. Different state widths would resize our outlined
    // buttons on hover even with expansion disabled. Keep all frame geometry
    // constant; colors still communicate hover, focus and pressed states.
    let widgets = &mut style.visuals.widgets;
    let corner_radius = widgets.inactive.corner_radius;
    for state in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.expansion = 0.0;
        state.bg_stroke.width = 1.0;
        state.corner_radius = corner_radius;
    }
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(35, 39, 47);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 50, 60);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(55, 61, 72);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, primary_text());
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    style.visuals.selection.bg_fill = Color32::from_rgb(65, 120, 170);
    ctx.set_global_style(style);
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
        .min_size(egui::vec2(236.0, 46.0))
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

fn compact_update_button(text: &str, available: bool) -> egui::Button<'_> {
    let fill = if available {
        Color32::from_rgb(236, 161, 54)
    } else {
        Color32::from_rgb(38, 42, 50)
    };
    let text_color = if available {
        Color32::from_rgb(22, 18, 12)
    } else {
        secondary_text()
    };

    egui::Button::new(RichText::new(text).small().strong().color(text_color))
        .corner_radius(egui::CornerRadius::same(7))
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, panel_stroke()))
}

fn header_chip(ui: &mut egui::Ui, text: &str, width: f32, text_color: Color32, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(7), fill);
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::same(7),
            Stroke::new(1.0_f32, panel_stroke()),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(11.0),
            text_color,
        );
    }
}

fn header_status_text(ui: &mut egui::Ui, text: &str, color: Color32) {
    let width = match text {
        "Up to date" | "Checking" => 68.0,
        text if text.starts_with('v') => 68.0,
        text if text.starts_with("Installing") => 112.0,
        _ => 132.0,
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(11.0),
            color,
        );
    }
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

// A network wait does not need an animation or a continuous render loop.
fn busy_indicator(ui: &mut egui::Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        for offset in [-0.3, 0.0, 0.3] {
            let center = rect.center() + egui::vec2(size * offset, 0.0);
            ui.painter().circle_filled(center, size * 0.09, color);
        }
    }
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

fn draw_roster_group(
    ui: &mut egui::Ui,
    title: &str,
    members: &[RosterMember],
    can_edit: bool,
    profile: &ProfileUi,
    change: &mut Option<(String, Option<RaidRole>)>,
) {
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
        ui.push_id(&member.user_id, |ui| {
            draw_roster_member_row(ui, member, can_edit, profile, change);
        });
    }
}

fn draw_roster_member_row(
    ui: &mut egui::Ui,
    member: &RosterMember,
    can_edit: bool,
    profile: &ProfileUi,
    change: &mut Option<(String, Option<RaidRole>)>,
) {
    let row_height = 36.0;
    let row_width = ui.available_width().max(260.0);
    let (row_rect, _) =
        ui.allocate_exact_size(egui::vec2(row_width, row_height), egui::Sense::hover());
    if !ui.is_rect_visible(row_rect) {
        return;
    }

    let dot_color = if member.online {
        success_accent()
    } else {
        Color32::from_rgb(87, 95, 108)
    };
    let dot_center = egui::pos2(row_rect.left() + 8.0, row_rect.center().y);
    if ui.is_rect_visible(row_rect) {
        let painter = ui.painter_at(row_rect);
        painter.circle_filled(dot_center, 6.0, dot_color);
        painter.circle_stroke(
            dot_center,
            7.0,
            Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(dot_color.r(), dot_color.g(), dot_color.b(), 70),
            ),
        );
    }

    let name_left = row_rect.left() + 34.0;
    let role_width = if can_edit { 106.0 } else { 0.0 };
    let status_width = if row_width < 520.0 { 148.0 } else { 220.0 };
    let status_rect = egui::Rect::from_min_max(
        egui::pos2(row_rect.right() - status_width - role_width, row_rect.top()),
        row_rect.right_bottom() - egui::vec2(role_width, 0.0),
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
            ui.shrink_clip_rect(name_rect);
            profile::role_icon(ui, member.raid_role);
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
            ui.shrink_clip_rect(status_rect);
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
    if can_edit {
        let rect = egui::Rect::from_min_max(
            egui::pos2(row_rect.right() - role_width + 6.0, row_rect.top()),
            row_rect.right_bottom(),
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                let mut role = profile.role_for_member(&member.user_id, member.raid_role);
                if profile::role_picker(ui, ("roster-role", &member.user_id), &mut role) {
                    *change = Some((member.user_id.clone(), role));
                }
            },
        );
    }
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
    let base = 52.0 + 18.0 + 30.0 + 6.0 + 20.0 + 24.0 + 46.0;
    if matches!(state, AuthUiState::Checking | AuthUiState::Refreshing) {
        base + 44.0
    } else if matches!(state, AuthUiState::Denied(_) | AuthUiState::Retrying) {
        base + 52.0
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
        AuthUiState::Refreshing => (
            "Checking Discord",
            "Restoring your saved sign-in.".to_string(),
            "Checking...",
            false,
        ),
        AuthUiState::Retrying => (
            "Reconnecting to Discord",
            "Your sign-in is saved. Brick will retry automatically.".to_string(),
            "Retry now",
            true,
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
        "This Discord account needs a Raider or Officer role in an eligible guild.".to_string()
    } else if lower.contains("timed out") {
        "Discord login timed out. Try again when the browser prompt is ready.".to_string()
    } else if lower.contains("invalid_client") || lower.contains("configured") {
        "Brick could not use its Discord app configuration.".to_string()
    } else if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("revoked")
        || lower.contains("expired")
    {
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

fn app_update_status_text(state: &AppUpdateUiState) -> String {
    match state {
        AppUpdateUiState::Idle => String::new(),
        AppUpdateUiState::Checking => "Checking".to_string(),
        AppUpdateUiState::UpToDate => "Up to date".to_string(),
        AppUpdateUiState::Available(_) => "Update available".to_string(),
        AppUpdateUiState::Installing => "Installing update".to_string(),
        AppUpdateUiState::Error(message) => message.clone(),
    }
}

fn friendly_app_update_problem(status: &str) -> String {
    let lower = status.to_ascii_lowercase();
    if lower.contains("signature") || lower.contains("sha") || lower.contains("mismatch") {
        "Update not trusted".to_string()
    } else if lower.contains("supported") {
        "No installer for this device".to_string()
    } else {
        "Could not check updates".to_string()
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
    let disk_full = lower.contains("os error 112")
        || lower.contains("os error 28")
        || lower.contains("not enough space")
        || lower.contains("no space left")
        || lower.contains("disk full")
        || lower.contains("storage full");
    if settings_problem(status) {
        if disk_full {
            "Brick could not save its settings because the disk is full. Free some space; Brick will retry automatically.".to_string()
        } else if lower.contains("save") || lower.contains("write") {
            "Brick could not save its settings. Check disk space and access to Brick's settings folder.".to_string()
        } else {
            "Brick could not read its settings. Check access to Brick's settings folder; Brick will retry automatically.".to_string()
        }
    } else if disk_full {
        "Brick ran out of disk space. Free some space; Brick will retry automatically.".to_string()
    } else if lower.contains("download")
        || lower.contains("request")
        || lower.contains("not available")
    {
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

fn settings_problem(status: &str) -> bool {
    let lower = status.to_ascii_lowercase();
    lower.contains("brick settings")
        || lower.contains("settings.json")
        || lower.contains("appdata/localappdata")
        || lower.contains("home/xdg_config_home")
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eframe::App as _;

    // No disk, network, tray, or saved user session is needed to exercise scheduling.
    fn app() -> BrickApp {
        let now = Instant::now();
        BrickApp {
            view: AppView::default(),
            status: "Ready".into(),
            view_error: None,
            egui_ctx: egui::Context::default(),
            sync_lock: Arc::new(Mutex::new(())),
            auth_state: AuthUiState::Authorized(AuthorizedUser {
                user_id: "test".into(),
                display_name: "Test".into(),
                username: "test".into(),
                guild_name: "Advance".into(),
                role_label: "Raider".into(),
                expires_at_unix: u64::MAX,
                created_at_unix: u64::MAX,
                guild_id: crate::guild::ADVANCE.into(),
                guilds: Vec::new(),
            }),
            presence_state: PresenceUiState::Idle,
            streams: StreamsUi::default(),
            profile: ProfileUi::default(),
            active_tab: MainTab::Home,
            sync_rx: None,
            auth_rx: None,
            guild_rx: None,
            guild_switching: false,
            guild_access_lost: false,
            last_guild_check: now,
            app_update_rx: None,
            app_update_install_rx: None,
            roster_rx: None,
            app_update_state: AppUpdateUiState::UpToDate,
            brick_texture: None,
            tray: None,
            tray_attempted: true,
            window_visible: true,
            initial_visibility_applied: true,
            #[cfg(target_os = "linux")]
            native_wayland: false,
            quit_requested: false,
            confirm_logout: false,
            show_request_rx: mpsc::channel().1,
            last_show_request: None,
            last_auth_check: now,
            last_view_refresh: now,
            last_app_update_check: now,
            last_roster_refresh: now,
            roster_notice: None,
        }
    }

    #[test]
    fn guild_access_loss_pauses_the_panel_and_recovers_another_membership() {
        let mut app = app();
        let AuthUiState::Authorized(user) = &app.auth_state else {
            unreachable!();
        };
        let mut next = user.clone();
        next.guild_id = crate::guild::ASCENDANCE.into();
        next.guild_name = "Ascendance".into();
        let old_access = crate::guild::Access::new(
            "fixture-token".into(),
            user.guild_id.clone(),
            user.user_id.clone(),
            crate::guild::generation(),
        );
        let (guild_tx, guild_rx) = mpsc::channel();
        app.guild_rx = Some(guild_rx); // Already pending: no network or credential access.
        let (_roster_tx, roster_rx) = mpsc::channel();
        app.roster_rx = Some(roster_rx);
        app.roster_notice = Some("Previous guild data".into());

        app.handle_guild_access_loss();
        assert!(app.auth_state.is_authorized());
        assert!(app.guild_access_lost);
        assert!(app.roster_rx.is_none());
        assert!(app.roster_notice.is_none());
        assert!(old_access.check().is_err());

        guild_tx
            .send(Err("Temporary service failure".to_string().into()))
            .unwrap();
        app.poll_guilds();
        assert!(app.auth_state.is_authorized());
        assert!(app.guild_access_lost);

        let (guild_tx, guild_rx) = mpsc::channel();
        app.guild_rx = Some(guild_rx);
        guild_tx.send(Ok(next.clone())).unwrap();
        app.poll_guilds();
        let AuthUiState::Authorized(current) = &app.auth_state else {
            panic!("The other eligible guild must remain available without login");
        };
        assert_eq!(current.guild_id, crate::guild::ASCENDANCE);
        assert_eq!(current.user_id, next.user_id);
        assert_eq!(current.created_at_unix, next.created_at_unix);
        assert!(!app.guild_access_lost);
    }

    // Inspect both allocated rows and painted frames/text after the previous-frame
    // WidgetState has caught up. A single hover frame misses egui's ButtonStyle path.
    pub(crate) fn assert_static_button_hover(labels: &[&str], mut draw: impl FnMut(&mut egui::Ui)) {
        #[derive(Debug, PartialEq)]
        enum Geometry {
            Rect(egui::Rect, egui::CornerRadius, f32),
            Text(String, egui::Pos2, egui::Vec2),
        }
        fn collect(shape: &egui::Shape, out: &mut Vec<Geometry>) {
            match shape {
                egui::Shape::Rect(rect) => out.push(Geometry::Rect(
                    rect.rect,
                    rect.corner_radius,
                    rect.stroke.width,
                )),
                egui::Shape::Text(text) => out.push(Geometry::Text(
                    text.galley.text().into(),
                    text.pos,
                    text.galley.size(),
                )),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, out);
                    }
                }
                _ => {}
            }
        }
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            configure_style(&ctx);
            ctx.set_pixels_per_point(scale);
            let mut frame = |events| {
                let mut bounds = egui::Rect::NOTHING;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(980.0, 720.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        draw(ui);
                        bounds = ui.min_rect();
                    },
                );
                let mut geometry = Vec::new();
                for shape in output.shapes {
                    collect(&shape.shape, &mut geometry);
                }
                (bounds, geometry)
            };
            frame(vec![]);
            frame(vec![]);
            let baseline = frame(vec![]);
            for label in labels {
                let center = baseline
                    .1
                    .iter()
                    .find_map(|part| match part {
                        Geometry::Text(text, pos, size) if text == label => {
                            Some(*pos + *size * 0.5)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("Missing button {label}"));
                // The first frame discovers hover; later frames allocate with it.
                for n in 0..4 {
                    assert_eq!(
                        baseline,
                        frame(vec![egui::Event::PointerMoved(center)]),
                        "Button {label}, hover frame {n}, scale {scale} changed geometry",
                    );
                }
                for n in 0..4 {
                    assert_eq!(
                        baseline,
                        frame(vec![egui::Event::PointerGone]),
                        "Button {label}, exit frame {n}, scale {scale} changed geometry",
                    );
                }
            }
        }
    }

    #[test]
    fn common_buttons_keep_painted_geometry_on_hover() {
        assert_static_button_hover(
            &[
                "Refresh",
                "Choose Folder",
                "Log out",
                "Save profile",
                "Advanced",
            ],
            |ui| {
                ui.horizontal(|ui| {
                    secondary_button(ui, "Refresh");
                    primary_button(ui, "Choose Folder");
                    danger_button(ui, "Log out");
                    let _ = ui.button("Save profile");
                    ui.add_sized(
                        [80.0, 28.0],
                        egui::Button::new("Advanced").stroke(Stroke::new(1.0_f32, panel_stroke())),
                    );
                });
            },
        );
    }

    #[test]
    fn both_update_headers_keep_painted_geometry_on_hover() {
        for available in [false, true] {
            for review in [false, true] {
                let mut app = app();
                if available {
                    app.app_update_state = AppUpdateUiState::Available("0.4.7".into());
                }
                let label = if available {
                    "Update now"
                } else if review {
                    "Check updates"
                } else {
                    "Check for updates"
                };
                assert_static_button_hover(&["Home", "Roster", "Streams", label], |ui| {
                    if review {
                        app.draw_review_header(ui);
                    } else {
                        app.draw_header(ui);
                        app.draw_tab_bar(ui);
                    }
                });
            }
        }
    }

    fn overdue() -> Instant {
        Instant::now() - Duration::from_secs(600)
    }

    #[test]
    fn review_header_keeps_navigation_and_update_states_in_one_bounded_row() {
        for size in [
            egui::vec2(720.0, 560.0),
            egui::vec2(980.0, 600.0),
            egui::vec2(1440.0, 900.0),
        ] {
            for state in [
                AppUpdateUiState::UpToDate,
                AppUpdateUiState::Checking,
                AppUpdateUiState::Available("0.4.2".into()),
                AppUpdateUiState::Installing,
                AppUpdateUiState::Error("Update not trusted".into()),
                AppUpdateUiState::Error("An unexpectedly long update failure message ".repeat(20)),
            ] {
                let mut app = app();
                if let AuthUiState::Authorized(user) = &mut app.auth_state {
                    user.guild_name =
                        "A guild with a deliberately long name for layout checks".into();
                    user.guilds = ["11", "22", "33"].into_iter().map(|id| serde_json::from_value(serde_json::json!({
                        "guildId": id, "guildName": user.guild_name, "displayName": "Raider", "roleLabel": "Raider",
                        "roleIds": ["1"], "authorizedRoleIds": ["1"]
                    })).unwrap()).collect();
                    user.guild_id = "11".into();
                }
                app.active_tab = MainTab::Streams;
                app.app_update_state = state.clone();
                if matches!(
                    state,
                    AppUpdateUiState::Checking | AppUpdateUiState::Installing
                ) {
                    app.app_update_rx = Some(mpsc::channel().1);
                }
                let ctx = egui::Context::default();
                configure_style(&ctx);
                for _ in 0..2 {
                    let _ = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ui| {
                            egui::Frame::NONE
                                .inner_margin(egui::Margin::symmetric(20, 12))
                                .show(ui, |ui| {
                                    let before = ui.available_rect_before_wrap();
                                    app.draw_review_header(ui);
                                    assert!(
                                        ui.min_rect().right() <= before.right() + 1.0,
                                        "Header expanded beyond the window"
                                    );
                                    assert!(
                                        ui.min_rect().bottom() <= before.top() + 35.0,
                                        "Header wrapped into the replay workspace: size={size:?}, actual={:?}, before={before:?}", ui.min_rect()
                                    );
                                });
                        },
                    );
                }
            }
        }
    }

    #[test]
    fn settings_failures_are_distinct_from_addon_failures_even_without_clients() {
        let mut app = app();
        for error in [
            "Failed to parse settings.json: EOF while parsing a value at line 1 column 0",
            "Failed to save Brick settings at C:\\Users\\User\\settings.json: There is not enough space on the disk. (os error 112)",
            "Failed to read Brick settings at settings.json: Access is denied. (os error 5)",
        ] {
            app.status = error.into();
            let status = app.display_status();
            assert_eq!(status.title, "Settings need attention");
            assert!(status.detail.contains("settings"));
            assert!(!status.detail.contains("Advance Raid Tools"));
            assert!(!status.detail.contains("WoW folders"));
            assert!(status.version.is_none());
        }
        assert!(
            friendly_problem("Failed to write settings.json: os error 112")
                .contains("disk is full")
        );
        assert!(friendly_problem(
            "Failed to save Brick settings: No space left on device (os error 28)"
        )
        .contains("disk is full"));
    }

    #[test]
    fn successful_view_retry_clears_its_error_but_preserves_unrelated_failures() {
        let mut app = app();
        app.apply_view_refresh(Err("Failed to read Brick settings".into()));
        app.apply_view_refresh(Ok(AppView::default()));
        assert_eq!(app.status, "Ready");
        assert!(app.view_error.is_none());

        app.apply_view_refresh(Err("Failed to read Brick settings".into()));
        app.status = "Failed to verify addon signature".into();
        app.apply_view_refresh(Ok(AppView::default()));
        assert_eq!(app.status, "Failed to verify addon signature");
    }

    #[test]
    fn confirmed_roster_role_keeps_sections_and_discards_older_refresh() {
        let mut app = app();
        app.presence_state = PresenceUiState::Ready(serde_json::from_value(serde_json::json!({
            "generatedAt":"2026-09-10", "onlineWindowSeconds":60, "canEditRoles":true,
            "officers":[{"userId":"1","name":"Officer","role":"Officer","online":true,"raidRole":"dps"}],
            "raiders":[
                {"userId":"2","name":"Zulu","role":"Raider","online":true,"raidRole":"healer"},
                {"userId":"3","name":"Alpha","role":"Raider","online":true,"raidRole":"tank"}
            ]
        })).unwrap());
        let (tx, rx) = mpsc::channel();
        app.roster_rx = Some(rx);
        app.apply_roster_role("2", Some(RaidRole::Tank));
        let PresenceUiState::Ready(roster) = &app.presence_state else {
            panic!("roster was cleared");
        };
        assert_eq!(roster.officers[0].user_id, "1");
        assert_eq!(
            roster
                .raiders
                .iter()
                .map(|member| member.user_id.as_str())
                .collect::<Vec<_>>(),
            ["3", "2"]
        );
        assert_eq!(roster.raiders[1].raid_role, Some(RaidRole::Tank));
        assert!(app.roster_rx.is_none());
        assert!(tx.send(Ok(roster.clone())).is_err());
        assert!(app.last_roster_refresh.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn disabling_open_at_login_does_not_change_addon_update_status() {
        let mut app = app();
        app.view.settings.startup_enabled = false;
        app.view.settings.clients.push(WowClient {
            id: "retail".into(),
            flavor: addon::Flavor::Retail,
            path: "World of Warcraft/_retail_".into(),
            game_version: None,
            last_installed_version: Some("1.7.1".into()),
            last_installed_sha256: Some("abc123".into()),
            last_sync_at: None,
        });
        assert_eq!(app.display_status().title, "Up to date");

        app.status = "Installed 1.7.1 on 1 client(s).".into();
        assert_eq!(app.display_status().title, "Updated");

        app.sync_rx = Some(mpsc::channel().1);
        assert_eq!(app.display_status().title, "Checking for updates");
    }

    #[test]
    fn unknown_wayland_visibility_preserves_hidden_state_until_user_focuses() {
        let mut app = app();
        app.window_visible = false;
        app.update_window_visibility(None, None, false);
        assert!(!app.window_visible);
        app.update_window_visibility(None, None, true);
        assert!(app.window_visible);
        app.update_window_visibility(Some(true), Some(true), false);
        assert!(!app.window_visible);
        app.update_window_visibility(Some(true), Some(false), false);
        assert!(app.window_visible);
    }

    #[test]
    fn completed_update_check_keeps_hidden_window_hidden() {
        let mut app = app();
        app.window_visible = false;
        let (tx, rx) = mpsc::channel();
        app.app_update_rx = Some(rx);
        tx.send(Ok(Some(AvailableAppUpdate {
            version: "9.0.0".into(),
        })))
        .unwrap();
        let ctx = app.egui_ctx.clone();
        let output = ctx.run_ui(egui::RawInput::default(), |_| app.poll_app_update(&ctx));
        assert!(!app.window_visible);
        assert!(app.app_update_rx.is_none());
        assert!(matches!(
            app.app_update_state,
            AppUpdateUiState::Available(_)
        ));
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .is_empty());
    }

    #[test]
    fn offered_update_does_not_keep_an_expired_repaint_deadline() {
        let mut app = app();
        app.app_update_state = AppUpdateUiState::Available("9.0.0".into());
        app.last_app_update_check = overdue();
        app.start_periodic_app_update_check();
        assert!(app.app_update_rx.is_none());
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn workers_sleep_until_completion_instead_of_polling_every_100ms() {
        let mut app = app();
        app.app_update_rx = Some(mpsc::channel().1);
        app.sync_rx = Some(mpsc::channel().1);
        app.auth_rx = Some(mpsc::channel().1);
        app.roster_rx = Some(mpsc::channel().1);
        app.active_tab = MainTab::Roster;
        app.last_app_update_check = overdue();
        app.last_view_refresh = overdue();
        app.last_auth_check = overdue();
        app.last_roster_refresh = overdue();
        assert_eq!(app.next_repaint_after(), Duration::from_secs(60));
    }

    #[test]
    fn pending_installer_work_does_not_close_or_repaint_the_window() {
        let mut app = app();
        let (_tx, rx) = mpsc::channel();
        app.app_update_install_rx = Some(rx);
        app.app_update_state = AppUpdateUiState::Installing;
        app.last_app_update_check = overdue();
        let ctx = app.egui_ctx.clone();
        let output = ctx.run_ui(egui::RawInput::default(), |_| {
            app.poll_app_update_install(&ctx);
        });
        assert!(!app.quit_requested);
        assert!(app.app_update_install_rx.is_some());
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .is_empty());
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn unavailable_roster_does_not_retry_on_every_frame() {
        let mut app = app();
        app.active_tab = MainTab::Roster;
        app.presence_state = PresenceUiState::Unavailable("Not configured".into());
        app.last_roster_refresh = overdue();
        app.start_roster_refresh_if_stale();
        assert!(app.roster_rx.is_none());
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn failed_view_read_waits_before_retrying() {
        let mut app = app();
        app.last_view_refresh = overdue();
        app.apply_view_refresh(Err("Unreadable settings".into()));
        assert_eq!(app.status, "Unreadable settings");
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn hidden_window_skips_roster_and_filesystem_refreshes() {
        let mut app = app();
        app.window_visible = false;
        app.active_tab = MainTab::Roster;
        app.last_roster_refresh = overdue();
        app.last_view_refresh = overdue();
        app.start_roster_refresh_if_stale();
        app.refresh_view_if_stale();
        assert!(app.roster_rx.is_none());
        assert_eq!(app.status, "Ready");
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn due_checks_still_wake_and_completed_workers_are_consumed() {
        let mut app = app();
        app.last_app_update_check = overdue();
        assert_eq!(app.next_repaint_after(), Duration::ZERO);
        let (tx, rx) = mpsc::channel();
        app.app_update_rx = Some(rx);
        tx.send(Ok(Some(AvailableAppUpdate {
            version: "9.0.0".into(),
        })))
        .unwrap();
        app.poll_app_update(&app.egui_ctx.clone());
        assert!(matches!(
            app.app_update_state,
            AppUpdateUiState::Available(_)
        ));
        assert!(app.next_repaint_after() > Duration::from_secs(50));
    }

    #[test]
    fn rendering_a_network_wait_does_not_request_animation_frames() {
        let mut app = app();
        app.app_update_rx = Some(mpsc::channel().1);
        app.app_update_state = AppUpdateUiState::Checking;
        app.sync_rx = Some(mpsc::channel().1);
        let ctx = app.egui_ctx.clone();
        let mut frame = eframe::Frame::_new_kittest();
        // Allow egui's initial layout and font passes to settle first.
        for _ in 0..5 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| app.ui(ui, &mut frame));
        }
        let output = ctx.run_ui(egui::RawInput::default(), |ui| app.ui(ui, &mut frame));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::from_secs(1)
        );
    }
    #[test]
    fn a_temporary_refresh_failure_preserves_retry_state_without_busy_polling() {
        let mut app = app();
        app.auth_state = AuthUiState::Refreshing;
        let (tx, rx) = mpsc::channel();
        app.auth_rx = Some(rx);
        tx.send(Err(RefreshError {
            message: "Temporary Discord failure".into(),
            retryable: true,
        }))
        .unwrap();
        app.poll_auth();
        assert!(matches!(app.auth_state, AuthUiState::Retrying));
        assert!(!app.auth_state.is_authorized());
        assert!(app.auth_rx.is_none());
        let delay = app.next_repaint_after();
        assert!(delay >= Duration::from_secs(59) && delay <= Duration::from_secs(60));
        let (title, detail, button, enabled) = login_copy(&app.auth_state);
        assert_eq!(title, "Reconnecting to Discord");
        assert!(detail.contains("sign-in is saved"));
        assert_eq!(button, "Retry now");
        assert!(enabled);
    }

    #[test]
    fn a_confirmed_authentication_rejection_requires_login_instead_of_retrying_forever() {
        let mut app = app();
        let (tx, rx) = mpsc::channel();
        app.auth_rx = Some(rx);
        tx.send(Err(RefreshError::rejected(
            "Discord login was revoked".into(),
        )))
        .unwrap();
        app.poll_auth();
        assert!(matches!(app.auth_state, AuthUiState::Denied(_)));
        assert!(!app.auth_state.is_authorized());
        assert!(login_copy(&app.auth_state).1.contains("revoked"));
    }

    #[test]
    fn restoring_saved_credentials_does_not_ask_for_a_browser_prompt() {
        let copy = login_copy(&AuthUiState::Refreshing);
        assert_eq!(copy.0, "Checking Discord");
        assert!(!copy.1.contains("browser"));
        assert!(!copy.3);
    }
}
