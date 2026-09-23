//! Completed WCL data, independent of the video/POV. Fixed encrypted slots bound
//! disk usage even after crashes; the small index is the only commit point.
use super::{check_cancelled, persistent::AGE, Client, Config, Pull, Session};
use crate::guild::Access;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const SLOTS: usize = 16;
const CHUNK: usize = 2 * 1024 * 1024 - 128;
// A maximum-sized 16 MiB signature plus its small cache revision envelope.
const MAX_ENTRY: usize = 16 * 1024 * 1024 + 128;
const MAX_INDEX: usize = CHUNK;
const INLINE: usize = 32 * 1024;
const MAX_ENTRIES: usize = 256;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    at: u64,
    bytes: usize,
    digest: String,
    slots: Vec<usize>,
    inline: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u8,
    identity: String,
    entries: BTreeMap<String, Entry>,
}
trait Storage {
    fn read(&self, slot: Option<usize>) -> Result<Option<Vec<u8>>, String>;
    fn write(&mut self, slot: Option<usize>, bytes: &[u8]) -> Result<(), String>;
    fn remove(&mut self, slot: usize) -> Result<(), String>;
}
struct Disk(String);
impl Disk {
    fn scope(&self, slot: Option<usize>) -> String {
        match slot {
            Some(slot) => format!("{}:slot:{slot}", self.0),
            None => format!("{}:index", self.0),
        }
    }
}
impl Storage for Disk {
    fn read(&self, slot: Option<usize>) -> Result<Option<Vec<u8>>, String> {
        crate::protected_cache::load(&self.scope(slot))
    }
    fn write(&mut self, slot: Option<usize>, bytes: &[u8]) -> Result<(), String> {
        crate::protected_cache::save(&self.scope(slot), bytes)
    }
    fn remove(&mut self, slot: usize) -> Result<(), String> {
        crate::protected_cache::remove(&self.scope(Some(slot)))
    }
}
struct Store<B> {
    backend: B,
    index: Index,
}
pub(super) struct Cache(Store<Disk>);
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn fresh(at: u64, now: u64) -> bool {
    at <= now && now - at < AGE.as_secs()
}
fn valid_index(index: &Index) -> bool {
    let mut used = BTreeSet::new();
    index.version == 1
        && valid_digest(&index.identity)
        && index.entries.len() <= MAX_ENTRIES
        && index.entries.iter().all(|(key, entry)| {
            valid_digest(key)
                && valid_digest(&entry.digest)
                && entry.bytes > 0
                && entry.bytes <= MAX_ENTRY
                && match &entry.inline {
                    Some(data) => {
                        entry.bytes <= INLINE
                            && entry.slots.is_empty()
                            && data.len() <= INLINE.div_ceil(3) * 4
                            && STANDARD.decode(data).is_ok_and(|bytes| {
                                bytes.len() == entry.bytes && digest(&bytes) == entry.digest
                            })
                    }
                    None => {
                        entry.slots.len() == entry.bytes.div_ceil(CHUNK)
                            && entry
                                .slots
                                .iter()
                                .all(|slot| *slot < SLOTS && used.insert(*slot))
                    }
                }
        })
}
impl<B: Storage> Store<B> {
    fn open(backend: B, identity: String, now: u64) -> Result<Self, String> {
        let saved = backend.read(None)?;
        let index = saved
            .as_deref()
            .filter(|b| b.len() <= MAX_INDEX)
            .and_then(|b| serde_json::from_slice::<Index>(b).ok())
            .filter(|i| valid_index(i) && i.identity == identity)
            .unwrap_or(Index {
                version: 1,
                identity,
                entries: BTreeMap::new(),
            });
        let mut store = Self { backend, index };
        let mut pruned = store.index.clone();
        pruned.entries.retain(|_, e| fresh(e.at, now));
        let bytes = serde_json::to_vec(&pruned).map_err(|_| "Invalid cache index.")?;
        if saved.as_deref() != Some(&bytes) {
            store.commit(pruned)?;
        }
        store.remove_unused();
        Ok(store)
    }
    fn commit(&mut self, index: Index) -> Result<(), String> {
        let bytes = serde_json::to_vec(&index).map_err(|_| "Invalid cache index.")?;
        if bytes.len() > MAX_INDEX || !valid_index(&index) {
            return Err("Invalid cache index.".into());
        }
        self.backend.write(None, &bytes)?;
        self.index = index;
        Ok(())
    }
    fn used(&self) -> BTreeSet<usize> {
        self.index
            .entries
            .values()
            .flat_map(|e| e.slots.iter().copied())
            .collect()
    }
    fn remove_unused(&mut self) {
        let used = self.used();
        for slot in 0..SLOTS {
            if !used.contains(&slot) {
                let _ = self.backend.remove(slot);
            }
        }
    }
    fn clear(&mut self) -> Result<(), String> {
        let mut index = self.index.clone();
        index.entries.clear();
        self.commit(index)?;
        self.remove_unused();
        Ok(())
    }
    fn get(&self, key: &str, now: u64) -> Option<Vec<u8>> {
        let entry = self.index.entries.get(key).filter(|e| fresh(e.at, now))?;
        if let Some(data) = &entry.inline {
            let bytes = STANDARD.decode(data).ok()?;
            return (bytes.len() == entry.bytes && digest(&bytes) == entry.digest).then_some(bytes);
        }
        let mut bytes = Vec::with_capacity(entry.bytes);
        for (part, slot) in entry.slots.iter().enumerate() {
            let chunk = self.backend.read(Some(*slot)).ok()??;
            let expected = (entry.bytes - part * CHUNK).min(CHUNK);
            if chunk.len() != expected {
                return None;
            }
            bytes.extend(chunk);
        }
        (digest(&bytes) == entry.digest).then_some(bytes)
    }
    fn put(&mut self, key: String, bytes: &[u8], now: u64) -> Result<(), String> {
        if !valid_digest(&key) || bytes.is_empty() || bytes.len() > MAX_ENTRY {
            return Err("Cache entry is too large or invalid.".into());
        }
        let hash = digest(bytes);
        if self
            .index
            .entries
            .get(&key)
            .is_some_and(|e| fresh(e.at, now) && e.digest == hash)
            && self.get(&key, now).is_some()
        {
            return Ok(()); // No write and no sliding retention on unchanged data.
        }
        let inline = (bytes.len() <= INLINE).then(|| STANDARD.encode(bytes));
        let needed = if inline.is_some() {
            0
        } else {
            bytes.len().div_ceil(CHUNK)
        };
        let mut index = self.index.clone();
        index.entries.retain(|k, e| k != &key && fresh(e.at, now));
        while index.entries.values().map(|e| e.slots.len()).sum::<usize>() + needed > SLOTS {
            let oldest = index
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
                .ok_or("Invalid cache allocation.")?;
            index.entries.remove(&oldest);
        }
        let used: BTreeSet<_> = index
            .entries
            .values()
            .flat_map(|e| e.slots.iter().copied())
            .collect();
        let slots: Vec<_> = (0..SLOTS)
            .filter(|s| !used.contains(s))
            .take(needed)
            .collect();
        index.entries.insert(
            key.clone(),
            Entry {
                at: now,
                bytes: bytes.len(),
                digest: hash,
                slots: slots.clone(),
                inline,
            },
        );
        while index.entries.len() > MAX_ENTRIES
            || serde_json::to_vec(&index)
                .map_err(|_| "Invalid cache index.")?
                .len()
                > MAX_INDEX
        {
            let oldest = index
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
                .ok_or("Cache index is full.")?;
            index.entries.remove(&oldest);
        }
        if needed > 0 {
            // Remove references durably BEFORE reusing slots. Interruption can
            // lose a replaceable entry, never expose another entry's bytes.
            let mut pending = index.clone();
            pending.entries.remove(&key);
            self.commit(pending)?;
            for (slot, chunk) in slots.iter().zip(bytes.chunks(CHUNK)) {
                self.backend.write(Some(*slot), chunk)?;
            }
        }
        self.commit(index)?;
        self.remove_unused();
        Ok(())
    }
}
impl Cache {
    fn load(config: &Config, session: &Session, access: &Access) -> Option<Self> {
        access.check().ok()?;
        if config.discord_guild_id != access.guild_id
            || config.user_id != access.user_id
            || !valid_digest(&session.cache_id)
        {
            return None;
        }
        let identity = digest(
            &serde_json::to_vec(&(&session.cache_id, &config.client_id, config.guild_id)).ok()?,
        );
        let scope = format!("wcl-data-v1:{}:{}", config.discord_guild_id, config.user_id);
        let store = Store::open(Disk(scope), identity, super::now_secs()).ok()?;
        access.check().ok()?;
        Some(Self(store))
    }
    pub fn invalidate(&mut self) {
        let _ = self.0.clear();
    }
}
/// The row stays unique when a fight grows or its bounds are corrected. Its
/// revision must match exactly before the stored payload can be reused.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) struct Key {
    id: String,
    revision: String,
}
#[derive(Serialize, Deserialize)]
struct Payload<T> {
    revision: String,
    value: T,
}
pub(super) fn pull_key(kind: &str, pull: &Pull) -> Key {
    Key {
        id: digest(&serde_json::to_vec(&(kind, &pull.report, pull.id)).expect("scalar cache key")),
        revision: digest(
            &serde_json::to_vec(&(
                pull.report_start_ms,
                pull.start_ms,
                pull.end_ms,
                pull.encounter,
                pull.difficulty,
            ))
            .expect("scalar cache key"),
        ),
    }
}
pub(super) fn master_key(pull: &Pull) -> Key {
    Key {
        id: digest(&serde_json::to_vec(&("master-v1", &pull.report)).expect("scalar cache key")),
        revision: digest(&pull.report_start_ms.to_le_bytes()),
    }
}

