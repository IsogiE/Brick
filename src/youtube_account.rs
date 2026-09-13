//! Optional read-only YouTube discovery. OAuth credentials never enter the
//! player, guild requests, URLs, logs, or the VPS.
use std::{
    collections::HashSet,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::blocking::{Client as HttpClient, Response};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{credential_store::Store, guild::Access};

const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";
const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/youtube/v3/";
const MAX_BODY: u64 = 1024 * 1024;
static STORE_LOCK: Mutex<()> = Mutex::new(());
const CLIENT_ID: &str = match option_env!("BRICK_YOUTUBE_CLIENT_ID") {
    Some(value) => value,
    None => "",
};
// Google requires this companion value for some Desktop clients. Installed
// apps cannot keep it confidential: it is application configuration, never a
// replacement for PKCE or the user's separately protected OAuth grant.
const CLIENT_SECRET: &str = match option_env!("BRICK_YOUTUBE_CLIENT_SECRET") {
    Some(value) => value,
    None => "",
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub channel_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    version: u32,
    client_id: String,
    account: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    channels_checked_at: u64,
    #[serde(default)]
    reconnect_required: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

/// Moved into exactly one UI worker at a time. No shared mutable token cache;
/// dropping the owning UI cancels its worker before any subsequent save/share.
pub struct Account {
    http: HttpClient,
    session: Option<Session>,
    channels: Vec<Channel>,
}

impl Account {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            http: HttpClient::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(25))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("Brick YouTube connection")
                .build()
                .map_err(|_| "Couldn't start the YouTube connection.")?,
            session: None,
            channels: Vec::new(),
        })
    }

    pub fn configured() -> bool {
        valid_client_id(CLIENT_ID) && valid_client_companion(CLIENT_SECRET)
    }
    pub fn connected(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| !session.reconnect_required)
    }
    pub fn needs_reconnect(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.reconnect_required)
    }
    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn restore(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        if !Self::configured() {
            return Ok(());
        }
        let Some(session) = load_session(access)? else {
            return Ok(());
        };
        current(access, cancel)?;
        let fresh = session.channels_checked_at <= now()
            && now().saturating_sub(session.channels_checked_at) < 86_400;
        self.channels = session.channels.clone();
        self.session = Some(session);
        if self.needs_reconnect() {
            Err("Please reconnect YouTube.".into())
        } else if fresh {
            Ok(())
        } else {
            self.load_channels(access, cancel)
        }
    }

    pub fn connect(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        if !Self::configured() {
            return Err("YouTube connection isn't available in this build yet.".into());
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "Couldn't open the local YouTube sign-in callback.")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "Couldn't start YouTube sign-in.")?;
        let address = listener
            .local_addr()
            .map_err(|_| "Couldn't start YouTube sign-in.")?;
        let redirect = format!("http://{address}/");
        let state = random()?;
        let verifier = random()?;
        let authorize = authorize_url(CLIENT_ID, &redirect, &state, &verifier)?;
        crate::browser::open(authorize.as_str())?;
        let code = receive_code(&listener, &state, &address.to_string(), || {
            current(access, cancel)
        })?;
        current(access, cancel)?;
        let response = self
            .http
            .post(TOKEN)
            .form(&[
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("redirect_uri", redirect.as_str()),
            ])
            .send()
            .map_err(|_| "YouTube sign-in couldn't finish. Try again.")?;
        let token = token_response(response).map_err(|failure| failure.message)?;
        let session = session_from_token(token, &access.user_id, None, now())?;
        // Save a newly issued refresh token before any fallible channel lookup.
        current(access, cancel)?;
        save(access, &session, cancel)?;
        self.session = Some(session);
        self.channels.clear();
        self.load_channels(access, cancel)
    }

    pub fn disconnect(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        current(access, cancel)?;
        // This action forgets only this device. Provider-wide revocation can
        // invalidate the user's connections on their other computers too.
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "YouTube credential storage is unavailable.")?;
        current(access, cancel)?;
        store(access)?.remove().map_err(storage_error)?;
        self.session = None;
        self.channels.clear();
        Ok(())
    }

    fn bearer(&mut self, access: &Access, cancel: &AtomicBool) -> Result<String, String> {
        current(access, cancel)?;
        let session = self
            .session
            .as_ref()
            .ok_or("Connect YouTube to choose your channel.")?;
        if session.account != access.user_id
            || session.client_id != CLIENT_ID
            || session.reconnect_required
        {
            return Err("Please reconnect YouTube.".into());
        }
        if session.expires_at > now().saturating_add(60) {
            return Ok(session.access_token.clone());
        }
        let session = session_transaction(
            || current(access, cancel),
            || load_session(access),
            |mut saved| {
                // A previous guild's in-flight refresh may have already rotated
                // the grant. The protected store, not this UI's cache, wins.
                if saved.reconnect_required || saved.expires_at > now().saturating_add(60) {
                    return Ok(saved);
                }
                let Some(refresh) = saved.refresh_token.as_ref() else {
                    saved.reconnect_required = true;
                    return Ok(saved);
                };
                let response = self
                    .http
                    .post(TOKEN)
                    .form(&[
                        ("client_id", CLIENT_ID),
                        ("client_secret", CLIENT_SECRET),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh.as_str()),
                    ])
                    .send()
                    .map_err(|_| "YouTube couldn't reconnect. It will try again later.")?;
                let token = match token_response(response) {
                    Ok(token) => token,
                    Err(failure) if failure.invalid_grant => {
                        saved.reconnect_required = true;
                        return Ok(saved);
                    }
                    Err(failure) => return Err(failure.message),
                };
                let mut refreshed =
                    session_from_token(token, &access.user_id, Some(refresh.clone()), now())?;
                refreshed.channels = saved.channels;
                refreshed.channels_checked_at = saved.channels_checked_at;
                Ok(refreshed)
            },
            |session| save_protected(access, session),
        )?;
        let bearer = session.access_token.clone();
        self.session = Some(session);
        // The rotation is saved even if the guild changed during the network
        // call, but no response/share may continue into that obsolete panel.
        current(access, cancel)?;
        if self.needs_reconnect() {
            return Err("Please reconnect YouTube.".into());
        }
        Ok(bearer)
    }

    fn get(
        &mut self,
        endpoint: &str,
        query: &[(&str, &str)],
        access: &Access,
        cancel: &AtomicBool,
    ) -> Result<serde_json::Value, String> {
        // No provider-controlled pagination URL can receive the bearer.
        if !matches!(endpoint, "channels" | "liveBroadcasts") {
            return Err("Invalid YouTube request.".into());
        }
        let bearer = self.bearer(access, cancel)?;
        let response = self
            .http
            .get(format!("{API}{endpoint}"))
            .bearer_auth(&bearer)
            .query(query)
            .send()
            .map_err(|_| "YouTube couldn't be reached. It will try again later.")?;
        current(access, cancel)?;
        if !response.status().is_success() {
            if response.status().as_u16() == 401 {
                let saved = session_transaction(
                    || current(access, cancel),
                    || load_session(access),
                    |mut saved| {
                        // Never mark a newer token invalid because an older
                        // cancelled request received an authorization failure.
                        if saved.access_token == bearer {
                            saved.reconnect_required = true;
                        }
                        Ok(saved)
                    },
                    |saved| save_protected(access, saved),
                )?;
                self.session = Some(saved);
                return Err("Please reconnect YouTube.".into());
            }
            return Err(
                "YouTube checks are temporarily unavailable. Brick will try again later.".into(),
            );
        }
        serde_json::from_slice(&bounded(response)?)
            .map_err(|_| "YouTube returned an unreadable response.".into())
    }

    fn load_channels(&mut self, access: &Access, cancel: &AtomicBool) -> Result<(), String> {
        let mut channels = Vec::new();
        let mut page = String::new();
        // Typical accounts own one channel. Bound unusually large accounts to
        // two pages and report incompleteness rather than silently choosing one.
        for _ in 0..2 {
            let mut query = vec![
                ("part", "id,snippet"),
                ("mine", "true"),
                ("maxResults", "50"),
            ];
            if !page.is_empty() {
                query.push(("pageToken", page.as_str()));
            }
            let body = self.get("channels", &query, access, cancel)?;
            channels.extend(parse_channels(&body)?);
            page = next_page(&body)?;
            if page.is_empty() {
                break;
            }
        }
        if !page.is_empty() {
            return Err("This account has too many channels to list. Connect the intended YouTube channel directly.".into());
        }
        let mut ids = HashSet::new();
        if channels.iter().any(|c| !ids.insert(c.channel_id.clone())) {
            return Err("YouTube returned an invalid channel list.".into());
        }
        current(access, cancel)?;
        self.channels = channels;
        if let Some(session) = &mut self.session {
            session.channels = self.channels.clone();
            session.channels_checked_at = now();
            save(access, session, cancel)?;
        }
        Ok(())
    }

    /// A server-issued quota lease reserves these three API calls across all
    /// guilds before the UI invokes discovery. Never page through old history.
    pub fn broadcasts(
        &mut self,
        channel: &str,
        access: &Access,
        cancel: &AtomicBool,
    ) -> Result<Vec<String>, String> {
        if !self
            .channels
            .iter()
            .any(|owned| owned.channel_id == channel)
        {
            return Err("Choose one of your connected YouTube channels.".into());
        }
        let mut ids = Vec::new();
        for status in ["active", "upcoming", "completed"] {
            let body = self.get(
                "liveBroadcasts",
                &[
                    ("part", "id,snippet,status"),
                    ("broadcastStatus", status),
                    ("broadcastType", "all"),
                    ("maxResults", "50"),
                ],
                access,
                cancel,
            )?;
            // The mine and broadcastStatus filters are mutually exclusive.
            for id in parse_broadcasts(&body, channel, now())? {
                if ids.len() < 50 && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        current(access, cancel)?;
        Ok(ids)
    }
}

fn store(access: &Access) -> Result<Store, String> {
    if access.user_id.is_empty()
        || access.user_id.len() > 20
        || !access.user_id.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("Invalid Brick account.".into());
    }
    Store::youtube(&access.user_id)
}
fn load_session(access: &Access) -> Result<Option<Session>, String> {
    let Some(bytes) = store(access)?.load().map_err(storage_error)? else {
        return Ok(None);
    };
    let session: Session =
        serde_json::from_slice(&bytes).map_err(|_| "Please reconnect YouTube.")?;
    if session.version != 1
        || session.client_id != CLIENT_ID
        || session.account != access.user_id
        || !credential(&session.access_token)
        || session
            .refresh_token
            .as_ref()
            .is_some_and(|value| !credential(value))
        || !valid_saved_channels(&session.channels)
    {
        return Err("Please reconnect YouTube.".into());
    }
    Ok(Some(session))
}
fn save(access: &Access, session: &Session, cancel: &AtomicBool) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "YouTube credential storage is unavailable.")?;
    current(access, cancel)?;
    save_protected(access, session)
}
fn save_protected(access: &Access, session: &Session) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(session).map_err(|_| "Couldn't protect the YouTube connection.")?;
    store(access)?.save(&bytes).map_err(storage_error)
}
/// Serialize refresh with new connections and device disconnect. Once an
/// authorized refresh starts, its rotated credentials must survive panel
/// cancellation; a later disconnect acquires this same lock and deletes them.
fn session_transaction(
    check: impl FnOnce() -> Result<(), String>,
    load: impl FnOnce() -> Result<Option<Session>, String>,
    update: impl FnOnce(Session) -> Result<Session, String>,
    persist: impl FnOnce(&Session) -> Result<(), String>,
) -> Result<Session, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "YouTube credential storage is unavailable.")?;
    check()?;
    let saved = load()?.ok_or("Please reconnect YouTube.")?;
    let next = update(saved)?;
    persist(&next)?;
    Ok(next)
}
fn storage_error(message: String) -> String {
    message.replace("Warcraft Logs", "YouTube")
}
fn current(access: &Access, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        return Err("YouTube connection cancelled.".into());
    }
    access.check()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn credential(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| b.is_ascii_graphic())
}
pub fn valid_channel_id(value: &str) -> bool {
    value.len() == 24 && value.starts_with("UC") && value.bytes().all(id_byte)
}
fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
fn valid_client_id(value: &str) -> bool {
    value
        .strip_suffix(".apps.googleusercontent.com")
        .is_some_and(|prefix| (3..=160).contains(&prefix.len()) && prefix.bytes().all(id_byte))
}
fn random() -> Result<String, String> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Couldn't start a secure YouTube connection.")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn authorize_url(client: &str, redirect: &str, state: &str, verifier: &str) -> Result<Url, String> {
    if !valid_client_id(client) {
        return Err("Invalid YouTube client configuration.".into());
    }
    let mut url = Url::parse(AUTHORIZE).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", client),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("state", state),
        ("code_challenge_method", "S256"),
        (
            "code_challenge",
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)).as_str(),
        ),
        ("access_type", "offline"),
        ("prompt", "consent select_account"),
    ]);
    Ok(url)
}
fn bounded(response: Response) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|len| len > MAX_BODY) {
        return Err("YouTube returned too much data.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "YouTube's response couldn't be read.")?;
    if bytes.len() as u64 > MAX_BODY {
        return Err("YouTube returned too much data.".into());
    }
    Ok(bytes)
}
struct TokenFailure {
    message: String,
    invalid_grant: bool,
}
fn valid_client_companion(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|byte| byte.is_ascii_graphic())
}
fn token_failure(status: u16, bytes: &[u8]) -> TokenFailure {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    let code = value.as_ref().and_then(|value| value["error"].as_str());
    let invalid_grant = status == 400 && code == Some("invalid_grant");
    let configuration_error = matches!(code, Some("invalid_client" | "unauthorized_client"))
        || (code == Some("invalid_request")
            && value
                .as_ref()
                .and_then(|value| value["error_description"].as_str())
                == Some("client_secret is missing."));
    TokenFailure {
        message: if invalid_grant {
            "Please reconnect YouTube."
        } else if configuration_error {
            "This Brick build's YouTube connection is not configured correctly. Please update Brick."
        } else {
            "YouTube couldn't authorize the connection. Try again later."
        }.into(),
        invalid_grant,
    }
}
fn token_response(response: Response) -> Result<TokenResponse, TokenFailure> {
    let success = response.status().is_success();
    let status = response.status().as_u16();
    let bytes = bounded(response).map_err(|message| TokenFailure {
        message,
        invalid_grant: false,
    })?;
    if !success {
        return Err(token_failure(status, &bytes));
    }
    serde_json::from_slice(&bytes).map_err(|_| TokenFailure {
        message: "YouTube returned an invalid connection.".into(),
        invalid_grant: false,
    })
}
fn session_from_token(
    token: TokenResponse,
    account: &str,
    previous_refresh: Option<String>,
    at: u64,
) -> Result<Session, String> {
    if !token.token_type.eq_ignore_ascii_case("Bearer")
        || !credential(&token.access_token)
        || token
            .refresh_token
            .as_ref()
            .is_some_and(|value| !credential(value))
        || !(61..=604_800).contains(&token.expires_in)
        || token
            .scope
            .as_ref()
            .is_some_and(|value| !value.split_whitespace().any(|scope| scope == SCOPE))
    {
        return Err("YouTube didn't grant the requested read-only connection.".into());
    }
    Ok(Session {
        version: 1,
        client_id: CLIENT_ID.into(),
        account: account.into(),
        access_token: token.access_token,
        refresh_token: token.refresh_token.or(previous_refresh),
        expires_at: at
            .checked_add(token.expires_in)
            .ok_or("Invalid YouTube connection expiry.")?,
        channels: Vec::new(),
        channels_checked_at: 0,
        reconnect_required: false,
    })
}
fn parse_channels(body: &serde_json::Value) -> Result<Vec<Channel>, String> {
    items(body)?
        .iter()
        .map(|item| {
            let id = item["id"]
                .as_str()
                .filter(|value| valid_channel_id(value))
                .ok_or("YouTube returned an invalid channel.")?;
            let title = item["snippet"]["title"]
                .as_str()
                .filter(|value| value.len() <= 4096)
                .ok_or("YouTube returned an invalid channel.")?;
            Ok(Channel {
                channel_id: id.into(),
                title: title
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(150)
                    .collect(),
                url: format!("https://www.youtube.com/channel/{id}"),
            })
        })
        .collect()
}
fn valid_saved_channels(channels: &[Channel]) -> bool {
    let mut ids = HashSet::new();
    channels.len() <= 100
        && channels.iter().all(|channel| {
            valid_channel_id(&channel.channel_id)
                && ids.insert(&channel.channel_id)
                && channel.title.chars().count() <= 150
                && !channel.title.chars().any(char::is_control)
                && channel.url == format!("https://www.youtube.com/channel/{}", channel.channel_id)
        })
}
fn items(body: &serde_json::Value) -> Result<&Vec<serde_json::Value>, String> {
    body["items"]
        .as_array()
        .filter(|items| items.len() <= 50)
        .ok_or_else(|| "YouTube returned an invalid list.".into())
}
fn next_page(body: &serde_json::Value) -> Result<String, String> {
    match body.get("nextPageToken") {
        None => Ok(String::new()),
        Some(value) => value
            .as_str()
            .filter(|value| credential(value) && value.len() <= 512)
            .map(str::to_owned)
            .ok_or_else(|| "YouTube returned an invalid channel list.".into()),
    }
}
fn parse_broadcasts(
    body: &serde_json::Value,
    channel: &str,
    at: u64,
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for item in items(body)? {
        let id = item["id"]
            .as_str()
            .filter(|value| value.len() == 11 && value.bytes().all(id_byte))
            .ok_or("YouTube returned an invalid broadcast.")?;
        if item["snippet"]["channelId"].as_str() != Some(channel)
            || !matches!(
                item["status"]["privacyStatus"].as_str(),
                Some("public" | "unlisted")
            )
        {
            continue;
        }
        if item["status"]["lifeCycleStatus"].as_str() == Some("complete") {
            let recent = item["snippet"]["actualEndTime"]
                .as_str()
                .and_then(|value| {
                    time::OffsetDateTime::parse(
                        value,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .ok()
                })
                .and_then(|value| u64::try_from(value.unix_timestamp()).ok())
                .is_some_and(|end| {
                    end <= at.saturating_add(300) && at.saturating_sub(end) <= 14 * 86_400
                });
            if !recent {
                continue;
            }
        }
        result.push(id.into());
    }
    Ok(result)
}

enum Callback {
    Code(String),
    Denied,
    Rejected,
}
fn parse_callback(request: &[u8], state: &str, host: &str) -> Option<Callback> {
    let text = std::str::from_utf8(request).ok()?;
    let (header, body) = text.split_once("\r\n\r\n")?;
    if !body.is_empty() || header.len() > 16 * 1024 {
        return None;
    }
    let mut lines = header.split("\r\n");
    let mut first = lines.next()?.split(' ');
    if first.next()? != "GET" {
        return None;
    }
    let target = first.next()?;
    if !matches!(first.next()?, "HTTP/1.1" | "HTTP/1.0")
        || first.next().is_some()
        || !target.starts_with("/?")
        || target.contains('#')
        || target.contains('\\')
    {
        return None;
    }
    let mut hosts = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            hosts.push(value.trim());
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return None;
        }
    }
    if hosts != [host] {
        return None;
    }
    let url = Url::parse(&format!("http://{host}{target}")).ok()?;
    let pairs: Vec<_> = url.query_pairs().collect();
    for name in ["state", "code", "error"] {
        if pairs.iter().filter(|(key, _)| key == name).count() > 1 {
            return None;
        }
    }
    let get = |name: &str| {
        let mut values = pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    };
    if get("state")? != state {
        return None;
    }
    if let Some(error) = get("error") {
        return (credential(error) && get("code").is_none()).then_some(
            if error == "access_denied" {
                Callback::Denied
            } else {
                Callback::Rejected
            },
        );
    }
    let code = get("code")?;
    credential(code).then(|| Callback::Code(code.to_owned()))
}

