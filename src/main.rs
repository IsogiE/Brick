#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod addon;
mod app_update;
mod autostart;
mod discord_auth;
mod presence;
mod single_instance;
mod tray;
mod ui;

use std::{
    env,
    sync::{Arc, Mutex},
};

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let startup_mode = env::args().any(|arg| arg == "--startup");
    let start_hidden = startup_mode && addon::startup_minimized_enabled();
    let _instance_guard = match single_instance::acquire() {
        Ok(guard) => Some(guard),
        Err(single_instance::InstanceLockError::AlreadyRunning) => {
            if !startup_mode {
                let _ = single_instance::request_show();
            }
            return Ok(());
        }
        Err(single_instance::InstanceLockError::Other(error)) => {
            eprintln!("{error}");
            None
        }
    };

    let sync_lock = Arc::new(Mutex::new(()));
    addon::spawn_watcher(sync_lock.clone());
    presence::spawn_heartbeat_watcher();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Brick")
        .with_inner_size([980.0, 760.0])
        .with_min_inner_size([760.0, 640.0])
        .with_app_id("dev.isogi.brick");

    if let Some(icon) = ui::load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    if start_hidden {
        viewport = viewport.with_visible(false);
    }

    let options = eframe::NativeOptions {
        viewport,
        #[cfg(target_os = "windows")]
        renderer: eframe::Renderer::Wgpu,
        #[cfg(not(target_os = "windows"))]
        renderer: eframe::Renderer::Glow,
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
