//! Personal Twitch identity through Public-client Device Code Flow. Grants
//! stay in the account's OS credential store, never in guild/player requests.
use crate::{credential_store::Store, guild::Access};
use reqwest::blocking::{Client as HttpClient, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const CLIENT_ID: &str = match option_env!("BRICK_TWITCH_CLIENT_ID") {
    Some(id) => id,
    None => "",
};
const MAX_BODY: u64 = 64 * 1024;
const CONNECT_LIMIT: Duration = Duration::from_secs(10 * 60);
const VALIDATE_INTERVAL: Duration = Duration::from_secs(60 * 60);
static STORE_LOCK: Mutex<()> = Mutex::new(());
// Only successful validations in this process may skip another guild's
// startup lookup. Disk timestamps never bypass Twitch's startup validation.
static VALIDATED: OnceLock<Mutex<HashMap<[u8; 32], Instant>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub user_id: String,
    pub login: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    version: u32,
    client_id: String,
    account: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    channel: Option<Channel>,
    reconnect_required: bool,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Vec<String>,
}

/// Revoke the stored access token and discard this device's refresh token.
/// Twitch's public revocation endpoint does not promise project-wide grant
/// removal; the user's Connections page remains authoritative for that.
pub(crate) fn revoke_for_erasure(account: &str) -> Result<(), String> {
    let _guard = lock()?;
    let store = Store::twitch(account)?;
    let Some(bytes) = store.load().map_err(storage_error)? else {
        return Ok(());
    };
    let session: Session = serde_json::from_slice(&bytes)
        .map_err(|_| "Couldn't read the Twitch connection for deletion.")?;
    if session.version != 1
        || session.account != account
        || !valid_client_id(&session.client_id)
        || !credential(&session.access_token)
    {
        return Err("Couldn't verify the Twitch connection for deletion.".into());
    }
    crate::account_erasure::revoke_token(
        "https://id.twitch.tv/oauth2/revoke",
        &[
            ("client_id", session.client_id.as_str()),
            ("token", session.access_token.as_str()),
        ],
        false,
    )?;
    forget_validation(&session);
    store.remove().map_err(storage_error)
}
#[derive(Deserialize)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}
#[derive(Deserialize)]
struct Validation {
    client_id: String,
    login: String,
    user_id: String,
    #[serde(default, deserialize_with = "deserialize_scopes")]
    scopes: Vec<String>,
    expires_in: u64,
}

struct Provider {
    http: HttpClient,
    client_id: String,
    device_url: String,
    token_url: String,
    validate_url: String,
    users_url: String,
}

// An absent/null list carries no scopes, like the empty list. Match Twitch's
// own client's nullable slice decoding without accepting malformed strings,
// numbers, objects, or granting any scope we did not request.
fn deserialize_scopes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Pending,
    SlowDown,
    Retry(u64),
    Denied,
    Expired,
    InvalidGrant,
    Invalid,
    Binding,
}
impl Failure {
    fn message(&self) -> String {
        match self {
            Self::Denied => "Twitch connection was declined.",
            Self::Expired => "Twitch sign-in expired. Try connecting again.",
            Self::InvalidGrant | Self::Binding => "Please reconnect Twitch.",
            Self::Invalid => "Twitch returned an invalid connection. Try again later.",
            _ => "Twitch couldn't be reached. Brick will try again later.",
        }
        .into()
    }
}

/// One worker owns this value. Refreshes additionally serialize against other
/// guild panels and device disconnect through STORE_LOCK.
pub struct Account {
    provider: Provider,
    session: Option<Session>,
}