impl Client {
    fn data_cache(&mut self, access: &Access) -> Option<&mut Cache> {
        access.check().ok()?;
        check_cancelled(&self.cancel).ok()?;
        let config = self.config.as_ref()?;
        if config.user_id != access.user_id || config.discord_guild_id != access.guild_id {
            return None;
        }
        let session = self.session.as_ref()?;
        if session.expires_at <= super::now_secs() {
            return None;
        }
        if self.data_cache.is_none() {
            self.data_cache = Cache::load(config, session, access);
        }
        self.data_cache.as_mut()
    }
    pub(super) fn cached_data<T: DeserializeOwned>(
        &mut self,
        access: &Access,
        key: &Key,
    ) -> Option<T> {
        let bytes = self.data_cache(access)?.0.get(&key.id, super::now_secs())?;
        let payload: Payload<T> = serde_json::from_slice(&bytes).ok()?;
        if payload.revision != key.revision {
            return None;
        }
        access.check().ok()?;
        check_cancelled(&self.cancel).ok()?;
        Some(payload.value)
    }
    pub(super) fn cache_data<T: Serialize>(&mut self, access: &Access, key: Key, value: &T) {
        // Bounded serializer prevents even an unexpectedly oversized value from
        // allocating an unbounded temporary before the store enforces its cap.
        struct Writer(Vec<u8>);
        impl std::io::Write for Writer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                if data.len() > MAX_ENTRY.saturating_sub(self.0.len()) {
                    return Err(std::io::Error::other("cache limit"));
                }
                self.0.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer(Vec::new());
        if serde_json::to_writer(
            &mut writer,
            &Payload {
                revision: key.revision,
                value,
            },
        )
        .is_err()
        {
            return;
        }
        if let Some(cache) = self.data_cache(access) {
            let _ = cache.0.put(key.id, &writer.0, super::now_secs());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Memory {
        files: BTreeMap<Option<usize>, Vec<u8>>,
        writes: usize,
        fail_at: Option<usize>,
    }
    impl Storage for Memory {
        fn read(&self, slot: Option<usize>) -> Result<Option<Vec<u8>>, String> {
            Ok(self.files.get(&slot).cloned())
        }
        fn write(&mut self, slot: Option<usize>, bytes: &[u8]) -> Result<(), String> {
            self.writes += 1;
            if self.fail_at == Some(self.writes) {
                return Err("simulated interruption".into());
            }
            self.files.insert(slot, bytes.to_vec());
            Ok(())
        }
        fn remove(&mut self, slot: usize) -> Result<(), String> {
            self.files.remove(&Some(slot));
            Ok(())
        }
    }
    fn open() -> Store<Memory> {
        Store::open(Memory::default(), digest(b"account"), 1).unwrap()
    }
    #[test]
    fn restart_upserts_skip_unchanged_writes_and_reads_do_not_extend_retention() {
        let mut store = open();
        let key = digest(b"report/pull");
        store.put(key.clone(), b"first", 10).unwrap();
        let writes = store.backend.writes;
        for now in 11..110 {
            store.put(key.clone(), b"first", now).unwrap();
            assert_eq!(store.get(&key, now).unwrap(), b"first");
        }
        assert_eq!(store.backend.writes, writes);
        let mut store = Store::open(store.backend, digest(b"account"), 110).unwrap();
        assert_eq!(store.backend.writes, writes);
        store.put(key.clone(), b"updated", 111).unwrap();
        assert_eq!(store.index.entries.len(), 1);
        assert_eq!(store.get(&key, 111).unwrap(), b"updated");
        assert!(store.get(&key, 111 + AGE.as_secs()).is_none());
        assert!(store.get(&key, u64::MAX).is_none());
        let store = Store::open(store.backend, digest(b"account"), 111 + AGE.as_secs()).unwrap();
        assert_eq!(store.backend.files.len(), 1);
    }
    #[test]
    fn fixed_slots_bound_growth_and_support_maximum_signature() {
        let mut store = open();
        for id in 1u16..300 {
            store
                .put(digest(&id.to_le_bytes()), &[7; 50], id as u64)
                .unwrap();
        }
        assert_eq!(store.index.entries.len(), MAX_ENTRIES);
        assert!(store.backend.files.len() <= SLOTS + 1);
        let large = vec![7; MAX_ENTRY];
        let key = digest(b"large");
        store.put(key.clone(), &large, 400).unwrap();
        assert_eq!(store.get(&key, 400).unwrap(), large);
        assert!(
            store.backend.files.values().map(Vec::len).sum::<usize>() <= SLOTS * CHUNK + MAX_INDEX
        );
        assert!(store.put(key, &vec![0; MAX_ENTRY + 1], 101).is_err());
    }
    #[test]
    fn interruptions_and_tampering_are_misses_and_retry_repairs_without_growth() {
        for failure in 1..=3 {
            let mut store = open();
            let key = digest(b"pull");
            store.put(key.clone(), &vec![1; INLINE + 1], 1).unwrap();
            store.backend.fail_at = Some(store.backend.writes + failure);
            assert!(store.put(key.clone(), &vec![2; INLINE + 1], 2).is_err());
            store.backend.fail_at = None;
            let mut store = Store::open(store.backend, digest(b"account"), 2).unwrap();
            assert!(store.get(&key, 2).is_none_or(|v| v == vec![1; INLINE + 1]));
            store.put(key.clone(), &vec![2; INLINE + 1], 2).unwrap();
            assert_eq!(store.get(&key, 2).unwrap(), vec![2; INLINE + 1]);
            let slot = store.index.entries[&key].slots[0];
            store.backend.files.get_mut(&Some(slot)).unwrap()[0] ^= 1;
            assert!(store.get(&key, 2).is_none());
            store.put(key.clone(), &vec![2; INLINE + 1], 3).unwrap();
            assert_eq!(store.get(&key, 3).unwrap(), vec![2; INLINE + 1]);
        }
    }
    #[test]
    fn full_slots_and_small_index_evict_old_data_with_fixed_total_bounds() {
        let mut store = open();
        for id in 0..SLOTS + 3 {
            store
                .put(
                    digest(&id.to_le_bytes()),
                    &vec![id as u8; CHUNK],
                    id as u64 + 1,
                )
                .unwrap();
        }
        assert_eq!(store.used().len(), SLOTS);
        assert!(store.get(&digest(&0usize.to_le_bytes()), 20).is_none());
        for id in 0..90usize {
            store
                .put(
                    digest(format!("small{id}").as_bytes()),
                    &vec![4; INLINE],
                    100 + id as u64,
                )
                .unwrap();
            assert!(
                store.backend.files.values().map(Vec::len).sum::<usize>()
                    <= SLOTS * CHUNK + MAX_INDEX
            );
        }
        assert!(store.index.entries.len() < 90 + SLOTS);
        let store = Store::open(store.backend, digest(b"account"), u64::MAX).unwrap();
        assert_eq!(store.backend.files.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the isolated Secret Service used by CI"]
    fn linux_keyring_roundtrip_wcl_data_uses_fixed_encrypted_slots() {
        let config = Config {
            client_id: uuid::Uuid::new_v4().to_string(),
            guild_id: 123,
            user_id: "103".into(),
            discord_guild_id: crate::guild::ADVANCE.into(),
            content_alignment: None,
        };
        let session = Session {
            rate_budget: Default::default(),
            cache_id: super::super::random(),
            client_id: config.client_id.clone(),
            user_id: config.user_id.clone(),
            access_token: "synthetic-wcl".into(),
            refresh_token: None,
            expires_at: super::super::now_secs() + 3600,
        };
        let access = Access::new(
            "synthetic-discord".into(),
            config.discord_guild_id.clone(),
            config.user_id.clone(),
            crate::guild::generation(),
        );
        let mut cache = Cache::load(&config, &session, &access).unwrap();
        let bytes = vec![42; CHUNK + 1];
        let key = digest(b"report-pull");
        let now = super::super::now_secs();
        cache.0.put(key.clone(), &bytes, now).unwrap();
        let files = || {
            std::iter::once(None)
                .chain((0..SLOTS).map(Some))
                .filter_map(|slot| {
                    let scope = Disk(format!(
                        "wcl-data-v1:{}:{}",
                        config.discord_guild_id, config.user_id
                    ))
                    .scope(slot);
                    let path = crate::addon::config_dir()
                        .unwrap()
                        .join(format!("guild-cache-{}.dat", digest(scope.as_bytes())));
                    std::fs::read(path).ok().map(|bytes| (slot, bytes))
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = files();
        assert_eq!(before.len(), 3);
        assert!(!before
            .values()
            .any(|bytes| bytes.windows(32).any(|b| b == [42; 32])));
        cache.0.put(key.clone(), &bytes, now + 1).unwrap();
        assert_eq!(files(), before);
        drop(cache);
        let mut restored = Cache::load(&config, &session, &access).unwrap();
        assert_eq!(restored.0.get(&key, now).unwrap(), bytes);
        assert_eq!(files(), before);
        restored.invalidate();
        assert_eq!(files().len(), 1);
        remove_test_cache(&config);
    }

    #[test]
    fn changed_authorization_and_invalid_indexes_cannot_restore_data() {
        let mut store = open();
        let key = digest(b"pull");
        store.put(key.clone(), b"private", 1).unwrap();
        let store = Store::open(store.backend, digest(b"reconnected-account"), 2).unwrap();
        assert!(store.get(&key, 2).is_none());
        assert_eq!(store.backend.files.len(), 1);
        let mut index = store.index;
        index.entries.insert(
            key,
            Entry {
                at: 1,
                bytes: usize::MAX,
                digest: digest(b"x"),
                slots: vec![usize::MAX],
                inline: None,
            },
        );
        assert!(!valid_index(&index));
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(super) fn remove_test_cache(config: &Config) {
    let disk = Disk(format!(
        "wcl-data-v1:{}:{}",
        config.discord_guild_id, config.user_id
    ));
    for slot in std::iter::once(None).chain((0..SLOTS).map(Some)) {
        let scope = disk.scope(slot);
        crate::protected_cache::remove(&scope).unwrap();
        crate::credential_store::Store::new(&format!("guild-cache-key-v1:{scope}"))
            .unwrap()
            .remove()
            .unwrap();
    }
}
