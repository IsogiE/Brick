//! OS-protected credentials, isolated by provider and signed-in Brick account.
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::PathBuf};

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(account: &str) -> Result<Self, String> {
        let id = hex::encode(Sha256::digest(account.as_bytes()));
        Ok(Self {
            path: crate::addon::config_dir()?.join(format!("warcraftlogs-{id}.dat")),
        })
    }

    pub fn load(&self) -> Result<Option<Vec<u8>>, String> {
        let mut bytes = Vec::new();
        match fs::File::open(&self.path) {
            Ok(file) => {
                file.take(128 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "Couldn't read the saved Warcraft Logs login.")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("Couldn't read the saved Warcraft Logs login.".into()),
        }
        if bytes.len() > 128 * 1024 {
            return Err("Saved Warcraft Logs login is invalid.".into());
        }
        #[cfg(target_os = "linux")]
        if bytes == b"BRICK-WCL-KEYRING-v1\n" {
            return self
                .entry()?
                .get_secret()
                .map(Some)
                .map_err(|_| "Unlock your desktop keyring to use Warcraft Logs.".into());
        }
        #[cfg(target_os = "windows")]
        if let Some(encrypted) = bytes.strip_prefix(b"BRICK-WCL-DPAPI-v1\n") {
            return crypt(encrypted, false).map(Some);
        }
        Err("Saved Warcraft Logs login is not protected. Please sign in again.".into())
    }

    pub fn save(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > 64 * 1024 {
            return Err("Invalid Warcraft Logs credentials.".into());
        }
        #[cfg(target_os = "linux")]
        let payload = {
            self.entry()?
                .set_secret(bytes)
                .map_err(|_| "Unlock your desktop keyring to save your Warcraft Logs login.")?;
            b"BRICK-WCL-KEYRING-v1\n".to_vec()
        };
        #[cfg(target_os = "windows")]
        let payload = {
            let mut payload = b"BRICK-WCL-DPAPI-v1\n".to_vec();
            payload.extend(crypt(bytes, true)?);
            payload
        };
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let payload: Vec<u8> = {
            let _ = bytes;
            return Err("Protected credential storage is unavailable.".into());
        };
        crate::atomic_file::write(&self.path, &payload)
            .map_err(|_| "Couldn't save the protected Warcraft Logs login.".into())
    }

    pub fn remove(&self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => (),
            Err(_) => {
                return Err(
                    "Unlock your desktop keyring to remove your Warcraft Logs login.".into(),
                )
            }
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Couldn't remove the saved Warcraft Logs login.".into()),
        }
    }

    #[cfg(target_os = "linux")]
    fn entry(&self) -> Result<keyring::Entry, String> {
        let id = hex::encode(Sha256::digest(self.path.as_os_str().as_encoded_bytes()));
        keyring::Entry::new("dev.isogi.brick.warcraftlogs", &id)
            .map_err(|_| "Warcraft Logs credential storage is unavailable.".into())
    }
}

#[cfg(target_os = "windows")]
fn crypt(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    if bytes.is_empty() || bytes.len() > 128 * 1024 {
        return Err("Invalid Warcraft Logs credentials.".into());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err("Windows couldn't access the protected Warcraft Logs login.".into());
    }
    let result = if output.pbData.is_null() {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        unsafe {
            LocalFree(output.pbData.cast());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> Store {
        Store {
            path: std::env::temp_dir().join(format!("brick-wcl-store-{}", uuid::Uuid::new_v4())),
        }
    }
    #[test]
    fn rejects_plaintext_and_oversized_credentials() {
        let store = store();
        assert!(store.load().unwrap().is_none());
        fs::write(&store.path, b"fixture-secret").unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&[]).is_err());
        assert!(store.save(&vec![b'x'; 64 * 1024 + 1]).is_err());
        assert_eq!(fs::read(&store.path).unwrap(), b"fixture-secret");
        fs::write(&store.path, vec![b'x'; 128 * 1024 + 1]).unwrap();
        assert!(store.load().is_err());
        fs::remove_file(store.path).unwrap();
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_dpapi_roundtrip_and_tamper_rejection() {
        let store = store();
        let secret = b"fixture-access-and-refresh-credentials";
        store.save(secret).unwrap();
        let mut payload = fs::read(&store.path).unwrap();
        assert!(!payload.windows(secret.len()).any(|part| part == secret));
        assert_eq!(store.load().unwrap().unwrap(), secret);
        *payload.last_mut().unwrap() ^= 1;
        fs::write(&store.path, payload).unwrap();
        assert!(store.load().is_err());
        store.remove().unwrap();
        assert!(store.load().unwrap().is_none());
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated, unlocked Secret Service test session"]
    fn linux_keyring_roundtrip_keeps_wcl_credentials_out_of_profile() {
        let other = store();
        let store = store();
        store.save(b"fixture-access-and-refresh").unwrap();
        assert_eq!(fs::read(&store.path).unwrap(), b"BRICK-WCL-KEYRING-v1\n");
        assert_eq!(
            store.load().unwrap().unwrap(),
            b"fixture-access-and-refresh"
        );
        assert!(other.load().unwrap().is_none());
        assert!(matches!(
            other.entry().unwrap().get_secret(),
            Err(keyring::Error::NoEntry)
        ));
        store.remove().unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(matches!(
            store.entry().unwrap().get_secret(),
            Err(keyring::Error::NoEntry)
        ));
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated process with no Secret Service available"]
    fn linux_keyring_failure_never_saves_wcl_plaintext() {
        let store = store();
        assert!(store.save(b"fixture-access-and-refresh").is_err());
        assert!(!store.path.exists());
    }
}
