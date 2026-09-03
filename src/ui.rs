use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

use eframe::egui::{self, Color32, RichText, TextureHandle};

use crate::{
    addon::{self, AppView, LogLevel, SyncSummary},
    autostart, tray,
};

const ICON_BYTES: &[u8] = include_bytes!("assets/brick.png");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Dashboard,
    Settings,
    Logs,
}

pub struct BrickApp {
    view: AppView,
    active_tab: Tab,
    status: String,
    sync_lock: Arc<Mutex<()>>,
    sync_rx: Option<mpsc::Receiver<Result<SyncSummary, String>>>,
    brick_texture: Option<TextureHandle>,
    tray: Option<tray::TrayState>,
    tray_attempted: bool,
    quit_requested: bool,
}

impl BrickApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        sync_lock: Arc<Mutex<()>>,
        startup_mode: bool,
    ) -> Self {
        configure_style(&cc.egui_ctx);
        let brick_texture = load_texture(&cc.egui_ctx);

        let (view, status) = match addon::load_view() {
            Ok(view) => (view, "Automation ready".to_string()),
            Err(error) => {
                let _ = addon::record_log(LogLevel::Error, error.clone());
                (AppView::default(), error)
            }
        };

        let mut app = Self {
            view,
            active_tab: Tab::Dashboard,
            status,
            sync_lock,
            sync_rx: None,
            brick_texture,
            tray: None,
            tray_attempted: false,
            quit_requested: false,
        };

        app.reconcile_autostart();
        if !app.view.setup_required && app.view.settings.watcher_enabled {
            app.start_sync(if startup_mode { "startup" } else { "launch" });
        }

        app
    }

    fn refresh_view(&mut self) {
        match addon::load_view() {
            Ok(view) => self.view = view,
            Err(error) => self.status = error,
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
                        self.status = if added == 1 {
                            "Saved 1 WoW install and enabled startup automation.".to_string()
                        } else if added == 0 {
                            "Selected WoW installs were already configured.".to_string()
                        } else {
                            format!("Saved {added} WoW installs and enabled startup automation.")
                        };
                    }
                    Err(error) => {
                        let _ = addon::record_log(LogLevel::Warn, error.clone());
                        self.status = error;
                    }
                }
                self.start_sync("first setup");
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
                self.status = "Removed WoW install.".to_string();
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
                    "Startup automation enabled.".to_string()
                } else {
                    "Startup automation paused.".to_string()
                };
            }
            Err(error) => {
                let _ = addon::record_log(LogLevel::Warn, error.clone());
                self.status = error;
            }
        }
    }

    fn start_sync(&mut self, source: &str) {
        if self.sync_rx.is_some() {
            return;
        }

        self.status = format!("Checking for addon updates ({source})...");
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
                self.status = "Sync worker stopped unexpectedly.".to_string();
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
                let error = "Tray backend is not available on this desktop session.".to_string();
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && !self.quit_requested && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    fn draw_top_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top-bar")
            .exact_height(32.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(17, 21, 27)))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("BRICK")
                            .strong()
                            .color(Color32::from_rgb(184, 197, 213)),
                    );
                    ui.add_space(8.0);
                    ui.label(RichText::new("Advance").color(Color32::from_rgb(128, 142, 160)));
                });
            });
    }

    fn draw_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .exact_width(188.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(17, 21, 27)))
            .show(ctx, |ui| {
                ui.add_space(24.0);
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    draw_icon(ui, self.brick_texture.as_ref(), 54.0);
                });
                ui.add_space(24.0);

                nav_button(ui, &mut self.active_tab, Tab::Dashboard, "Dashboard");
                nav_button(ui, &mut self.active_tab, Tab::Settings, "Settings");
                nav_button(ui, &mut self.active_tab, Tab::Logs, "Logs");
            });
    }

    fn draw_footer(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer")
            .exact_height(26.0)
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(9, 12, 16)))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!(
                            "Watcher: {}",
                            if self.view.settings.watcher_enabled {
                                "Active"
                            } else {
                                "Paused"
                            }
                        ))
                        .small()
                        .color(Color32::from_rgb(154, 168, 187)),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "Startup: {}",
                            if self.view.settings.startup_enabled {
                                "Enabled"
                            } else {
                                "Disabled"
                            }
                        ))
                        .small()
                        .color(Color32::from_rgb(154, 168, 187)),
                    );
                });
            });
    }

    fn draw_dashboard(&mut self, ui: &mut egui::Ui) {
        self.draw_hero(ui);
        ui.add_space(22.0);
        section_title(ui, "ADDON STATUS");

        if self.view.settings.clients.is_empty() {
            empty_state(ui, &self.status);
            ui.add_space(12.0);
            if primary_button(ui, "Choose WoW Folders").clicked() {
                self.pick_wow_folders();
            }
        } else {
            self.draw_client_table(ui);
        }

        ui.add_space(24.0);
        self.draw_automation_panel(ui);
        ui.add_space(20.0);
        self.draw_status(ui);
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "AUTOMATION");
        let automation_text_width = (ui.available_width() - 64.0).max(180.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(automation_text_width);
                ui.label(RichText::new("Run at desktop startup").strong());
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Brick starts with the desktop session and installs the current guild package automatically.",
                        )
                        .color(Color32::from_rgb(154, 168, 187)),
                    )
                    .wrap(),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if toggle(ui, self.view.settings.startup_enabled && self.view.settings.watcher_enabled)
                {
                    let enabled =
                        !(self.view.settings.startup_enabled && self.view.settings.watcher_enabled);
                    self.set_automation(enabled);
                }
            });
        });

        ui.add_space(28.0);
        section_title(ui, "WORLD OF WARCRAFT INSTALLS");
        if self.view.settings.clients.is_empty() {
            ui.label(
                RichText::new("No clients configured.").color(Color32::from_rgb(140, 152, 170)),
            );
        } else {
            let clients = self.view.settings.clients.clone();
            for client in clients {
                let details_width = (ui.available_width() - 92.0).max(180.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(details_width);
                        ui.label(RichText::new(client.flavor.label()).strong());
                        ui.add(
                            egui::Label::new(
                                RichText::new(client.path.clone())
                                    .small()
                                    .color(Color32::from_rgb(118, 131, 151)),
                            )
                            .wrap(),
                        )
                        .on_hover_text(&client.path);
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if danger_button(ui, "Remove").clicked() {
                            self.remove_client(&client.id);
                        }
                    });
                });
                ui.separator();
            }
        }

        ui.add_space(12.0);
        if secondary_button(ui, "Add WoW Folders").clicked() {
            self.pick_wow_folders();
        }

        ui.add_space(28.0);
        section_title(ui, "MAINTENANCE");
        ui.add_enabled_ui(self.sync_rx.is_none(), |ui| {
            if secondary_button(ui, "Check Now").clicked() {
                self.start_sync("manual");
            }
        });
    }

    fn draw_logs(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "LOGS");
        if self.view.logs.is_empty() {
            ui.label(RichText::new("No logs yet.").color(Color32::from_rgb(140, 152, 170)));
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                for entry in self.view.logs.iter().rev() {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(
                            RichText::new(&entry.at).color(Color32::from_rgb(125, 138, 156)),
                        );
                        ui.label(
                            RichText::new(entry.level.label())
                                .strong()
                                .color(level_color(entry.level)),
                        );
                        ui.label(
                            RichText::new(&entry.message).color(Color32::from_rgb(216, 226, 239)),
                        );
                    });
                    ui.separator();
                }
            });
    }

    fn draw_hero(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            draw_icon(ui, self.brick_texture.as_ref(), 48.0);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(3.0);
                ui.label(
                    RichText::new("Brick")
                        .heading()
                        .strong()
                        .color(Color32::WHITE),
                );
                ui.label(RichText::new("Advance").color(Color32::from_rgb(174, 190, 210)));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let automated =
                    self.view.settings.watcher_enabled && self.view.settings.startup_enabled;
                pill(
                    ui,
                    if automated { "AUTOMATED" } else { "PAUSED" },
                    automated,
                );
            });
        });
    }

    fn draw_client_table(&self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let client_width = (width * 0.34).clamp(180.0, 280.0);
        let version_width = (width * 0.18).clamp(86.0, 150.0);
        let sync_width = (width * 0.21).clamp(90.0, 175.0);

        egui::Grid::new("clients")
            .num_columns(4)
            .striped(true)
            .min_col_width(76.0)
            .spacing([14.0, 10.0])
            .show(ui, |ui| {
                table_head(ui, "CLIENT");
                table_head(ui, "VERSION");
                table_head(ui, "LAST SYNC");
                table_head(ui, "STATUS");
                ui.end_row();

                for client in &self.view.settings.clients {
                    ui.vertical(|ui| {
                        ui.set_width(client_width);
                        ui.label(RichText::new(client.flavor.label()).strong());
                        ui.add(
                            egui::Label::new(
                                RichText::new(&client.path)
                                    .small()
                                    .color(Color32::from_rgb(118, 131, 151)),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&client.path);
                    });
                    ui.add_sized(
                        [version_width, 18.0],
                        egui::Label::new(client.last_installed_version.as_deref().unwrap_or("-"))
                            .truncate(),
                    );
                    ui.add_sized(
                        [sync_width, 18.0],
                        egui::Label::new(client.last_sync_at.as_deref().unwrap_or("-")).truncate(),
                    );
                    pill(ui, "AUTOMATED", true);
                    ui.end_row();
                }
            });
    }

    fn draw_automation_panel(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "STARTUP AUTOMATION");
        let text_width = (ui.available_width() - 64.0).max(180.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(text_width);
                let text = if self.view.setup_required {
                    "Choose a WoW install once; updates are automatic after that."
                } else {
                    "Checks the signed feed at launch and PC startup, then installs the current guild package when needed."
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(text).color(Color32::from_rgb(154, 168, 187)),
                    )
                    .wrap(),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let enabled = self.view.settings.startup_enabled && self.view.settings.watcher_enabled;
                if toggle(ui, enabled) {
                    self.set_automation(!enabled);
                }
            });
        });
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        ui.add(
            egui::Label::new(
                RichText::new(&self.status)
                    .strong()
                    .color(status_color(&self.status)),
            )
            .wrap(),
        );
    }

    fn pick_wow_folders(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Select World of Warcraft folder(s)")
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
        self.handle_close_request(ctx);
        self.poll_sync();

        self.draw_top_bar(ctx);
        self.draw_sidebar(ctx);
        self.draw_footer(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(23, 27, 34))
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ctx, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| match self.active_tab {
                    Tab::Dashboard => self.draw_dashboard(ui),
                    Tab::Settings => self.draw_settings(ui),
                    Tab::Logs => self.draw_logs(ui),
                });
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

