//! Best-effort housekeeping for explicitly disposable native-browser caches.
//! Cookies, storage, protected settings, installers and WoW paths are excluded.
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, Once,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const IDLE_GRACE: Duration = Duration::from_secs(60);
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
// Never evict newly written cache files merely to hit the best-effort size target.
const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 12;
const PASS_BUDGET: Duration = Duration::from_millis(500);
static STARTED: Once = Once::new();
static PLAYERS: AtomicUsize = AtomicUsize::new(0);
static LAST_PLAYER: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) struct PlayerLease;
impl PlayerLease {
    pub(crate) fn new() -> Self {
        PLAYERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for PlayerLease {
    fn drop(&mut self) {
        if let Ok(mut last) = LAST_PLAYER.lock() {
            *last = Some(Instant::now());
        }
        PLAYERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn browser_idle() -> bool {
    PLAYERS.load(Ordering::Acquire) == 0
        && LAST_PLAYER
            .lock()
            .is_ok_and(|last| last.is_none_or(|at| at.elapsed() >= IDLE_GRACE))
}

pub(crate) fn start() {
    STARTED.call_once(|| {
        let _ = thread::Builder::new()
            .name("brick-cache-cleanup".into())
            .spawn(|| loop {
                if browser_idle() {
                    clean(
                        &cache_roots(),
                        SystemTime::now(),
                        Policy::default(),
                        browser_idle,
                    );
                }
                // One sleeping worker, independent of egui repaint frequency.
                thread::sleep(INTERVAL);
            });
    });
}

#[cfg(target_os = "windows")]
fn cache_roots() -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    if let Ok(path) = crate::stream_player::windows_profile::data_directory() {
        profiles.push(path.join("EBWebView"));
    }
    // Older current-user installs used this location. Never clean caches in a
    // shared Program Files install, where another Windows user may be active.
    if let Some(local) = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        profiles.push(local.join("Brick/brick.exe.WebView2/EBWebView"));
    }
    let mut roots = Vec::new();
    for profile in profiles {
        for suffix in [
            "Default/Cache",
            "Default/Code Cache",
            "Default/GPUCache",
            "Default/Media Cache",
            "ShaderCache",
            "GrShaderCache",
            "GPUPersistentCache",
            "Crashpad/reports",
            "Crashpad/attachments",
        ] {
            let path = profile.join(suffix);
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    roots
}

#[cfg(not(target_os = "windows"))]
fn cache_roots() -> Vec<PathBuf> {
    fn base(variable: &str, fallback: &str) -> Option<PathBuf> {
        std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join(fallback))
            })
    }
    [
        base("XDG_CACHE_HOME", ".cache"),
        base("XDG_DATA_HOME", ".local/share"),
    ]
    .into_iter()
    .flatten()
    .flat_map(|base| {
        [
            base.join("dev.isogi.brick/WebKitCache"),
            base.join("dev.isogi.brick/CacheStorage"),
        ]
    })
    .collect()
}

#[derive(Clone, Copy)]
struct Policy {
    bytes: u64,
    age: Duration,
    min_age: Duration,
    entries: usize,
    budget: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            bytes: MAX_BYTES,
            age: MAX_AGE,
            min_age: MIN_AGE,
            entries: MAX_ENTRIES,
            budget: PASS_BUDGET,
        }
    }
}
#[derive(Default, Debug)]
struct Outcome {
    visited: usize,
    removed: usize,
    bytes_removed: u64,
}
struct Candidate {
    root: PathBuf,
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn linked(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and other reparse points, not just symbolic links.
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn plain_directory(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        && path
            .ancestors()
            .all(|part| fs::symlink_metadata(part).is_ok_and(|m| m.is_dir() && !linked(&m)))
}

fn remove_unchanged(file: &Candidate) -> bool {
    // Recheck all ancestors immediately before mutation. Never follow a linked
    // cache root, nested directory, junction or a file replaced since the scan.
    if !file.path.starts_with(&file.root)
        || !plain_directory(&file.root)
        || !file.path.parent().is_some_and(plain_directory)
    {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(&file.path) else {
        return false;
    };
    if !metadata.is_file()
        || linked(&metadata)
        || metadata.len() != file.size
        || metadata.modified().ok() != Some(file.modified)
    {
        return false;
    }
    fs::remove_file(&file.path).is_ok()
}

fn clean(roots: &[PathBuf], now: SystemTime, policy: Policy, idle: impl Fn() -> bool) -> Outcome {
    let started = Instant::now();
    let mut outcome = Outcome::default();
    let mut candidates = Vec::new();
    let mut total = 0_u64;
    let allowed = || idle() && started.elapsed() < policy.budget;
    // A partial scan must still leave time to reclaim the entries it found.
    // Otherwise an already-large cache could exhaust every pass without progress.
    let scanning = || idle() && started.elapsed() < policy.budget / 2;
    'roots: for root in roots {
        if !scanning() || outcome.visited >= policy.entries {
            break;
        }
        if !plain_directory(root) {
            continue;
        }
        let mut pending = vec![(root.clone(), 0)];
        while let Some((directory, depth)) = pending.pop() {
            if !scanning() || outcome.visited >= policy.entries {
                break 'roots;
            }
            if !plain_directory(&directory) {
                continue;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries {
                if !scanning() || outcome.visited >= policy.entries {
                    break 'roots;
                }
                outcome.visited += 1;
                let Ok(entry) = entry else {
                    continue;
                };
                let path = entry.path();
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if linked(&metadata) {
                    continue;
                }
                if metadata.is_dir() {
                    if depth < MAX_DEPTH {
                        pending.push((path, depth + 1));
                    }
                } else if metadata.is_file() {
                    total = total.saturating_add(metadata.len());
                    if let Ok(modified) = metadata.modified() {
                        candidates.push(Candidate {
                            root: root.clone(),
                            path,
                            size: metadata.len(),
                            modified,
                        });
                    }
                }
            }
        }
    }
    candidates.sort_unstable_by_key(|file| file.modified);
    for file in candidates {
        if !allowed() {
            break;
        }
        let age = now.duration_since(file.modified).unwrap_or_default();
        if age >= policy.age || (total > policy.bytes && age >= policy.min_age) {
            if remove_unchanged(&file) {
                total = total.saturating_sub(file.size);
                outcome.removed += 1;
                outcome.bytes_removed = outcome.bytes_removed.saturating_add(file.size);
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("brick-cache-policy-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str, size: usize, age: u64) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![b'x'; size]).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(age)),
                )
                .unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn policy() -> Policy {
        Policy {
            bytes: 20,
            age: Duration::from_secs(100),
            min_age: Duration::from_secs(10),
            entries: 100,
            budget: Duration::from_secs(5),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_cache_roots_cover_the_explicit_profile_and_stay_per_user() {
        let profile = crate::stream_player::windows_profile::data_directory().unwrap();
        let roots = cache_roots();
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        assert!(roots.contains(&profile.join("EBWebView/Default/Cache")));
        assert!(roots.iter().all(|root| root.starts_with(&local)));
        assert!(!roots.contains(&profile));
        assert!(!roots.contains(&profile.join("EBWebView/Default")));
    }

    #[test]
    fn age_and_size_cleanup_preserve_recent_data_and_everything_outside_named_cache_roots() {
        let f = Fixture::new();
        let expired = f.file("profile/Cache/expired", 5, 110);
        let oldest = f.file("profile/Cache/oldest", 12, 50);
        let retained = f.file("profile/Cache/retained", 12, 20);
        let recent = f.file("profile/Cache/recent", 5, 0);
        for name in [
            "profile/Cookies",
            "profile/Local Storage/state",
            "profile/Network/Cookies",
            "settings.json",
            "discord-auth.dat",
            "warcraftlogs-fixture.dat",
            "updates/pending.exe",
            "WoW/Interface/AddOns/keep.lua",
        ] {
            f.file(name, 100, 1000);
        }
        let result = clean(
            &[f.0.join("profile/Cache")],
            SystemTime::now(),
            policy(),
            || true,
        );
        assert_eq!(result.removed, 2);
        assert!(!expired.exists() && !oldest.exists());
        assert!(retained.exists() && recent.exists());
        for name in [
            "profile/Cookies",
            "profile/Local Storage/state",
            "profile/Network/Cookies",
            "settings.json",
            "discord-auth.dat",
            "warcraftlogs-fixture.dat",
            "updates/pending.exe",
            "WoW/Interface/AddOns/keep.lua",
        ] {
            assert!(f.0.join(name).exists(), "{name}");
        }
    }

    #[test]
    fn byte_target_is_shared_across_roots_and_recent_files_remain_retryable() {
        let f = Fixture::new();
        let oldest = f.file("cache-a/a", 15, 50);
        let newer = f.file("cache-b/b", 15, 20);
        let active = f.file("cache-b/active", 30, 0);
        let outcome = clean(
            &[f.0.join("cache-a"), f.0.join("cache-b")],
            SystemTime::now(),
            policy(),
            || true,
        );
        assert_eq!(outcome.removed, 2);
        assert!(!oldest.exists() && !newer.exists() && active.exists());
    }

    #[test]
    fn active_browser_or_scan_budget_prevents_unbounded_background_work() {
        let f = Fixture::new();
        for n in 0..20 {
            f.file(&format!("Cache/{n}"), 1, 1000);
        }
        let roots = [f.0.join("Cache")];
        assert_eq!(
            clean(&roots, SystemTime::now(), policy(), || false).visited,
            0
        );
        assert_eq!(
            clean(
                &roots,
                SystemTime::now(),
                Policy {
                    budget: Duration::ZERO,
                    ..policy()
                },
                || true
            )
            .visited,
            0
        );
        let outcome = clean(
            &roots,
            SystemTime::now(),
            Policy {
                entries: 3,
                ..policy()
            },
            || true,
        );
        assert!(outcome.visited <= 3 && outcome.removed <= 3);
        assert!(fs::read_dir(&roots[0]).unwrap().count() >= 17);
    }

    #[test]
    fn changed_file_and_non_absolute_or_parent_roots_are_not_deleted() {
        let f = Fixture::new();
        let path = f.file("Cache/entry", 1, 1000);
        let m = fs::metadata(&path).unwrap();
        let candidate = Candidate {
            root: f.0.join("Cache"),
            path: path.clone(),
            size: m.len(),
            modified: m.modified().unwrap(),
        };
        fs::write(&path, b"replacement").unwrap();
        assert!(!remove_unchanged(&candidate));
        for root in [PathBuf::from("Cache"), f.0.join("Cache/../Cache")] {
            assert_eq!(
                clean(&[root], SystemTime::now(), policy(), || true).removed,
                0
            );
        }
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_ancestor_nested_directory_and_file_never_escape_cache() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = Fixture::new();
        let kept = outside.file("keep", 20, 1000);
        fs::create_dir_all(f.0.join("Cache")).unwrap();
        symlink(&outside.0, f.0.join("Cache/linked-directory")).unwrap();
        symlink(&kept, f.0.join("Cache/linked-file")).unwrap();
        symlink(&outside.0, f.0.join("linked-root")).unwrap();
        assert_eq!(
            clean(
                &[
                    f.0.join("Cache"),
                    f.0.join("linked-root"),
                    f.0.join("linked-root/nested")
                ],
                SystemTime::now(),
                policy(),
                || true
            )
            .removed,
            0
        );
        assert!(kept.exists());
        assert!(f.0.join("Cache/linked-file").is_symlink());
        let path = f.file("swap/entry", 1, 1000);
        let m = fs::metadata(&path).unwrap();
        let candidate = Candidate {
            root: f.0.join("swap"),
            path,
            size: m.len(),
            modified: m.modified().unwrap(),
        };
        fs::rename(f.0.join("swap"), f.0.join("old-swap")).unwrap();
        symlink(&outside.0, f.0.join("swap")).unwrap();
        assert!(!remove_unchanged(&candidate));
        assert!(kept.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_and_locked_cache_file_are_preserved() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        let outside = Fixture::new();
        let kept = outside.file("keep", 20, 1000);
        let junction = f.0.join("Cache");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside.0)
            .output()
            .unwrap();
        assert!(output.status.success(), "Could not create junction fixture");
        assert_eq!(
            clean(&[junction.clone()], SystemTime::now(), policy(), || true).removed,
            0
        );
        assert!(kept.exists());
        fs::remove_dir(junction).unwrap();
        let locked = f.file("Cache/locked", 20, 1000);
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        assert_eq!(
            clean(&[f.0.join("Cache")], SystemTime::now(), policy(), || true).removed,
            0
        );
        drop(lock);
        assert_eq!(
            clean(&[f.0.join("Cache")], SystemTime::now(), policy(), || true).removed,
            1
        );
    }
}