impl Account {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            provider: Provider::new(CLIENT_ID)?,
            session: None,
        })
    }
    pub fn configured() -> bool {
        valid_client_id(CLIENT_ID)
    }
    pub fn connected(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| !s.reconnect_required && s.channel.is_some())
    }
    pub fn needs_reconnect(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.reconnect_required)
    }
    pub fn check_after(&self) -> Option<Duration> {
        let session = self.session.as_ref().filter(|s| !s.reconnect_required)?;
        let delay = VALIDATED
            .get_or_init(Default::default)
            .lock()
            .ok()
            .and_then(|cache| {
                cache
                    .get(&validation_key(session))
                    .map(|at| VALIDATE_INTERVAL.saturating_sub(at.elapsed()))
            });
        Some(
            delay
                .unwrap_or(Duration::from_secs(1))
                .max(Duration::from_secs(1)),
        )
    }
    pub fn channel(&self) -> Option<&Channel> {
        self.session
            .as_ref()
            .filter(|s| !s.reconnect_required)
            .and_then(|s| s.channel.as_ref())
    }
    pub fn restore(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        if !Self::configured() {
            return Ok(());
        }
        self.session = load_session(access, &self.provider.client_id)?;
        current(access, cancel)?;
        if self.session.is_some() {
            self.check(access, cancel)?;
        }
        Ok(())
    }
    pub fn connect(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        if !Self::configured() {
            return Err("Twitch connection isn't available in this build yet.".into());
        }
        let device = self.provider.device().map_err(|e| e.message())?;
        current(access, cancel)?;
        // Construct the official activation URL ourselves; the provider's
        // response cannot launch a different host, scheme or command.
        let url = activation_url(&device)?;
        crate::browser::open(url.as_str())?;
        let token = self.provider.poll(
            &device,
            || current(access, cancel),
            |duration| wait(duration, || current(access, cancel)),
        )?;
        current(access, cancel)?;
        let session = session_from_token(token, &self.provider.client_id, &access.user_id, None)?;
        save(access, &session, cancel)?;
        self.session = Some(session);
        self.check(access, cancel)
    }
    pub fn disconnect(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        let _guard = lock()?;
        current(access, cancel)?;
        // Forget this device only; revocation could affect another computer.
        store(access)?.remove().map_err(storage_error)?;
        if let Some(session) = &self.session {
            forget_validation(session);
        }
        self.session = None;
        Ok(())
    }
    pub fn check(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        if session.account != access.user_id || session.client_id != self.provider.client_id {
            return Err("Please reconnect Twitch.".into());
        }
        if session.reconnect_required {
            return Err("Please reconnect Twitch.".into());
        }
        if recently_validated(session) {
            return Ok(());
        }
        let first = session.access_token.clone();
        let validation = match self.provider.validate(&first) {
            Ok(value) => value,
            Err(Failure::InvalidGrant) => {
                self.refresh(&first, access, cancel)?;
                current(access, cancel)?;
                let session = self.session.as_ref().ok_or("Please reconnect Twitch.")?;
                match self.provider.validate(&session.access_token) {
                    Ok(value) => value,
                    Err(Failure::InvalidGrant) => {
                        self.reject_current(access, cancel)?;
                        return Err("Please reconnect Twitch.".into());
                    }
                    Err(failure) => return Err(failure.message()),
                }
            }
            Err(failure) => return Err(failure.message()),
        };
        current(access, cancel)?;
        let session = self.session.as_ref().ok_or("Please reconnect Twitch.")?;
        if validate_binding(&validation, session).is_err() {
            diagnostic(CheckStage::ValidateBinding, None, &Failure::Binding);
            self.reject_current(access, cancel)?;
            return Err("Please reconnect Twitch.".into());
        }
        let channel = match self.provider.channel(&session.access_token, &validation) {
            Ok(channel) => channel,
            Err(Failure::InvalidGrant | Failure::Binding) => {
                self.reject_current(access, cancel)?;
                return Err("Please reconnect Twitch.".into());
            }
            Err(failure) => return Err(failure.message()),
        };
        current(access, cancel)?;
        let bearer = session.access_token.clone();
        let saved = transaction(
            || current(access, cancel),
            || load_session(access, &self.provider.client_id),
            |mut saved| {
                if saved.access_token == bearer && !saved.reconnect_required {
                    saved.channel = Some(channel);
                    saved.expires_at = expiry_timestamp(validation.expires_in, now())
                        .map_err(|failure| failure.message())?;
                }
                Ok(saved)
            },
            |session| save_protected(access, session),
        )?;
        if saved.access_token == bearer && !saved.reconnect_required {
            remember_validation(&saved);
        }
        self.session = Some(saved);
        current(access, cancel)
    }
    fn refresh(
        &mut self,
        rejected: &str,
        access: &Access,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let session = transaction(
            || current(access, cancel),
            || load_session(access, &self.provider.client_id),
            |mut saved| {
                // Another panel may already have consumed the one-use refresh
                // token. Always prefer its protected replacement.
                if saved.access_token != rejected || saved.reconnect_required {
                    return Ok(saved);
                }
                let token = match self.provider.refresh(&saved.refresh_token) {
                    Ok(token) => token,
                    Err(Failure::InvalidGrant) => {
                        saved.reconnect_required = true;
                        return Ok(saved);
                    }
                    Err(failure) => return Err(failure.message()),
                };
                session_from_token(
                    token,
                    &self.provider.client_id,
                    &access.user_id,
                    saved.channel,
                )
            },
            |session| save_protected(access, session),
        )?;
        // Persist the rotated token even if cancellation/guild switch arrived
        // during the HTTP exchange. No stale UI or guild operation continues.
        self.session = Some(session);
        current(access, cancel)?;
        if self.needs_reconnect() {
            Err("Please reconnect Twitch.".into())
        } else {
            Ok(())
        }
    }
    fn reject_current(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        let bearer = self
            .session
            .as_ref()
            .ok_or("Please reconnect Twitch.")?
            .access_token
            .clone();
        self.session = Some(transaction(
            || current(access, cancel),
            || load_session(access, &self.provider.client_id),
            |mut saved| {
                if saved.access_token == bearer {
                    forget_validation(&saved);
                    saved.reconnect_required = true;
                }
                Ok(saved)
            },
            |session| save_protected(access, session),
        )?);
        Ok(())
    }
}

