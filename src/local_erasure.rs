//! Explicit device reset. Never follows saved WoW paths or reads keyring secrets.
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

const APP_ID: &str = "dev.isogi.brick";
const MARKER: &str = "local-reset-pending-v1";
const PENDING: &[u8] = b"BRICK-LOCAL-RESET-v1\n";
static WRITES: Fence = Fence(Mutex::new(false));
static RESET: Mutex<()> = Mutex::new(());

struct Fence(Mutex<bool>);

impl Fence {
    fn write(&self) -> io::Result<WritePermit<'_>> {
        let guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("Brick storage is unavailable."))?;
        if *guard {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Brick is resetting its local data. Restart when it finishes.",
            ));
        }
        Ok(WritePermit { _guard: guard })
    }
}

/// Holds a storage transaction through its keyring and filesystem writes.
pub(crate) struct WritePermit<'a> {
    _guard: MutexGuard<'a, bool>,
}

pub(crate) fn write_permit() -> io::Result<WritePermit<'static>> {
    WRITES.write()
}

#[derive(Debug)]
struct Plan {
    roots: Vec<PathBuf>,
    files: Vec<PathBuf>,
    marker: PathBuf,
}

impl Plan {
    fn current() -> Result<Self, String> {
        Self::from_environment(|name| std::env::var_os(name).map(PathBuf::from))
    }

    fn from_environment(get: impl Fn(&str) -> Option<PathBuf>) -> Result<Self, String> {
        fn absolute(value: PathBuf) -> Result<PathBuf, String> {
            if !plain_absolute(&value) {
                return Err("Brick's data directory is not a safe absolute path.".into());
            }
            Ok(value)
        }
        let mut roots = Vec::new();
        #[cfg(target_os = "linux")]
        let mut files = Vec::new();
        #[cfg(target_os = "windows")]
        let files = Vec::new();
        #[cfg(target_os = "linux")]
        let primary = {
            let home = get("HOME").map(absolute).transpose()?;
            let mut config = None;
            for (variable, fallback) in [
                ("XDG_CONFIG_HOME", ".config"),
                ("XDG_DATA_HOME", ".local/share"),
                ("XDG_CACHE_HOME", ".cache"),
            ] {
                let explicit = get(variable).map(absolute).transpose()?;
                let default = home.as_ref().map(|home| home.join(fallback));
                if variable == "XDG_CONFIG_HOME" {
                    config = explicit.clone().or_else(|| default.clone());
                }
                for base in explicit.into_iter().chain(default) {
                    if variable == "XDG_CONFIG_HOME" {
                        files.push(base.join("autostart/Brick.desktop"));
                        files.push(base.join("autostart/dev.isogi.brick.desktop"));
                    }
                    roots.push(base.join(APP_ID));
                }
            }
            config
                .ok_or("Brick's configuration directory is unavailable.")?
                .join(APP_ID)
        };
        #[cfg(target_os = "windows")]
        let primary = {
            let roaming = get("APPDATA").map(absolute).transpose()?;
            let local = get("LOCALAPPDATA").map(absolute).transpose()?;
            for base in roaming.iter().chain(local.iter()) {
                roots.push(base.join(APP_ID));
            }
            if let Some(local) = &local {
                // Historical per-user browser directory only, never Brick.exe.
                roots.push(local.join("Brick/brick.exe.WebView2"));
            }
            roaming
                .or(local)
                .ok_or("Brick's configuration directory is unavailable.")?
                .join(APP_ID)
        };
        roots.sort();
        roots.dedup();
        Ok(Self {
            marker: primary.join(MARKER),
            roots,
            files,
        })
    }

