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
    #[cfg(target_os = "windows")]
    _mutex: Option<WindowsInstanceMutex>,
    file: Option<File>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

pub fn acquire() -> Result<InstanceGuard, InstanceLockError> {
    #[cfg(target_os = "windows")]
    {
        let mutex = match acquire_windows_mutex() {
            Ok(mutex) => Some(mutex),
            Err(InstanceLockError::AlreadyRunning) => {
                return Err(InstanceLockError::AlreadyRunning);
            }
            Err(InstanceLockError::Other(error)) => {
                eprintln!("{error}");
                None
            }
        };

        return match acquire_file_lock() {
            Ok(file) => Ok(InstanceGuard {
                _mutex: mutex,
                file: Some(file),
            }),
            Err(InstanceLockError::AlreadyRunning) => Err(InstanceLockError::AlreadyRunning),
            Err(error) if mutex.is_some() => {
                if let InstanceLockError::Other(error) = error {
                    eprintln!("{error}");
                }
                Ok(InstanceGuard {
                    _mutex: mutex,
                    file: None,
                })
            }
            Err(error) => Err(error),
        };
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(InstanceGuard {
            file: Some(acquire_file_lock()?),
        })
    }
}

fn acquire_file_lock() -> Result<File, InstanceLockError> {
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
        Ok(()) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(InstanceLockError::AlreadyRunning)
        }
        Err(error) => Err(InstanceLockError::Other(format!(
            "Failed to lock {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(target_os = "windows")]
struct WindowsInstanceMutex {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
impl Drop for WindowsInstanceMutex {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn acquire_windows_mutex() -> Result<WindowsInstanceMutex, InstanceLockError> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
    };

    let name = "Local\\dev.isogi.brick.single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        SetLastError(0);
    }
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error().unwrap_or_default() as u32;
        if code == ERROR_ACCESS_DENIED {
            return Err(InstanceLockError::AlreadyRunning);
        }
        return Err(InstanceLockError::Other(format!(
            "Failed to create Brick single-instance mutex: {error}"
        )));
    }

    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Err(InstanceLockError::AlreadyRunning);
    }

    Ok(WindowsInstanceMutex { handle })
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::{acquire, InstanceLockError};

    #[test]
    fn windows_mutex_blocks_second_instance() {
        let _guard = match acquire() {
            Ok(guard) => guard,
            Err(InstanceLockError::AlreadyRunning) => return,
            Err(InstanceLockError::Other(error)) => panic!("{error}"),
        };

        assert!(matches!(acquire(), Err(InstanceLockError::AlreadyRunning)));
    }
}