impl Provider {
    fn new(client_id: &str) -> Result<Self, String> {
        Ok(Self {
            http: HttpClient::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("Brick Twitch connection")
                .build()
                .map_err(|_| "Couldn't start the Twitch connection.")?,
            client_id: client_id.into(),
            device_url: "https://id.twitch.tv/oauth2/device".into(),
            token_url: "https://id.twitch.tv/oauth2/token".into(),
            validate_url: "https://id.twitch.tv/oauth2/validate".into(),
            users_url: "https://api.twitch.tv/helix/users".into(),
        })
    }
    fn device(&self) -> Result<Device, Failure> {
        let response = self
            .http
            .post(&self.device_url)
            .form(&[("client_id", self.client_id.as_str()), ("scopes", "")])
            .send()
            .map_err(|_| Failure::Retry(0))?;
        decode(response)
    }
    fn poll(
        &self,
        device: &Device,
        check: impl Fn() -> Result<(), String>,
        mut pause: impl FnMut(Duration) -> Result<(), String>,
    ) -> Result<Token, String> {
        let limit = Duration::from_secs(device.expires_in).min(CONNECT_LIMIT);
        let deadline = Instant::now() + limit;
        let mut schedule = PollSchedule::new(device.interval)?;
        // Wall time and count both bound the attempt, even with bad clocks or
        // a provider repeatedly returning authorization_pending.
        for _ in 0..120 {
            check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if schedule.delay >= remaining {
                return Err(Failure::Expired.message());
            }
            pause(schedule.delay)?;
            check()?;
            if Instant::now() >= deadline {
                return Err(Failure::Expired.message());
            }
            let result = self
                .http
                .post(&self.token_url)
                .timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(20)),
                )
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("scopes", ""),
                    ("device_code", device.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .map_err(|_| Failure::Retry(0))
                .and_then(decode::<Token>);
            check()?;
            match result {
                Ok(token) => return Ok(token),
                Err(failure) => schedule.next(failure).map_err(|e| e.message())?,
            }
        }
        Err(Failure::Expired.message())
    }
    fn refresh(&self, refresh: &str) -> Result<Token, Failure> {
        decode(
            self.http
                .post(&self.token_url)
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                ])
                .send()
                .map_err(|_| Failure::Retry(0))?,
        )
    }
    fn validate(&self, bearer: &str) -> Result<Validation, Failure> {
        decode_check(
            self.http
                .get(&self.validate_url)
                .bearer_auth(bearer)
                .send()
                .map_err(|_| {
                    diagnostic(CheckStage::ValidateRequest, None, &Failure::Retry(0));
                    Failure::Retry(0)
                })?,
            CheckStage::ValidateResponse,
        )
    }
    fn channel(&self, bearer: &str, validation: &Validation) -> Result<Channel, Failure> {
        let body: serde_json::Value = decode_check(
            self.http
                .get(&self.users_url)
                .bearer_auth(bearer)
                .header("Client-Id", &self.client_id)
                .send()
                .map_err(|_| {
                    diagnostic(CheckStage::UserRequest, None, &Failure::Retry(0));
                    Failure::Retry(0)
                })?,
            CheckStage::UserResponse,
        )?;
        parse_channel(&body, validation)
            .inspect_err(|failure| diagnostic(CheckStage::UserIdentity, Some(200), failure))
    }
}

