#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod addon;
mod autostart;
mod tray;
mod ui;

use std::{
    env,
    sync::{Arc, Mutex},
};

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let startup_mode = env::args().any(|arg| arg == "--startup");
    let sync_lock = Arc::new(Mutex::new(()));
    addon::spawn_watcher(sync_lock.clone());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Brick")
        .with_inner_size([740.0, 560.0])
        .with_min_inner_size([640.0, 520.0])
        .with_app_id("dev.isogi.brick");

    if let Some(icon) = ui::load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    if startup_mode {
        viewport = viewport.with_visible(false);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Brick",
        options,
        Box::new(move |cc| {
            Ok(Box::new(ui::BrickApp::new(
                cc,
                sync_lock.clone(),
                startup_mode,
            )))
        }),
    )
}
