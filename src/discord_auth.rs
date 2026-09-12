use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::{addon, download};

const API_BASE: &str = "https://discord.com/api/v10";
const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const APP_USER_AGENT: &str = "Brick/0.2 (+https://github.com/IsogiE/Brick-Releases)";
const REDIRECT_PATH: &str = "/discord/callback";
const AUTH_CALLBACK_POLL_PATH: &str = "/v1/auth/callback";
const SESSION_FILE: &str = "discord-auth.dat";
const LEGACY_SESSION_FILE: &str = "discord-auth.json";
const SESSION_SCHEMA: u32 = 2;
const REFRESH_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_AUTH_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_CALLBACK_BYTES: u64 = 16 * 1024;
const MAX_SESSION_BYTES: u64 = 128 * 1024;
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

static SESSION_GENERATION: AtomicU64 = AtomicU64::new(0);
static SESSION_STORAGE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static SESSION_REFRESH_LOCK: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
static HTTP_CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
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

#[derive(Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    authorization_pending: bool,
}

#[derive(Debug, Clone)]
pub struct RefreshError {
    pub message: String,
    pub retryable: bool,
}

impl RefreshError {
    pub fn rejected(message: String) -> Self {
        Self {
            message,
            retryable: false,
        }
    }
}

impl From<String> for RefreshError {
    fn from(message: String) -> Self {
        Self {
            message,
            retryable: true,
        }
    }
}

impl From<RefreshError> for String {
    fn from(error: RefreshError) -> Self {
        error.message
    }
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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
    let generation = SESSION_GENERATION.load(Ordering::SeqCst);
    let config = auth_config()?;
    let request = login_request(&config)?;
    crate::browser::open(&request.authorize_url)?;
    let code = wait_for_remote_callback(&request.state)?;
    let token = exchange_code(&config, &code, &request.verifier)?;
    let session = verified_session_from_token(&config, token, None)?;
    save_session_if_current(&session, generation)?;
    Ok(authorized_user(&session, &config))
}

pub fn refresh_saved_session() -> Result<AuthorizedUser, RefreshError> {
    let config = auth_config()?;
    let session = current_or_refreshed_session(&config, true)?
        .ok_or_else(|| RefreshError::rejected("Please sign in with Discord.".to_string()))?;
    Ok(authorized_user(&session, &config))
}

fn current_or_refreshed_session(
    config: &AuthConfig,
    retry_now: bool,
) -> Result<Option<AuthSession>, RefreshError> {
    let mut retry_at = SESSION_REFRESH_LOCK
        .lock()
        .map_err(|_| "Discord session refresh is unavailable.".to_string())?;
    let generation = SESSION_GENERATION.load(Ordering::SeqCst);
    let Some(session) = load_session()? else {
        return Ok(None);
    };
    if !session_matches_config(&session, config) {
        clear_session_if_current(Some(generation))?;
        *retry_at = None;
        return Ok(None);
    }
    if session_age_expired(&session, now_unix_secs()) {
        clear_session_if_current(Some(generation))?;
        *retry_at = None;
        return Err(RefreshError::rejected(SESSION_RENEWAL_MESSAGE.to_string()));
    }
    if session_access_token_current(&session, now_unix_secs()) {
        return Ok(Some(session));
    }
    if !retry_now && retry_at.is_some_and(|deadline| Instant::now() < deadline) {
        return Err(
            "Discord is temporarily unavailable. Your saved sign-in will be retried."
                .to_string()
                .into(),
        );
    }
    let result = renew_session(
        config,
        session,
        now_unix_secs(),
        |refresh| refresh_token(config, refresh),
        |session| verify_session(config, session),
        |session| save_session_if_current(session, generation),
    );
    match result {
        Ok(session) => {
            *retry_at = None;
            Ok(Some(session))
        }
        Err(error) => {
            if error.retryable {
                *retry_at = Some(Instant::now() + REFRESH_RETRY_DELAY);
            } else {
                clear_session_if_current(Some(generation))?;
                *retry_at = None;
            }
            Err(error)
        }
    }
}

