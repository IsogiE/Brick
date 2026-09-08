use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 4096;
const ACKNOWLEDGEMENT_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const MAX_CHANGES_PER_PLAYER: u8 = 16;

// Persist only public classification IDs and their original acknowledgement
// expiry. Cookies, provider accounts, URLs and arbitrary browser data never fit
// this type. Unknown future labels continue to use Twitch's normal prompt.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
enum Label {
    DebatedSocialIssuesAndPolitics,
    DrugsIntoxication,
    Gambling,
    MatureGame,
    ProfanityVulgarity,
    SexualThemes,
    ViolentGraphic,
}

type Acknowledgements = BTreeMap<Label, u64>;

#[derive(Clone)]
pub struct Preferences(Arc<Mutex<State>>);

struct State {
    path: Option<PathBuf>,
    acknowledgements: Acknowledgements,
    writing: bool,
}

impl Preferences {
    /// Called by the existing player preparation worker, never the GUI thread.
    pub fn load() -> Self {
        Self::load_from(
            crate::addon::config_dir()
                .ok()
                .map(|dir| dir.join("stream-preferences.json")),
        )
    }

    fn load_from(path: Option<PathBuf>) -> Self {
        let acknowledgements = path
            .as_ref()
            .and_then(|path| {
                let mut bytes = Vec::new();
                File::open(path)
                    .ok()?
                    .take((MAX_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() > MAX_BYTES {
                    return None;
                }
                let mut values: Acknowledgements = serde_json::from_slice(&bytes).ok()?;
                retain_current(&mut values, now_ms());
                Some(values)
            })
            .unwrap_or_default();
        Self(Arc::new(Mutex::new(State {
            path,
            acknowledgements,
            writing: false,
        })))
    }

    fn snapshot(&self) -> Acknowledgements {
        let mut values = self
            .0
            .lock()
            .map(|state| state.acknowledgements.clone())
            .unwrap_or_default();
        retain_current(&mut values, now_ms());
        values
    }

    fn record(&self, acknowledgements: Acknowledgements) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if state.acknowledgements == acknowledgements {
            return;
        }
        state.acknowledgements = acknowledgements;
        if state.writing || state.path.is_none() {
            return;
        }
        state.writing = true;
        let pending = self.clone();
        // One writer coalesces changes into the latest bounded value. It exits
        // once caught up; no parked thread, timer or per-frame work remains.
        let spawned = thread::Builder::new()
            .name("stream-preferences".into())
            .spawn(move || pending.write_pending());
        if spawned.is_err() {
            state.writing = false;
        }
    }

    fn write_pending(&self) {
        loop {
            let (path, values) = match self.0.lock() {
                Ok(state) => (state.path.clone(), state.acknowledgements.clone()),
                Err(_) => return,
            };
            if let (Some(path), Ok(bytes)) = (path, serde_json::to_vec(&values)) {
                // This is a nonsecret preference, saved atomically with private
                // file permissions by the same helper used for app settings.
                let _ = crate::atomic_file::write(&path, &bytes);
            }
            let Ok(mut state) = self.0.lock() else {
                return;
            };
            if state.acknowledgements == values {
                state.writing = false;
                return;
            }
        }
    }
}

#[derive(Clone)]
pub struct PreferenceBridge(Arc<BridgeState>);

struct BridgeState {
    wrapper: String,
    nonce: String,
    active: AtomicBool,
    changes: AtomicU8,
    preferences: Preferences,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    nonce: String,
    acknowledgements: Acknowledgements,
}

impl PreferenceBridge {
    pub fn new(wrapper: &str, preferences: Preferences) -> Self {
        Self(Arc::new(BridgeState {
            wrapper: wrapper.to_owned(),
            nonce: uuid::Uuid::new_v4().to_string(),
            active: AtomicBool::new(true),
            changes: AtomicU8::new(0),
            preferences,
        }))
    }

    pub fn scripts(&self, wrapper_origin: &str) -> (String, String) {
        let wrapper = include_str!("stream_preference_relay.js")
            .replace("__BRICK_WRAPPER_URL__", &json(&self.0.wrapper))
            .replace("__BRICK_NONCE__", &json(&self.0.nonce));
        let provider = include_str!("stream_preference_capture.js")
            .replace("__BRICK_WRAPPER_ORIGIN__", &json(wrapper_origin))
            .replace(
                "__BRICK_ACKNOWLEDGEMENTS__",
                &serde_json::to_string(&self.0.preferences.snapshot())
                    .unwrap_or_else(|_| "{}".into()),
            );
        (wrapper, provider)
    }

    pub fn receive(&self, wrapper: &str, body: &str) {
        if !self.0.active.load(Ordering::Relaxed)
            || wrapper != self.0.wrapper
            || body.len() > MAX_BYTES
            || self.0.changes.load(Ordering::Relaxed) >= MAX_CHANGES_PER_PLAYER
        {
            return;
        }
        let Ok(mut message) = serde_json::from_str::<Message>(body) else {
            return;
        };
        if message.nonce != self.0.nonce {
            return;
        }
        retain_current(&mut message.acknowledgements, now_ms());
        if message.acknowledgements != self.0.preferences.snapshot() {
            self.0.changes.fetch_add(1, Ordering::Relaxed);
            self.0.preferences.record(message.acknowledgements);
        }
    }

