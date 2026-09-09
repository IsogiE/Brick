#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::process::Command;

pub fn open(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .map_err(|error| format!("Failed to open your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|error| format!("Failed to open your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        desktop::open(url)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod desktop {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        path::Path,
        process::{Command, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc, OnceLock,
        },
        thread,
        time::{Duration, Instant},
    };

    const HANDOFF: Duration = Duration::from_millis(50);
    const MAX_LAUNCHES: usize = 4;
    const FAILED: &str = "Failed to open your browser.";
    static ACTIVE: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    type Environment = BTreeMap<OsString, OsString>;

    struct Permit(Arc<AtomicUsize>);
    impl Permit {
        fn acquire(active: Arc<AtomicUsize>) -> Result<Self, String> {
            active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < MAX_LAUNCHES).then_some(count + 1)
                })
                .map(|_| Self(active))
                .map_err(|_| "Your browser is still opening. Please try again shortly.".into())
        }
    }
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(super) fn open(url: &str) -> Result<(), String> {
        open_with(
            url.to_owned(),
            host_environment(std::env::vars_os().collect()),
            ACTIVE.get_or_init(|| Arc::new(AtomicUsize::new(0))).clone(),
            HANDOFF,
        )
    }

    fn open_with(
        url: String,
        environment: Environment,
        active: Arc<AtomicUsize>,
        handoff: Duration,
    ) -> Result<(), String> {
        // Reserve capacity before creating either a worker or a child process.
        let permit = Permit::acquire(active)?;
        let (send, receive) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("browser-launch".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _permit = permit;
                let deadline = Instant::now() + handoff;
                let result = launch(&url, &environment, deadline);
                let _ = send.send(result);
            })
            .map_err(|_| FAILED.to_owned())?;
        match receive.recv_timeout(handoff) {
            Ok(result) => result,
            // Some desktop launchers exec the browser and live as long as its
            // window. Accept its handoff without blocking egui/OAuth; the one
            // worker waits/reaps it without polling, then releases capacity.
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(FAILED.into()),
        }
    }

    fn launch(url: &str, environment: &Environment, deadline: Instant) -> Result<(), String> {
        for launcher in ["xdg-open", "gio", "kde-open", "gnome-open"] {
            let mut command = Command::new(launcher);
            command.env_clear().envs(environment).stdin(Stdio::null());
            // OAuth URLs contain one-time state/codes. A broken launcher must
            // not print its arguments or inherited environment into app logs.
            command.stdout(Stdio::null()).stderr(Stdio::null());
            if launcher == "gio" {
                command.arg("open");
            }
            let Ok(mut child) = command.arg(url).spawn() else {
                continue;
            };
            if child.wait().is_ok_and(|status| status.success()) {
                return Ok(());
            }
            // Do not open a second browser after a long-lived accepted browser
            // eventually closes with an error. Immediate failures still fall back.
            if Instant::now() >= deadline {
                return Err(FAILED.into());
            }
        }
        Err(FAILED.into())
    }

    fn host_environment(mut environment: Environment) -> Environment {
        let Some(appdir) = environment.get(std::ffi::OsStr::new("APPDIR")) else {
            return environment;
        };
        let appdir = Path::new(appdir).to_path_buf();
        if !appdir.is_absolute() {
            return environment;
        }
        // AppRun prefixes these variables with its bundled libraries, helpers,
        // schemas and plugins. Keep every host entry, including custom xdg-open.
        for key in [
            "PATH",
            "LD_LIBRARY_PATH",
            "PYTHONPATH",
            "PERLLIB",
            "XDG_DATA_DIRS",
            "GTK_PATH",
            "GIO_EXTRA_MODULES",
            "QT_PLUGIN_PATH",
            "QML_IMPORT_PATH",
            "QML2_IMPORT_PATH",
            "GSETTINGS_SCHEMA_DIR",
            "GST_PLUGIN_PATH",
            "GST_PLUGIN_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SYSTEM_PATH_1_0",
        ] {
            let Some(value) = environment.get(std::ffi::OsStr::new(key)) else {
                continue;
            };
            let paths: Vec<_> = std::env::split_paths(value).collect();
            let retained: Vec<_> = paths
                .iter()
                .filter(|path| !path.starts_with(&appdir))
                .collect();
            if retained.len() == paths.len() {
                continue;
            }
            if retained.is_empty() {
                environment.remove(std::ffi::OsStr::new(key));
            } else if let Ok(value) = std::env::join_paths(retained) {
                environment.insert(key.into(), value);
            }
        }
        for key in [
            "PYTHONHOME",
            "GTK_DATA_PREFIX",
            "GTK_EXE_PREFIX",
            "GTK_IM_MODULE_FILE",
            "GDK_PIXBUF_MODULE_FILE",
            "GST_PLUGIN_SCANNER",
            "GST_PLUGIN_SCANNER_1_0",
            "GST_PTP_HELPER",
            "GST_PTP_HELPER_1_0",
        ] {
            if environment
                .get(std::ffi::OsStr::new(key))
                .is_some_and(|value| Path::new(value).starts_with(&appdir))
            {
                environment.remove(std::ffi::OsStr::new(key));
            }
        }
        let brick_registry = environment
            .get(std::ffi::OsStr::new("GST_REGISTRY_1_0"))
            .is_some_and(|value| {
                Path::new(value).ends_with("dev.isogi.brick/gstreamer-registry.bin")
            });
        if brick_registry {
            environment.remove(std::ffi::OsStr::new("GST_REGISTRY_1_0"));
            environment.remove(std::ffi::OsStr::new("GST_REGISTRY_REUSE_PLUGIN_SCANNER"));
        }
        // The bundled GTK hook sets these even when the desktop is Wayland.
        if environment
            .get(std::ffi::OsStr::new("GDK_BACKEND"))
            .is_some_and(|value| value == "x11")
        {
            environment.remove(std::ffi::OsStr::new("GDK_BACKEND"));
        }
        if environment
            .get(std::ffi::OsStr::new("GTK_THEME"))
            .is_some_and(|value| value.to_string_lossy().starts_with("Adwaita:"))
        {
            environment.remove(std::ffi::OsStr::new("GTK_THEME"));
        }
        for key in ["APPDIR", "APPIMAGE", "APPIMAGE_GTK_THEME", "ARGV0"] {
            environment.remove(std::ffi::OsStr::new(key));
        }
        environment
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{fs, os::unix::fs::PermissionsExt};

        fn environment(values: &[(&str, &str)]) -> Environment {
            values
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect()
        }

        #[test]
        fn appimage_launch_preserves_host_desktop_and_removes_only_bundled_paths() {
            let original = environment(&[
                ("APPDIR", "/tmp/Brick.AppDir"),
                ("APPIMAGE", "/apps/Brick.AppImage"),
                ("PATH", "/tmp/Brick.AppDir/usr/bin:/custom/bin:/usr/bin"),
                ("LD_LIBRARY_PATH", "/tmp/Brick.AppDir/usr/lib:/host/lib"),
                ("PYTHONHOME", "/tmp/Brick.AppDir/usr/"),
                (
                    "PYTHONPATH",
                    "/tmp/Brick.AppDir/usr/share/pyshared/:/host/python",
                ),
                (
                    "PERLLIB",
                    "/tmp/Brick.AppDir/usr/share/perl5/:/tmp/Brick.AppDir/usr/lib/perl5/:/host/perl",
                ),
                (
                    "XDG_DATA_DIRS",
                    "/tmp/Brick.AppDir/usr/share:/usr/share:/custom/share",
                ),
                ("GIO_EXTRA_MODULES", "/tmp/Brick.AppDir/usr/lib/gio/modules"),
                ("GTK_IM_MODULE_FILE", "/host/immodules.cache"),
                (
                    "GSETTINGS_SCHEMA_DIR",
                    "/tmp/Brick.AppDir//usr/share/schemas:/tmp/Brick.AppDir/usr/share/other",
                ),
                ("DISPLAY", ":0"),
                ("WAYLAND_DISPLAY", "wayland-0"),
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ("XDG_CURRENT_DESKTOP", "KDE"),
                ("XAUTHORITY", "/host/Xauthority"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ]);
            let clean = host_environment(original.clone());
            for key in [
                "DISPLAY",
                "WAYLAND_DISPLAY",
                "DBUS_SESSION_BUS_ADDRESS",
                "XDG_CURRENT_DESKTOP",
                "XAUTHORITY",
                "XDG_RUNTIME_DIR",
                "GTK_IM_MODULE_FILE",
            ] {
                assert_eq!(
                    clean.get(std::ffi::OsStr::new(key)),
                    original.get(std::ffi::OsStr::new(key))
                );
            }
            assert_eq!(
                clean.get(std::ffi::OsStr::new("PATH")).unwrap(),
                "/custom/bin:/usr/bin"
            );
            assert_eq!(
                clean.get(std::ffi::OsStr::new("LD_LIBRARY_PATH")).unwrap(),
                "/host/lib"
            );
            assert_eq!(
                clean.get(std::ffi::OsStr::new("PYTHONPATH")).unwrap(),
                "/host/python"
            );
            assert_eq!(
                clean.get(std::ffi::OsStr::new("PERLLIB")).unwrap(),
                "/host/perl"
            );
            assert_eq!(
                clean.get(std::ffi::OsStr::new("XDG_DATA_DIRS")).unwrap(),
                "/usr/share:/custom/share"
            );
            for key in [
                "APPDIR",
                "APPIMAGE",
                "PYTHONHOME",
                "GIO_EXTRA_MODULES",
                "GSETTINGS_SCHEMA_DIR",
            ] {
                assert!(!clean.contains_key(std::ffi::OsStr::new(key)));
            }
            let ordinary = environment(&[
                ("PATH", "/custom/bin"),
                ("LD_LIBRARY_PATH", "/host/lib"),
                ("PYTHONHOME", "/host/python-home"),
                ("PYTHONPATH", "/host/python"),
                ("PERLLIB", "/host/perl"),
                ("GTK_THEME", "Custom"),
            ]);
            assert_eq!(host_environment(ordinary.clone()), ordinary);
            let mut bundled = ordinary.clone();
            bundled.insert("APPDIR".into(), "/tmp/Brick.AppDir".into());
            assert_eq!(host_environment(bundled), ordinary);
        }

        struct Scripts(std::path::PathBuf);
        impl Scripts {
            fn new() -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "brick-browser-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&path).unwrap();
                Self(path)
            }
            fn write(&self, name: &str, body: &str) {
                let path = self.0.join(name);
                fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            fn environment(&self) -> Environment {
                environment(&[("PATH", self.0.to_str().unwrap())])
            }
        }
        impl Drop for Scripts {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn failed_launcher_falls_back_and_url_remains_one_literal_argument() {
            let scripts = Scripts::new();
            scripts.write("xdg-open", "exit 17");
            scripts.write("gio", r#"test "$#" = 2 && test "$1" = open && test "$2" = 'https://example.test/?state=a&code=$(false)'"#);
            assert!(launch(
                "https://example.test/?state=a&code=$(false)",
                &scripts.environment(),
                Instant::now() + Duration::from_secs(5)
            )
            .is_ok());
            scripts.write("gio", "exit 19");
            assert!(launch(
                "https://example.test",
                &scripts.environment(),
                Instant::now() + Duration::from_secs(5)
            )
            .is_err());
        }

        #[test]
        fn live_launcher_does_not_block_handoff_and_capacity_is_reserved_before_spawn() {
            let scripts = Scripts::new();
            scripts.write(
                "xdg-open",
                r#": > "${0%/*}/started"
while [ ! -f "${0%/*}/release" ]; do /bin/sleep 0.01; done"#,
            );
            let active = Arc::new(AtomicUsize::new(0));

            struct Release {
                path: std::path::PathBuf,
                active: Arc<AtomicUsize>,
            }
            impl Drop for Release {
                fn drop(&mut self) {
                    // Release even if an assertion unwinds, then give the worker
                    // a bounded window to reap its child before deleting scripts.
                    let _ = fs::write(&self.path, b"release");
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while self.active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            let release = Release {
                path: scripts.0.join("release"),
                active: active.clone(),
            };
            assert!(open_with(
                "https://example.test".into(),
                scripts.environment(),
                active.clone(),
                Duration::ZERO
            )
            .is_ok());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !scripts.0.join("started").is_file() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(scripts.0.join("started").is_file());
            assert_eq!(active.load(Ordering::Acquire), 1);
            drop(release);
            assert_eq!(
                active.load(Ordering::Acquire),
                0,
                "the accepted child must be reaped"
            );
            let permits: Vec<_> = (0..MAX_LAUNCHES)
                .map(|_| Permit::acquire(active.clone()).unwrap())
                .collect();
            assert!(open_with(
                "https://example.test".into(),
                scripts.environment(),
                active.clone(),
                HANDOFF
            )
            .is_err());
            assert_eq!(active.load(Ordering::Acquire), MAX_LAUNCHES);
            drop(permits);
            assert_eq!(active.load(Ordering::Acquire), 0);
        }
    }
}
