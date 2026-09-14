//! Identity-only account erasure. No guild selection or caller-supplied user ID.
use reqwest::{blocking::Client, Method};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PATH: &str = "/v1/account/erasure";
const MAX_BODY: u64 = 16 * 1024;
const FAILED: &str = "Couldn't request deletion. Try again.";
const JOURNAL: &str = "account-erasure-pending.json";
static BLOCK_NORMAL_REQUESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn block_normal_requests() {
    BLOCK_NORMAL_REQUESTS.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn requests_blocked() -> bool {
    BLOCK_NORMAL_REQUESTS.load(std::sync::atomic::Ordering::SeqCst)
}

fn journal_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::addon::config_dir()?.join(JOURNAL))
}

/// A pending confirmed erasure opens only the privacy flow on restart. It
/// must never resume normal account workers from the saved credentials.
pub(crate) fn pending() -> bool {
    journal_path().is_ok_and(|path| match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    })
}

pub(crate) fn prepare() -> Result<Identity, String> {
    let identity = crate::discord_auth::erasure_identity()?;
    if pending() {
        let path = journal_path()?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| FAILED)?;
        if !meta.file_type().is_file() || meta.len() > 1024 {
            return Err("The pending deletion needs local recovery.".into());
        }
        let bytes = std::fs::read(path).map_err(|_| FAILED)?;
        let saved: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| FAILED)?;
        if saved.get("userId").and_then(|value| value.as_str()) != Some(identity.user_id.as_str()) {
            return Err("Sign in to the Discord account that requested deletion.".into());
        }
    }
    Ok(identity)
}

/// This is a durable statement of the user's already-confirmed intent, not
/// authorization. The server always determines identity from the bearer.
pub(crate) fn remember(identity: &Identity) -> Result<(), String> {
    if pending() {
        block_normal_requests();
        return Ok(());
    }
    crate::atomic_file::write(
        &journal_path()?,
        &serde_json::to_vec(&json!({"version": 1, "userId": identity.user_id}))
            .map_err(|_| FAILED)?,
    )
    .map_err(|_| "Couldn't save the deletion request. Try again.")?;
    block_normal_requests();
    Ok(())
}

pub(crate) fn finish(identity: &Identity) -> Result<Receipt, String> {
    crate::discord_auth::drain_pending_session_writes()?;
    crate::addon::quiesce_for_erasure()?;
    let receipt = request(identity)?;
    // Keep protected grants retryable if either provider is unreachable.
    // Neither personal grant is ever forwarded to the Brick server.
    let youtube = crate::youtube_account::revoke_for_erasure(&identity.user_id);
    let twitch = crate::twitch_account::revoke_for_erasure(&identity.user_id);
    youtube?;
    twitch?;
    crate::local_erasure::reset()?;
    Ok(receipt)
}

pub(crate) fn revoke_token(url: &str, form: &[(&str, &str)], google: bool) -> Result<(), String> {
    let failure = "Couldn't remove a streaming connection. Try deletion again.";
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| failure)?;
    let response = client.post(url).form(form).send().map_err(|_| failure)?;
    let status = response.status();
    if status.as_u16() == 200 {
        return Ok(());
    }
    let bytes = crate::download::read_response(response, MAX_BODY, "revocation response")
        .map_err(|_| failure)?;
    if revoked_or_absent(status.as_u16(), &bytes, google) {
        Ok(())
    } else {
        Err(failure.into())
    }
}

fn revoked_or_absent(status: u16, bytes: &[u8], google: bool) -> bool {
    if status != 400 {
        return false;
    }
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    if google {
        body.get("error").and_then(|value| value.as_str()) == Some("invalid_token")
    } else {
        body.get("message").and_then(|value| value.as_str()) == Some("Invalid token")
    }
}

// Deliberately neither Debug nor Serialize: a privacy operation must never
// expose the user's bearer token in logs, receipts, URLs or guild requests.
pub(crate) struct Identity {
    token: String,
    pub(crate) user_id: String,
    pub(crate) name: String,
}

