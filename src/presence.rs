use std::{
    env,
    sync::LazyLock,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{discord_auth, download};

const APP_USER_AGENT: &str = "Brick/0.2 (+https://github.com/IsogiE/Brick-Releases)";
const REQUEST_TIMEOUT_SECS: u64 = 20;
const HEARTBEAT_INTERVAL_SECS: u64 = 60;
const MAX_ROSTER_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_STATUS_RESPONSE_BYTES: u64 = 16 * 1024;
const PRESENCE_API_URL: &str = match option_env!("BRICK_PRESENCE_API_URL") {
    Some(value) => value,
    None => "",
};

static HTTP_CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(APP_USER_AGENT)
        .build()
        .map_err(|error| format!("Failed to create roster HTTP client: {error}"))
});

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    pub generated_at: String,
    pub online_window_seconds: u64,
    pub officers: Vec<RosterMember>,
    pub raiders: Vec<RosterMember>,
    #[serde(default)]
    pub can_edit_roles: bool,
}

impl Roster {
    pub fn sort_members(&mut self) {
        for members in [&mut self.officers, &mut self.raiders] {
            members.sort_by_cached_key(|member| {
                (
                    crate::profile::role_order(member.raid_role),
                    member.name.to_lowercase(),
                    member.user_id.clone(),
                )
            });
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterMember {
    pub user_id: String,
    pub name: String,
    #[serde(default)]
    pub raid_role: Option<crate::profile::RaidRole>,
    pub role: String,
    pub online: bool,
    pub last_seen_at: Option<String>,
    pub app_version: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatRequest {
    app_version: &'static str,
    platform: &'static str,
    sent_at_unix: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiError {
    error: Option<String>,
}

pub fn configured() -> bool {
    !PRESENCE_API_URL.trim().is_empty()
}

pub fn configuration_error() -> String {
    "Brick was built without BRICK_PRESENCE_API_URL.".to_string()
}

pub fn send_heartbeat(access_token: &crate::guild::Access) -> Result<(), String> {
    let client = http_client()?;
    let url = access_token.endpoint("/v1/heartbeat")?;
    let body = HeartbeatRequest {
        app_version: env!("CARGO_PKG_VERSION"),
        platform: env::consts::OS,
        sent_at_unix: now_unix_secs(),
    };

    let response = client
        .post(url)
        .bearer_auth(access_token.secret())
        .json(&body)
        .send()
        .map_err(|error| format!("Roster heartbeat failed: {error}"))?;

    expect_success(response, "Roster heartbeat failed")
}

pub fn spawn_heartbeat_watcher() {
    if !configured() {
        return;
    }

    thread::spawn(move || loop {
        if let Ok(Some(access_token)) = discord_auth::current_or_refreshed_access_token() {
            let _ = send_heartbeat(&access_token);
        }

        thread::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    });
}

pub fn fetch_roster(access_token: &crate::guild::Access) -> Result<Roster, String> {
    let client = http_client()?;
    let url = access_token.endpoint("/v1/roster")?;
    let response = client
        .get(url)
        .bearer_auth(access_token.secret())
        .send()
        .map_err(|error| format!("Roster refresh failed: {error}"))?;

    let status = response.status();
    let max_bytes = if status.is_success() {
        MAX_ROSTER_RESPONSE_BYTES
    } else {
        MAX_STATUS_RESPONSE_BYTES
    };
    let body = download::read_response(response, max_bytes, "Roster refresh failed")?;

    if !status.is_success() {
        return Err(api_error("Roster refresh failed", status.as_u16(), &body));
    }

    serde_json::from_slice::<Roster>(&body)
        .map(|mut roster| {
            roster.sort_members();
            roster
        })
        .map_err(|error| format!("Roster refresh failed: invalid server response: {error}"))
}

pub(crate) fn profile_request(
    method: reqwest::Method,
    path: &str,
    access_token: &crate::guild::Access,
    expected_user: &str,
    body: Option<&serde_json::Value>,
) -> Result<crate::profile::Profile, String> {
    let client = http_client()?;
    for attempt in 0..2 {
        let mut request = client
            .request(method.clone(), access_token.endpoint(path)?)
            .bearer_auth(access_token.secret())
            .header("x-brick-profile-user", expected_user);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .map_err(|_| "Profile service could not be reached. Try again.".to_string())?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(5)
            .clamp(1, 10);
        let bytes = download::read_response(
            response,
            MAX_STATUS_RESPONSE_BYTES,
            "Profile request failed",
        )?;
        // Officer edits share a server-side burst limit. One bounded retry runs
        // on the existing worker so rapid choices neither vanish nor animate UI.
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
            std::thread::sleep(Duration::from_secs(retry_after));
            continue;
        }
        if !status.is_success() {
            return Err(api_error("Profile request failed", status.as_u16(), &bytes));
        }
        return serde_json::from_slice(&bytes)
            .map_err(|_| "Profile service returned invalid settings.".to_string());
    }
    unreachable!("The final profile request attempt always returns")
}

fn expect_success(response: reqwest::blocking::Response, prefix: &str) -> Result<(), String> {
    let status = response.status();
    let body = download::read_response(response, MAX_STATUS_RESPONSE_BYTES, prefix)?;

    if status.is_success() {
        Ok(())
    } else {
        Err(api_error(prefix, status.as_u16(), &body))
    }
}

fn api_error(prefix: &str, status: u16, body: &[u8]) -> String {
    if let Ok(error) = serde_json::from_slice::<ApiError>(body) {
        if let Some(message) = error.error.filter(|value| !value.trim().is_empty()) {
            return format!("{prefix}: {message}");
        }
    }

    format!("{prefix}: HTTP {status}")
}

pub(crate) fn endpoint_url(path: &str) -> Result<Url, String> {
    let base = PRESENCE_API_URL.trim();
    if base.is_empty() {
        return Err(configuration_error());
    }

    let parsed =
        Url::parse(base).map_err(|error| format!("Invalid Brick presence API URL: {error}"))?;
    if parsed.scheme() != "https" && parsed.host_str() != Some("127.0.0.1") {
        return Err("Brick presence API URL must use HTTPS.".to_string());
    }

    parsed
        .join(path)
        .map_err(|error| format!("Invalid Brick presence API endpoint: {error}"))
}

pub(crate) fn http_client() -> Result<Client, String> {
    match &*HTTP_CLIENT {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{api_error, ApiError, Roster};

    #[test]
    fn parses_roster_response() {
        let roster: Roster = serde_json::from_str(
            r#"{
                "generatedAt": "2026-09-04T14:00:00Z",
                "onlineWindowSeconds": 180,
                "officers": [{
                    "userId": "1",
                    "name": "Officer",
                    "role": "Officer",
                    "online": true,
                    "lastSeenAt": "2026-09-04T14:00:00Z",
                    "appVersion": "0.2.6",
                    "platform": "linux"
                }],
                "raiders": []
            }"#,
        )
        .unwrap();

        assert_eq!(roster.online_window_seconds, 180);
        assert_eq!(roster.officers[0].name, "Officer");
        assert!(roster.officers[0].online);
        assert!(roster.raiders.is_empty());
    }

    #[test]
    fn extracts_api_error_message() {
        let body = serde_json::to_vec(&ApiError {
            error: Some("nope".to_string()),
        })
        .unwrap();

        assert_eq!(
            api_error("Roster refresh failed", 403, &body),
            "Roster refresh failed: nope"
        );
    }
}
