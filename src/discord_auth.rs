use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{form_urlencoded, Url};
use uuid::Uuid;

use crate::addon;

const API_BASE: &str = "https://discord.com/api/v10";
const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const APP_USER_AGENT: &str = "Brick/0.2 (+https://github.com/IsogiE/Brick-Releases)";
const REDIRECT_PORT: u16 = 53631;
const REDIRECT_PATH: &str = "/discord/callback";
const SESSION_FILE: &str = "discord-auth.json";
const LOGIN_TIMEOUT_SECS: u64 = 180;
const EXPIRY_SAFETY_SECS: u64 = 60;
const ADVANCE_GUILD_ID: &str = "1166119057993515100";

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
    user_id: String,
    username: String,
    global_name: Option<String>,
    guild_nick: Option<String>,
    role_ids: Vec<String>,
    authorized_role_ids: Vec<String>,
    authorized_at_unix: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
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

    if session_is_current(&session) {
        return Ok(SessionStatus::Authorized(authorized_user(
            &session, &config,
        )));
    }

    Ok(SessionStatus::NeedsRefresh)
}

pub fn login_with_browser() -> Result<AuthorizedUser, String> {
    let config = auth_config()?;
    let request = login_request(&config)?;
    let listener = bind_callback_listener()?;
    open_browser(&request.authorize_url)?;
    let code = wait_for_callback(listener, &request.state)?;
    let token = exchange_code(&config, &code, &request.verifier)?;
    let session = verified_session_from_token(&config, token)?;
    save_session(&session)?;
    Ok(authorized_user(&session, &config))
}

pub fn refresh_saved_session() -> Result<AuthorizedUser, String> {
    let config = auth_config()?;
    let Some(session) = load_session()? else {
        return Err("Please sign in with Discord.".to_string());
    };

    if session_matches_config(&session, &config) && session_is_current(&session) {
        return Ok(authorized_user(&session, &config));
    }

    let token = match refresh_token(&config, &session.refresh_token) {
        Ok(token) => token,
        Err(error) => {
            let _ = clear_session();
            return Err(error);
        }
    };

    let session = match verified_session_from_token(&config, token) {
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
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| format!("Failed to remove {}: {error}", path.display()))?;
    }
    Ok(())
}

pub fn session_expired(expires_at_unix: u64) -> bool {
    token_expired(expires_at_unix, now_unix_secs())
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
    let mut url = Url::parse(AUTHORIZE_URL)
        .map_err(|error| format!("Failed to build Discord login URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri())
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

fn bind_callback_listener() -> Result<TcpListener, String> {
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|error| {
        format!(
            "Brick could not open the local Discord login callback port {REDIRECT_PORT}: {error}"
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Failed to configure Discord login callback: {error}"))?;
    Ok(listener)
}

fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(LOGIN_TIMEOUT_SECS);

    loop {
        if Instant::now() >= deadline {
            return Err("Discord login timed out.".to_string());
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let result = read_callback_code(&mut stream, expected_state);
                let page = if result.is_ok() {
                    callback_page("Brick login complete", "You can return to Brick.")
                } else {
                    callback_page("Brick login failed", "Return to Brick and try again.")
                };
                let _ = write_http_response(&mut stream, &page);
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("Discord login callback failed: {error}")),
        }
    }
}

fn read_callback_code(
    stream: &mut std::net::TcpStream,
    expected_state: &str,
) -> Result<String, String> {
    let mut buffer = [0_u8; 8192];
    let count = stream
        .read(&mut buffer)
        .map_err(|error| format!("Failed to read Discord login callback: {error}"))?;
    let request = String::from_utf8_lossy(&buffer[..count]);
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| "Discord login callback was empty.".to_string())?;
    let target = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "Discord login callback was malformed.".to_string())?;

    let callback_url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|error| format!("Discord login callback URL was invalid: {error}"))?;

    if callback_url.path() != REDIRECT_PATH {
        return Err("Discord login callback path did not match Brick.".to_string());
    }

    let params = callback_url
        .query()
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if let Some((_, error)) = params.iter().find(|(key, _)| key == "error") {
        return Err(format!("Discord rejected the login: {error}"));
    }

    let state = params
        .iter()
        .find_map(|(key, value)| (key == "state").then_some(value.as_str()))
        .ok_or_else(|| "Discord login callback did not include state.".to_string())?;
    if state != expected_state {
        return Err("Discord login state did not match.".to_string());
    }

    params
        .iter()
        .find_map(|(key, value)| (key == "code").then(|| value.clone()))
        .ok_or_else(|| "Discord login callback did not include an authorization code.".to_string())
}

fn exchange_code(
    config: &AuthConfig,
    code: &str,
    verifier: &str,
) -> Result<DiscordTokenResponse, String> {
    let redirect_uri = redirect_uri();
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
        schema: 1,
        client_id: config.client_id.clone(),
        guild_id: config.guild_id.clone(),
        access_token: token.access_token,
        refresh_token: token.refresh_token.unwrap_or_default(),
        expires_at_unix: now.saturating_add(token.expires_in),
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
        role_label: config.role_label.clone(),
        expires_at_unix: session.expires_at_unix,
    }
}

fn session_matches_config(session: &AuthSession, config: &AuthConfig) -> bool {
    session.schema == 1
        && session.client_id == config.client_id
        && session.guild_id == config.guild_id
        && session
            .authorized_role_ids
            .iter()
            .any(|role_id| config.allowed_role_ids.contains(role_id))
}

fn session_is_current(session: &AuthSession) -> bool {
    !token_expired(session.expires_at_unix, now_unix_secs())
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
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let session = serde_json::from_str(&contents)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
    Ok(Some(session))
}

fn save_session(session: &AuthSession) -> Result<(), String> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(session)
        .map_err(|error| format!("Failed to serialize Discord session: {error}"))?;
    write_private_file(&path, &json)
}

fn write_private_file(path: &PathBuf, contents: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("Failed to open {}: {error}", path.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
        file.write_all(b"\n")
            .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        fs::write(path, format!("{contents}\n"))
            .map_err(|error| format!("Failed to write {}: {error}", path.display()))
    }
}

fn session_path() -> Result<PathBuf, String> {
    Ok(addon::config_dir()?.join(SESSION_FILE))
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(APP_USER_AGENT)
        .build()
        .map_err(|error| format!("Failed to create Discord HTTP client: {error}"))
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

fn redirect_uri() -> String {
    format!("http://127.0.0.1:{REDIRECT_PORT}{REDIRECT_PATH}")
}

fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn callback_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>body{{margin:0;background:#15171c;color:#f1f4f8;font:16px system-ui,sans-serif;display:grid;place-items:center;height:100vh}}main{{border:1px solid #2f343d;border-radius:10px;background:#1d2026;padding:28px 34px;box-shadow:0 24px 60px rgba(0,0,0,.35)}}h1{{margin:0 0 8px;font-size:24px}}p{{margin:0;color:#b7c2d0}}</style></head><body><main><h1>{title}</h1><p>{body}</p></main></body></html>"
    )
}

fn write_http_response(stream: &mut std::net::TcpStream, body: &str) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("Failed to write Discord login callback response: {error}"))
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
    use super::{parse_allowed_role_ids, pkce_challenge, token_expired};

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
    fn pkce_challenge_is_url_safe() {
        let challenge = pkce_challenge("abc123");

        assert_eq!(challenge.len(), 43);
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
    }
}