fn receive_code(
    listener: &TcpListener,
    state: &str,
    host: &str,
    check: impl Fn() -> Result<(), String>,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        check()?;
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                let callback = read_callback(&mut stream)
                    .and_then(|bytes| parse_callback(&bytes, state, host));
                let status = if callback.is_some() {
                    "200 OK"
                } else {
                    "400 Bad Request"
                };
                let body = if callback.is_some() {
                    "You can return to Brick."
                } else {
                    "This sign-in callback was not accepted."
                };
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes());
                match callback {
                    Some(Callback::Code(code)) => {
                        check()?;
                        return Ok(code);
                    }
                    Some(Callback::Denied) => return Err("YouTube connection cancelled.".into()),
                    Some(Callback::Rejected) => {
                        return Err(
                            "YouTube couldn't authorize the connection. Try again later.".into(),
                        )
                    }
                    None => (),
                }
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(_) => {
                return Err("The local YouTube callback stopped. Try connecting again.".into())
            }
        }
    }
    Err("YouTube sign-in timed out. Try connecting again.".into())
}
fn read_callback(stream: &mut TcpStream) -> Option<Vec<u8>> {
    // Winsock inherits the listener's nonblocking mode. Accepted callbacks
    // need bounded blocking reads so delayed/fragmented browser headers work.
    stream.set_nonblocking(false).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while bytes.len() <= 16 * 1024 && Instant::now() < deadline {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            return Some(bytes);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHANNEL: &str = "UCabcdefghijklmnopqrstuv";
    #[test]
    fn authorization_uses_readonly_pkce_and_no_guild_or_bearer() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let url = authorize_url(
            "123-test.apps.googleusercontent.com",
            "http://127.0.0.1:1234/",
            "state",
            verifier,
        )
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["scope"], SCOPE);
        assert_eq!(
            query["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!url.as_str().contains(verifier));
        assert!(!query.contains_key("client_secret"));
        assert!(!query.contains_key("guild_id"));
    }
    fn callback(target: &str, headers: &str) -> Vec<u8> {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n{headers}\r\n").into_bytes()
    }
    #[test]
    fn callback_binds_state_host_method_and_single_code() {
        let parse = |bytes: &[u8]| parse_callback(bytes, "expected", "127.0.0.1:1234");
        assert!(
            matches!(parse(&callback("/?state=expected&code=valid", "")), Some(Callback::Code(code)) if code == "valid")
        );
        assert!(matches!(
            parse(&callback("/?state=expected&error=access_denied", "")),
            Some(Callback::Denied)
        ));
        for target in [
            "/?state=wrong&code=valid",
            "/?state=expected&state=wrong&code=x",
            "/?state=expected&code=x&code=y",
            "/?state=expected&code=x&error=access_denied&error=access_denied",
            "http://evil.test/?state=expected&code=x",
            "/wrong?state=expected&code=x",
            "/?state=expected&code=%0a",
            "/?state=expected&code=x#fragment",
        ] {
            assert!(parse(&callback(target, "")).is_none(), "{target}");
        }
        assert!(parse(&callback("/?state=expected&code=x", "Host: evil.test\r\n")).is_none());
        assert!(parse(&callback(
            "/?state=expected&code=x",
            "Content-Length: 9\r\n"
        ))
        .is_none());
        assert!(
            parse(b"POST /?state=expected&code=x HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n")
                .is_none()
        );
    }
    #[test]
    fn discovery_never_shares_private_or_other_channels_and_bounds_history() {
        let item = serde_json::json!({"id":"abcdefghijk", "snippet":{"channelId":CHANNEL,"actualEndTime":"2026-09-13T20:00:00Z"},
            "status":{"privacyStatus":"unlisted", "lifeCycleStatus":"complete"}});
        let at = 1789416000;
        assert_eq!(
            parse_broadcasts(&serde_json::json!({"items":[item.clone()]}), CHANNEL, at).unwrap(),
            ["abcdefghijk"]
        );
        for (key, value) in [("privacyStatus", "private")] {
            let mut changed = item.clone();
            changed["status"][key] = value.into();
            assert!(
                parse_broadcasts(&serde_json::json!({"items":[changed]}), CHANNEL, at)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(parse_broadcasts(
            &serde_json::json!({"items":[item.clone()]}),
            "UCotherchannel00000000000",
            at
        )
        .unwrap()
        .is_empty());
        assert!(parse_broadcasts(
            &serde_json::json!({"items":[item.clone()]}),
            CHANNEL,
            at + 30 * 86400
        )
        .unwrap()
        .is_empty());
        assert!(
            parse_broadcasts(&serde_json::json!({"items":vec![item; 51]}), CHANNEL, at).is_err()
        );
    }
    #[test]
    fn channels_reject_untrusted_links_and_malformed_identities() {
        assert!(valid_channel_id(CHANNEL));
        let body = serde_json::json!({"items":[{"id":CHANNEL,"snippet":{"title":"Our\nchannel","customUrl":"https://evil.test"}}]});
        let channel = parse_channels(&body).unwrap().pop().unwrap();
        assert_eq!(channel.title, "Ourchannel");
        assert_eq!(
            channel.url,
            format!("https://www.youtube.com/channel/{CHANNEL}")
        );
        assert!(parse_channels(
            &serde_json::json!({"items":[{"id":"../bad","snippet":{"title":"x"}}]})
        )
        .is_err());
    }
    #[test]
    fn refresh_rotation_is_retained_and_bad_grants_are_rejected() {
        let token = |refresh: Option<&str>| TokenResponse {
            access_token: "access".into(),
            token_type: "Bearer".into(),
            refresh_token: refresh.map(str::to_owned),
            expires_in: 3600,
            scope: Some(SCOPE.into()),
        };
        assert_eq!(
            session_from_token(token(None), "123", Some("old".into()), 100)
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("old")
        );
        assert_eq!(
            session_from_token(token(Some("rotated")), "123", Some("old".into()), 100)
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated")
        );
        let mut invalid = token(None);
        invalid.scope = Some("other".into());
        assert!(session_from_token(invalid, "123", None, 100).is_err());
        let mut invalid = token(None);
        invalid.expires_in = u64::MAX;
        assert!(session_from_token(invalid, "123", None, 100).is_err());
    }
    #[test]
    fn cancelled_callback_does_not_wait_or_consume_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            receive_code(&listener, "state", "unused", || Err("cancelled".into())).unwrap_err(),
            "cancelled"
        );
    }
    #[test]
    fn callback_handles_delayed_fragmented_headers_on_nonblocking_accepted_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let host = address.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            ready_rx.recv().unwrap();
            for part in [
                "GET /?state=expected&code=fixture-code HTTP/1.1\r\n".to_owned(),
                format!("Host: {host}\r\n"),
                "\r\n".to_owned(),
            ] {
                std::thread::sleep(Duration::from_millis(30));
                stream.write_all(part.as_bytes()).unwrap();
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        // Reproduce Winsock's inherited mode on every test platform.
        stream.set_nonblocking(true).unwrap();
        ready_tx.send(()).unwrap();
        let request = read_callback(&mut stream).unwrap();
        assert!(
            matches!(parse_callback(&request, "expected", &address.to_string()),
            Some(Callback::Code(code)) if code == "fixture-code")
        );
        client.join().unwrap();
    }
    #[test]
    fn stale_or_cancelled_workers_never_reach_credential_storage() {
        let token = TokenResponse {
            access_token: "fixture".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("fixture-refresh".into()),
            expires_in: 3600,
            scope: Some(SCOPE.into()),
        };
        let session = session_from_token(token, "123", None, 100).unwrap();
        let stale = Access::new(
            "fixture".into(),
            crate::guild::ADVANCE.into(),
            "123".into(),
            crate::guild::generation().wrapping_sub(1),
        );
        assert!(save(&stale, &session, &AtomicBool::new(false)).is_err());
        let access = Access::new(
            "fixture".into(),
            crate::guild::ADVANCE.into(),
            "123".into(),
            crate::guild::generation(),
        );
        assert!(save(&access, &session, &AtomicBool::new(true)).is_err());
    }
    #[test]
    fn loopback_ignores_invalid_request_then_accepts_exact_authorization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let host = address.to_string();
        let worker_host = host.clone();
        let client = std::thread::spawn(move || {
            for state in ["wrong", "expected"] {
                let mut stream = TcpStream::connect(address).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                write!(
                    stream,
                    "GET /?state={state}&code=fixture-code HTTP/1.1\r\nHost: {worker_host}\r\n\r\n"
                )
                .unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                assert!(response.contains(if state == "wrong" {
                    "400 Bad Request"
                } else {
                    "200 OK"
                }));
                assert!(!response.contains("fixture-code"));
                assert!(response.contains("Cache-Control: no-store"));
            }
        });
        assert_eq!(
            receive_code(&listener, "expected", &host, || Ok(())).unwrap(),
            "fixture-code"
        );
        client.join().unwrap();
    }
    #[test]
    fn cached_channels_remain_canonical_and_account_independent() {
        let channel = Channel {
            channel_id: CHANNEL.into(),
            title: "Raid channel".into(),
            url: format!("https://www.youtube.com/channel/{CHANNEL}"),
        };
        assert!(valid_saved_channels(&[channel.clone()]));
        assert!(!valid_saved_channels(&[channel.clone(), channel.clone()]));
        let mut altered = channel;
        altered.url = "https://evil.test".into();
        assert!(!valid_saved_channels(&[altered]));
    }
    #[test]
    fn configuration_errors_do_not_revoke_users_grants_or_echo_provider_data() {
        for (status, body) in [
            (
                400,
                r#"{"error":"invalid_request","error_description":"client_secret is missing."}"#,
            ),
            (
                401,
                r#"{"error":"invalid_client","error_description":"synthetic-sensitive-data"}"#,
            ),
            (
                400,
                r#"{"error":"unauthorized_client","error_description":"synthetic-sensitive-data"}"#,
            ),
        ] {
            let failure = token_failure(status, body.as_bytes());
            assert!(!failure.invalid_grant);
            assert!(failure.message.contains("not configured correctly"));
            assert!(!failure.message.contains("synthetic-sensitive-data"));
            assert!(!failure.message.contains("client_secret"));
        }
        let revoked = token_failure(
            400,
            br#"{"error":"invalid_grant","error_description":"synthetic-sensitive-data"}"#,
        );
        assert!(revoked.invalid_grant);
        assert_eq!(revoked.message, "Please reconnect YouTube.");
        let transient = token_failure(503, br#"{"error":"synthetic-sensitive-data"}"#);
        assert!(!transient.invalid_grant);
        assert!(!transient.message.contains("synthetic-sensitive-data"));
        assert!(!valid_client_companion(""));
        assert!(!valid_client_companion("value\n"));
        assert!(!valid_client_companion(&"x".repeat(1025)));
        assert!(valid_client_companion("synthetic-desktop-companion"));
    }
    fn fixture_session() -> Session {
        session_from_token(
            TokenResponse {
                access_token: "fixture-access".into(),
                token_type: "Bearer".into(),
                refresh_token: Some("fixture-old".into()),
                expires_in: 3600,
                scope: Some(SCOPE.into()),
            },
            "123",
            None,
            100,
        )
        .unwrap()
    }
    #[test]
    fn rotation_survives_panel_cancellation_after_the_request_started() {
        let cancelled = AtomicBool::new(false);
        let saved = std::cell::RefCell::new(None);
        let result = session_transaction(
            || {
                if cancelled.load(Ordering::Acquire) {
                    Err("cancelled".into())
                } else {
                    Ok(())
                }
            },
            || Ok(Some(fixture_session())),
            |mut session| {
                cancelled.store(true, Ordering::Release);
                session.refresh_token = Some("fixture-rotated".into());
                Ok(session)
            },
            |session| {
                *saved.borrow_mut() = Some(session.refresh_token.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result.refresh_token.as_deref(), Some("fixture-rotated"));
        assert_eq!(
            saved.into_inner().flatten().as_deref(),
            Some("fixture-rotated")
        );
        assert!(cancelled.load(Ordering::Acquire));
    }
    #[test]
    fn removed_grants_cannot_be_recreated_by_an_old_refresh() {
        assert!(session_transaction(
            || Ok(()),
            || Ok(None),
            |_| panic!("A deleted grant must never reach the provider"),
            |_| panic!("A deleted grant must never be restored")
        )
        .is_err());
        assert!(session_transaction(
            || Err("cancelled".into()),
            || panic!("Cancellation must precede loading credentials"),
            |_| panic!("Cancellation must precede provider calls"),
            |_| panic!("Cancellation must precede credential writes")
        )
        .is_err());
    }
    #[test]
    fn device_disconnect_wins_over_a_refresh_already_in_flight() {
        use std::sync::{mpsc, Arc};
        let saved = Arc::new(Mutex::new(Some(fixture_session())));
        let thread_saved = saved.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let refresh = std::thread::spawn(move || {
            session_transaction(
                || Ok(()),
                || Ok(thread_saved.lock().unwrap().clone()),
                |mut session| {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                    session.refresh_token = Some("fixture-rotated".into());
                    Ok(session)
                },
                |session| {
                    *thread_saved.lock().unwrap() = Some(session.clone());
                    Ok(())
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let disconnected = saved.clone();
        let disconnect = std::thread::spawn(move || {
            let _guard = STORE_LOCK.lock().unwrap();
            *disconnected.lock().unwrap() = None;
        });
        release_tx.send(()).unwrap();
        refresh.join().unwrap().unwrap();
        disconnect.join().unwrap();
        assert!(saved.lock().unwrap().is_none());
    }
    #[test]
    fn revoked_grants_restore_as_reconnect_needed_without_discovery() {
        let mut session = fixture_session();
        session.reconnect_required = true;
        let restored =
            serde_json::from_slice::<Session>(&serde_json::to_vec(&session).unwrap()).unwrap();
        let mut account = Account::new().unwrap();
        account.session = Some(restored);
        assert!(!account.connected());
        assert!(account.needs_reconnect());
    }
}
