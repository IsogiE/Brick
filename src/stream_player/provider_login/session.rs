//! Only Brick's own provider cookies cross this boundary, into the OS vault.
//! No provider cookie enters a Brick request, log, preference file or IPC bridge.
use crate::{credential_store::Store, streams::Provider};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
};

const MAX_COOKIES: usize = 128;
const MAX_BYTES: usize = 60 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
    /// 0=None, 1=Lax, 2=Strict on both browser APIs.
    pub same_site: u8,
}

pub(super) fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Cookie {
    pub fn valid(&self, provider: &Provider, at: i64) -> bool {
        let domain = self.domain.strip_prefix('.').unwrap_or(&self.domain);
        let owned = match provider {
            Provider::Youtube => matches!(domain, "youtube.com" | "www.youtube.com"),
            Provider::Twitch => matches!(domain, "twitch.tv" | "www.twitch.tv"),
        };
        owned
            && !self.name.is_empty()
            && self.name.len() <= 256
            && self.value.len() <= 8192
            && !self
                .name
                .bytes()
                .any(|c| c <= 32 || c >= 127 || b"()<>@,;:\\\"/[]?={}".contains(&c))
            && !self.value.bytes().any(|c| c < 32 || c == 127)
            && self.path.starts_with('/')
            && self.path.len() <= 1024
            && !self.path.bytes().any(|c| c < 32 || c == 127)
            && self.same_site <= 2
            && self.expires.is_none_or(|expires| expires > at)
            && (!self.name.starts_with("__Secure-") || self.secure)
            && (!self.name.starts_with("__Host-")
                || (self.secure && self.path == "/" && !self.domain.starts_with('.')))
    }
}