fn configure_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    ctx.set_style(style);
}

fn nav_button(ui: &mut egui::Ui, active: &mut Tab, tab: Tab, text: &str) {
    let selected = *active == tab;
    let response = ui.add_sized([156.0, 38.0], egui::Button::selectable(selected, text));
    if response.clicked() {
        *active = tab;
    }
}

fn draw_icon(ui: &mut egui::Ui, texture: Option<&TextureHandle>, size: f32) {
    if let Some(texture) = texture {
        ui.image((texture.id(), egui::vec2(size, size)));
    } else {
        ui.allocate_space(egui::vec2(size, size));
    }
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(Color32::from_rgb(159, 183, 211)),
    );
}

fn empty_state(ui: &mut egui::Ui, status: &str) {
    ui.label(
        RichText::new("No WoW install configured")
            .strong()
            .color(Color32::WHITE),
    );
    ui.add(egui::Label::new(RichText::new(status).color(Color32::from_rgb(140, 152, 170))).wrap());
}

fn table_head(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(Color32::from_rgb(146, 163, 185)),
    );
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(16, 19, 25)),
        )
        .fill(Color32::from_rgb(248, 179, 45)),
    )
}

fn secondary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(226, 235, 247)),
        )
        .fill(Color32::from_rgb(32, 39, 51)),
    )
}

fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .strong()
                .color(Color32::from_rgb(255, 116, 123)),
        )
        .fill(Color32::from_rgb(32, 39, 51)),
    )
}

fn pill(ui: &mut egui::Ui, text: &str, ok: bool) {
    let color = if ok {
        Color32::from_rgb(67, 220, 139)
    } else {
        Color32::from_rgb(174, 185, 200)
    };
    ui.label(RichText::new(text).small().strong().color(color));
}

fn toggle(ui: &mut egui::Ui, on: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(46.0, 24.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if on {
            Color32::from_rgb(23, 69, 47)
        } else {
            Color32::from_rgb(48, 54, 65)
        };
        let knob = if on {
            Color32::from_rgb(64, 217, 138)
        } else {
            Color32::from_rgb(141, 150, 165)
        };
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

fn level_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Info => Color32::from_rgb(96, 231, 155),
        LogLevel::Warn => Color32::from_rgb(246, 197, 108),
        LogLevel::Error => Color32::from_rgb(255, 116, 123),
    }
}

fn status_color(status: &str) -> Color32 {
    let status = status.to_ascii_lowercase();
    if status.contains("failed") || status.contains("error") {
        Color32::from_rgb(255, 116, 123)
    } else if status.contains("not available")
        || status.contains("not published")
        || status.contains("waiting")
    {
        Color32::from_rgb(246, 197, 108)
    } else {
        Color32::from_rgb(96, 231, 155)
    }
}