// Persist a rotated refresh token before any fallible identity/role request.
// Pending credentials carry no authorization, including when read by old builds.
fn renew_session(
    config: &AuthConfig,
    mut session: AuthSession,
    now: u64,
    refresh: impl FnOnce(&str) -> Result<DiscordTokenResponse, RefreshError>,
    verify: impl FnOnce(AuthSession) -> Result<AuthSession, RefreshError>,
    mut save: impl FnMut(&AuthSession) -> Result<(), String>,
) -> Result<AuthSession, RefreshError> {
    if token_expired(session.expires_at_unix, now) {
        let created_at = session_created_at_unix(&session);
        let token = refresh(&session.refresh_token)?;
        session = pending_session(config, token, created_at, now);
        save(&session)?;
    }
    let session = verify(session)?;
    save(&session)?;
    Ok(session)
}

pub fn clear_session() -> Result<(), String> {
    clear_session_if_current(None)
}

fn clear_session_if_current(generation: Option<u64>) -> Result<(), String> {
    let _guard = SESSION_STORAGE_LOCK
        .lock()
        .map_err(|_| "Discord session storage is unavailable.".to_string())?;
    if generation.is_some_and(|value| SESSION_GENERATION.load(Ordering::SeqCst) != value) {
        return Ok(());
    }
    SESSION_GENERATION.fetch_add(1, Ordering::SeqCst);
    let path = session_path()?;
    #[cfg(target_os = "linux")]
    if path.exists() {
        delete_keyring_session(&path)?;
    }
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
    refreshed_access_token().map_err(String::from)
}

pub fn refreshed_access_token() -> Result<Option<String>, RefreshError> {
    let config = auth_config()?;
    current_or_refreshed_session(&config, false)
        .map(|session| session.map(|session| session.access_token))
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
                last_error = Some(format!(
                    "Discord login callback check failed: {}",
                    error.without_url()
                ));
                thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
                continue;
            }
        };
        let status = response.status();
        let body = download::read_response(response, MAX_CALLBACK_BYTES, "Discord login callback")?;

        if status.as_u16() == 202 {
            thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
            continue;
        }

        if status.is_success() {
            return parse_auth_callback(&body);
        }

        let message = discord_error_message(status.as_u16(), &body);
        if status.is_server_error() || status.as_u16() == 429 {
            last_error = Some(format!("Discord login callback check failed: {message}"));
            thread::sleep(Duration::from_millis(LOGIN_POLL_INTERVAL_MS));
            continue;
        }
        return Err(format!("Discord login callback check failed: {message}"));
    }
}

fn parse_auth_callback(body: &[u8]) -> Result<String, String> {
    let callback: AuthCallbackResponse = serde_json::from_slice(body)
        .map_err(|_| "Discord login callback was invalid.".to_string())?;
    if callback.error.is_some() {
        return Err("Discord login was rejected. Please try again.".to_string());
    }
    callback
        .code
        .filter(|code| {
            !code.is_empty() && code.len() <= 2048 && code.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(|| {
            "Discord login callback did not include a valid authorization code.".to_string()
        })
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
    post_token_request(&params, "Discord token exchange failed").map_err(String::from)
}

fn refresh_token(
    config: &AuthConfig,
    refresh_token: &str,
) -> Result<DiscordTokenResponse, RefreshError> {
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
) -> Result<DiscordTokenResponse, RefreshError> {
    let client = http_client()?;
    let response = client
        .post(format!("{API_BASE}/oauth2/token"))
        .form(params)
        .send()
        .map_err(|error| format!("{error_prefix}: {}", error.without_url()))?;
    let status = response.status();
    let body = download::read_response(response, MAX_AUTH_RESPONSE_BYTES, error_prefix)?;

    if !status.is_success() {
        return Err(refresh_http_error(
            status.as_u16(),
            &body,
            error_prefix,
            true,
        ));
    }

    let token: DiscordTokenResponse = serde_json::from_slice(&body)
        .map_err(|error| format!("{error_prefix}: invalid Discord response: {error}"))?;
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err(format!("{error_prefix}: Discord returned an unsupported token type.").into());
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
        ).into());
    }
    Ok(token)
}

