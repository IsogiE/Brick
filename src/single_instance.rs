use std::{
    fs::{self, File, OpenOptions},
    io,
};

use fs2::FileExt;
use uuid::Uuid;

use crate::addon;

const LOCK_FILE: &str = "brick.lock";
const SHOW_REQUEST_FILE: &str = "show-request";
#[cfg(target_os = "windows")]
const WINDOW_HANDLE_FILE: &str = "main-window";

pub enum InstanceLockError {
    AlreadyRunning,
    Other(String),
}

pub struct InstanceGuard {
    file: File,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn acquire() -> Result<InstanceGuard, InstanceLockError> {
    let dir = addon::config_dir().map_err(InstanceLockError::Other)?;
    fs::create_dir_all(&dir).map_err(|error| {
        InstanceLockError::Other(format!("Failed to create {}: {error}", dir.display()))
    })?;

    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            InstanceLockError::Other(format!("Failed to open {}: {error}", path.display()))
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok(InstanceGuard { file }),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(InstanceLockError::AlreadyRunning)
        }
        Err(error) => Err(InstanceLockError::Other(format!(
            "Failed to lock {}: {error}",
            path.display()
        ))),
    }
}

pub fn request_show() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    if show_saved_main_window()? {
        return Ok(());
    }

    let dir = addon::config_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
    fs::write(dir.join(SHOW_REQUEST_FILE), Uuid::new_v4().to_string())
        .map_err(|error| format!("Failed to signal running Brick instance: {error}"))
}

pub fn read_show_request() -> Result<Option<String>, String> {
    let path = addon::config_dir()?.join(SHOW_REQUEST_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let token = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?
        .trim()
        .to_string();

    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(token))
    }
}

#[cfg(target_os = "windows")]
pub fn remember_main_window_handle(hwnd: isize) -> Result<(), String> {
    let dir = addon::config_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
    fs::write(dir.join(WINDOW_HANDLE_FILE), hwnd.to_string())
        .map_err(|error| format!("Failed to remember Brick window: {error}"))
}

#[cfg(target_os = "windows")]
fn show_saved_main_window() -> Result<bool, String> {
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{IsWindow, SetForegroundWindow, ShowWindowAsync, SW_RESTORE},
    };

    let path = addon::config_dir()?.join(WINDOW_HANDLE_FILE);
    let Ok(contents) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(hwnd) = contents.trim().parse::<isize>() else {
        return Ok(false);
    };
    if hwnd == 0 {
        return Ok(false);
    }

    unsafe {
        let hwnd = hwnd as HWND;
        if IsWindow(hwnd) == 0 {
            return Ok(false);
        }

        ShowWindowAsync(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }

    Ok(true)
}
