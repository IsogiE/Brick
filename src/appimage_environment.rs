//! Drop the current bundle's paths before launching a browser or a replacement
//! AppImage. The old mount disappears when the running instance exits.

use std::{collections::BTreeMap, ffi::OsString, path::Path};

pub(crate) type Environment = BTreeMap<OsString, OsString>;

pub(crate) fn host_environment(mut environment: Environment) -> Environment {
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
        .is_some_and(|value| Path::new(value).ends_with("dev.isogi.brick/gstreamer-registry.bin"));
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
}
