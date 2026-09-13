#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::process::Command;

/// Use the same desktop launcher for egui hyperlinks and embedded provider
/// links as OAuth. Eframe's default launcher inherits the AppImage environment.
pub fn open_pending_urls(ctx: &eframe::egui::Context) -> Result<(), String> {
    dispatch_pending_urls(ctx, open)
}

fn dispatch_pending_urls(
    ctx: &eframe::egui::Context,
    mut launch: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut urls = Vec::new();
    ctx.output_mut(|output| {
        output.commands.retain(|command| {
            if let eframe::egui::OutputCommand::OpenUrl(request) = command {
                urls.push(request.url.clone());
                false
            } else {
                true
            }
        });
    });
    // Launch outside egui's output lock, and consume each request exactly once.
    let mut result = Ok(());
    for url in urls {
        if let Err(error) = launch(&url) {
            result = Err(error);
        }
    }
    result
}

#[cfg(test)]
mod output_tests {
    use super::*;
    use eframe::egui::{Context, OpenUrl, OutputCommand, RawInput};

    #[test]
    fn hyperlinks_and_provider_links_share_the_launcher_without_consuming_clipboard_output() {
        let ctx = Context::default();
        let mut opened = Vec::new();
        let output = ctx.run_ui(RawInput::default(), |ui| {
            ui.ctx().copy_text("copied text".into());
            ui.ctx().open_url(OpenUrl::new_tab(
                "https://www.warcraftlogs.com/reports/example",
            ));
            ui.ctx().open_url(OpenUrl::new_tab(
                "https://www.youtube.com/watch?v=abcDEF_12-3&t=30",
            ));
            let mut launch = |url: &str| {
                opened.push(url.to_owned());
                Ok(())
            };
            dispatch_pending_urls(ui.ctx(), &mut launch).unwrap();
            dispatch_pending_urls(ui.ctx(), &mut launch).unwrap();
        });
        assert_eq!(
            opened,
            [
                "https://www.warcraftlogs.com/reports/example",
                "https://www.youtube.com/watch?v=abcDEF_12-3&t=30",
            ]
        );
        assert!(
            matches!(&output.platform_output.commands[..], [OutputCommand::CopyText(text)] if text == "copied text")
        );
    }

    #[test]
    fn failed_launch_is_reported_and_not_retried_by_eframe() {
        let ctx = Context::default();
        ctx.open_url(OpenUrl::new_tab("https://www.youtube.com/"));
        assert_eq!(
            dispatch_pending_urls(&ctx, |_| Err("Browser unavailable".into())),
            Err("Browser unavailable".into())
        );
        assert!(ctx.output(|output| output.commands.is_empty()));
    }
}

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
    use crate::appimage_environment::{host_environment, Environment};

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