    pub fn close(&self) {
        self.0.active.store(false, Ordering::Relaxed);
    }
}

fn retain_current(values: &mut Acknowledgements, now: u64) {
    values.retain(|_, expires| {
        *expires > now && *expires <= now.saturating_add(ACKNOWLEDGEMENT_LIFETIME_MS)
    });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn json(value: &str) -> String {
    serde_json::to_string(value).expect("A string can always be encoded as JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    const WRAPPER: &str = "https://brick.example/v1/streams/player/1/twitch";

    fn message(bridge: &PreferenceBridge, values: serde_json::Value) -> String {
        serde_json::json!({"nonce": bridge.0.nonce, "acknowledgements": values}).to_string()
    }

    #[test]
    fn bridge_accepts_only_its_wrapper_nonce_and_nonsecret_label_shape() {
        let preferences = Preferences::load_from(None);
        let bridge = PreferenceBridge::new(WRAPPER, preferences.clone());
        let expiry = now_ms() + 60_000;
        let valid = message(&bridge, serde_json::json!({"Gambling": expiry}));
        bridge.receive("https://player.twitch.tv/", &valid);
        bridge.receive(WRAPPER, &valid.replace(&bridge.0.nonce, "wrong"));
        bridge.receive(WRAPPER, &"x".repeat(MAX_BYTES + 1));
        bridge.receive(
            WRAPPER,
            &message(&bridge, serde_json::json!({"auth-token": expiry})),
        );
        bridge.receive(
            WRAPPER,
            &message(&bridge, serde_json::json!({"Gambling": "secret"})),
        );
        assert!(preferences.snapshot().is_empty());
        bridge.receive(WRAPPER, &valid);
        assert_eq!(preferences.snapshot()[&Label::Gambling], expiry);
        bridge.receive(WRAPPER, &valid);
        assert_eq!(bridge.0.changes.load(Ordering::Relaxed), 1);
        bridge.close();
        bridge.receive(WRAPPER, &message(&bridge, serde_json::json!({})));
        assert_eq!(preferences.snapshot()[&Label::Gambling], expiry);
    }

    #[test]
    fn acknowledgement_expiry_is_preserved_and_never_extended() {
        let now = now_ms();
        let mut values = BTreeMap::from([
            (Label::Gambling, now + 1234),
            (Label::MatureGame, now),
            (Label::ViolentGraphic, now + ACKNOWLEDGEMENT_LIFETIME_MS + 1),
        ]);
        retain_current(&mut values, now);
        assert_eq!(values, BTreeMap::from([(Label::Gambling, now + 1234)]));
    }

    #[test]
    fn a_player_cannot_queue_unbounded_preference_changes() {
        let preferences = Preferences::load_from(None);
        let bridge = PreferenceBridge::new(WRAPPER, preferences.clone());
        let expiry = now_ms() + 60_000;
        for change in 0..MAX_CHANGES_PER_PLAYER + 8 {
            bridge.receive(
                WRAPPER,
                &message(
                    &bridge,
                    serde_json::json!({"Gambling": expiry + u64::from(change)}),
                ),
            );
        }
        assert_eq!(
            bridge.0.changes.load(Ordering::Relaxed),
            MAX_CHANGES_PER_PLAYER
        );
        assert_eq!(
            preferences.snapshot()[&Label::Gambling],
            expiry + u64::from(MAX_CHANGES_PER_PLAYER) - 1
        );
    }

    #[test]
    fn expired_malformed_and_unknown_saved_preferences_never_become_consent() {
        let root =
            std::env::temp_dir().join(format!("brick-consent-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("stream-preferences.json");
        let now = now_ms();
        for invalid in [
            "{interrupted".to_string(),
            "null".to_string(),
            "{\"loggedIn\":{\"account\":[\"Gambling\"]}}".to_string(),
            "{\"Gambling\":\"secret\"}".to_string(),
            format!("{{\"UnknownFutureLabel\":{}}}", now + 60_000),
            format!("{{\"Gambling\":{now}}}"),
            format!(
                "{{\"Gambling\":{}}}",
                now + ACKNOWLEDGEMENT_LIFETIME_MS + 60_000
            ),
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(Preferences::load_from(Some(path.clone()))
                .snapshot()
                .is_empty());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistence_is_bounded_private_and_contains_no_browser_account_data() {
        let root =
            std::env::temp_dir().join(format!("brick-consent-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("stream-preferences.json");
        File::create(&path)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let preferences = Preferences::load_from(Some(path.clone()));
        assert!(preferences.snapshot().is_empty());
        let bridge = PreferenceBridge::new(WRAPPER, preferences.clone());
        let expiry = now_ms() + 60_000;
        bridge.receive(
            WRAPPER,
            &message(&bridge, serde_json::json!({"SexualThemes": expiry})),
        );
        for _ in 0..100 {
            if !preferences.0.lock().unwrap().writing {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!preferences.0.lock().unwrap().writing);
        let saved = fs::read_to_string(&path).unwrap();
        assert_eq!(saved, format!("{{\"SexualThemes\":{expiry}}}"));
        assert_eq!(
            Preferences::load_from(Some(path.clone())).snapshot()[&Label::SexualThemes],
            expiry
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_script_does_not_receive_the_native_bridge_nonce() {
        let bridge = PreferenceBridge::new(WRAPPER, Preferences::load_from(None));
        let (relay, capture) = bridge.scripts("https://brick.example");
        assert!(relay.contains(&bridge.0.nonce));
        assert!(!capture.contains(&bridge.0.nonce));
        assert!(!capture.contains(WRAPPER));
    }
}
