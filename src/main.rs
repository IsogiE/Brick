#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod addon;
mod app_update;
mod atomic_file;
mod autostart;
mod browser;
mod cache_maintenance;
mod credential_store;
mod defensives;
mod discord_auth;
mod download;
mod presence;
mod profile;
mod recordings_ui;
mod replay_digits;
mod replay_edge;
mod replay_library;
mod replay_marker;
#[cfg(test)]
mod replay_smoke;
mod replay_sync;
mod review_compare;
mod review_compare_ui;
mod review_ui;
mod single_instance;
mod stream_player;
mod stream_preferences;
mod streams;
mod streams_ui;
mod tray;
mod ui;
mod warcraftlogs;

use std::{
    env,
    sync::{Arc, Mutex},
};

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let startup_mode = env::args().any(|arg| arg == "--startup");
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
            return Ok(());
        }
    };

    cache_maintenance::start();

    let start_hidden = startup_mode && addon::startup_minimized_enabled();

    let sync_lock = Arc::new(Mutex::new(()));
    addon::spawn_watcher(sync_lock.clone());
    presence::spawn_heartbeat_watcher();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Brick")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([980.0, 720.0])
        .with_clamp_size_to_monitor_size(true)
        .with_app_id("dev.isogi.brick");

    if let Some(icon) = ui::load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    if start_hidden {
        viewport = viewport.with_visible(false);
    }

    let options = eframe::NativeOptions {
        viewport,
        #[cfg(target_os = "linux")]
        event_loop_builder: Some(Box::new(|builder| {
            // Winit's Wayland backend cannot hide or restore a window. Use
            // Xwayland where available so closing to the tray remains reversible.
            use winit::platform::x11::EventLoopBuilderExtX11 as _;
            if env::var_os("DISPLAY").is_some_and(|display| !display.is_empty()) {
                builder.with_x11();
            }
        })),
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
