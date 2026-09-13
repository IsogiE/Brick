use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
    time::{Duration, Instant},
};

use fs2::FileExt;
use uuid::Uuid;

use crate::addon;

const LOCK_FILE: &str = "brick.lock";
const SHOW_REQUEST_FILE: &str = "show-request";
#[cfg(target_os = "windows")]
const WINDOW_HANDLE_FILE: &str = "main-window";

#[derive(Debug)]
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

/// A replacement starts before the old window finishes shutting down. Wait for
/// its real lock release, without delaying ordinary launches or waiting forever.
pub fn acquire_for_start(update_restart: bool) -> Result<InstanceGuard, InstanceLockError> {
    if update_restart {
        acquire_until_released(acquire, Duration::from_secs(30))
    } else {
        acquire()
    }
}

fn acquire_until_released<T>(
    mut acquire: impl FnMut() -> Result<T, InstanceLockError>,
    timeout: Duration,
) -> Result<T, InstanceLockError> {
    let deadline = Instant::now() + timeout;
    loop {
        match acquire() {
            Err(InstanceLockError::AlreadyRunning) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            result => return result,
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
    acquire_file_lock_in(&dir)
}

fn acquire_file_lock_in(dir: &Path) -> Result<File, InstanceLockError> {
    fs::create_dir_all(dir).map_err(|error| {
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

#[cfg(all(test, target_os = "linux"))]
mod restart_tests {
    use super::*;

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("brick-restart-lock-{}", Uuid::new_v4())))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replacement_waits_for_a_real_lock_after_the_old_800ms_window() {
        let directory = Directory::new();
        let old = acquire_file_lock_in(&directory.0).unwrap();
        assert!(matches!(
            acquire_file_lock_in(&directory.0),
            Err(InstanceLockError::AlreadyRunning)
        ));
        let started = Instant::now();
        let close = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1_200));
            drop(old);
        });
        let new = acquire_until_released(
            || acquire_file_lock_in(&directory.0),
            Duration::from_secs(5),
        )
        .unwrap();
        close.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(1_200));
        assert!(matches!(
            acquire_file_lock_in(&directory.0),
            Err(InstanceLockError::AlreadyRunning)
        ));
        drop(new);
        assert!(acquire_file_lock_in(&directory.0).is_ok());
    }

    #[test]
    fn a_stuck_instance_has_a_bounded_wait_and_other_errors_return_immediately() {
        let directory = Directory::new();
        let _old = acquire_file_lock_in(&directory.0).unwrap();
        let started = Instant::now();
        assert!(matches!(
            acquire_until_released(
                || acquire_file_lock_in(&directory.0),
                Duration::from_millis(150)
            ),
            Err(InstanceLockError::AlreadyRunning)
        ));
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut attempts = 0;
        let result = acquire_until_released::<()>(
            || {
                attempts += 1;
                Err(InstanceLockError::Other("permission denied".into()))
            },
            Duration::from_secs(30),
        );
        assert!(matches!(result, Err(InstanceLockError::Other(_))));
        assert_eq!(attempts, 1);
    }
}