struct PollSchedule {
    delay: Duration,
    failures: u8,
}
impl PollSchedule {
    fn new(interval: u64) -> Result<Self, String> {
        if !(1..=1800).contains(&interval) {
            return Err(Failure::Invalid.message());
        }
        Ok(Self {
            delay: Duration::from_secs(interval.max(5)),
            failures: 0,
        })
    }
    fn next(&mut self, failure: Failure) -> Result<(), Failure> {
        match failure {
            Failure::Pending => self.failures = 0,
            Failure::SlowDown => {
                self.failures = 0;
                self.delay += Duration::from_secs(5);
            }
            Failure::Retry(after) => {
                self.failures += 1;
                if self.failures >= 5 {
                    return Err(Failure::Retry(after));
                }
                self.delay = self
                    .delay
                    .max(Duration::from_secs(after))
                    .max(Duration::from_secs(5 * (1 << self.failures)));
            }
            other => return Err(other),
        }
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, Failure> {
    let status = response.status().as_u16();
    let retry = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    if response.content_length().is_some_and(|n| n > MAX_BODY) {
        return Err(Failure::Invalid);
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::Retry(0))?;
    if bytes.len() as u64 > MAX_BODY {
        return Err(Failure::Invalid);
    }
    if !(200..300).contains(&status) {
        return Err(response_failure(status, &bytes, retry));
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)
}
#[derive(Clone, Copy)]
enum CheckStage {
    ValidateRequest,
    ValidateResponse,
    ValidateBinding,
    UserRequest,
    UserResponse,
    UserIdentity,
}
fn decode_check<T: serde::de::DeserializeOwned>(
    response: Response,
    stage: CheckStage,
) -> Result<T, Failure> {
    let status = response.status().as_u16();
    decode(response).inspect_err(|failure| diagnostic(stage, Some(status), failure))
}
fn diagnostic(stage: CheckStage, status: Option<u16>, failure: &Failure) {
    // Opt-in local troubleshooting only. Never include provider bodies,
    // credential-bearing headers, OAuth codes, tokens or account identities.
    if std::env::var_os("BRICK_PROVIDER_DIAGNOSTICS").is_some_and(|value| value == "1") {
        eprintln!("{}", diagnostic_line(stage, status, failure));
    }
}
fn diagnostic_line(stage: CheckStage, status: Option<u16>, failure: &Failure) -> String {
    let stage = match stage {
        CheckStage::ValidateRequest => "validate_request",
        CheckStage::ValidateResponse => "validate_response",
        CheckStage::ValidateBinding => "validate_binding",
        CheckStage::UserRequest => "users_request",
        CheckStage::UserResponse => "users_response",
        CheckStage::UserIdentity => "users_identity",
    };
    let category = match failure {
        Failure::Pending => "pending",
        Failure::SlowDown => "slow_down",
        Failure::Retry(_) => "temporary",
        Failure::Denied => "denied",
        Failure::Expired => "expired",
        Failure::InvalidGrant => "invalid_grant",
        Failure::Invalid => "invalid_response",
        Failure::Binding => "account_binding",
    };
    format!(
        "Brick Twitch check: stage={stage} status={} failure={category}",
        status.map_or_else(|| "none".into(), |value| value.to_string())
    )
}
fn response_failure(status: u16, bytes: &[u8], retry: u64) -> Failure {
    if status == 429 || status >= 500 {
        return Failure::Retry(retry);
    }
    if status == 401 {
        return Failure::InvalidGrant;
    }
    if status == 403 {
        return Failure::Denied;
    }
    if status == 400 {
        let body = serde_json::from_slice::<serde_json::Value>(bytes).unwrap_or_default();
        // Twitch sometimes pairs error: "Bad Request" with the OAuth reason
        // in message. Only recognized protocol codes affect grant state.
        for code in [body["error"].as_str(), body["message"].as_str()]
            .into_iter()
            .flatten()
        {
            let failure = match code {
                "authorization_pending" => Failure::Pending,
                "slow_down" => Failure::SlowDown,
                "access_denied" | "authorization_declined" => Failure::Denied,
                "expired_token" | "invalid device code" => Failure::Expired,
                "invalid_grant" | "Invalid refresh token" | "invalid refresh token" => {
                    Failure::InvalidGrant
                }
                _ => continue,
            };
            return failure;
        }
    }
    Failure::Invalid
}
fn activation_url(device: &Device) -> Result<Url, String> {
    let supplied = Url::parse(&device.verification_uri).map_err(|_| Failure::Invalid.message())?;
    if supplied.scheme() != "https"
        || supplied.host_str() != Some("www.twitch.tv")
        || supplied.path() != "/activate"
        || supplied.port().is_some()
        || !supplied.username().is_empty()
        || supplied.password().is_some()
        || supplied.fragment().is_some()
        || !(1..=1800).contains(&device.expires_in)
        || !(1..=1800).contains(&device.interval)
        || !(4..=32).contains(&device.user_code.len())
        || !device
            .user_code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !credential(&device.device_code)
    {
        return Err(Failure::Invalid.message());
    }
    let mut url = Url::parse("https://www.twitch.tv/activate").unwrap();
    url.query_pairs_mut()
        .extend_pairs([("public", "true"), ("device-code", &device.user_code)]);
    Ok(url)
}
fn validate_binding(value: &Validation, session: &Session) -> Result<(), Failure> {
    if value.client_id != session.client_id
        || !digits(&value.user_id)
        || !login(&value.login)
        || !value.scopes.is_empty()
        || expiry_timestamp(value.expires_in, now()).is_err()
        || session
            .channel
            .as_ref()
            .is_some_and(|c| c.user_id != value.user_id)
    {
        Err(Failure::Binding)
    } else {
        Ok(())
    }
}
fn parse_channel(body: &serde_json::Value, validation: &Validation) -> Result<Channel, Failure> {
    let rows = body["data"]
        .as_array()
        .filter(|r| r.len() == 1)
        .ok_or(Failure::Invalid)?;
    let row = &rows[0];
    let id = row["id"].as_str().ok_or(Failure::Invalid)?;
    let name = row["login"].as_str().ok_or(Failure::Invalid)?;
    let title = row["display_name"].as_str().ok_or(Failure::Invalid)?;
    if id != validation.user_id || name != validation.login {
        return Err(Failure::Binding);
    }
    let channel = Channel {
        user_id: id.into(),
        login: name.into(),
        title: title.into(),
        url: format!("https://www.twitch.tv/{name}"),
    };
    if valid_channel(&channel) {
        Ok(channel)
    } else {
        Err(Failure::Invalid)
    }
}
fn valid_channel(channel: &Channel) -> bool {
    digits(&channel.user_id)
        && login(&channel.login)
        && !channel.title.is_empty()
        && channel.title.chars().count() <= 100
        && !channel.title.chars().any(char::is_control)
        && channel.url == format!("https://www.twitch.tv/{}", channel.login)
}
fn valid_client_id(value: &str) -> bool {
    (10..=128).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn digits(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
}
fn login(value: &str) -> bool {
    (1..=25).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn credential(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn current(access: &Access, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Twitch connection cancelled.".into());
    }
    access.check()
}
fn wait(duration: Duration, check: impl Fn() -> Result<(), String>) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    loop {
        check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}
fn session_from_token(
    token: Token,
    client: &str,
    account: &str,
    channel: Option<Channel>,
) -> Result<Session, String> {
    if !token.token_type.eq_ignore_ascii_case("bearer")
        || !token.scope.is_empty()
        || !credential(&token.access_token)
        || !credential(&token.refresh_token)
    {
        return Err(Failure::Invalid.message());
    }
    Ok(Session {
        version: 1,
        client_id: client.into(),
        account: account.into(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: expiry_timestamp(token.expires_in, now())
            .map_err(|failure| failure.message())?,
        channel,
        reconnect_required: false,
    })
}
fn expiry_timestamp(seconds: u64, at: u64) -> Result<u64, Failure> {
    if seconds == 0 {
        return Err(Failure::Invalid);
    }
    at.checked_add(seconds).ok_or(Failure::Invalid)
}
fn store(access: &Access) -> Result<Store, String> {
    if !digits(&access.user_id) {
        return Err("Invalid Brick account.".into());
    }
    Store::twitch(&access.user_id)
}
fn storage_error(message: String) -> String {
    message.replace("Warcraft Logs", "Twitch")
}
fn load_session(access: &Access, client: &str) -> Result<Option<Session>, String> {
    let Some(bytes) = store(access)?.load().map_err(storage_error)? else {
        return Ok(None);
    };
    let session: Session =
        serde_json::from_slice(&bytes).map_err(|_| "Please reconnect Twitch.")?;
    if session.version != 1
        || session.account != access.user_id
        || session.client_id != client
        || !credential(&session.access_token)
        || !credential(&session.refresh_token)
        || session
            .channel
            .as_ref()
            .is_some_and(|channel| !valid_channel(channel))
    {
        return Err("Please reconnect Twitch.".into());
    }
    Ok(Some(session))
}
fn lock() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    STORE_LOCK
        .lock()
        .map_err(|_| "Twitch credential storage is unavailable.".into())
}
fn save_protected(access: &Access, session: &Session) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(session).map_err(|_| "Couldn't protect the Twitch connection.")?;
    store(access)?.save(&bytes).map_err(storage_error)
}
fn save(access: &Access, session: &Session, cancel: &AtomicBool) -> Result<(), String> {
    let _guard = lock()?;
    current(access, cancel)?;
    save_protected(access, session)
}
fn transaction(
    check: impl FnOnce() -> Result<(), String>,
    load: impl FnOnce() -> Result<Option<Session>, String>,
    update: impl FnOnce(Session) -> Result<Session, String>,
    persist: impl FnOnce(&Session) -> Result<(), String>,
) -> Result<Session, String> {
    let _guard = lock()?;
    check()?;
    let next = update(load()?.ok_or("Please reconnect Twitch.")?)?;
    persist(&next)?;
    Ok(next)
}
fn validation_key(session: &Session) -> [u8; 32] {
    Sha256::digest(format!(
        "{}:{}:{}",
        session.account, session.client_id, session.access_token
    ))
    .into()
}
fn recently_validated(session: &Session) -> bool {
    session.channel.is_some()
        && !session.reconnect_required
        && session.expires_at > now()
        && VALIDATED
            .get_or_init(Default::default)
            .lock()
            .is_ok_and(|cache| {
                cache
                    .get(&validation_key(session))
                    .is_some_and(|at| at.elapsed() < VALIDATE_INTERVAL)
            })
}
fn remember_validation(session: &Session) {
    if let Ok(mut cache) = VALIDATED.get_or_init(Default::default).lock() {
        cache.retain(|_, at| at.elapsed() < VALIDATE_INTERVAL);
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.insert(validation_key(session), Instant::now());
    }
}
fn forget_validation(session: &Session) {
    if let Ok(mut cache) = VALIDATED.get_or_init(Default::default).lock() {
        cache.remove(&validation_key(session));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        cell::{Cell, RefCell},
        io::Write,
        net::TcpListener,
        sync::mpsc,
    };

    const TEST_CLIENT: &str = "fixtureclient1234567890";
    fn token() -> Token {
        Token {
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            token_type: "bearer".into(),
            expires_in: 14_400,
            scope: vec![],
        }
    }
    fn channel() -> Channel {
        Channel {
            user_id: "42".into(),
            login: "fixture".into(),
            title: "Fixture".into(),
            url: "https://www.twitch.tv/fixture".into(),
        }
    }
    fn session() -> Session {
        session_from_token(token(), TEST_CLIENT, "123", Some(channel())).unwrap()
    }
    fn validation() -> Validation {
        Validation {
            client_id: TEST_CLIENT.into(),
            login: "fixture".into(),
            user_id: "42".into(),
            scopes: vec![],
            expires_in: 14_400,
        }
    }
    fn device() -> Device {
        Device {
            device_code: "fixture-device".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://www.twitch.tv/activate?public=true&device-code=ABCD-EFGH"
                .into(),
            expires_in: 1800,
            interval: 5,
        }
    }
    fn fixture(
        responses: Vec<(u16, String)>,
    ) -> (
        Provider,
        mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for (status, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("fixture accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 64 * 1024);
                    if let Some(split) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                tx.send(String::from_utf8(bytes).unwrap()).unwrap();
                write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut provider = Provider::new(TEST_CLIENT).unwrap();
        provider.http = HttpClient::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        provider.device_url = format!("{base}/device");
        provider.token_url = format!("{base}/token");
        provider.validate_url = format!("{base}/validate");
        provider.users_url = format!("{base}/users");
        (provider, rx, worker)
    }
    fn token_json() -> String {
        json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","token_type":"bearer","expires_in":14400,"scope":[]}).to_string()
    }

    #[test]
    fn device_authorization_requests_no_scopes_or_secret_and_polls_with_backoff() {
        let device_json = json!({"device_code":"fixture-device","user_code":"ABCD-EFGH","verification_uri":"https://www.twitch.tv/activate","expires_in":1800,"interval":5}).to_string();
        let (provider, requests, worker) = fixture(vec![
            (200, device_json),
            (400, json!({"message":"authorization_pending"}).to_string()),
            (400, json!({"error":"slow_down"}).to_string()),
            (200, token_json()),
        ]);
        let device = provider.device().unwrap();
        let pauses = RefCell::new(Vec::new());
        let token = provider
            .poll(
                &device,
                || Ok(()),
                |delay| {
                    pauses.borrow_mut().push(delay.as_secs());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(token.access_token, "fixture-access");
        assert_eq!(*pauses.borrow(), [5, 5, 10]);
        worker.join().unwrap();
        let requests: Vec<_> = requests.try_iter().collect();
        assert_eq!(requests.len(), 4);
        for request in &requests {
            assert!(request.contains("scopes="));
            assert!(!request.contains("client_secret"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            assert!(!request.contains("fixture-access"));
        }
        assert!(requests[0].starts_with("POST /device "));
        assert!(requests[1].contains("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    }
    #[test]
    fn public_refresh_and_own_identity_use_only_fixed_endpoints_and_bound_identity() {
        let valid = json!({"client_id":TEST_CLIENT,"login":"fixture","user_id":"42","scopes":[],"expires_in":14400}).to_string();
        let own =
            json!({"data":[{"id":"42","login":"fixture","display_name":"Fixture"}]}).to_string();
        let (provider, requests, worker) =
            fixture(vec![(200, token_json()), (200, valid), (200, own)]);
        provider.refresh("fixture-refresh&literal").unwrap();
        let validation = provider.validate("fixture-access").unwrap();
        assert_eq!(
            provider.channel("fixture-access", &validation).unwrap(),
            channel()
        );
        worker.join().unwrap();
        let requests: Vec<_> = requests.try_iter().collect();
        assert!(requests[0].contains("refresh_token=fixture-refresh%26literal"));
        assert!(!requests[0].contains("client_secret"));
        assert!(requests[1].starts_with("GET /validate HTTP"));
        assert!(requests[2].starts_with("GET /users HTTP"));
        assert!(requests[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-access"));
        assert!(requests[2]
            .to_ascii_lowercase()
            .contains(&format!("client-id: {TEST_CLIENT}")));
    }
    #[test]
    fn empty_scope_validation_lists_can_be_null_or_absent() {
        for scopes in [Some(serde_json::Value::Null), Some(json!([])), None] {
            let mut validation_body = json!({"client_id":TEST_CLIENT,"login":"fixture","user_id":"42","expires_in":14400});
            if let Some(scopes) = scopes {
                validation_body["scopes"] = scopes;
            }
            let (provider, _requests, worker) = fixture(vec![
                (200, token_json()),
                (200, validation_body.to_string()),
                (
                    200,
                    json!({"data":[{"id":"42","login":"fixture","display_name":"Fixture"}]})
                        .to_string(),
                ),
            ]);
            let received = provider.poll(&device(), || Ok(()), |_| Ok(())).unwrap();
            let saved = session_from_token(received, TEST_CLIENT, "123", None).unwrap();
            let validation = provider.validate(&saved.access_token).unwrap();
            assert!(validate_binding(&validation, &saved).is_ok());
            assert_eq!(
                provider.channel(&saved.access_token, &validation).unwrap(),
                channel()
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn scope_normalization_never_accepts_malformed_or_extra_permissions() {
        for scopes in [
            json!(""),
            json!("user:read:email"),
            json!(42),
            json!({}),
            json!([null]),
        ] {
            let mut body: serde_json::Value = serde_json::from_str(&token_json()).unwrap();
            body["scope"] = scopes.clone();
            assert!(serde_json::from_value::<Token>(body).is_err());
            let body = json!({"client_id":TEST_CLIENT,"login":"fixture","user_id":"42","scopes":scopes,"expires_in":14400});
            assert!(serde_json::from_value::<Validation>(body).is_err());
        }
        let mut body: serde_json::Value = serde_json::from_str(&token_json()).unwrap();
        body["scope"] = json!(["user:read:email"]);
        assert!(session_from_token(
            serde_json::from_value(body).unwrap(),
            TEST_CLIENT,
            "123",
            None
        )
        .is_err());
        let body = json!({"client_id":TEST_CLIENT,"login":"fixture","user_id":"42","scopes":["user:read:email"],"expires_in":14400});
        assert!(validate_binding(&serde_json::from_value(body).unwrap(), &session()).is_err());
    }
    #[test]
    fn documented_long_lifetime_is_valid_but_does_not_extend_hourly_validation() {
        // https://dev.twitch.tv/docs/authentication/validate-tokens/
        const DOCUMENTED_LIFETIME: u64 = 5_520_838;
        let mut token = token();
        token.access_token = uuid::Uuid::new_v4().to_string();
        token.expires_in = DOCUMENTED_LIFETIME;
        let saved = session_from_token(token, TEST_CLIENT, "123", Some(channel())).unwrap();
        assert!(saved.expires_at > now() + 7 * 86_400);
        let mut validated = validation();
        validated.expires_in = DOCUMENTED_LIFETIME;
        assert!(validate_binding(&validated, &saved).is_ok());
        remember_validation(&saved);
        let account = Account {
            provider: Provider::new(TEST_CLIENT).unwrap(),
            session: Some(saved.clone()),
        };
        let delay = account.check_after().unwrap();
        assert!(delay <= Duration::from_secs(3600));
        assert!(delay > Duration::from_secs(3590));
        forget_validation(&saved);
        validated.client_id = "other-client".into();
        assert!(validate_binding(&validated, &saved).is_err());
        validated.client_id = TEST_CLIENT.into();
        validated.user_id = "999".into();
        assert!(validate_binding(&validated, &saved).is_err());
        validated.user_id = "42".into();
        validated.scopes.push("user:read:email".into());
        assert!(validate_binding(&validated, &saved).is_err());
    }
    #[test]
    fn zero_negative_and_overflowing_lifetimes_are_rejected() {
        assert_eq!(expiry_timestamp(0, 100), Err(Failure::Invalid));
        assert_eq!(expiry_timestamp(1, u64::MAX), Err(Failure::Invalid));
        assert_eq!(expiry_timestamp(u64::MAX, 1), Err(Failure::Invalid));
        assert_eq!(expiry_timestamp(1, 100), Ok(101));
        for seconds in [0, u64::MAX] {
            let mut token = token();
            token.expires_in = seconds;
            assert!(session_from_token(token, TEST_CLIENT, "123", None).is_err());
            let mut validated = validation();
            validated.expires_in = seconds;
            assert!(validate_binding(&validated, &session()).is_err());
        }
        let mut body: serde_json::Value = serde_json::from_str(&token_json()).unwrap();
        body["expires_in"] = json!(-1);
        assert!(serde_json::from_value::<Token>(body).is_err());
        let body = json!({"client_id":TEST_CLIENT,"login":"fixture","user_id":"42","scopes":[],"expires_in":-1});
        assert!(serde_json::from_value::<Validation>(body).is_err());
    }
    #[test]
    fn local_diagnostic_contains_only_fixed_stage_status_and_failure_category() {
        assert_eq!(
            diagnostic_line(CheckStage::ValidateResponse, Some(200), &Failure::Invalid),
            "Brick Twitch check: stage=validate_response status=200 failure=invalid_response"
        );
        assert_eq!(
            diagnostic_line(CheckStage::UserRequest, None, &Failure::Retry(u64::MAX)),
            "Brick Twitch check: stage=users_request status=none failure=temporary"
        );
    }
    #[test]
    fn malformed_oversized_and_redirect_responses_are_not_credentials() {
        let (provider, _requests, worker) = fixture(vec![
            (200, "{".into()),
            (200, "x".repeat(MAX_BODY as usize + 1)),
            (302, token_json()),
        ]);
        assert!(matches!(provider.refresh("fixture"), Err(Failure::Invalid)));
        assert!(matches!(provider.refresh("fixture"), Err(Failure::Invalid)));
        assert!(matches!(provider.refresh("fixture"), Err(Failure::Invalid)));
        worker.join().unwrap();
    }
    #[test]
    fn activation_never_opens_a_provider_controlled_destination() {
        let mut device = device();
        assert_eq!(
            activation_url(&device).unwrap().as_str(),
            "https://www.twitch.tv/activate?public=true&device-code=ABCD-EFGH"
        );
        for url in [
            "http://www.twitch.tv/activate",
            "https://evil.test/activate",
            "https://www.twitch.tv.evil.test/activate",
            "https://owner@www.twitch.tv/activate",
            "https://www.twitch.tv/activate#evil",
            "https://www.twitch.tv:8443/activate",
            "file:///activate",
            "https://www.twitch.tv/login",
        ] {
            device.verification_uri = url.into();
            assert!(activation_url(&device).is_err());
        }
        device = self::device();
        device.user_code = "code&redirect=evil".into();
        assert!(activation_url(&device).is_err());
    }
    #[test]
    fn slow_down_retry_after_and_failure_cutoff_are_bounded() {
        let mut schedule = PollSchedule::new(1).unwrap();
        assert_eq!(schedule.delay.as_secs(), 5);
        schedule.next(Failure::SlowDown).unwrap();
        assert_eq!(schedule.delay.as_secs(), 10);
        schedule.next(Failure::Retry(120)).unwrap();
        assert_eq!(schedule.delay.as_secs(), 120);
        for _ in 0..3 {
            schedule.next(Failure::Retry(0)).unwrap();
        }
        assert!(schedule.next(Failure::Retry(0)).is_err());
        assert!(PollSchedule::new(0).is_err());
        assert!(PollSchedule::new(1801).is_err());
        assert_eq!(response_failure(429, b"{}", 240), Failure::Retry(240));
        assert_eq!(
            response_failure(503, b"{\"message\":\"Invalid refresh token\"}", 60),
            Failure::Retry(60)
        );
        assert_eq!(
            response_failure(400, b"{\"message\":\"Invalid refresh token\"}", 0),
            Failure::InvalidGrant
        );
        assert_eq!(
            response_failure(
                400,
                br#"{"error":"Bad Request","status":400,"message":"Invalid refresh token"}"#,
                0
            ),
            Failure::InvalidGrant
        );
        assert_eq!(
            response_failure(400, b"{\"message\":\"access_denied\"}", 0),
            Failure::Denied
        );
    }
    #[test]
    fn cancellation_and_short_expiry_prevent_token_requests() {
        let provider = Provider::new(TEST_CLIENT).unwrap();
        let waits = Cell::new(0);
        assert!(provider
            .poll(
                &device(),
                || Err("cancelled".into()),
                |_| {
                    waits.set(waits.get() + 1);
                    Ok(())
                }
            )
            .is_err());
        let mut device = device();
        device.expires_in = 1;
        assert!(provider
            .poll(
                &device,
                || Ok(()),
                |_| {
                    waits.set(waits.get() + 1);
                    Ok(())
                }
            )
            .is_err());
        assert_eq!(waits.get(), 0);
        let cancelled = Cell::new(false);
        assert!(provider
            .poll(
                &self::device(),
                || if cancelled.get() {
                    Err("cancelled".into())
                } else {
                    Ok(())
                },
                |_| {
                    cancelled.set(true);
                    Ok(())
                }
            )
            .is_err());
        let at = Instant::now();
        assert!(wait(Duration::from_secs(60), || Err("cancelled".into())).is_err());
        assert!(at.elapsed() < Duration::from_secs(1));
        let stale = Access::new(
            "fixture".into(),
            crate::guild::ADVANCE.into(),
            "123".into(),
            crate::guild::generation().wrapping_sub(1),
        );
        assert!(current(&stale, &AtomicBool::new(false)).is_err());
        let mut account = Account {
            provider,
            session: Some(session()),
        };
        let other = Access::new(
            "fixture".into(),
            crate::guild::ADVANCE.into(),
            "456".into(),
            crate::guild::generation(),
        );
        assert!(account.check(&other, &AtomicBool::new(false)).is_err());
    }
    #[test]
    fn binding_rejects_other_clients_users_scopes_and_malformed_channels() {
        let session = session();
        assert!(validate_binding(&validation(), &session).is_ok());
        let mut value = validation();
        value.client_id = "other-client".into();
        assert_eq!(validate_binding(&value, &session), Err(Failure::Binding));
        value = validation();
        value.user_id = "999".into();
        assert_eq!(validate_binding(&value, &session), Err(Failure::Binding));
        value = validation();
        value.scopes.push("user:read:email".into());
        assert_eq!(validate_binding(&value, &session), Err(Failure::Binding));
        let value = validation();
        assert_eq!(
            parse_channel(
                &json!({"data":[{"id":"999","login":"fixture","display_name":"Fixture"}]}),
                &value
            ),
            Err(Failure::Binding)
        );
        assert!(parse_channel(&json!({"data":[]}), &value).is_err());
        let mut channel = channel();
        channel.url = "https://evil.test".into();
        assert!(!valid_channel(&channel));
        channel = self::channel();
        channel.title = "bad\nname".into();
        assert!(!valid_channel(&channel));
        let mut token = token();
        token.scope.push("chat:edit".into());
        assert!(session_from_token(token, TEST_CLIENT, "123", None).is_err());
    }
    #[test]
    fn rotation_survives_mid_request_cancellation_but_transient_failure_preserves_store() {
        let saved = RefCell::new(session());
        let cancelled = Cell::new(false);
        transaction(
            || Ok(()),
            || Ok(Some(saved.borrow().clone())),
            |mut value| {
                value.access_token = "rotated-access".into();
                value.refresh_token = "rotated-refresh".into();
                cancelled.set(true);
                Ok(value)
            },
            |next| {
                *saved.borrow_mut() = next.clone();
                Ok(())
            },
        )
        .unwrap();
        assert!(cancelled.get());
        assert_eq!(saved.borrow().refresh_token, "rotated-refresh");
        let writes = Cell::new(0);
        assert!(transaction(
            || Ok(()),
            || Ok(Some(saved.borrow().clone())),
            |_| Err("temporary".into()),
            |_| {
                writes.set(writes.get() + 1);
                Ok(())
            }
        )
        .is_err());
        assert_eq!(writes.get(), 0);
        assert_eq!(saved.borrow().refresh_token, "rotated-refresh");
        assert!(transaction(
            || Err("stale guild".into()),
            || panic!("stale load"),
            |_| panic!("stale refresh"),
            |_| panic!("stale save")
        )
        .is_err());
    }
    #[test]
    fn validation_cache_is_process_local_and_bound_to_account_client_and_token() {
        let mut session = session();
        session.access_token = uuid::Uuid::new_v4().to_string();
        assert!(!recently_validated(&session));
        remember_validation(&session);
        assert!(recently_validated(&session));
        for other in ["account", "client", "token"] {
            let mut copy = session.clone();
            match other {
                "account" => copy.account = "456".into(),
                "client" => copy.client_id = "differentclient123".into(),
                _ => copy.access_token = "different-token".into(),
            }
            assert!(!recently_validated(&copy));
        }
        forget_validation(&session);
        assert!(!recently_validated(&session));
    }
    #[test]
    fn absent_or_revoked_sessions_do_not_schedule_background_polling() {
        let mut account = Account {
            provider: Provider::new(TEST_CLIENT).unwrap(),
            session: None,
        };
        assert!(account.check_after().is_none());
        let mut saved = session();
        saved.access_token = uuid::Uuid::new_v4().to_string();
        account.session = Some(saved.clone());
        assert_eq!(account.check_after(), Some(Duration::from_secs(1)));
        remember_validation(&saved);
        assert!(account.check_after().unwrap() > Duration::from_secs(3590));
        saved.reconnect_required = true;
        account.session = Some(saved.clone());
        assert!(account.check_after().is_none());
        forget_validation(&saved);
    }
}