fn pending_session(
    config: &AuthConfig,
    token: DiscordTokenResponse,
    created_at: u64,
    now: u64,
) -> AuthSession {
    AuthSession {
        schema: SESSION_SCHEMA,
        client_id: config.client_id.clone(),
        guild_id: config.guild_id.clone(),
        access_token: token.access_token,
        refresh_token: token.refresh_token.unwrap_or_default(),
        expires_at_unix: now.saturating_add(token.expires_in),
        created_at_unix: created_at,
        user_id: String::new(),
        username: String::new(),
        global_name: None,
        guild_nick: None,
        role_ids: Vec::new(),
        authorized_role_ids: Vec::new(),
        authorized_at_unix: 0,
        authorization_pending: true,
    }
}

fn verified_session_from_token(
    config: &AuthConfig,
    token: DiscordTokenResponse,
    created_at_unix: Option<u64>,
) -> Result<AuthSession, RefreshError> {
    let now = now_unix_secs();
    verify_session(
        config,
        pending_session(
            config,
            token,
            created_at_unix.filter(|v| *v > 0).unwrap_or(now),
            now,
        ),
    )
}

fn verify_session(config: &AuthConfig, session: AuthSession) -> Result<AuthSession, RefreshError> {
    let user = fetch_user(&session.access_token)?;
    let member = fetch_member(config, &session.access_token)?;
    authorize_session(config, session, user, member, now_unix_secs())
}

fn authorize_session(
    config: &AuthConfig,
    mut session: AuthSession,
    user: DiscordUser,
    member: DiscordMember,
    now: u64,
) -> Result<AuthSession, RefreshError> {
    let authorized_role_ids = member
        .roles
        .iter()
        .filter(|role_id| config.allowed_role_ids.contains(*role_id))
        .cloned()
        .collect::<Vec<_>>();
    if authorized_role_ids.is_empty() {
        return Err(RefreshError::rejected(format!(
            "This Discord account does not have {} in {}.",
            config.role_label, config.guild_name
        )));
    }
    session.user_id = user.id;
    session.username = user.username;
    session.global_name = user.global_name;
    session.guild_nick = member.nick;
    session.role_ids = member.roles;
    session.authorized_role_ids = authorized_role_ids;
    session.authorized_at_unix = now;
    session.authorization_pending = false;
    Ok(session)
}

fn fetch_user(access_token: &str) -> Result<DiscordUser, RefreshError> {
    get_discord_json(
        &format!("{API_BASE}/users/@me"),
        access_token,
        "Discord user lookup failed",
    )
}

fn fetch_member(config: &AuthConfig, access_token: &str) -> Result<DiscordMember, RefreshError> {
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
) -> Result<T, RefreshError> {
    let client = http_client()?;
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .map_err(|error| format!("{error_prefix}: {}", error.without_url()))?;
    let status = response.status();
    let body = download::read_response(response, MAX_AUTH_RESPONSE_BYTES, error_prefix)?;

    if !status.is_success() {
        return Err(refresh_http_error(
            status.as_u16(),
            &body,
            error_prefix,
            false,
        ));
    }

    serde_json::from_slice(&body)
        .map_err(|error| format!("{error_prefix}: invalid Discord response: {error}").into())
}

fn refresh_http_error(
    status: u16,
    body: &[u8],
    prefix: &str,
    token_endpoint: bool,
) -> RefreshError {
    let message = format!("{prefix}: {}", discord_error_message(status, body));
    let rejected = if token_endpoint {
        status == 400
            && serde_json::from_slice::<DiscordErrorResponse>(body)
                .is_ok_and(|error| error.error.as_deref() == Some("invalid_grant"))
    } else {
        matches!(status, 401 | 403 | 404)
    };
    if rejected {
        RefreshError::rejected(message)
    } else {
        message.into()
    }
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
        role_label: authorized_role_label(&session.user_id, &session.authorized_role_ids, config),
        expires_at_unix: session.expires_at_unix,
        created_at_unix: session_created_at_unix(session),
    }
}

