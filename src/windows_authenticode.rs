use sha2::{Digest, Sha256};
use std::{
    fs::File,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::Security::WinTrust::*;

pub(super) fn verify(path: &Path, file: &File) -> Result<(), String> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path.as_ptr(),
        hFile: file.as_raw_handle(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT | WTD_DISABLE_MD2_MD4,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // The locked file and UTF-16 path remain alive for verification and state cleanup.
    let status = unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    let result = if status != 0 {
        Err(format!(
            "Brick installer signature could not be verified (Windows error {status:#x})."
        ))
    } else {
        // Windows has verified the file digest, signing chain, timestamp and
        // revocation status. Independently enforce Brick's publisher roster.
        unsafe { verify_publisher(data.hWVTStateData) }
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    result
}

unsafe fn verify_publisher(state: windows_sys::Win32::Foundation::HANDLE) -> Result<(), String> {
    let provider = WTHelperProvDataFromStateData(state);
    if provider.is_null() {
        return Err("Brick installer has no verified publisher.".into());
    }
    let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
    if signer.is_null() {
        return Err("Brick installer has no verified signer.".into());
    }
    let certificate = WTHelperGetProvCertFromChain(signer, 0);
    if certificate.is_null() || (*certificate).pCert.is_null() {
        return Err("Brick installer has no verified certificate.".into());
    }
    let cert = &*(*certificate).pCert;
    if cert.pbCertEncoded.is_null() || cert.cbCertEncoded == 0 || cert.cbCertEncoded > 65536 {
        return Err("Brick installer publisher certificate is invalid.".into());
    }
    let digest = hex::encode(Sha256::digest(std::slice::from_raw_parts(
        cert.pbCertEncoded,
        cert.cbCertEncoded as usize,
    )));
    let publishers: Vec<String> =
        serde_json::from_str(include_str!("../security/windows-publishers.json"))
            .map_err(|_| "Brick publisher policy is invalid.".to_string())?;
    if !publishers.iter().any(|allowed| allowed == &digest) {
        return Err("The installer was not signed by an approved Brick publisher.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_installer_is_rejected() {
        let path =
            std::env::temp_dir().join(format!("brick-unsigned-{}.exe", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"MZ unsigned fixture").unwrap();
        let file = File::open(&path).unwrap();
        assert!(verify(&path, &file).is_err());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "requires the checksum-verified public Windows installer fixture"]
    fn authenticode_accepts_brick_and_rejects_tampering_and_other_publishers() {
        let path = std::path::PathBuf::from(
            std::env::var_os("BRICK_SIGNED_INSTALLER_FIXTURE").expect("signed fixture"),
        );
        assert!(verify(&path, &File::open(&path).unwrap()).is_ok());
        let tampered =
            std::env::temp_dir().join(format!("brick-tampered-{}.exe", uuid::Uuid::new_v4()));
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[1024] ^= 1;
        std::fs::write(&tampered, bytes).unwrap();
        assert!(verify(&tampered, &File::open(&tampered).unwrap()).is_err());
        std::fs::remove_file(tampered).unwrap();
        let other = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/cmd.exe");
        assert!(verify(&other, &File::open(&other).unwrap()).is_err());
    }
}
