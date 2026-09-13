//! Bounded encrypted cache files; only their small keys live in the OS vault.
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs,
    io::Read,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

const PREFIX: &[u8] = b"BRICK-GUILD-CACHE-v1\n";
const MAX_BYTES: usize = 2 * 1024 * 1024;
type CachedKey = (String, Arc<aead::LessSafeKey>);
static KEYS: LazyLock<Mutex<VecDeque<CachedKey>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

fn path(scope: &str) -> Result<PathBuf, String> {
    Ok(crate::addon::config_dir()?.join(format!(
        "guild-cache-{}.dat",
        hex::encode(Sha256::digest(scope))
    )))
}

fn key(scope: &str, create: bool) -> Result<Option<Arc<aead::LessSafeKey>>, String> {
    // Cache a bounded number of small keys and serialize first creation. The
    // vault remains authoritative; errors never install or replace a key.
    let mut keys = KEYS
        .lock()
        .map_err(|_| "Cache encryption is unavailable.")?;
    if let Some((_, key)) = keys.iter().find(|(saved, _)| saved == scope) {
        return Ok(Some(Arc::clone(key)));
    }
    let store = crate::credential_store::Store::new(&format!("guild-cache-key-v1:{scope}"))?;
    let bytes = match store.load()? {
        Some(bytes) => bytes,
        None if !create => return Ok(None),
        None => {
            let mut bytes = vec![0; 32];
            SystemRandom::new()
                .fill(&mut bytes)
                .map_err(|_| "Cache encryption is unavailable.")?;
            store.save(&bytes)?;
            bytes
        }
    };
    let mut bytes = bytes;
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &bytes);
    bytes.fill(0);
    let key = Arc::new(aead::LessSafeKey::new(
        key.map_err(|_| "Invalid cache encryption key.")?,
    ));
    if keys.len() >= 4 {
        keys.pop_front();
    }
    keys.push_back((scope.to_string(), Arc::clone(&key)));
    Ok(Some(key))
}

pub fn load(scope: &str) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    match fs::File::open(path(scope)?) {
        Ok(file) => {
            file.take((MAX_BYTES + 128) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| "Couldn't read the guild cache.")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Couldn't read the guild cache.".into()),
    }
    let key = key(scope, false)?.ok_or("The guild cache key is unavailable.")?;
    decrypt(&key, scope, bytes).map(Some)
}

pub fn save(scope: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_BYTES {
        return Err("Guild cache is too large.".into());
    }
    let key = key(scope, true)?.ok_or("The guild cache key is unavailable.")?;
    let payload = encrypt(&key, scope, bytes)?;
    crate::atomic_file::write(&path(scope)?, &payload)
        .map_err(|_| "Couldn't save the guild cache.".into())
}

fn encrypt(key: &aead::LessSafeKey, scope: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "Cache encryption is unavailable.")?;
    let mut body = bytes.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(scope),
        &mut body,
    )
    .map_err(|_| "Couldn't encrypt the guild cache.")?;
    let mut result = PREFIX.to_vec();
    result.extend(nonce);
    result.extend(body);
    Ok(result)
}

fn decrypt(key: &aead::LessSafeKey, scope: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let invalid = || "The encrypted guild cache is invalid.".to_string();
    if bytes.len() > MAX_BYTES + PREFIX.len() + 28 {
        return Err(invalid());
    }
    let data = bytes
        .strip_prefix(PREFIX)
        .filter(|data| data.len() >= 28)
        .ok_or_else(invalid)?;
    let mut body = data[12..].to_vec();
    let nonce = aead::Nonce::try_assume_unique_for_key(&data[..12]).map_err(|_| invalid())?;
    key.open_in_place(nonce, aead::Aad::from(scope), &mut body)
        .map(|bytes| bytes.to_vec())
        .map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encryption_binds_the_guild_account_and_rejects_tampering() {
        let key =
            aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM, &[7; 32]).unwrap());
        let bytes = encrypt(&key, "advance:123", b"private-report-and-video").unwrap();
        assert!(!bytes.windows(14).any(|bytes| bytes == b"private-report"));
        assert_eq!(
            decrypt(&key, "advance:123", bytes.clone()).unwrap(),
            b"private-report-and-video"
        );
        assert!(decrypt(&key, "ascendance:123", bytes.clone()).is_err());
        assert!(decrypt(&key, "advance:456", bytes.clone()).is_err());
        let mut damaged = bytes;
        *damaged.last_mut().unwrap() ^= 1;
        assert!(decrypt(&key, "advance:123", damaged).is_err());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod vault_tests {
    use super::*;
    #[test]
    #[ignore = "requires the isolated Secret Service used by CI"]
    fn linux_keyring_roundtrip_guild_cache_uses_independent_keys_and_rejects_swapped_files() {
        let first = format!("1166119057993515100:{}", uuid::Uuid::new_v4());
        let second = format!("481024965852921856:{}", uuid::Uuid::new_v4());
        save(&first, b"Advance private cache").unwrap();
        save(&second, b"Ascendance private cache").unwrap();
        assert_eq!(load(&first).unwrap().unwrap(), b"Advance private cache");
        assert_eq!(load(&second).unwrap().unwrap(), b"Ascendance private cache");
        let ciphertext = fs::read(path(&first).unwrap()).unwrap();
        fs::write(path(&second).unwrap(), &ciphertext).unwrap();
        assert!(load(&second).is_err());
        assert_eq!(fs::read(path(&second).unwrap()).unwrap(), ciphertext);
        for scope in [first, second] {
            fs::remove_file(path(&scope).unwrap()).unwrap();
            crate::credential_store::Store::new(&format!("guild-cache-key-v1:{scope}"))
                .unwrap()
                .remove()
                .unwrap();
        }
    }

    #[test]
    #[ignore = "requires an isolated process without a Secret Service"]
    fn linux_keyring_failure_guild_cache_never_writes_plaintext() {
        let scope = format!("1166119057993515100:{}", uuid::Uuid::new_v4());
        assert!(save(&scope, b"private cache").is_err());
        assert!(!path(&scope).unwrap().exists());
    }
}
