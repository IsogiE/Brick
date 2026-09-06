use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::addon;

const API_BASE: &str = "https://discord.com/api/v10";
const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const APP_USER_AGENT: &str = "Brick/0.2 (+https://github.com/IsogiE/Brick-Releases)";
const REDIRECT_PATH: &str = "/discord/callback";
const AUTH_CALLBACK_POLL_PATH: &str = "/v1/auth/callback";
const SESSION_FILE: &str = "discord-auth.dat";
const LEGACY_SESSION_FILE: &str = "discord-auth.json";
const SESSION_SCHEMA: u32 = 2;
const LOGIN_TIMEOUT_SECS: u64 = 180;
const LOGIN_POLL_INTERVAL_MS: u64 = 750;
const EXPIRY_SAFETY_SECS: u64 = 60;
const MAX_SESSION_AGE_SECS: u64 = 30 * 24 * 60 * 60;
const SESSION_RENEWAL_MESSAGE: &str = "Saved Discord login expired. Please sign in again.";
const ADVANCE_GUILD_ID: &str = "1166119057993515100";
const OFFICER_ROLE_ID: &str = "1167061441023582258";
const RAIDER_ROLE_ID: &str = "1199377026168143872";

#[cfg(target_os = "windows")]
const PROTECTED_SESSION_PREFIX: &[u8] = b"BRICK-DISCORD-AUTH-DPAPI-v1\n";

const DISCORD_CLIENT_ID: &str = match option_env!("BRICK_DISCORD_CLIENT_ID") {
    Some(value) => value,
    None => "",
};
const DISCORD_GUILD_ID: &str = match option_env!("BRICK_DISCORD_GUILD_ID") {
    Some(value) => value,
    None => ADVANCE_GUILD_ID,
};
const DISCORD_ALLOWED_ROLE_IDS: &str = match option_env!("BRICK_DISCORD_ALLOWED_ROLE_IDS") {
    Some(value) => value,
    None => "1167061441023582258,1199377026168143872",
};
const DISCORD_GUILD_NAME: &str = match option_env!("BRICK_DISCORD_GUILD_NAME") {
    Some(value) => value,
    None => "Advance",
};
const DISCORD_ALLOWED_ROLE_LABEL: &str = match option_env!("BRICK_DISCORD_ALLOWED_ROLE_LABEL") {
    Some(value) => value,
    None => "Raider or Officer",
};
const PRESENCE_API_URL: &str = match option_env!("BRICK_PRESENCE_API_URL") {
    Some(value) => value,
    None => "",
};
const DISCORD_REDIRECT_URI: &str = match option_env!("BRICK_DISCORD_REDIRECT_URI") {
    Some(value) => value,
    None => "",
};