fn authorized_role_label(user_id: &str, role_ids: &[String], config: &AuthConfig) -> String {
    if role_ids.iter().any(|role_id| role_id == OFFICER_ROLE_ID)
        || (user_id == "341518802208423957"
            && role_ids.iter().any(|role_id| role_id == RAIDER_ROLE_ID))
    {
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
        && (session.authorization_pending
            || session
                .authorized_role_ids
                .iter()
                .any(|role_id| config.allowed_role_ids.contains(role_id)))
}

fn session_access_token_current(session: &AuthSession, now: u64) -> bool {
    !session.authorization_pending && !token_expired(session.expires_at_unix, now)
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
    let _guard = SESSION_STORAGE_LOCK
        .lock()
        .map_err(|_| "Discord session storage is unavailable.".to_string())?;
    load_session_unlocked()
}

fn load_session_unlocked() -> Result<Option<AuthSession>, String> {
    load_session_at(&session_path()?, &legacy_session_path()?)
}

fn load_session_at(path: &Path, legacy_path: &Path) -> Result<Option<AuthSession>, String> {
    if path.exists() {
        let loaded = load_session_file(&path)?;
        if loaded.needs_resave || loaded.session.schema != SESSION_SCHEMA {
            save_session_at(path, legacy_path, &loaded.session)?;
        } else {
            remove_session_file(legacy_path)?;
        }
        return Ok(Some(loaded.session));
    }

    if legacy_path.exists() {
        let loaded = load_session_file(&legacy_path)?;
        save_session_at(path, legacy_path, &loaded.session)?;
        remove_session_file(&legacy_path)?;
        return Ok(Some(loaded.session));
    }

    Ok(None)
}

fn save_session_if_current(session: &AuthSession, generation: u64) -> Result<(), String> {
    let _guard = SESSION_STORAGE_LOCK
        .lock()
        .map_err(|_| "Discord session storage is unavailable.".to_string())?;
    if SESSION_GENERATION.load(Ordering::SeqCst) != generation {
        return Err("The Discord sign-in changed while access was being checked.".to_string());
    }
    save_session_unlocked(session)
}

fn save_session_unlocked(session: &AuthSession) -> Result<(), String> {
    save_session_at(&session_path()?, &legacy_session_path()?, session)
}

fn save_session_at(path: &Path, legacy_path: &Path, session: &AuthSession) -> Result<(), String> {
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
    let payload = session_payload_for_write(&path, &json)?;
    write_private_bytes(&path, &payload)?;
    remove_session_file(legacy_path)
}

struct LoadedSession {
    session: AuthSession,
    needs_resave: bool,
}

fn load_session_file(path: &Path) -> Result<LoadedSession, String> {
    let mut contents = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_SESSION_BYTES + 1).read_to_end(&mut contents))
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if contents.len() as u64 > MAX_SESSION_BYTES {
        return Err("Saved Discord session is too large.".to_string());
    }
    let needs_resave = session_file_needs_resave(&contents);
    let plaintext = session_plaintext_bytes(path, &contents)?;
    let session = serde_json::from_slice(&plaintext)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
    Ok(LoadedSession {
        session,
        needs_resave,
    })
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
fn session_payload_for_write(_path: &Path, plaintext: &[u8]) -> Result<Vec<u8>, String> {
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

#[cfg(target_os = "linux")]
const KEYRING_SESSION_PREFIX: &[u8] = b"BRICK-DISCORD-AUTH-KEYRING-v1\n";

#[cfg(target_os = "linux")]
fn keyring_error(_error: keyring::Error) -> String {
    "Brick could not access your desktop keyring. Unlock your keyring and try again. Discord credentials will not be saved in plaintext.".to_string()
}

#[cfg(target_os = "linux")]
fn session_keyring_entry(path: &Path) -> Result<keyring::Entry, String> {
    // Separate isolated profiles without putting account IDs or tokens in attributes.
    let id = hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()));
    keyring::Entry::new("dev.isogi.brick.discord", &id).map_err(keyring_error)
}

#[cfg(target_os = "linux")]
fn delete_keyring_session(path: &Path) -> Result<(), String> {
    match session_keyring_entry(path)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_error(error)),
    }
}