    fn pending(&self) -> Result<bool, String> {
        for root in &self.roots {
            let marker = root.join(MARKER);
            if marker == self.marker || root.file_name().is_some_and(|name| name == APP_ID) {
                match fs::symlink_metadata(&marker) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(_) => return Err("Couldn't check Brick's pending local reset.".into()),
                    Ok(metadata) => {
                        verify_path(&marker)?;
                        if !metadata.is_file() || metadata.len() != PENDING.len() as u64 {
                            return Err("Brick's pending local reset needs attention.".into());
                        }
                        if fs::read(&marker).map_err(|_| "Couldn't read Brick's pending reset.")?
                            != PENDING
                        {
                            return Err("Brick's pending local reset needs attention.".into());
                        }
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

/// Call only after stopping views/workers and completing remote revocations.
/// All Brick accounts on this device are cleared; this is not server erasure.
pub(crate) fn reset() -> Result<(), String> {
    let _reset = RESET
        .lock()
        .map_err(|_| "Brick's local reset is unavailable.")?;
    reset_with(&Plan::current()?, crate::credential_store::remove_all_local)
}

pub(crate) fn is_pending() -> Result<bool, String> {
    Plan::current()?.pending()
}

/// Run before normal startup. A failed/partial reset must never resume login.
/// A completed reset still requires exiting this process (its fence stays shut).
pub(crate) fn resume_pending() -> Result<bool, String> {
    let plan = Plan::current()?;
    if !plan.pending()? {
        return Ok(false);
    }
    let _reset = RESET
        .lock()
        .map_err(|_| "Brick's local reset is unavailable.")?;
    reset_with(&plan, crate::credential_store::remove_all_local)?;
    Ok(true)
}

fn reset_with(plan: &Plan, erase_vault: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    reset_using(plan, &WRITES, erase_vault, || {
        crate::protected_cache::forget_keys();
        crate::replay_library::forget_local_cache();
        crate::autostart::clear_for_erasure()
    })
}

fn reset_using(
    plan: &Plan,
    fence: &Fence,
    erase_vault: impl FnOnce() -> Result<(), String>,
    clear_runtime: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    // Do not begin deletion unless all roots are within ordinary directories.
    for root in &plan.roots {
        verify_path(root)?;
    }
    for path in &plan.files {
        verify_path(path)?;
    }
    let mut guard = fence
        .0
        .lock()
        .map_err(|_| "Brick storage is unavailable.")?;
    // Persist intent before closing the gate; in-flight saves have now drained.
    let permit = WritePermit { _guard: guard };
    crate::atomic_file::write_permitted(&plan.marker, PENDING, &permit)
        .map_err(|_| "Couldn't save Brick's pending local reset. Nothing was removed.")?;
    guard = permit._guard;
    *guard = true;
    drop(guard);

    erase_vault()?;
    clear_runtime()?;
    // The marker survives every partial filesystem failure, including a locked
    // native profile. Never report success and silently leave user data behind.
    for root in &plan.roots {
        clear_directory(root, 0, Some(root.as_path()) == plan.marker.parent())?;
    }
    for path in &plan.files {
        verify_path(path)?;
        match fs::remove_file(path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return Err("Couldn't remove Brick's startup setting.".into()),
        }
    }
    // Remove pending markers only when all keyring/file cleanup succeeded.
    for root in &plan.roots {
        let marker = root.join(MARKER);
        match fs::remove_file(&marker) {
            Ok(()) => sync_directory(root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return Err("Couldn't finish Brick's local reset. Try again.".into()),
        }
    }
    Ok(())
}

fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|part| {
            matches!(
                part,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
}

fn link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn verify_path(path: &Path) -> Result<(), String> {
    if !plain_absolute(path) {
        return Err("Brick's local reset refused an unsafe path.".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if link(&metadata) => {
                return Err("Brick's local reset cannot follow redirected data folders.".into())
            }
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(_) => return Err("Couldn't inspect Brick's local data.".into()),
        }
    }
    Ok(())
}

fn clear_directory(path: &Path, depth: usize, current_profile: bool) -> Result<(), String> {
    if depth > 64 {
        return Err("Brick's local data is nested too deeply to remove safely.".into());
    }
    verify_path(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("Couldn't inspect Brick's local data.".into()),
    };
    if !metadata.is_dir() {
        return Err("Brick's local data folder has an unexpected type.".into());
    }
    for entry in fs::read_dir(path).map_err(|_| "Couldn't open Brick's local data.")? {
        let entry = entry.map_err(|_| "Couldn't inspect Brick's local data.")?;
        if depth == 0 && entry.file_name() == MARKER {
            continue;
        }
        // Keep the live empty lock: unlinking it would permit a second process
        // to bypass its locked inode. This contains no user data.
        if depth == 0 && current_profile && entry.file_name() == "brick.lock" {
            verify_path(&entry.path())?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|_| "Couldn't inspect Brick's instance lock.")?;
            if !meta.is_file() || meta.len() != 0 {
                return Err("Brick's instance lock has unexpected contents.".into());
            }
            continue;
        }
        let path = entry.path();
        verify_path(&path)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| "Couldn't inspect Brick's local data.")?;
        if metadata.is_dir() {
            clear_directory(&path, depth + 1, false)?;
            fs::remove_dir(&path).map_err(|_| "Couldn't remove Brick's local folder.")?;
        } else if metadata.is_file() {
            fs::remove_file(&path).map_err(|_| "Couldn't remove Brick's local file.")?;
        } else {
            return Err("Brick's local reset refused an unexpected file type.".into());
        }
    }
    sync_directory(path)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| "Couldn't finish removing Brick's local data.")?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{mpsc, Arc},
        time::Duration,
    };
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("brick-erasure-fixture-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn plan(&self) -> Plan {
            let profile = self.0.join(APP_ID);
            Plan {
                marker: profile.join(MARKER),
                roots: vec![profile],
                files: Vec::new(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn failed_reset_keeps_intent_and_blocks_saves_until_retry_finishes() {
        let f = Fixture::new();
        let plan = f.plan();
        fs::create_dir_all(&plan.roots[0]).unwrap();
        fs::write(
            plan.roots[0].join("youtube-old-marker.dat"),
            b"protected fixture",
        )
        .unwrap();
        fs::write(plan.roots[0].join("brick.lock"), b"").unwrap();
        let outside = f.0.join("World of Warcraft");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"untouched").unwrap();
        let fence = Fence(Mutex::new(false));
        assert!(reset_using(&plan, &fence, || Err("vault unavailable".into()), || Ok(())).is_err());
        assert!(plan.pending().unwrap());
        assert!(fence.write().is_err());
        assert!(plan.roots[0].join("youtube-old-marker.dat").exists());
        reset_using(&plan, &fence, || Ok(()), || Ok(())).unwrap();
        assert!(!plan.pending().unwrap());
        assert!(fence.write().is_err());
        assert_eq!(fs::read_dir(&plan.roots[0]).unwrap().count(), 1);
        assert!(plan.roots[0].join("brick.lock").exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"untouched");
    }
    #[test]
    fn reset_waits_for_inflight_transaction_then_rejects_a_stale_writer() {
        let fence = Arc::new(Fence(Mutex::new(false)));
        let current = fence.write().unwrap();
        let other = Arc::clone(&fence);
        let (started, waiting) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let reset = std::thread::spawn(move || {
            started.send(()).unwrap();
            let mut state = other.0.lock().unwrap();
            *state = true;
            finished.send(()).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done.try_recv().is_err());
        drop(current);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        reset.join().unwrap();
        assert!(fence.write().is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn plan_covers_custom_and_default_xdg_without_install_or_wow_paths() {
        let values = HashMap::from([
            ("HOME", PathBuf::from("/fixture/home")),
            ("XDG_CONFIG_HOME", PathBuf::from("/fixture/custom-config")),
            ("XDG_DATA_HOME", PathBuf::from("/fixture/custom-data")),
            ("XDG_CACHE_HOME", PathBuf::from("/fixture/custom-cache")),
        ]);
        let plan = Plan::from_environment(|name| values.get(name).cloned()).unwrap();
        assert_eq!(plan.roots.len(), 6);
        assert_eq!(plan.files.len(), 4);
        assert!(plan.files.contains(&PathBuf::from(
            "/fixture/home/.config/autostart/dev.isogi.brick.desktop"
        )));
        assert_eq!(
            plan.marker,
            Path::new("/fixture/custom-config/dev.isogi.brick").join(MARKER)
        );
        for path in plan.roots {
            assert_eq!(path.file_name().unwrap(), APP_ID);
        }
        assert!(Plan::from_environment(|name| {
            if name == "HOME" {
                Some(PathBuf::from("/fixture/home"))
            } else if name == "XDG_CONFIG_HOME" {
                Some(PathBuf::from("relative"))
            } else {
                None
            }
        })
        .is_err());
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn plan_covers_roaming_local_and_legacy_browser_without_install_directory() {
        let values = HashMap::from([
            ("APPDATA", PathBuf::from(r"C:\fixture\Roaming")),
            ("LOCALAPPDATA", PathBuf::from(r"C:\fixture\Local")),
        ]);
        let plan = Plan::from_environment(|name| values.get(name).cloned()).unwrap();
        assert_eq!(plan.roots.len(), 3);
        assert!(plan
            .roots
            .contains(&PathBuf::from(r"C:\fixture\Local\Brick\brick.exe.WebView2")));
        assert!(!plan
            .roots
            .contains(&PathBuf::from(r"C:\fixture\Local\Brick")));
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires the offline disposable Windows reset fixture"]
    fn windows_device_reset_preserves_junction_targets_and_retries_locked_files() {
        use std::{os::windows::fs::OpenOptionsExt, process::Command};
        use windows_sys::Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_ANY},
        };

        // Stop before any registry/profile access unless the reviewed offline
        // helper supplied an exact synthetic profile in its disposable VM.
        assert_eq!(
            std::env::var("BRICK_LOCAL_ERASURE_FIXTURE").as_deref(),
            Ok("1")
        );
        let root = PathBuf::from(std::env::var_os("BRICK_LOCAL_ERASURE_FIXTURE_DIR").unwrap());
        let task = root.parent().unwrap();
        assert_eq!(
            task.parent(),
            Some(Path::new(r"C:\BrickProviderValidation"))
        );
        let task_name = task.file_name().unwrap().to_str().unwrap();
        assert_eq!(task_name.len(), 32);
        assert!(task_name.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            root.file_name().unwrap(),
            format!("brick-erasure-fixture-{task_name}").as_str()
        );
        let profile = root.join("profile");
        for (name, expected) in [
            ("USERPROFILE", profile.clone()),
            ("APPDATA", profile.join("Roaming")),
            ("LOCALAPPDATA", profile.join("Local")),
        ] {
            assert_eq!(PathBuf::from(std::env::var_os(name).unwrap()), expected);
        }
        let plan = Plan::current().unwrap();
        assert!(plan.roots.iter().all(|path| path.starts_with(&root)));
        for path in &plan.roots {
            assert!(
                !path.exists(),
                "fixture must start with an empty app profile"
            );
        }

        const STARTUP_KEYS: [&str; 2] = [
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
        ];
        fn startup_value(key: &str, name: &str) -> bool {
            let key: Vec<_> = key.encode_utf16().chain(Some(0)).collect();
            let name: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
            let mut size = 0;
            let result = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    RRF_RT_ANY,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            assert!(matches!(
                result,
                ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND
            ));
            result == ERROR_SUCCESS
        }
        for key in STARTUP_KEYS {
            assert!(
                !startup_value(key, "Brick"),
                "never replace an existing startup entry"
            );
            assert!(!startup_value(key, "BrickErasureOtherFixture"));
            let added = Command::new(r"C:\Windows\System32\reg.exe")
                .args([
                    "add",
                    &format!("HKCU\\{key}"),
                    "/v",
                    "BrickErasureOtherFixture",
                    "/t",
                    "REG_SZ",
                    "/d",
                    "synthetic-nonexistent-command",
                    "/f",
                ])
                .output()
                .unwrap();
            assert!(added.status.success());
        }

        let other = root.join("OtherApplication");
        let wow = root.join("WorldOfWarcraft");
        let installed = profile.join("Local/Brick/Brick.exe");
        for directory in [&other, &wow, installed.parent().unwrap()] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(other.join("keep"), b"other application").unwrap();
        fs::write(wow.join("keep"), b"installed addon").unwrap();
        fs::write(&installed, b"installed Brick executable").unwrap();
        let make_junction = |stage: &str, link: &Path| {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let result = Command::new(r"C:\Windows\System32\cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(&other)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "synthetic junction creation failed at {stage}: status={}; stdout={:?}; stderr={:?}",
                result.status,
                String::from_utf8_lossy(&result.stdout[..result.stdout.len().min(4096)]),
                String::from_utf8_lossy(&result.stderr[..result.stderr.len().min(4096)])
            );
        };

        let redirected = profile.join("Local").join(APP_ID);
        make_junction("profile root", &redirected);
        assert!(reset().unwrap_err().contains("redirected"));
        assert!(!is_pending().unwrap());
        assert!(write_permit().is_ok());
        fs::remove_dir(&redirected).unwrap();

        let stores = [
            crate::credential_store::Store::new("fixture-only").unwrap(),
            crate::credential_store::Store::youtube("fixture-only").unwrap(),
            crate::credential_store::Store::twitch("fixture-only").unwrap(),
        ];
        for store in &stores {
            store.save(b"synthetic DPAPI grant").unwrap();
            assert_eq!(store.load().unwrap().unwrap(), b"synthetic DPAPI grant");
        }
        crate::protected_cache::save("fixture-only-guild", b"synthetic cache").unwrap();
        crate::autostart::set_enabled(true).unwrap();
        for key in STARTUP_KEYS {
            assert!(startup_value(key, "Brick"));
        }
        let nested = redirected.join("WebView2").join("redirected");
        make_junction("nested browser state", &nested);
        let legacy = profile.join("Local/Brick/brick.exe.WebView2");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("synthetic-cookie"), b"synthetic browser state").unwrap();

        assert!(reset().unwrap_err().contains("redirected"));
        assert!(is_pending().unwrap());
        assert!(write_permit().is_err());
        for key in STARTUP_KEYS {
            assert!(!startup_value(key, "Brick"));
        }
        fs::remove_dir(&nested).unwrap();
        let locked_path = plan.marker.parent().unwrap().join("locked-browser-state");
        let locked = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&locked_path)
            .unwrap();
        assert!(reset().is_err());
        assert!(is_pending().unwrap());
        assert!(locked.metadata().unwrap().is_file());
        drop(locked);
        assert!(resume_pending().unwrap());
        assert!(!is_pending().unwrap());
        for store in &stores {
            assert!(store.load().unwrap().is_none());
            assert!(store.save(b"stale worker grant").is_err());
        }
        assert!(crate::protected_cache::save("fixture-only-guild", b"stale cache").is_err());
        assert!(crate::autostart::set_enabled(true).is_err());
        for path in &plan.roots {
            assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
        }
        for key in STARTUP_KEYS {
            assert!(startup_value(key, "BrickErasureOtherFixture"));
            assert!(!startup_value(key, "Brick"));
        }
        assert_eq!(fs::read(other.join("keep")).unwrap(), b"other application");
        assert_eq!(fs::read(wow.join("keep")).unwrap(), b"installed addon");
        assert_eq!(fs::read(installed).unwrap(), b"installed Brick executable");
    }
    #[cfg(unix)]
    #[test]
    fn redirected_root_is_refused_and_nested_link_keeps_pending_marker() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.0.join("other-app");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"untouched").unwrap();
        let plan = f.plan();
        symlink(&outside, &plan.roots[0]).unwrap();
        let fence = Fence(Mutex::new(false));
        assert!(reset_using(
            &plan,
            &fence,
            || panic!("vault must not be touched"),
            || Ok(())
        )
        .is_err());
        assert!(fence.write().is_ok());
        fs::remove_file(&plan.roots[0]).unwrap();
        fs::create_dir(&plan.roots[0]).unwrap();
        symlink(&outside, plan.roots[0].join("redirected")).unwrap();
        assert!(reset_using(&plan, &fence, || Ok(()), || Ok(())).is_err());
        assert!(plan.pending().unwrap());
        assert!(fence.write().is_err());
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"untouched");
    }
}