static SESSION_REFRESH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static HTTP_CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(APP_USER_AGENT)
        .build()
        .map_err(|error| format!("Failed to create Discord HTTP client: {error}"))
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    ConfigMissing(String),
    SignedOut,
    NeedsRefresh,
    Authorized(AuthorizedUser),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedUser {
    pub user_id: String,
    pub display_name: String,
    pub username: String,
    pub guild_name: String,
    pub role_label: String,
    pub expires_at_unix: u64,
    pub created_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSession {
    schema: u32,
    client_id: String,
    guild_id: String,
    access_token: String,
    refresh_token: String,
    expires_at_unix: u64,
    #[serde(default)]
    created_at_unix: u64,
    user_id: String,
    username: String,
    global_name: Option<String>,
    guild_nick: Option<String>,
    role_ids: Vec<String>,
    authorized_role_ids: Vec<String>,
    authorized_at_unix: u64,
}

#[derive(Debug, Deserialize)]
struct DiscordTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    global_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordMember {
    nick: Option<String>,
    roles: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthCallbackResponse {
    code: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct AuthConfig {
    client_id: String,
    guild_id: String,
    guild_name: String,
    role_label: String,
    allowed_role_ids: HashSet<String>,
}

struct LoginRequest {
    state: String,
    verifier: String,
    authorize_url: String,
}

pub fn saved_session_status() -> Result<SessionStatus, String> {
    let config = match auth_config() {
        Ok(config) => config,
        Err(error) => return Ok(SessionStatus::ConfigMissing(error)),
    };

    let Some(session) = load_session()? else {
        return Ok(SessionStatus::SignedOut);
    };

    if !session_matches_config(&session, &config) {
        clear_session()?;
        return Ok(SessionStatus::SignedOut);
    }

    let now = now_unix_secs();
    if session_age_expired(&session, now) {
        clear_session()?;
        return Ok(SessionStatus::SignedOut);
    }

    if session_access_token_current(&session, now) {
        return Ok(SessionStatus::Authorized(authorized_user(
            &session, &config,
        )));
    }

    Ok(SessionStatus::NeedsRefresh)
}

pub fn login_with_browser() -> Result<AuthorizedUser, String> {
    let config = auth_config()?;
    let request = login_request(&config)?;
    open_browser(&request.authorize_url)?;
    let code = wait_for_remote_callback(&request.state)?;
    let token = exchange_code(&config, &code, &request.verifier)?;
    let session = verified_session_from_token(&config, token, None)?;
    save_session(&session)?;
    Ok(authorized_user(&session, &config))
}

pub fn refresh_saved_session() -> Result<AuthorizedUser, String> {
    let config = auth_config()?;
    let _guard = SESSION_REFRESH_LOCK
        .lock()
        .map_err(|_| "Discord session refresh lock was poisoned.".to_string())?;
    let Some(session) = load_session()? else {
        return Err("Please sign in with Discord.".to_string());
    };

    if !session_matches_config(&session, &config) {
        clear_session()?;
        return Err("Please sign in with Discord.".to_string());
    }

    let now = now_unix_secs();
    if session_age_expired(&session, now) {
        clear_session()?;
        return Err(SESSION_RENEWAL_MESSAGE.to_string());
    }

    if session_access_token_current(&session, now) {
        return Ok(authorized_user(&session, &config));
    }

    let created_at_unix = session_created_at_unix(&session);

    let token = match refresh_token(&config, &session.refresh_token) {
        Ok(token) => token,
        Err(error) => {
            let _ = clear_session();
            return Err(error);
        }
    };

    let session = match verified_session_from_token(&config, token, Some(created_at_unix)) {
        Ok(session) => session,
        Err(error) => {
            let _ = clear_session();
            return Err(error);
        }
    };

    save_session(&session)?;
    Ok(authorized_user(&session, &config))
}

pub fn clear_session() -> Result<(), String> {
    let path = session_path()?;
    remove_session_file(&path)?;
    let legacy_path = legacy_session_path()?;
    remove_session_file(&legacy_path)?;
    Ok(())
}

pub fn session_expired(expires_at_unix: u64) -> bool {
    token_expired(expires_at_unix, now_unix_secs())
}

pub fn session_renewal_due(created_at_unix: u64) -> bool {
    session_age_expired_at(created_at_unix, now_unix_secs())
}

pub fn current_access_token() -> Result<String, String> {
    let config = auth_config()?;
    let Some(session) = load_session()? else {
        return Err("Please sign in with Discord.".to_string());
    };

    if !session_matches_config(&session, &config) {
        return Err("Discord session does not match this Brick build.".to_string());
    }
    let now = now_unix_secs();
    if session_age_expired(&session, now) {
        clear_session()?;
        return Err(SESSION_RENEWAL_MESSAGE.to_string());
    }
    if !session_access_token_current(&session, now) {
        return Err("Discord session needs refresh.".to_string());
    }

    Ok(session.access_token)
}

pub fn current_or_refreshed_access_token() -> Result<Option<String>, String> {
    let config = auth_config()?;
    let _guard = SESSION_REFRESH_LOCK
        .lock()
        .map_err(|_| "Discord session refresh lock was poisoned.".to_string())?;
    let Some(session) = load_session()? else {
        return Ok(None);
    };

    if !session_matches_config(&session, &config) {
        clear_session()?;
        return Ok(None);
    }

    let now = now_unix_secs();
    if session_age_expired(&session, now) {
        clear_session()?;
        return Ok(None);
    }

    if session_access_token_current(&session, now) {
        return Ok(Some(session.access_token));
    }

    let created_at_unix = session_created_at_unix(&session);

    let token = match refresh_token(&config, &session.refresh_token) {
        Ok(token) => token,
        Err(error) => {
            let _ = clear_session();
            return Err(error);
        }
    };

    let session = match verified_session_from_token(&config, token, Some(created_at_unix)) {
        Ok(session) => session,
        Err(error) => {
            let _ = clear_session();
            return Err(error);
        }
    };
    let access_token = session.access_token.clone();
    save_session(&session)?;
    Ok(Some(access_token))
}

pub fn role_label() -> &'static str {
    let label = DISCORD_ALLOWED_ROLE_LABEL.trim();
    if label.is_empty() {
        "Raider or Officer"
    } else {
        label
    }
}

pub fn guild_name() -> &'static str {
    let guild = DISCORD_GUILD_NAME.trim();
    if guild.is_empty() {
        "Advance"
    } else {
        guild
    }
}

fn auth_config() -> Result<AuthConfig, String> {
    let client_id = DISCORD_CLIENT_ID.trim();
    let guild_id = DISCORD_GUILD_ID.trim();
    let allowed_role_ids = parse_allowed_role_ids(DISCORD_ALLOWED_ROLE_IDS);

    if client_id.is_empty() || guild_id.is_empty() || allowed_role_ids.is_empty() {
        return Err(
            "Discord login needs BRICK_DISCORD_CLIENT_ID, BRICK_DISCORD_GUILD_ID, and BRICK_DISCORD_ALLOWED_ROLE_IDS."
                .to_string(),
        );
    }
    redirect_uri()?;

    Ok(AuthConfig {
        client_id: client_id.to_string(),
        guild_id: guild_id.to_string(),
        guild_name: guild_name().to_string(),
        role_label: role_label().to_string(),
        allowed_role_ids,
    })
}

fn parse_allowed_role_ids(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn login_request(config: &AuthConfig) -> Result<LoginRequest, String> {
    let state = random_token();
    let verifier = format!("{}{}", random_token(), random_token());
    let challenge = pkce_challenge(&verifier);
    let redirect = redirect_uri()?;
    let mut url = Url::parse(AUTHORIZE_URL)
        .map_err(|error| format!("Failed to build Discord login URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", "identify guilds.members.read")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    Ok(LoginRequest {
        state,
        verifier,
        authorize_url: url.to_string(),
    })
}

fn wait_for_remote_callback(expected_state: &str) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);
    let client = http_client()?;
    let mut last_error = None;

    loop {
        if Instant::now() >= deadline {
            return Err(last_error.unwrap_or_else(|| "Discord login timed out.".to_string()));
        }

        let url = auth_callback_url(expected_state)?;
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(format!("Discord login callback check failed: {error}"));
                thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
                continue;
            }
        };
        let status = response.status();
        let body = response
            .bytes()
            .map_err(|error| format!("Discord login callback check failed: {error}"))?;

        if status.as_u16() == 202 {
            thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
            continue;
        }

        if status.is_success() {
            let callback: AuthCallbackResponse = serde_json::from_slice(&body)
                .map_err(|error| format!("Discord login callback was invalid: {error}"))?;
            if let Some(error) = callback.error.filter(|value| !value.trim().is_empty()) {
                return Err(error);
            }
            return callback
                .code
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    "Discord login callback did not include an authorization code.".to_string()
                });
        }

        let message = discord_error_message(status.as_u16(), &body);
        if status.is_server_error() {
            last_error = Some(format!("Discord login callback check failed: {message}"));
            thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
            continue;
        }
        return Err(format!("Discord login callback check failed: {message}"));
    }
}

