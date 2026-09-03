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
    autostart, single_instance, tray,
};

const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");

pub struct BrickApp {
    view: AppView,
    status: String,
    sync_lock: Arc<Mutex<()>>,
    sync_rx: Option<mpsc::Receiver<Result<SyncSummary, String>>>,
    brick_texture: Option<TextureHandle>,
    tray: Option<tray::TrayState>,
    tray_attempted: bool,
    quit_requested: bool,
    last_show_request: Option<String>,
    last_view_refresh: Instant,
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

        let mut app = Self {
            view,
            status,
            sync_lock,
            sync_rx: None,
            brick_texture,
            tray: None,
            tray_attempted: false,
            quit_requested: false,
            last_show_request: single_instance::read_show_request().ok().flatten(),
            last_view_refresh: Instant::now(),
        };

        app.reconcile_autostart();
        if !app.view.setup_required && app.view.settings.watcher_enabled {
            app.start_sync();
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

    fn start_sync(&mut self) {
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
        self.draw_header(ui);
        ui.add_space(18.0);
        self.draw_status_panel(ui);
        ui.add_space(18.0);
        self.draw_installs_section(ui);
        ui.add_space(18.0);
        self.draw_settings_panel(ui);
    }

    fn draw_header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            draw_icon(ui, self.brick_texture.as_ref(), 44.0);
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(
                    RichText::new("Brick")
                        .heading()
                        .strong()
                        .color(Color32::from_rgb(244, 247, 251)),
                );
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
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let text_width = (ui.available_width() - 74.0).max(180.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(text_width, 34.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label(
                                RichText::new("Open at login")
                                    .strong()
                                    .color(primary_text()),
                            );
                        },
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let enabled = self.view.settings.startup_enabled
                            && self.view.settings.watcher_enabled;
                        if toggle(ui, enabled) {
                            self.set_automation(!enabled);
                        }
                    });
                },
            );
        });
    }

    fn display_status(&self) -> DisplayStatus {
        let version = current_version(&self.view.settings.clients);

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
                title: "Advance Raid Tools updated".to_string(),
                detail: String::new(),
                accent: success_accent(),
                accent_soft: Color32::from_rgb(26, 59, 42),
                version,
            };
        }

        DisplayStatus {
            title: "Advance Raid Tools is up to date".to_string(),
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
        self.handle_close_request(ctx);
        self.poll_sync();
        self.refresh_view_if_stale();

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
