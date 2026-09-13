//! Guild identity is captured with each request, never read at send time.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

pub const ADVANCE: &str = "1166119057993515100";
#[cfg(test)]
pub const ASCENDANCE: &str = "481024965852921856";
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PANEL: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Only the UI publishes a context, after discarding the previous panel.
pub fn activate(guild: &str, user: &str) {
    if let Ok(mut panel) = PANEL.lock() {
        *panel = Some((guild.to_string(), user.to_string()));
    }
}

pub fn ensure_panel(guild: &str, user: &str) -> Result<(), String> {
    let panel = PANEL.lock().map_err(|_| "Guild context is unavailable.")?;
    if panel
        .as_ref()
        .is_some_and(|(active_guild, active_user)| active_guild == guild && active_user == user)
    {
        Ok(())
    } else {
        Err("The selected guild or account changed.".into())
    }
}

thread_local! {
    static REQUEST_GENERATION: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub fn request_generation() -> u64 {
    REQUEST_GENERATION
        .with(|expected| expected.get())
        .unwrap_or_else(generation)
}

pub fn with_generation<T>(expected: u64, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUEST_GENERATION.with(|value| value.set(self.0));
        }
    }
    let _restore = Restore(REQUEST_GENERATION.with(|value| value.replace(Some(expected))));
    work()
}

/// Capture on the caller before starting/waiting on an asynchronous worker.
pub fn spawn(work: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    let expected = request_generation();
    std::thread::spawn(move || {
        with_generation(expected, || {
            if ensure_current(expected).is_ok() {
                work();
            }
        })
    })
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub fn invalidate() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

pub fn ensure_current(expected: u64) -> Result<(), String> {
    if generation() == expected {
        Ok(())
    } else {
        Err("The selected guild or account changed.".into())
    }
}

#[derive(Clone)]
pub struct Access {
    secret: String,
    pub guild_id: String,
    pub user_id: String,
    generation: u64,
}

impl Access {
    pub fn new(secret: String, guild_id: String, user_id: String, generation: u64) -> Self {
        Self {
            secret,
            guild_id,
            user_id,
            generation,
        }
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn check(&self) -> Result<(), String> {
        ensure_current(self.generation)
    }

    pub fn endpoint(&self, path: &str) -> Result<url::Url, String> {
        self.check()?;
        crate::presence::endpoint_url(&scoped_path(&self.guild_id, path)?)
    }

    pub fn cache_id(&self) -> String {
        format!("{}:{}", self.guild_id, self.user_id)
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(format!(
            "{}:{}:{}",
            self.guild_id, self.user_id, self.secret
        ))
        .into()
    }
}

impl std::ops::Deref for Access {
    type Target = str;
    fn deref(&self) -> &str {
        self.secret()
    }
}

impl std::fmt::Debug for Access {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuildAccess")
            .field("guild_id", &self.guild_id)
            .finish_non_exhaustive()
    }
}

pub fn scoped_path(guild_id: &str, path: &str) -> Result<String, String> {
    if guild_id.is_empty()
        || guild_id.len() > 20
        || !guild_id.bytes().all(|b| b.is_ascii_digit())
        || !path.starts_with("/v1/")
        || path.contains("..")
        || path.contains('\\')
        || path.contains('#')
        || path.contains('%')
    {
        return Err("Invalid guild request.".into());
    }
    Ok(format!("/v2/guilds/{guild_id}{path}"))
}

#[cfg(test)]
impl From<&str> for Access {
    fn from(secret: &str) -> Self {
        Self::new(secret.into(), ADVANCE.into(), "123".into(), generation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_workers_cannot_acquire_a_new_guild_context() {
        let stale = generation().wrapping_sub(1);
        with_generation(stale, || {
            assert_eq!(request_generation(), stale);
            assert!(ensure_current(request_generation()).is_err());
            let token = Access::new("secret".into(), ADVANCE.into(), "123".into(), stale);
            assert!(token.check().is_err());
            assert!(token.endpoint("/v1/roster").is_err());
            assert!(!format!("{token:?}").contains("secret"));
        });
    }

    #[test]
    fn guild_paths_cannot_escape_their_namespace() {
        assert_eq!(
            scoped_path(ASCENDANCE, "/v1/roster").unwrap(),
            "/v2/guilds/481024965852921856/v1/roster"
        );
        for guild in ["", "../advance", "1?other=2", "123456789012345678901"] {
            assert!(scoped_path(guild, "/v1/roster").is_err());
        }
        for path in [
            "//evil.test",
            "/v1/../roster",
            "/v1/%2e%2e/roster",
            "/v1/a#b",
            "/discord/callback",
        ] {
            assert!(scoped_path(ADVANCE, path).is_err());
        }
    }

    #[test]
    fn identical_oauth_tokens_do_not_share_guild_or_account_cache_keys() {
        let first = Access::new("token".into(), ADVANCE.into(), "123".into(), generation());
        let other = Access::new(
            "token".into(),
            ASCENDANCE.into(),
            "123".into(),
            generation(),
        );
        assert_ne!(first.fingerprint(), other.fingerprint());
        assert_ne!(first.cache_id(), other.cache_id());
    }
}
