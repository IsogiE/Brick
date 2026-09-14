#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod account_erasure;
mod account_erasure_ui;
mod addon;
mod app_update;
#[cfg(all(unix, not(target_os = "macos")))]
mod appimage_environment;
mod atomic_file;
mod autostart;
mod browser;
mod cache_maintenance;
mod credential_store;
mod defensives;
mod discord_auth;
mod download;
mod guild;
mod local_erasure;
mod presence;
mod profile;
mod protected_cache;
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
mod twitch_account;
mod twitch_account_ui;
mod ui;
mod warcraftlogs;
mod youtube_account;
mod youtube_account_ui;

use std::{
    env,
    sync::{Arc, Mutex},
};

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    if env::args().any(|arg| arg == "--local-erasure-protocol") {
        println!("BRICK-LOCAL-ERASURE-v1");
        return Ok(());
    }
    let purge_local = env::args().any(|arg| arg == "--purge-local-data");
    let update_restart = env::args().any(|arg| arg == "--update-restart");
    let startup_mode = env::args().any(|arg| arg == "--startup");
    let _instance_guard = match single_instance::acquire_for_start(update_restart) {
        Ok(guard) => Some(guard),
        Err(single_instance::InstanceLockError::AlreadyRunning) => {
            if purge_local {
                eprintln!("Close Brick before removing its local data.");
                std::process::exit(1);
            }
            if !startup_mode {
                let _ = single_instance::request_show();
            }
            return Ok(());
        }
        Err(single_instance::InstanceLockError::Other(error)) => {
            eprintln!("{error}");
            if purge_local {
                std::process::exit(1);
            }
            return Ok(());
        }
    };

    // Complete interrupted local reset before any credentials, views or writers
    // can start. Erasure failure deliberately leaves the normal app unopened.
    let reset = if purge_local {
        local_erasure::reset().map(|()| true)
    } else {
        local_erasure::resume_pending()
    };
    match reset {
        Ok(true) => {
            println!("Brick's local data was removed.");
            return Ok(());
        }
        Ok(false) => (),
        Err(error) => {
            eprintln!("{error}");
            if purge_local {
                std::process::exit(1);
            }
            // The privacy-only UI offers retry when the vault or a native
            // profile remains locked; normal startup below stays disabled.
        }
    }

    let erasure_pending = account_erasure::pending() || local_erasure::is_pending().unwrap_or(true);
    if erasure_pending {
        account_erasure::block_normal_requests();
    }
    if !erasure_pending {
        cache_maintenance::start();
    }

    let start_hidden = !erasure_pending && startup_mode && addon::startup_minimized_enabled();

    let sync_lock = Arc::new(Mutex::new(()));
    if !erasure_pending {
        addon::spawn_watcher(sync_lock.clone());
        presence::spawn_heartbeat_watcher();
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Brick")
        .with_inner_size([1440.0, 980.0])
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
