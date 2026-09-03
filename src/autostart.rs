use std::{env, path::PathBuf};

use auto_launch::AutoLaunchBuilder;

const APP_NAME: &str = "Brick";

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let launcher = launcher()?;
    if enabled {
        launcher
            .enable()
            .map_err(|error| format!("Failed to enable startup automation: {error}"))
    } else {
        launcher
            .disable()
            .map_err(|error| format!("Failed to disable startup automation: {error}"))
    }
}

pub fn reconcile_enabled(enabled: bool) -> Result<(), String> {
    let launcher = launcher()?;
    let current = launcher.is_enabled().unwrap_or(false);
    if current == enabled {
        return Ok(());
    }

    if enabled {
        launcher
            .enable()
            .map_err(|error| format!("Failed to enable startup automation: {error}"))
    } else {
        launcher
            .disable()
            .map_err(|error| format!("Failed to disable startup automation: {error}"))
    }
}

fn launcher() -> Result<auto_launch::AutoLaunch, String> {
    let app_path = launch_path()?;
    let app_path = app_path
        .to_str()
        .ok_or_else(|| "Brick executable path is not valid UTF-8.".to_string())?;

    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(app_path)
        .set_args(&["--startup"])
        .build()
        .map_err(|error| format!("Failed to configure startup automation: {error}"))
}

fn launch_path() -> Result<PathBuf, String> {
    if let Some(appimage) = env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(appimage));
    }

    env::current_exe().map_err(|error| format!("Failed to locate Brick executable: {error}"))
}
