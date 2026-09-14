use std::{env, path::PathBuf};

use auto_launch::AutoLaunchBuilder;

const APP_NAME: &str = "Brick";

/// Device reset removes this Windows user's exact Brick preference values.
/// Generic launcher disable also probes HKLM and leaves StartupApproved behind.
pub(crate) fn clear_for_erasure() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{
                RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
            },
        };
        let failure = "Couldn't remove Brick's startup settings. Try again.";
        let name: Vec<_> = APP_NAME.encode_utf16().chain(Some(0)).collect();
        for path in [
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
        ] {
            let path: Vec<_> = path.encode_utf16().chain(Some(0)).collect();
            let mut key = std::ptr::null_mut();
            let opened = unsafe {
                RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_SET_VALUE, &mut key)
            };
            match opened {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => continue,
                ERROR_SUCCESS => (),
                _ => return Err(failure.into()),
            }
            let deleted = unsafe { RegDeleteValueW(key, name.as_ptr()) };
            let closed = unsafe { RegCloseKey(key) };
            if !matches!(deleted, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND) || closed != ERROR_SUCCESS {
                return Err(failure.into());
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    set_enabled(false)
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let _permit = enabled
        .then(crate::local_erasure::write_permit)
        .transpose()
        .map_err(|error| error.to_string())?;
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
    let _permit = enabled
        .then(crate::local_erasure::write_permit)
        .transpose()
        .map_err(|error| error.to_string())?;
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
