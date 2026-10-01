//! Return post-install launches to the interactive desktop user's shell.

use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    core::{Interface, BSTR},
    Win32::{
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, IServiceProvider,
                CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
            },
            Variant::VARIANT,
        },
        UI::Shell::{
            IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
            SID_STopLevelBrowser, ShellWindows, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
        },
    },
};

pub(crate) fn relaunch() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    // The helper accepts no external executable or command. It can only reopen
    // this installed binary, before taking the app/profile's instance lock.
    launch(&executable, "--update-restart").map_err(|e| e.to_string())
}

fn launch(executable: &Path, arguments: &str) -> windows::core::Result<()> {
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balanced with successful initialization on this thread.
            unsafe { CoUninitialize() };
        }
    }
    // Use Explorer's desktop automation object, as in Microsoft's Execute in
    // Explorer sample. Creating a fresh Shell.Application object here could
    // instead leave the new app running with the installer's elevated token.
    // SAFETY: interfaces remain on this STA thread and release before COM ends;
    // strings and VARIANT values remain alive through each synchronous call.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = Apartment;
        let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
        let location = VARIANT::default();
        let mut desktop = 0;
        let dispatch = shell.FindWindowSW(
            &location,
            &location,
            SWC_DESKTOP,
            &mut desktop,
            SWFO_NEEDDISPATCH,
        )?;
        let services: IServiceProvider = dispatch.cast()?;
        let browser: IShellBrowser = services.QueryService(&SID_STopLevelBrowser)?;
        let background: IDispatch = browser
            .QueryActiveShellView()?
            .GetItemObject(SVGIO_BACKGROUND)?;
        let folder: IShellFolderViewDual = background.cast()?;
        let application: IShellDispatch2 = folder.Application()?.cast()?;
        application.ShellExecute(
            &BSTR::from_wide(&executable.as_os_str().encode_wide().collect::<Vec<_>>()),
            &VARIANT::from(arguments),
            &VARIANT::default(),
            &VARIANT::from("open"),
            &VARIANT::from(1_i32),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Explorer and an explicit native token-elevation probe executable"]
    fn explorer_launch_does_not_inherit_elevation() {
        let fixture = std::path::PathBuf::from(
            std::env::var_os("BRICK_LAUNCH_ELEVATION_FIXTURE")
                .expect("Explicit probe executable required"),
        );
        let receipt = std::path::PathBuf::from(
            std::env::var_os("BRICK_LAUNCH_ELEVATION_RECEIPT")
                .expect("Explicit owned receipt path required"),
        );
        assert!(fixture.is_absolute() && fixture.is_file());
        assert!(receipt.is_absolute() && !receipt.exists());
        let argument = format!("\"{}\"", receipt.to_str().unwrap());
        launch(&fixture, &argument).unwrap();
        let start = std::time::Instant::now();
        loop {
            if let Ok(bytes) = std::fs::read(&receipt) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    assert_eq!(
                        value["elevated"], false,
                        "The desktop launch must leave installer privileges behind"
                    );
                    break;
                }
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "The native launch did not return its elevation receipt"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::remove_file(receipt).unwrap();
    }
}
