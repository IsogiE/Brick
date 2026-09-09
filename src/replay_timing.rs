//! Local video alignment preferences. All disk access belongs on the review worker.
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File, io::Read, path::PathBuf};

const MAX_BYTES: usize = 16 * 1024;
const MAX_ENTRIES: usize = 64;
const MAX_SECONDS: i64 = 600;
const SAVE_ERROR: &str = "Couldn't save the video alignment.";
pub const DEFAULT_SECONDS: i64 = 6;

pub struct Store {
    path: Option<PathBuf>,
    corrections: BTreeMap<String, i64>,
}

impl Store {
    /// Load once on the review worker; `get` only reads the bounded memory cache.
    pub fn load() -> Self {
        Self::load_from(
            crate::addon::config_dir()
                .ok()
                .map(|dir| dir.join("replay-timing.json")),
        )
    }

    fn load_from(path: Option<PathBuf>) -> Self {
        let corrections = path
            .as_ref()
            .and_then(|path| {
                // Never read an unbounded file or intentionally open a device/pipe.
                if !std::fs::metadata(path).ok()?.is_file() {
                    return None;
                }
                let mut bytes = Vec::new();
                File::open(path)
                    .ok()?
                    .take((MAX_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() > MAX_BYTES {
                    return None;
                }
                let values: BTreeMap<String, i64> = serde_json::from_slice(&bytes).ok()?;
                if values.len() > MAX_ENTRIES
                    || values.iter().any(|(key, seconds)| {
                        key.len() != 64
                            || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
                            || !(-MAX_SECONDS..=MAX_SECONDS).contains(seconds)
                    })
                {
                    return None;
                }
                let count = values.len();
                let normalized: BTreeMap<_, _> = values
                    .into_iter()
                    .map(|(key, seconds)| (key.to_ascii_lowercase(), seconds))
                    .collect();
                (normalized.len() == count).then_some(normalized)
            })
            .unwrap_or_default();
        Self { path, corrections }
    }

    pub fn get(&self, provider: &str, video: &str, report: &str) -> i64 {
        context_key(provider, video, report)
            .and_then(|key| self.corrections.get(&key).copied())
            .unwrap_or(DEFAULT_SECONDS)
    }

    /// Positive seconds move the video later for the same event in the log.
    /// Zero is an explicit saved choice. Resetting to the visible default removes
    /// only this recording/report's override.
    pub fn set(
        &mut self,
        provider: &str,
        video: &str,
        report: &str,
        seconds: i64,
    ) -> Result<(), String> {
        let key = context_key(provider, video, report)
            .filter(|_| (-MAX_SECONDS..=MAX_SECONDS).contains(&seconds))
            .ok_or("Invalid video alignment.")?;
        if self
            .corrections
            .get(&key)
            .copied()
            .unwrap_or(DEFAULT_SECONDS)
            == seconds
        {
            return Ok(());
        }
        let path = self.path.as_ref().ok_or(SAVE_ERROR)?;
        let mut next = self.corrections.clone();
        if seconds == DEFAULT_SECONDS {
            next.remove(&key);
        } else {
            if !next.contains_key(&key) && next.len() >= MAX_ENTRIES {
                next.pop_first();
            }
            next.insert(key, seconds);
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| SAVE_ERROR)?;
        if bytes.len() > MAX_BYTES {
            return Err(SAVE_ERROR.into());
        }
        // The shared helper creates the temporary and final files with private
        // Unix permissions; Windows uses the current user's config directory.
        crate::atomic_file::write(path, &bytes).map_err(|_| SAVE_ERROR)?;
        self.corrections = next;
        Ok(())
    }
}

fn context_key(provider: &str, video: &str, report: &str) -> Option<String> {
    if !matches!(provider, "youtube" | "twitch")
        || [video, report].iter().any(|value| {
            value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return None;
    }
    // Length prefixes distinguish contexts even when their concatenations match.
    // Persist only the digest: no report codes, video IDs, URLs or login tokens.
    let mut hash = Sha256::new();
    hash.update(b"brick-replay-timing-v1");
    for value in [provider, video, report] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    Some(hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("brick-replay-timing-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("replay-timing.json")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn corrections_persist_without_ids_and_stay_with_their_recording_and_report() {
        let root = TestDirectory::new();
        let mut store = Store::load_from(Some(root.path()));
        assert_eq!(store.get("youtube", "video-a", "report-a"), DEFAULT_SECONDS);
        store.set("youtube", "video-a", "report-a", 9).unwrap();
        store.set("youtube", "video-b", "report-a", -12).unwrap();
        store.set("youtube", "video-a", "report-b", 4).unwrap();
        let saved = fs::read_to_string(root.path()).unwrap();
        for identifier in ["youtube", "video-a", "video-b", "report-a", "report-b"] {
            assert!(!saved.contains(identifier));
        }
        let mut restored = Store::load_from(Some(root.path()));
        assert_eq!(restored.get("youtube", "video-a", "report-a"), 9);
        assert_eq!(restored.get("youtube", "video-b", "report-a"), -12);
        assert_eq!(restored.get("youtube", "video-a", "report-b"), 4);
        assert_eq!(
            restored.get("twitch", "video-a", "report-a"),
            DEFAULT_SECONDS
        );
        assert_eq!(
            restored.get("youtube", "video-b", "report-b"),
            DEFAULT_SECONDS
        );
        // Distinct component boundaries must not share a correction.
        restored.set("youtube", "ab", "c", 15).unwrap();
        assert_eq!(restored.get("youtube", "a", "bc"), DEFAULT_SECONDS);
        restored.set("youtube", "video-a", "report-a", 0).unwrap();
        let reset = Store::load_from(Some(root.path()));
        assert_eq!(reset.get("youtube", "video-a", "report-a"), 0);
        assert_eq!(reset.get("youtube", "video-b", "report-a"), -12);
    }

    #[test]
    fn explicit_zero_survives_restart_and_reset_only_removes_its_override() {
        let root = TestDirectory::new();
        let mut store = Store::load_from(Some(root.path()));
        assert_eq!(store.get("youtube", "video", "report"), 6);
        store.set("youtube", "video", "report", 0).unwrap();
        store.set("twitch", "other", "report", 9).unwrap();
        let mut restored = Store::load_from(Some(root.path()));
        assert_eq!(restored.get("youtube", "video", "report"), 0);
        restored
            .set("youtube", "video", "report", DEFAULT_SECONDS)
            .unwrap();
        let reset = Store::load_from(Some(root.path()));
        assert_eq!(reset.get("youtube", "video", "report"), DEFAULT_SECONDS);
        assert_eq!(reset.get("twitch", "other", "report"), 9);
        assert_eq!(reset.corrections.len(), 1);
    }

    #[test]
    fn malformed_or_oversized_saved_data_uses_the_visible_default() {
        let root = TestDirectory::new();
        let key = context_key("youtube", "video", "report").unwrap();
        let too_many: BTreeMap<_, _> = (0..=MAX_ENTRIES)
            .map(|index| (format!("{index:064x}"), 9))
            .collect();
        for invalid in [
            "{interrupted".into(),
            "null".into(),
            "[]".into(),
            format!(r#"{{"{key}":601}}"#),
            format!(r#"{{"{key}":-601}}"#),
            format!(r#"{{"{key}":1.5}}"#),
            format!(r#"{{"{key}":"9"}}"#),
            format!(r#"{{"{key}":9,"not-a-hash":4}}"#),
            format!(r#"{{"{}":9}}"#, "z".repeat(64)),
            serde_json::to_string(&too_many).unwrap(),
        ] {
            fs::write(root.path(), invalid).unwrap();
            let store = Store::load_from(Some(root.path()));
            assert!(store.corrections.is_empty());
            assert_eq!(store.get("youtube", "video", "report"), DEFAULT_SECONDS);
        }
        let mut oversized = format!(r#"{{"{key}":9}}"#).into_bytes();
        oversized.resize(MAX_BYTES + 1, b' ');
        fs::write(root.path(), oversized).unwrap();
        assert_eq!(
            Store::load_from(Some(root.path())).get("youtube", "video", "report"),
            DEFAULT_SECONDS
        );
    }

    #[test]
    fn invalid_changes_leave_saved_alignment_intact_and_capacity_is_bounded() {
        let root = TestDirectory::new();
        let mut store = Store::load_from(Some(root.path()));
        store.set("youtube", "video", "report", 9).unwrap();
        for seconds in [i64::MIN, -601, 601, i64::MAX] {
            assert!(store.set("youtube", "video", "report", seconds).is_err());
        }
        for (provider, video, report) in [
            ("unknown", "video", "report"),
            ("youtube", "", "report"),
            ("youtube", "video", ""),
            ("youtube", "https://example.invalid/video", "report"),
            ("youtube", "video", "report\n"),
            ("youtube", "video", "☃"),
        ] {
            assert!(store.set(provider, video, report, 10).is_err());
            assert_eq!(store.get(provider, video, report), DEFAULT_SECONDS);
        }
        assert!(store
            .set("youtube", &"x".repeat(129), "report", 10)
            .is_err());
        assert_eq!(
            Store::load_from(Some(root.path())).get("youtube", "video", "report"),
            9
        );
        for index in 0..MAX_ENTRIES + 5 {
            store
                .set("youtube", &format!("video-{index}"), "report", 600)
                .unwrap();
            assert_eq!(
                store.get("youtube", &format!("video-{index}"), "report"),
                600
            );
        }
        assert_eq!(store.corrections.len(), MAX_ENTRIES);
        assert!(fs::metadata(root.path()).unwrap().len() <= MAX_BYTES as u64);
        assert_eq!(
            Store::load_from(Some(root.path())).corrections.len(),
            MAX_ENTRIES
        );
    }
}