impl Identity {
    pub(crate) fn new(token: String, user_id: String, name: String) -> Result<Self, String> {
        if token.is_empty()
            || token.len() > 4096
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
            || user_id.is_empty()
            || user_id.len() > 20
            || !user_id.bytes().all(|byte| byte.is_ascii_digit())
            || name.len() > 512
        {
            return Err("Sign in with Discord to request deletion.".into());
        }
        Ok(Self {
            token,
            user_id,
            name,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Challenge {
    challenge: String,
    expires_at: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Receipt {
    request_id: String,
    status: String,
    pub(crate) active_data_deleted: bool,
    pub(crate) backups_pending: bool,
    suppression_retained: bool,
}

impl Receipt {
    fn validate(self) -> Result<Self, String> {
        if !opaque_id(&self.request_id)
            || !matches!(self.status.as_str(), "pending" | "complete")
            || !self.suppression_retained
            || (self.status == "complete" && (!self.active_data_deleted || self.backups_pending))
        {
            return Err(FAILED.into());
        }
        Ok(self)
    }
}

/// Called only after the explicit account-wide deletion confirmation.
/// A fresh server challenge binds acceptance to this verified identity. A
/// network failure never causes local credentials to be discarded first.
pub(crate) fn request(identity: &Identity) -> Result<Receipt, String> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Brick account deletion")
        .build()
        .map_err(|_| FAILED)?;
    let endpoint = crate::presence::endpoint_url(PATH)?;
    request_at(&client, endpoint, identity)
}

fn request_at(client: &Client, endpoint: url::Url, identity: &Identity) -> Result<Receipt, String> {
    let mut challenge_url = endpoint.clone();
    challenge_url.set_path(&format!("{}/challenge", endpoint.path()));
    let bytes = send(
        client,
        Method::POST,
        challenge_url,
        identity,
        Some(json!({})),
    )?;
    let challenge: Challenge = serde_json::from_slice(&bytes).map_err(|_| FAILED)?;
    validate_challenge(&challenge, now())?;
    let bytes = send(
        client,
        Method::POST,
        endpoint,
        identity,
        Some(json!({ "challenge": challenge.challenge })),
    )?;
    serde_json::from_slice::<Receipt>(&bytes)
        .map_err(|_| FAILED.to_string())?
        .validate()
}

fn send(
    client: &Client,
    method: Method,
    endpoint: url::Url,
    identity: &Identity,
    body: Option<serde_json::Value>,
) -> Result<Vec<u8>, String> {
    let mut request = client
        .request(method, endpoint)
        .bearer_auth(&identity.token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().map_err(|_| FAILED)?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Sign in with Discord to request deletion.".into());
    }
    if !status.is_success() {
        return Err(FAILED.into());
    }
    crate::download::read_response(response, MAX_BODY, "deletion response")
        .map_err(|_| FAILED.into())
}

fn opaque_id(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_challenge(challenge: &Challenge, now: i64) -> Result<(), String> {
    let expires = time::OffsetDateTime::parse(
        &challenge.expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| FAILED)?
    .unix_timestamp();
    if !opaque_id(&challenge.challenge) || expires <= now || expires > now.saturating_add(600) {
        return Err(FAILED.into());
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs().min(i64::MAX as u64) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erasure_requests_are_identity_bound_and_never_send_a_guild_or_target_user() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let challenge = "fixture_server_bound_challenge_123456";
        let expires = (time::OffsetDateTime::now_utc() + time::Duration::minutes(2))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let server = std::thread::spawn(move || {
            for (path, response) in [
                (
                    "/v1/account/erasure/challenge",
                    json!({"challenge": challenge, "expiresAt": expires}),
                ),
                (
                    "/v1/account/erasure",
                    json!({"requestId":"fixture_receipt_123456", "status":"pending",
                    "activeDataDeleted":true, "backupsPending":true, "suppressionRetained":true}),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                    assert!(bytes.len() < 8192);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break bytes.len();
                    }
                };
                let headers = std::str::from_utf8(&bytes).unwrap().to_ascii_lowercase();
                assert!(headers.starts_with(&format!("post {path} http/1.1\r\n")));
                assert!(headers.contains("authorization: bearer fixture-private-bearer\r\n"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(length < 1024);
                bytes.resize(header_end + length, 0);
                stream.read_exact(&mut bytes[header_end..]).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes[header_end..]).unwrap();
                assert_eq!(
                    body,
                    if path.ends_with("challenge") {
                        json!({})
                    } else {
                        json!({"challenge": challenge})
                    }
                );
                let response = serde_json::to_vec(&response).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let identity = Identity::new(
            "fixture-private-bearer".into(),
            "123".into(),
            "Fixture".into(),
        )
        .unwrap();
        let receipt = request_at(
            &client,
            url::Url::parse(&format!("http://{address}{PATH}")).unwrap(),
            &identity,
        )
        .unwrap();
        assert!(receipt.active_data_deleted);
        assert!(receipt.backups_pending);
        server.join().unwrap();
    }

    #[test]
    fn only_explicit_absent_token_errors_finish_an_idempotent_revocation() {
        assert!(revoked_or_absent(
            400,
            br#"{"error":"invalid_token"}"#,
            true
        ));
        assert!(revoked_or_absent(
            400,
            br#"{"message":"Invalid token"}"#,
            false
        ));
        for body in [
            br#"{"error":"invalid_client"}"#.as_slice(),
            b"not json",
            b"{}",
        ] {
            assert!(!revoked_or_absent(400, body, true));
            assert!(!revoked_or_absent(400, body, false));
        }
        assert!(!revoked_or_absent(
            500,
            br#"{"error":"invalid_token"}"#,
            true
        ));
        assert!(!revoked_or_absent(
            404,
            br#"{"message":"Invalid token"}"#,
            false
        ));
    }

    #[test]
    fn incomplete_erasure_cannot_be_reported_as_complete() {
        let receipt = |status, active, backups| {
            serde_json::from_value::<Receipt>(json!({
                "requestId": "fixture_receipt_123456", "status": status,
                "activeDataDeleted": active, "backupsPending": backups,
                "suppressionRetained": true
            }))
            .unwrap()
            .validate()
        };
        assert!(receipt("pending", false, true).is_ok());
        assert!(receipt("pending", true, true).is_ok());
        assert!(receipt("complete", false, false).is_err());
        assert!(receipt("complete", true, true).is_err());
        assert!(receipt("complete", true, false).is_ok());
    }

    #[test]
    fn challenge_must_be_bounded_and_current() {
        let mut challenge = Challenge {
            challenge: "fixture_challenge_123456".into(),
            expires_at: "2026-09-14T12:00:00Z".into(),
        };
        let expires = time::OffsetDateTime::parse(
            &challenge.expires_at,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
        .unix_timestamp();
        assert!(validate_challenge(&challenge, expires - 120).is_ok());
        assert!(validate_challenge(&challenge, expires).is_err());
        assert!(validate_challenge(&challenge, expires - 601).is_err());
        challenge.challenge = "secret\r\ninjection".into();
        assert!(validate_challenge(&challenge, expires - 120).is_err());
        assert!(Identity::new(
            "fixture\r\ncredential".into(),
            "123".into(),
            "Fixture".into()
        )
        .is_err());
    }
}