#[cfg(target_os = "linux")]
fn session_payload_for_write(path: &Path, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    session_keyring_entry(path)?
        .set_secret(plaintext)
        .map_err(keyring_error)?;
    Ok(KEYRING_SESSION_PREFIX.to_vec())
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn session_payload_for_write(_path: &Path, _plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Err("Secure Discord credential storage is unavailable on this platform.".to_string())
}

#[cfg(target_os = "windows")]
fn session_plaintext_bytes(_path: &Path, contents: &[u8]) -> Result<Vec<u8>, String> {
    if contents.starts_with(PROTECTED_SESSION_PREFIX) {
        return unprotect_session_payload(&contents[PROTECTED_SESSION_PREFIX.len()..]);
    }

    Ok(contents.to_vec())
}

#[cfg(not(target_os = "windows"))]
fn session_plaintext_bytes(_path: &Path, contents: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "linux")]
    if contents == KEYRING_SESSION_PREFIX {
        return session_keyring_entry(_path)?
            .get_secret()
            .map_err(keyring_error);
    }
    // Read legacy sessions solely to migrate them to protected storage.
    Ok(contents.to_vec())
}

#[cfg(target_os = "windows")]
fn session_file_needs_resave(contents: &[u8]) -> bool {
    !contents.starts_with(PROTECTED_SESSION_PREFIX)
}