fn exchange_code(
    config: &AuthConfig,
    code: &str,
    verifier: &str,
) -> Result<DiscordTokenResponse, String> {
    let redirect_uri = redirect_uri()?;
    let params = [
        ("client_id", config.client_id.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    post_token_request(&params, "Discord token exchange failed")
}

fn refresh_token(config: &AuthConfig, refresh_token: &str) -> Result<DiscordTokenResponse, String> {
    let params = [
        ("client_id", config.client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    post_token_request(&params, "Discord session refresh failed")
}

fn post_token_request(
    params: &[(&str, &str)],
    error_prefix: &str,
) -> Result<DiscordTokenResponse, String> {
    let client = http_client()?;
    let response = client
        .post(format!("{API_BASE}/oauth2/token"))
        .form(params)
        .send()
        .map_err(|error| format!("{error_prefix}: {error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .map_err(|error| format!("{error_prefix}: {error}"))?;

    if !status.is_success() {
        return Err(format!(
            "{error_prefix}: {}",
            discord_error_message(status.as_u16(), &body)
        ));
    }

    let token: DiscordTokenResponse = serde_json::from_slice(&body)
        .map_err(|error| format!("{error_prefix}: invalid Discord response: {error}"))?;
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err(format!(
            "{error_prefix}: Discord returned an unsupported token type."
        ));
    }
    if token
        .refresh_token
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return Err(format!(
            "{error_prefix}: Discord did not return a refresh token. Make sure the Brick Discord app is configured as a public OAuth2 client."
        ));
    }
    Ok(token)
}

fn verified_session_from_token(
    config: &AuthConfig,
    token: DiscordTokenResponse,
    created_at_unix: Option<u64>,
) -> Result<AuthSession, String> {
    let user = fetch_user(&token.access_token)?;
    let member = fetch_member(config, &token.access_token)?;
    let authorized_role_ids = member
        .roles
        .iter()
        .filter(|role_id| config.allowed_role_ids.contains(*role_id))
        .cloned()
        .collect::<Vec<_>>();

    if authorized_role_ids.is_empty() {
        return Err(format!(
            "This Discord account does not have {} in {}.",
            config.role_label, config.guild_name
        ));
    }

    let now = now_unix_secs();
    Ok(AuthSession {
        schema: SESSION_SCHEMA,
        client_id: config.client_id.clone(),
        guild_id: config.guild_id.clone(),
        access_token: token.access_token,
        refresh_token: token.refresh_token.unwrap_or_default(),
        expires_at_unix: now.saturating_add(token.expires_in),
        created_at_unix: created_at_unix.filter(|value| *value > 0).unwrap_or(now),
        user_id: user.id,
        username: user.username,
        global_name: user.global_name,
        guild_nick: member.nick,
        role_ids: member.roles,
        authorized_role_ids,
        authorized_at_unix: now,
    })
}

fn fetch_user(access_token: &str) -> Result<DiscordUser, String> {
    get_discord_json(
        &format!("{API_BASE}/users/@me"),
        access_token,
        "Discord user lookup failed",
    )
}

fn fetch_member(config: &AuthConfig, access_token: &str) -> Result<DiscordMember, String> {
    get_discord_json(
        &format!("{API_BASE}/users/@me/guilds/{}/member", config.guild_id),
        access_token,
        "Discord guild role lookup failed",
    )
}

fn get_discord_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    access_token: &str,
    error_prefix: &str,
) -> Result<T, String> {
    let client = http_client()?;
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .map_err(|error| format!("{error_prefix}: {error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .map_err(|error| format!("{error_prefix}: {error}"))?;

    if !status.is_success() {
        return Err(format!(
            "{error_prefix}: {}",
            discord_error_message(status.as_u16(), &body)
        ));
    }

    serde_json::from_slice(&body)
        .map_err(|error| format!("{error_prefix}: invalid Discord response: {error}"))
}

fn authorized_user(session: &AuthSession, config: &AuthConfig) -> AuthorizedUser {
    AuthorizedUser {
        user_id: session.user_id.clone(),
        display_name: session
            .guild_nick
            .clone()
            .or_else(|| session.global_name.clone())
            .unwrap_or_else(|| session.username.clone()),
        username: session.username.clone(),
        guild_name: config.guild_name.clone(),
        role_label: authorized_role_label(&session.authorized_role_ids, config),
        expires_at_unix: session.expires_at_unix,
        created_at_unix: session_created_at_unix(session),
    }
}

fn authorized_role_label(role_ids: &[String], config: &AuthConfig) -> String {
    if role_ids.iter().any(|role_id| role_id == OFFICER_ROLE_ID) {
        "Officer".to_string()
    } else if role_ids.iter().any(|role_id| role_id == RAIDER_ROLE_ID) {
        "Raider".to_string()
    } else {
        config.role_label.clone()
    }
}

fn session_matches_config(session: &AuthSession, config: &AuthConfig) -> bool {
    (session.schema == 1 || session.schema == SESSION_SCHEMA)
        && session.client_id == config.client_id
        && session.guild_id == config.guild_id
        && session
            .authorized_role_ids
            .iter()
            .any(|role_id| config.allowed_role_ids.contains(role_id))
}

fn session_access_token_current(session: &AuthSession, now: u64) -> bool {
    !token_expired(session.expires_at_unix, now)
}

fn session_age_expired(session: &AuthSession, now: u64) -> bool {
    session_age_expired_at(session_created_at_unix(session), now)
}

fn session_age_expired_at(created_at_unix: u64, now: u64) -> bool {
    created_at_unix == 0 || created_at_unix.saturating_add(MAX_SESSION_AGE_SECS) <= now
}

fn session_created_at_unix(session: &AuthSession) -> u64 {
    if session.created_at_unix == 0 {
        session.authorized_at_unix
    } else {
        session.created_at_unix
    }
}

fn token_expired(expires_at_unix: u64, now: u64) -> bool {
    now.saturating_add(EXPIRY_SAFETY_SECS) >= expires_at_unix
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn load_session() -> Result<Option<AuthSession>, String> {
    let path = session_path()?;
    if path.exists() {
        let loaded = load_session_file(&path)?;
        if loaded.needs_resave || loaded.session.schema != SESSION_SCHEMA {
            save_session(&loaded.session)?;
        } else {
            remove_legacy_session_file()?;
        }
        return Ok(Some(loaded.session));
    }

    let legacy_path = legacy_session_path()?;
    if legacy_path.exists() {
        let loaded = load_session_file(&legacy_path)?;
        save_session(&loaded.session)?;
        remove_session_file(&legacy_path)?;
        return Ok(Some(loaded.session));
    }

    Ok(None)
}

fn save_session(session: &AuthSession) -> Result<(), String> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }

    let mut session = session.clone();
    session.schema = SESSION_SCHEMA;
    if session.created_at_unix == 0 {
        session.created_at_unix = session.authorized_at_unix;
    }

    let mut json = serde_json::to_vec_pretty(&session)
        .map_err(|error| format!("Failed to serialize Discord session: {error}"))?;
    json.push(b'\n');
    let payload = session_payload_for_write(&json)?;
    write_private_bytes(&path, &payload)?;
    remove_legacy_session_file()
}

struct LoadedSession {
    session: AuthSession,
    needs_resave: bool,
}

fn load_session_file(path: &Path) -> Result<LoadedSession, String> {
    let contents =
        fs::read(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let needs_resave = session_file_needs_resave(&contents);
    let plaintext = session_plaintext_bytes(&contents)?;
    let session = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
    Ok(LoadedSession {
        session,
        needs_resave,
    })
}

fn remove_legacy_session_file() -> Result<(), String> {
    let legacy_path = legacy_session_path()?;
    remove_session_file(&legacy_path)
}

fn remove_session_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove {}: {error}", path.display())),
    }
}

fn write_private_bytes(path: &Path, contents: &[u8]) -> Result<(), String> {
    crate::atomic_file::write(path, contents)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

#[cfg(target_os = "windows")]
fn session_payload_for_write(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
    };

    let input_len = u32::try_from(plaintext.len())
        .map_err(|_| "Discord session is too large to encrypt.".to_string())?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };

    if ok == 0 {
        return Err(format!(
            "Failed to encrypt Discord session with Windows DPAPI: {}",
            std::io::Error::last_os_error()
        ));
    }

    let encrypted = if output.pbData.is_null() || output.cbData == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        unsafe {
            LocalFree(output.pbData.cast());
        }
    }

    let mut payload = Vec::with_capacity(PROTECTED_SESSION_PREFIX.len() + encrypted.len());
    payload.extend_from_slice(PROTECTED_SESSION_PREFIX);
    payload.extend_from_slice(&encrypted);
    Ok(payload)
}

#[cfg(not(target_os = "windows"))]
fn session_payload_for_write(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Ok(plaintext.to_vec())
}

#[cfg(target_os = "windows")]
fn session_plaintext_bytes(contents: &[u8]) -> Result<Vec<u8>, String> {
    if contents.starts_with(PROTECTED_SESSION_PREFIX) {
        return unprotect_session_payload(&contents[PROTECTED_SESSION_PREFIX.len()..]);
    }

    Ok(contents.to_vec())
}

#[cfg(not(target_os = "windows"))]
fn session_plaintext_bytes(contents: &[u8]) -> Result<Vec<u8>, String> {
    Ok(contents.to_vec())
}

#[cfg(target_os = "windows")]
fn session_file_needs_resave(contents: &[u8]) -> bool {
    !contents.starts_with(PROTECTED_SESSION_PREFIX)
}

#[cfg(not(target_os = "windows"))]
fn session_file_needs_resave(_contents: &[u8]) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn unprotect_session_payload(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };

    if encrypted.is_empty() {
        return Err("Saved Discord session is empty.".to_string());
    }

    let input_len = u32::try_from(encrypted.len())
        .map_err(|_| "Saved Discord session is too large to decrypt.".to_string())?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let mut description: windows_sys::core::PWSTR = ptr::null_mut();
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            &mut description,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };

    if !description.is_null() {
        unsafe {
            LocalFree(description.cast());
        }
    }

    if ok == 0 {
        return Err(format!(
            "Failed to decrypt Discord session with Windows DPAPI: {}",
            std::io::Error::last_os_error()
        ));
    }

    let plaintext = if output.pbData.is_null() || output.cbData == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        unsafe {
            LocalFree(output.pbData.cast());
        }
    }

    Ok(plaintext)
}