// Presence of provider-issued authentication cookies is the browser's local
// session state. It does not assert subscriptions or validate a revoked token.
pub(super) fn signed_in(provider: &Provider, cookies: &[Cookie], at: i64) -> bool {
    let has = |name: &str| {
        cookies.iter().any(|cookie| {
            cookie.name == name && !cookie.value.is_empty() && cookie.valid(provider, at)
        })
    };
    match provider {
        Provider::Youtube => has("SID") && has("HSID"),
        Provider::Twitch => has("auth-token"),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    account: String,
    provider: String,
    cookies: Vec<Cookie>,
}

fn encode(account: &str, provider: &Provider, cookies: &[Cookie]) -> Result<Vec<u8>, ()> {
    let cookies = cookies
        .iter()
        .filter(|cookie| cookie.expires.is_some() && cookie.valid(provider, now()))
        .cloned()
        .collect::<Vec<_>>();
    if cookies.len() > MAX_COOKIES {
        return Err(());
    }
    let bytes = serde_json::to_vec(&Saved {
        version: 1,
        account: account.into(),
        provider: provider.key().into(),
        cookies,
    })
    .map_err(|_| ())?;
    if bytes.len() > MAX_BYTES {
        return Err(());
    }
    Ok(bytes)
}

fn decode(account: &str, provider: &Provider, bytes: &[u8]) -> Result<Vec<Cookie>, ()> {
    if bytes.len() > MAX_BYTES {
        return Err(());
    }
    let saved: Saved = serde_json::from_slice(bytes).map_err(|_| ())?;
    if saved.version != 1
        || saved.account != account
        || saved.provider != provider.key()
        || saved.cookies.len() > MAX_COOKIES
    {
        return Err(());
    }
    // Expired cookies are discarded, never extended by a restart or update.
    Ok(saved
        .cookies
        .into_iter()
        .filter(|cookie| cookie.expires.is_some() && cookie.valid(provider, now()))
        .collect())
}

enum Operation {
    Save(Vec<u8>),
    Clear,
}
struct Job {
    generation: u64,
    operation: Operation,
}
enum ResultData {
    Loaded(Result<Vec<Cookie>, ()>),
    Saved(Result<(), ()>),
    Cleared(Result<(), ()>),
}
struct Reply {
    generation: u64,
    data: ResultData,
}

/// One serialized worker per personal provider. A sign-out generation fences
/// queued saves before deletion; device reset also uses Store's write fence.
pub(super) struct Jar {
    provider: Provider,
    account: Option<String>,
    cookies: RefCell<Vec<Cookie>>,
    ready: Cell<bool>,
    clearing: Cell<bool>,
    error: RefCell<Option<String>>,
    generation: Arc<AtomicU64>,
    send: Option<mpsc::SyncSender<Job>>,
    replies: Option<mpsc::Receiver<Reply>>,
    worker: RefCell<Option<JoinHandle<()>>>,
    pending: RefCell<Option<Operation>>,
    last_saved: RefCell<Vec<u8>>,
}

impl Jar {
    pub fn memory(provider: Provider) -> Self {
        Self {
            provider,
            account: None,
            cookies: RefCell::new(Vec::new()),
            ready: Cell::new(true),
            clearing: Cell::new(false),
            error: RefCell::new(None),
            generation: Arc::new(AtomicU64::new(0)),
            send: None,
            replies: None,
            worker: RefCell::new(None),
            pending: RefCell::new(None),
            last_saved: RefCell::new(Vec::new()),
        }
    }

    pub fn new(provider: Provider, account: &str) -> Self {
        let mut jar = Self::memory(provider.clone());
        jar.account = Some(account.into());
        let identity = format!("viewing-session-v1:{account}");
        let store = match provider {
            Provider::Youtube => Store::youtube(&identity),
            Provider::Twitch => Store::twitch(&identity),
        };
        let Ok(store) = store else {
            jar.storage_error();
            return jar;
        };
        let (send, jobs) = mpsc::sync_channel::<Job>(1);
        let (replies, receive) = mpsc::channel();
        let generation = jar.generation.clone();
        let account = account.to_owned();
        jar.ready.set(false);
        jar.worker = RefCell::new(Some(std::thread::spawn(move || {
            let loaded = store.load().map_err(|_| ()).and_then(|bytes| match bytes {
                Some(mut bytes) => {
                    let result = decode(&account, &provider, &bytes);
                    bytes.fill(0);
                    result
                }
                None => Ok(Vec::new()),
            });
            let _ = replies.send(Reply {
                generation: 0,
                data: ResultData::Loaded(loaded),
            });
            while let Ok(job) = jobs.recv() {
                if job.generation != generation.load(Ordering::SeqCst) {
                    continue;
                }
                let data = match job.operation {
                    Operation::Save(mut bytes) => {
                        let result = store.save(&bytes).map_err(|_| ());
                        bytes.fill(0);
                        ResultData::Saved(result)
                    }
                    Operation::Clear => ResultData::Cleared(store.remove().map_err(|_| ())),
                };
                let _ = replies.send(Reply {
                    generation: job.generation,
                    data,
                });
            }
        })));
        jar.send = Some(send);
        jar.replies = Some(receive);
        jar
    }

    fn storage_error(&self) {
        *self.error.borrow_mut() = Some(format!("Couldn't restore or save your {} viewing login. Unlock your device's protected credential storage and try again.", self.provider.label()));
    }

    pub fn tick(&self) {
        if let Some(replies) = &self.replies {
            while let Ok(reply) = replies.try_recv() {
                if reply.generation != self.generation.load(Ordering::SeqCst) {
                    continue;
                }
                match reply.data {
                    ResultData::Loaded(result) => {
                        self.ready.set(true);
                        match result {
                            Ok(cookies) => *self.cookies.borrow_mut() = cookies,
                            Err(()) => self.storage_error(),
                        }
                    }
                    ResultData::Saved(Err(())) => {
                        self.last_saved.borrow_mut().clear();
                        self.storage_error();
                    }
                    ResultData::Cleared(result) => {
                        self.clearing.set(false);
                        self.ready.set(true);
                        match result {
                            Ok(()) => {
                                self.cookies.borrow_mut().clear();
                                self.last_saved.borrow_mut().clear();
                            }
                            Err(()) => self.storage_error(),
                        }
                    }
                    ResultData::Saved(Ok(())) => (),
                }
            }
        }
        self.enqueue();
    }

    fn enqueue(&self) {
        let operation = self.pending.borrow_mut().take();
        if let (Some(send), Some(operation)) = (&self.send, operation) {
            let job = Job {
                generation: self.generation.load(Ordering::SeqCst),
                operation,
            };
            if let Err(error) = send.try_send(job) {
                match error {
                    mpsc::TrySendError::Full(job) => {
                        *self.pending.borrow_mut() = Some(job.operation)
                    }
                    mpsc::TrySendError::Disconnected(_) => self.storage_error(),
                }
            }
        }
    }

    pub fn ready(&self) -> bool {
        self.tick();
        self.ready.get()
    }
    pub fn signed_in(&self) -> bool {
        self.tick();
        signed_in(&self.provider, &self.cookies.borrow(), now())
    }
    pub fn cookies(&self) -> Vec<Cookie> {
        self.cookies.borrow().clone()
    }
    pub fn error(&self) -> Option<String> {
        self.error.borrow_mut().take()
    }

    pub fn observe(&self, mut cookies: Vec<Cookie>) {
        if !self.ready.get() || self.clearing.get() {
            return;
        }
        cookies.retain(|cookie| cookie.valid(&self.provider, now()));
        cookies.sort_by(|a, b| (&a.domain, &a.path, &a.name).cmp(&(&b.domain, &b.path, &b.name)));
        cookies.dedup_by(|a, b| a.domain == b.domain && a.path == b.path && a.name == b.name);
        if cookies.len() > MAX_COOKIES {
            self.storage_error();
            return;
        }
        let had_session = self.signed_in();
        let has_session = signed_in(&self.provider, &cookies, now());
        *self.cookies.borrow_mut() = cookies;
        if let Some(account) = &self.account {
            if has_session || had_session || !self.last_saved.borrow().is_empty() {
                let cookies = self.cookies.borrow();
                let saved = if has_session { cookies.as_slice() } else { &[] };
                match encode(account, &self.provider, saved) {
                    Ok(bytes) if *self.last_saved.borrow() != bytes => {
                        *self.last_saved.borrow_mut() = bytes.clone();
                        *self.pending.borrow_mut() = Some(Operation::Save(bytes));
                        self.enqueue();
                    }
                    Err(()) => self.storage_error(),
                    _ => (),
                }
            }
        }
    }

    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if self.send.is_some() {
            self.ready.set(false);
            self.clearing.set(true);
            *self.pending.borrow_mut() = Some(Operation::Clear);
            self.enqueue();
        } else {
            self.cookies.borrow_mut().clear();
        }
    }
}

