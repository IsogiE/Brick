//! WebView2 needs a writable user-data folder even when its profile is InPrivate.
use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

fn directory_under(local: Option<&OsStr>) -> Result<PathBuf, String> {
    let local = local
        .map(Path::new)
        .filter(|path| {
            path.is_absolute()
                && !path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
        })
        .ok_or("The stream player could not locate your local app data folder.")?;
    Ok(local.join("dev.isogi.brick").join("WebView2"))
}

pub(crate) fn data_directory() -> Result<PathBuf, String> {
    directory_under(std::env::var_os("LOCALAPPDATA").as_deref())
}

pub(super) fn context() -> Result<wry::WebContext, String> {
    let path = data_directory()?;
    std::fs::create_dir_all(&path)
        .map_err(|_| "The stream player could not prepare its local data folder.")?;
    Ok(wry::WebContext::new(Some(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_data_is_per_user_and_independent_of_the_install_directory() {
        for local in [
            r"C:\Users\Raider\AppData\Local",
            r"D:\Profiles\Healing Officer\Local",
        ] {
            let path = directory_under(Some(OsStr::new(local))).unwrap();
            assert_eq!(
                path,
                Path::new(local).join("dev.isogi.brick").join("WebView2")
            );
            assert!(path.is_absolute());
        }
        assert_ne!(
            directory_under(Some(OsStr::new(r"C:\Users\One\AppData\Local"))).unwrap(),
            directory_under(Some(OsStr::new(r"C:\Users\Two\AppData\Local"))).unwrap(),
        );
    }

    #[test]
    fn missing_or_relative_app_data_never_falls_back_to_the_executable_directory() {
        for local in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("relative")),
            Some(OsStr::new(r"C:relative")),
            Some(OsStr::new(r"C:\Users\..\Shared")),
        ] {
            assert!(directory_under(local).is_err());
        }
    }

    #[test]
    #[ignore = "requires a loopback-configured build, an isolated LOCALAPPDATA and a read-only executable directory"]
    fn native_player_uses_private_user_data_from_a_read_only_install() {
        use crate::stream_player::{Preferences, StreamPlayer};
        use eframe::egui;
        use std::sync::{Arc, Mutex};
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2Environment7, ICoreWebView2_13,
        };
        use windows::core::{Interface, BOOL, PWSTR};
        use wry::WebViewExtWindows;

        assert_eq!(
            std::env::var("BRICK_WEBVIEW_PROFILE_FIXTURE").as_deref(),
            Ok("1")
        );
        let base = option_env!("BRICK_PRESENCE_API_URL").unwrap_or("");
        assert!(
            base.starts_with("http://127.0.0.1:"),
            "Use the isolated loopback fixture build"
        );
        let elevated = std::env::var("BRICK_WEBVIEW_PROFILE_ELEVATED").as_deref() == Ok("1");
        let executable = std::env::current_exe().unwrap();
        let probe = executable
            .parent()
            .unwrap()
            .join(format!("brick-write-probe-{}", uuid::Uuid::new_v4()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(file) => {
                drop(file);
                std::fs::remove_file(&probe).unwrap();
                assert!(
                    elevated,
                    "Run this test without write access to its installation directory"
                );
            }
            Err(error) => {
                assert!(
                    !elevated,
                    "Elevated fixture should have install-directory access"
                );
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            }
        }

        struct App {
            url: String,
            player: Option<StreamPlayer>,
            outcome: Arc<Mutex<Option<Result<(), String>>>>,
        }
        impl eframe::App for App {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                if self.outcome.lock().unwrap().is_some() {
                    return;
                }
                let result = (|| -> Result<(), String> {
                    let player = StreamPlayer::new(
                        frame,
                        ui.ctx(),
                        &self.url,
                        "isolated-profile-fixture",
                        egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(600.0, 360.0)),
                        ui.ctx().pixels_per_point(),
                        Some(Preferences::in_memory()),
                    )?;
                    let view = player.webview.as_ref().unwrap();
                    let env: ICoreWebView2Environment7 = view
                        .environment()
                        .cast()
                        .map_err(|e| format!("Environment: {e}"))?;
                    let mut folder = PWSTR::null();
                    unsafe {
                        env.UserDataFolder(&mut folder)
                            .map_err(|e| format!("Data folder: {e}"))?;
                    }
                    let actual = unsafe { folder.to_string() }.map(PathBuf::from);
                    unsafe {
                        windows::Win32::System::Com::CoTaskMemFree(Some(folder.0.cast()));
                    }
                    let actual = actual.map_err(|e| format!("Data folder text: {e}"))?;
                    let expected = data_directory()?;
                    assert_eq!(
                        std::fs::canonicalize(&actual).unwrap(),
                        std::fs::canonicalize(&expected).unwrap()
                    );
                    let profile = unsafe {
                        view.webview()
                            .cast::<ICoreWebView2_13>()
                            .map_err(|e| e.to_string())?
                            .Profile()
                            .map_err(|e| e.to_string())?
                    };
                    let mut private = BOOL::default();
                    unsafe {
                        profile
                            .IsInPrivateModeEnabled(&mut private)
                            .map_err(|e| e.to_string())?;
                    }
                    assert!(
                        private.as_bool(),
                        "Moving browser data must preserve InPrivate mode"
                    );
                    eprintln!("WebView2 created; install-directory permissions checked; per-user data folder verified; InPrivate=true");
                    self.player = Some(player);
                    Ok(())
                })();
                *self.outcome.lock().unwrap() = Some(result);
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        let outcome = Arc::new(Mutex::new(None));
        let saved = outcome.clone();
        eframe::run_native(
            "Brick WebView2 permissions check",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 420.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    use winit::platform::windows::EventLoopBuilderExtWindows;
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            },
            Box::new(move |_| {
                Ok(Box::new(App {
                    url: format!("{base}/v1/streams/player/1/twitch"),
                    player: None,
                    outcome,
                }))
            }),
        )
        .unwrap();
        saved
            .lock()
            .unwrap()
            .take()
            .expect("The native fixture did not run")
            .unwrap();
    }
}
