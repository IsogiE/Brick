//! Start the generated AppRun only after removing the previous bundle's paths.
//! An older updater may pass a bundled libreadline that breaks host Bash before
//! a shell hook or Brick's main function has a chance to clean the environment.

#[path = "../../src/appimage_environment.rs"]
mod appimage_environment;

use std::{env, fs, os::unix::process::CommandExt, path::Path, process::Command};

fn launch() -> Result<(), Box<dyn std::error::Error>> {
    let executable = fs::read_link("/proc/self/exe")?;
    let directory = executable.parent().ok_or("Missing AppImage directory")?;
    let mut environment: appimage_environment::Environment = env::vars_os().collect();
    let runtime: Vec<_> = ["APPIMAGE", "ARGV0", "APPIMAGE_GTK_THEME"]
        .into_iter()
        .filter_map(|key| {
            environment
                .get(std::ffi::OsStr::new(key))
                .cloned()
                .map(|value| (key.into(), value))
        })
        .collect();
    // The incoming AppImage runtime has already replaced APPDIR. The GTK hook
    // in older Brick packages also retained its bundle root in GTK_DATA_PREFIX.
    // Recognize that complete Brick bundle, not an ordinary host GTK prefix.
    if let Some(previous) = environment
        .get(std::ffi::OsStr::new("GTK_DATA_PREFIX"))
        .cloned()
    {
        let previous = Path::new(&previous);
        if previous.is_absolute()
            && previous != directory
            && previous.join("AppRun").is_file()
            && previous.join("usr/bin/brick").is_file()
        {
            environment.insert("APPDIR".into(), previous.as_os_str().to_owned());
            environment = appimage_environment::host_environment(environment);
        }
    }
    // Always source the new bundle's hooks, including when launched through a
    // symlink or from an extracted AppImage with a stale APPDIR.
    environment.insert("APPDIR".into(), directory.as_os_str().to_owned());
    environment = appimage_environment::host_environment(environment);
    environment.extend(runtime);
    environment.insert("APPDIR".into(), directory.as_os_str().to_owned());
    let error = Command::new(directory.join("AppRun.launcher"))
        .args(env::args_os().skip(1))
        .current_dir(directory)
        .env_clear()
        .envs(environment)
        .exec();
    Err(error.into())
}

fn main() {
    if launch().is_err() {
        eprintln!("Brick could not start.");
        std::process::exit(1);
    }
}