#[cfg(target_os = "linux")]
fn session_file_needs_resave(contents: &[u8]) -> bool {
    contents != KEYRING_SESSION_PREFIX
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn session_file_needs_resave(_contents: &[u8]) -> bool {
    true
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
            authorized_role_label("12345", &["1199377026168143872".to_string()], &config),
            "Raider"
        );
        assert_eq!(
            authorized_role_label("12345", &["1167061441023582258".to_string()], &config),
            "Officer"
        );
        assert_eq!(
            authorized_role_label(
                "12345",
                &[
                    "1199377026168143872".to_string(),
                    "1167061441023582258".to_string()
                ],
                &config
            ),
            "Officer"
        );
    }

    #[test]
    fn maintainer_officer_label_requires_an_allowed_guild_role() {
        let config = test_config();
        assert_eq!(
            authorized_role_label(
                "341518802208423957",
                &[super::RAIDER_ROLE_ID.to_owned()],
                &config
            ),
            "Officer"
        );
        assert_eq!(
            authorized_role_label("341518802208423957", &[], &config),
            config.role_label
        );
    }

    #[test]
    fn rejects_hostile_callback_fields() {
        assert_eq!(
            super::parse_auth_callback(br#"{"code":"valid-code"}"#).unwrap(),
            "valid-code"
        );
        for body in [
            br#"{"code":""}"#.as_slice(),
            br#"{"code":"line\nfeed"}"#,
            br#"{"code":null}"#,
            br#"{"error":"untrusted instructions"}"#,
        ] {
            assert!(super::parse_auth_callback(body).is_err());
        }
        let oversized = serde_json::json!({"code": "a".repeat(2049)});
        assert!(super::parse_auth_callback(&serde_json::to_vec(&oversized).unwrap()).is_err());
    }

    #[test]
    fn auth_client_rejects_redirects_and_oversized_callback_bodies() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/should-not-follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", super::MAX_CALLBACK_BYTES + 1),
            format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}", "x".repeat(super::MAX_CALLBACK_BYTES as usize + 1)),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let redirect = response.contains("302 Found");
            let serving = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut input = [0; 2048];
                let _ = socket.read(&mut input);
                let _ = socket.write_all(response.as_bytes());
            });
            let response = super::http_client().unwrap().get(format!("http://{address}/")).send().unwrap();
            if redirect {
                assert_eq!(response.status().as_u16(), 302);
            } else {
                assert!(crate::download::read_response(response, super::MAX_CALLBACK_BYTES, "callback").is_err());
            }
            serving.join().unwrap();
        }
    }

    #[test]
    fn refuses_oversized_saved_credentials() {
        let path = std::env::temp_dir().join(format!("brick-session-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, vec![b'x'; super::MAX_SESSION_BYTES as usize + 1]).unwrap();
        assert!(super::load_session_file(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_credentials_are_encrypted_and_tampering_is_rejected() {
        let path = std::path::Path::new("test-profile");
        let plaintext = br#"{"accessToken":"fixture-access","refreshToken":"fixture-refresh"}"#;
        let encrypted = super::session_payload_for_write(path, plaintext).unwrap();
        assert!(!encrypted.windows(14).any(|part| part == b"fixture-access"));
        assert!(!super::session_file_needs_resave(&encrypted));
        assert_eq!(
            super::session_plaintext_bytes(path, &encrypted).unwrap(),
            plaintext
        );
        assert!(super::session_file_needs_resave(plaintext));
        let mut damaged = encrypted;
        *damaged.last_mut().unwrap() ^= 1;
        assert!(super::session_plaintext_bytes(path, &damaged).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated, unlocked Secret Service test session"]
    fn linux_keyring_roundtrip_keeps_credentials_out_of_profile() {
        let path =
            std::env::temp_dir().join(format!("brick-keyring-test-{}", uuid::Uuid::new_v4()));
        let legacy = path.with_extension("json");
        let plaintext = serde_json::json!({
            "schema": 1, "clientId": "fixture", "guildId": "fixture",
            "accessToken": "fixture-access", "refreshToken": "fixture-refresh",
            "expiresAtUnix": 2000, "userId": "12345", "username": "fixture",
            "globalName": null, "guildNick": null, "roleIds": [],
            "authorizedRoleIds": [], "authorizedAtUnix": 1000
        });
        std::fs::write(&legacy, serde_json::to_vec(&plaintext).unwrap()).unwrap();
        let session = super::load_session_at(&path, &legacy).unwrap().unwrap();
        assert_eq!(session.access_token, "fixture-access");
        assert!(!legacy.exists());
        let payload = std::fs::read(&path).unwrap();
        assert_eq!(payload, super::KEYRING_SESSION_PREFIX);
        let loaded = super::load_session_at(&path, &legacy).unwrap().unwrap();
        assert_eq!(loaded.schema, super::SESSION_SCHEMA);
        assert_eq!(loaded.created_at_unix, 1000);
        assert_eq!(loaded.refresh_token, "fixture-refresh");
        super::delete_keyring_session(&path).unwrap();
        assert!(super::session_plaintext_bytes(&path, &payload).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an isolated process with no Secret Service available"]
    fn linux_keyring_failure_never_returns_plaintext_for_saving() {
        let path =
            std::env::temp_dir().join(format!("brick-keyring-test-{}", uuid::Uuid::new_v4()));
        assert!(super::session_payload_for_write(&path, b"secret fixture").is_err());
        assert!(!path.exists());
        let legacy = path.with_extension("json");
        let plaintext = serde_json::json!({
            "schema": 1, "clientId": "fixture", "guildId": "fixture",
            "accessToken": "fixture-access", "refreshToken": "fixture-refresh",
            "expiresAtUnix": 2000, "userId": "12345", "username": "fixture",
            "globalName": null, "guildNick": null, "roleIds": [],
            "authorizedRoleIds": [], "authorizedAtUnix": 1000
        });
        let bytes = serde_json::to_vec(&plaintext).unwrap();
        std::fs::write(&legacy, &bytes).unwrap();
        let error = super::load_session_at(&path, &legacy).err().unwrap();
        assert!(error.contains("keyring"));
        assert!(!path.exists());
        assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        std::fs::remove_file(legacy).unwrap();
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn config() -> AuthConfig {
        AuthConfig {
            client_id: "test-client".into(),
            guild_id: "test-guild".into(),
            guild_name: "Test".into(),
            role_label: "Raider".into(),
            allowed_role_ids: HashSet::from(["test-role".into()]),
        }
    }
    fn token(access: &str, refresh: &str, expires_in: u64) -> DiscordTokenResponse {
        DiscordTokenResponse {
            access_token: access.into(),
            refresh_token: Some(refresh.into()),
            token_type: "Bearer".into(),
            expires_in,
        }
    }
    fn authorized(session: AuthSession, roles: Vec<String>) -> Result<AuthSession, RefreshError> {
        authorize_session(
            &config(),
            session,
            DiscordUser {
                id: "123".into(),
                username: "Test".into(),
                global_name: None,
            },
            DiscordMember { nick: None, roles },
            1000,
        )
    }
    fn expired() -> AuthSession {
        authorized(
            pending_session(&config(), token("old-access", "old-refresh", 100), 500, 500),
            vec!["test-role".into()],
        )
        .unwrap()
    }

    #[test]
    fn temporary_http_errors_do_not_revoke_personal_credentials() {
        for status in [408, 429, 500, 502, 503, 504] {
            for token_endpoint in [false, true] {
                assert!(
                    refresh_http_error(
                        status,
                        br#"{"error":"temporarily_unavailable"}"#,
                        "Discord",
                        token_endpoint
                    )
                    .retryable
                );
            }
        }
        assert!(
            refresh_http_error(400, br#"{"error":"invalid_client"}"#, "Discord", true).retryable
        );
        assert!(
            refresh_http_error(400, br#"{"message":"invalid_grant"}"#, "Discord", true).retryable
        );
        assert!(
            !refresh_http_error(400, br#"{"error":"invalid_grant"}"#, "Discord", true).retryable
        );
        for status in [401, 403, 404] {
            assert!(!refresh_http_error(status, b"{}", "Discord", false).retryable);
        }
    }

    #[test]
    fn failed_refresh_preserves_the_existing_saved_credential() {
        let session = expired();
        let result = renew_session(
            &config(),
            session,
            1000,
            |refresh| {
                assert_eq!(refresh, "old-refresh");
                Err("Connection timed out".to_string().into())
            },
            |_| panic!("No identity lookup before a successful refresh"),
            |_| panic!("Do not overwrite credentials after a failed refresh"),
        );
        assert!(result.err().unwrap().retryable);
    }

    #[test]
    fn a_rotated_token_survives_failed_role_lookup_and_can_resume_after_restart() {
        let mut saved = Vec::new();
        let result = renew_session(
            &config(),
            expired(),
            1000,
            |_| Ok(token("new-access", "new-refresh", 3600)),
            |session| {
                assert_eq!(session.access_token, "new-access");
                Err(refresh_http_error(503, b"{}", "Discord roles", false))
            },
            |session| {
                saved.push(serde_json::to_vec(session).unwrap());
                Ok(())
            },
        );
        assert!(result.err().unwrap().retryable);
        assert_eq!(saved.len(), 1);
        let pending: AuthSession = serde_json::from_slice(&saved[0]).unwrap();
        assert_eq!(pending.refresh_token, "new-refresh");
        assert!(pending.authorization_pending);
        assert!(pending.authorized_role_ids.is_empty());
        assert!(pending.role_ids.is_empty());
        assert!(session_matches_config(&pending, &config()));
        assert!(!session_access_token_current(&pending, 1001));
        let result = renew_session(
            &config(),
            pending,
            1001,
            |_| {
                panic!(
                    "Retry the role lookup using the saved replacement, not another token rotation"
                )
            },
            |session| authorized(session, vec!["test-role".into()]),
            |session| {
                saved.push(serde_json::to_vec(session).unwrap());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result.created_at_unix, 500);
        assert_eq!(result.expires_at_unix, 4600);
        assert!(!result.authorization_pending);
        assert!(session_access_token_current(&result, 1001));
        assert_eq!(saved.len(), 2);
    }

    #[test]
    fn revoked_roles_never_reuse_the_previous_authorization() {
        let mut saved = Vec::new();
        let result = renew_session(
            &config(),
            expired(),
            1000,
            |_| Ok(token("new-access", "new-refresh", 3600)),
            |session| authorized(session, vec!["unrelated-role".into()]),
            |session| {
                saved.push(session.clone());
                Ok(())
            },
        );
        assert!(!result.err().unwrap().retryable);
        assert_eq!(saved.len(), 1);
        assert!(saved[0].authorized_role_ids.is_empty());
        assert!(!session_access_token_current(&saved[0], 1001));
    }

    #[test]
    fn failed_secure_storage_never_returns_a_new_authorization() {
        let result = renew_session(
            &config(),
            expired(),
            1000,
            |_| Ok(token("new-access", "new-refresh", 3600)),
            |_| panic!("Stop when pending credentials cannot be saved securely"),
            |_| Err("Keyring unavailable".into()),
        );
        assert!(result.err().unwrap().retryable);
    }

    #[test]
    fn existing_session_records_default_to_verified_and_keep_the_original_age_limit() {
        let mut value = serde_json::to_value(expired()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("authorizationPending");
        let session: AuthSession = serde_json::from_value(value).unwrap();
        assert!(!session.authorization_pending);
        assert!(session_matches_config(&session, &config()));
        assert!(session_age_expired(&session, 500 + MAX_SESSION_AGE_SECS));
    }
}