impl Drop for Jar {
    fn drop(&mut self) {
        if let (Some(send), Some(operation)) = (&self.send, self.pending.get_mut().take()) {
            let _ = send.send(Job {
                generation: self.generation.load(Ordering::SeqCst),
                operation,
            });
        }
        self.send.take();
        if let Some(worker) = self.worker.get_mut().take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires disposable /fixture home and its own Secret Service bus"]
    fn protected_viewing_session_survives_restart_and_sign_out_wins_over_saves() {
        assert_eq!(
            std::env::var("BRICK_LOCAL_ERASURE_FIXTURE").as_deref(),
            Ok("1")
        );
        assert_eq!(std::env::var("HOME").as_deref(), Ok("/fixture/home"));
        assert_eq!(
            std::env::var("DBUS_SESSION_BUS_ADDRESS").as_deref(),
            Ok("unix:path=/fixture/bus")
        );
        let wait = |jar: &Jar| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !jar.ready() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(jar.ready());
            assert!(jar.error().is_none());
        };
        let grant = Store::twitch("123").unwrap();
        grant.save(b"synthetic separate channel grant").unwrap();
        let jar = Jar::new(Provider::Twitch, "123");
        wait(&jar);
        assert!(!jar.signed_in());
        jar.observe(vec![cookie("auth-token", ".twitch.tv")]);
        assert!(jar.signed_in());
        drop(jar); // Waits for the queued protected write.
        let jar = Jar::new(Provider::Twitch, "123");
        wait(&jar);
        assert!(jar.signed_in());
        assert_eq!(jar.cookies.borrow()[0].same_site, 1);
        let other = Jar::new(Provider::Twitch, "456");
        wait(&other);
        assert!(!other.signed_in());
        let youtube = Jar::new(Provider::Youtube, "123");
        wait(&youtube);
        assert!(!youtube.signed_in());
        for index in 0..32 {
            let mut updated = cookie("auth-token", ".twitch.tv");
            updated.value = format!("synthetic-rotation-{index}");
            jar.observe(vec![updated]);
        }
        jar.clear();
        // A callback already queued by the closing browser cannot save again.
        jar.observe(vec![cookie("auth-token", ".twitch.tv")]);
        wait(&jar);
        assert!(!jar.signed_in());
        drop(jar);
        let restarted = Jar::new(Provider::Twitch, "123");
        wait(&restarted);
        assert!(!restarted.signed_in());
        assert!(grant.load().unwrap().unwrap() == b"synthetic separate channel grant");
        restarted.observe(vec![cookie("auth-token", ".twitch.tv")]);
        drop(restarted);
        // On Linux the file contains only the marker; secret bytes live in the vault.
        let identity = format!("viewing-session-v1:{}", "123");
        use sha2::{Digest, Sha256};
        let file = crate::addon::config_dir().unwrap().join(format!(
            "twitch-{}.dat",
            hex::encode(Sha256::digest(identity.as_bytes()))
        ));
        assert!(std::fs::read(file).unwrap() == b"BRICK-TWITCH-KEYRING-v1\n");
        crate::local_erasure::reset().unwrap();
        assert!(Store::twitch(&identity).unwrap().load().unwrap().is_none());
        assert!(Store::twitch(&identity)
            .unwrap()
            .save(b"late viewing login")
            .is_err());
    }

