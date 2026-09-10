use std::{
    env,
    ffi::OsString,
    fs,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use windows::{
    core::{w, PCWSTR},
    Win32::{
        System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        },
        UI::Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW},
    },
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY},
};

fn machine_install_dir() -> Result<Option<PathBuf>, String> {
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Brick\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "InstallLocation\0".encode_utf16().collect();
    let mut buffer = vec![0u16; 32768];
    let mut bytes = (buffer.len() * 2) as u32;
    // Read the same 64-bit machine registration written by the NSIS installer.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    match status {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
        ERROR_SUCCESS => {
            buffer.truncate(buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len()));
            if buffer.first() == Some(&34) && buffer.last() == Some(&34) && buffer.len() > 1 {
                buffer = buffer[1..buffer.len() - 1].to_vec();
            }
            Ok((!buffer.is_empty()).then(|| PathBuf::from(OsString::from_wide(&buffer))))
        }
        _ => Err(format!(
            "Could not read Brick's installation location (Windows error {status})."
        )),
    }
}

fn is_machine_install(directory: &Path, registered: Option<&Path>) -> bool {
    registered
        .and_then(|path| fs::canonicalize(path).ok())
        .zip(fs::canonicalize(directory).ok())
        .is_some_and(|(machine, current)| {
            machine
                .as_os_str()
                .eq_ignore_ascii_case(current.as_os_str())
        })
}

fn installer_arguments(directory: &Path, all_users: bool) -> Vec<u16> {
    let scope = if all_users { "AllUsers" } else { "CurrentUser" };
    // NSIS requires /D= to be last and unquoted, including paths with spaces.
    format!("/S /R /NS /{scope} /D=")
        .encode_utf16()
        .chain(directory.as_os_str().encode_wide())
        .chain([0])
        .collect()
}

pub(super) fn launch(installer: &Path) -> Result<(), String> {
    let executable =
        env::current_exe().map_err(|error| format!("Could not locate Brick: {error}"))?;
    let directory = executable
        .parent()
        .ok_or("Could not locate Brick's installation folder.")?;
    let registered = machine_install_dir()?;
    let all_users = is_machine_install(directory, registered.as_deref());
    let file: Vec<u16> = installer.as_os_str().encode_wide().chain([0]).collect();
    let arguments = installer_arguments(directory, all_users);
    // ShellExecute handles an installer's UAC manifest. Command::spawn instead
    // fails with error 740 when Windows requires elevation. Only machine updates
    // explicitly request runas; cancelling UAC returns an error and keeps Brick open.
    let initialized =
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok() };
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: if all_users { w!("runas") } else { w!("open") },
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(arguments.as_ptr()),
        nShow: 1,
        ..Default::default()
    };
    // All strings remain alive until ShellExecute completes its launch request.
    let result = unsafe { ShellExecuteExW(&mut info) };
    if initialized {
        unsafe {
            CoUninitialize();
        }
    }
    result.map_err(|error| {
        if error.code().0 as u32 == 0x800704c7 {
            "Update cancelled. Brick is still running.".to_string()
        } else {
            format!("Failed to start Brick installer: {error}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_keep_their_scope_and_install_directory() {
        for (directory, machine, expected) in [
            (
                r"C:\Program Files\Brick",
                true,
                r"/S /R /NS /AllUsers /D=C:\Program Files\Brick",
            ),
            (
                r"C:\Users\Test User\AppData\Local\Brick",
                false,
                r"/S /R /NS /CurrentUser /D=C:\Users\Test User\AppData\Local\Brick",
            ),
        ] {
            let args = installer_arguments(Path::new(directory), machine);
            assert_eq!(args.last(), Some(&0));
            assert_eq!(
                String::from_utf16(&args[..args.len() - 1]).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn a_machine_registration_only_applies_to_its_own_directory() {
        let root = env::temp_dir().join(format!("brick-install-scope-{}", uuid::Uuid::new_v4()));
        let machine = root.join("machine");
        let user = root.join("user");
        fs::create_dir_all(&machine).unwrap();
        fs::create_dir_all(&user).unwrap();
        assert!(is_machine_install(&machine, Some(&machine)));
        assert!(!is_machine_install(&user, Some(&machine)));
        assert!(!is_machine_install(&user, None));
        assert!(!is_machine_install(&user, Some(&root.join("missing"))));
        fs::remove_dir_all(root).unwrap();
    }
}