fn session_path() -> Result<PathBuf, String> {
    Ok(addon::config_dir()?.join(SESSION_FILE))
}

fn legacy_session_path() -> Result<PathBuf, String> {
    Ok(addon::config_dir()?.join(LEGACY_SESSION_FILE))
}

fn http_client() -> Result<Client, String> {
    match &*HTTP_CLIENT {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn discord_error_message(status: u16, body: &[u8]) -> String {
    if let Ok(error) = serde_json::from_slice::<DiscordErrorResponse>(body) {
        if let Some(description) = error
            .error_description
            .filter(|value| !value.trim().is_empty())
        {
            return format!("{status} {description}");
        }
        if let Some(message) = error.message.filter(|value| !value.trim().is_empty()) {
            return format!("{status} {message}");
        }
        if let Some(code) = error.error.filter(|value| !value.trim().is_empty()) {
            return format!("{status} {code}");
        }
    }

    format!("HTTP {status}")
}

fn redirect_uri() -> Result<String, String> {
    let explicit = DISCORD_REDIRECT_URI.trim();
    if !explicit.is_empty() {
        validate_https_url(explicit, "Discord redirect URL")?;
        return Ok(explicit.to_string());
    }

    Ok(service_endpoint_url(REDIRECT_PATH)?.to_string())
}

fn auth_callback_url(state: &str) -> Result<Url, String> {
    let mut url = service_endpoint_url(AUTH_CALLBACK_POLL_PATH)?;
    url.query_pairs_mut().append_pair("state", state);
    Ok(url)
}

fn service_endpoint_url(path: &str) -> Result<Url, String> {
    let base = PRESENCE_API_URL.trim();
    if base.is_empty() {
        return Err("Discord login needs BRICK_PRESENCE_API_URL.".to_string());
    }

    validate_https_url(base, "Brick presence API URL")?;
    let parsed =
        Url::parse(base).map_err(|error| format!("Invalid Brick presence API URL: {error}"))?;
    parsed
        .join(path)
        .map_err(|error| format!("Invalid Brick service endpoint: {error}"))
}

fn validate_https_url(value: &str, label: &str) -> Result<(), String> {
    let parsed = Url::parse(value).map_err(|error| format!("Invalid {label}: {error}"))?;
    if parsed.scheme() != "https" && parsed.host_str() != Some("127.0.0.1") {
        return Err(format!("{label} must use HTTPS."));
    }
    Ok(())
}

fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .map_err(|error| format!("Failed to open Discord in your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|error| format!("Failed to open Discord in your browser: {error}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for command in ["xdg-open", "gio", "kde-open", "gnome-open"] {
            let result = if command == "gio" {
                Command::new(command).args(["open", url]).spawn()
            } else {
                Command::new(command).arg(url).spawn()
            };

            if result.is_ok() {
                return Ok(());
            }
        }

        Err("Failed to open Discord in your browser.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        authorized_role_label, parse_allowed_role_ids, pkce_challenge, session_age_expired_at,
        token_expired, AuthConfig, DiscordTokenResponse,
    };

    fn test_config() -> AuthConfig {
        AuthConfig {
            client_id: "client".to_string(),
            guild_id: "guild".to_string(),
            guild_name: "Advance".to_string(),
            role_label: "Raider or Officer".to_string(),
            allowed_role_ids: HashSet::new(),
        }
    }

    #[test]
    fn parses_allowed_role_ids() {
        let roles = parse_allowed_role_ids(" 1,2 ,, 3 ");

        assert!(roles.contains("1"));
        assert!(roles.contains("2"));
        assert!(roles.contains("3"));
        assert_eq!(roles.len(), 3);
    }

    #[test]
    fn keeps_session_until_expiry_window() {
        assert!(!token_expired(1_200, 1_000));
        assert!(token_expired(1_060, 1_000));
        assert!(token_expired(1_000, 1_000));
    }

    #[test]
    fn expires_saved_login_after_max_age() {
        assert!(!session_age_expired_at(
            1_000,
            1_000 + super::MAX_SESSION_AGE_SECS - 1
        ));
        assert!(session_age_expired_at(
            1_000,
            1_000 + super::MAX_SESSION_AGE_SECS
        ));
        assert!(session_age_expired_at(0, 1_000));
    }

    #[test]
    fn pkce_challenge_is_url_safe() {
        let challenge = pkce_challenge("abc123");

        assert_eq!(challenge.len(), 43);
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn parses_discord_token_response() {
        let token: DiscordTokenResponse = serde_json::from_str(
            r#"{
                "access_token": "access",
                "token_type": "Bearer",
                "expires_in": 604800,
                "refresh_token": "refresh",
                "scope": "identify guilds.members.read"
            }"#,
        )
        .unwrap();

        assert_eq!(token.access_token, "access");
        assert_eq!(token.token_type, "Bearer");
        assert_eq!(token.expires_in, 604800);
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
    }

    #[test]
    fn labels_authorized_role() {
        let config = test_config();

        assert_eq!(
            authorized_role_label(&["1199377026168143872".to_string()], &config),
            "Raider"
        );
        assert_eq!(
            authorized_role_label(&["1167061441023582258".to_string()], &config),
            "Officer"
        );
        assert_eq!(
            authorized_role_label(
                &[
                    "1199377026168143872".to_string(),
                    "1167061441023582258".to_string()
                ],
                &config
            ),
            "Officer"
        );
    }
}