    fn cookie(name: &str, domain: &str) -> Cookie {
        Cookie {
            name: name.into(),
            value: "synthetic-session".into(),
            domain: domain.into(),
            path: "/".into(),
            expires: Some(now() + 3600),
            secure: true,
            http_only: true,
            same_site: 1,
        }
    }
    #[test]
    fn only_matching_live_authentication_cookies_change_the_button() {
        let mut cookies = vec![cookie("auth-token", ".twitch.tv")];
        assert!(signed_in(&Provider::Twitch, &cookies, now()));
        assert!(!signed_in(&Provider::Youtube, &cookies, now()));
        cookies[0].domain = ".twitch.tv.evil.test".into();
        assert!(!signed_in(&Provider::Twitch, &cookies, now()));
        let mut cookies = vec![
            cookie("SID", ".youtube.com"),
            cookie("HSID", ".youtube.com"),
        ];
        assert!(signed_in(&Provider::Youtube, &cookies, now()));
        cookies[1].expires = Some(now() - 1);
        assert!(!signed_in(&Provider::Youtube, &cookies, now()));
        assert!(!signed_in(
            &Provider::Youtube,
            &[cookie("VISITOR_INFO1_LIVE", ".youtube.com")],
            now()
        ));
        assert!(!signed_in(
            &Provider::Youtube,
            &[cookie("SID", ".google.com"), cookie("HSID", ".google.com")],
            now()
        ));
    }
    #[test]
    fn saved_sessions_are_bound_to_account_and_provider_and_keep_expiry() {
        let original = vec![cookie("auth-token", ".twitch.tv")];
        let bytes = encode("123", &Provider::Twitch, &original).unwrap();
        let restored = decode("123", &Provider::Twitch, &bytes).unwrap();
        assert!(restored == original);
        assert!(decode("456", &Provider::Twitch, &bytes).is_err());
        assert!(decode("123", &Provider::Youtube, &bytes).is_err());
        let mut session_only = original;
        session_only[0].expires = None;
        assert!(decode(
            "123",
            &Provider::Twitch,
            &encode("123", &Provider::Twitch, &session_only).unwrap()
        )
        .unwrap()
        .is_empty());
    }
}
