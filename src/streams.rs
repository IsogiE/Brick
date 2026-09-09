use reqwest::Method;
use serde::Deserialize;

use crate::{download, presence};

#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Twitch,
    Youtube,
}

impl Provider {
    pub fn key(&self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::Youtube => "youtube",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Twitch => "Twitch",
            Self::Youtube => "YouTube",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Live,
    Offline,
    Checking,
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub user_id: String,
    pub name: String,
    pub provider: Provider,
    pub channel_id: String,
    pub url: String,
    pub status: Status,
    pub broadcast_state: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub own_streams: Vec<Stream>,
    #[serde(default)]
    pub unverified_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vod {
    pub user_id: String,
    pub name: String,
    pub provider: Provider,
    pub url: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Deserialize)]
struct Recordings {
    vods: Vec<Vod>,
}

pub fn fetch_vods(access_token: &str) -> Result<Vec<Vod>, Error> {
    let body = request(Method::GET, "/v1/streams/vods", access_token, None)?;
    let recordings: Recordings = serde_json::from_slice(&body)
        .map_err(|_| Error::from("The recording history could not be read.".to_string()))?;
    Ok(recordings
        .vods
        .into_iter()
        .filter(|vod| valid_vod_url(vod))
        .collect())
}

fn valid_vod_url(vod: &Vod) -> bool {
    let Ok(url) = url::Url::parse(&vod.url) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return false;
    }
    match vod.provider {
        Provider::Twitch => {
            url.host_str() == Some("www.twitch.tv")
                && url
                    .path()
                    .strip_prefix("/videos/")
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()))
        }
        Provider::Youtube => {
            url.host_str() == Some("www.youtube.com")
                && url.path() == "/watch"
                && url.query_pairs().filter(|(k, _)| k == "v").count() == 1
                && url
                    .query_pairs()
                    .find(|(k, _)| k == "v")
                    .is_some_and(|(_, id)| {
                        id.len() == 11
                            && id
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                    })
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub access_denied: bool,
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self {
            message,
            access_denied: false,
        }
    }
}

pub fn fetch(access_token: &str) -> Result<Snapshot, Error> {
    let body = request(Method::GET, "/v1/streams", access_token, None)?;
    serde_json::from_slice(&body).map_err(|_| {
        "The stream service returned an invalid response."
            .to_string()
            .into()
    })
}

pub fn save(access_token: &str, url: &str) -> Result<(), Error> {
    request(
        Method::PUT,
        "/v1/streams/me",
        access_token,
        Some(serde_json::json!({"url": url})),
    )?;
    Ok(())
}

pub fn remove(access_token: &str, provider: &Provider) -> Result<(), Error> {
    request(
        Method::DELETE,
        "/v1/streams/me",
        access_token,
        Some(serde_json::json!({"provider":provider})),
    )?;
    Ok(())
}

pub fn player_url(user_id: &str, provider: &Provider) -> Result<String, Error> {
    if user_id.is_empty() || user_id.len() > 20 || !user_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("This stream could not be opened.".to_string().into());
    }
    Ok(presence::endpoint_url(&format!("/v1/streams/player/{user_id}/{}", provider.key()))?.into())
}

pub(crate) fn request(
    method: Method,
    path: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> Result<Vec<u8>, Error> {
    let mut request = presence::http_client()?
        .request(method, presence::endpoint_url(path)?)
        .timeout(std::time::Duration::from_secs(20))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().map_err(|_| {
        Error::from("Couldn't reach the stream service. Brick will retry shortly.".to_string())
    })?;
    let status = response.status();
    let body = download::read_response(
        response,
        if status.is_success() {
            if path.starts_with("/v1/streams/vods") {
                32 * 1024 * 1024
            } else {
                2 * 1024 * 1024
            }
        } else {
            16 * 1024
        },
        "Stream request failed",
    )?;
    if !status.is_success() {
        let message = if status.as_u16() == 404 {
            "Streams aren't available on this server yet.".to_string()
        } else {
            serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .filter(|s| s.len() <= 512)
                .unwrap_or_else(|| format!("Stream request failed ({}).", status.as_u16()))
        };
        return Err(Error {
            message,
            access_denied: matches!(status.as_u16(), 401 | 403),
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_paths_cannot_escape_authenticated_endpoint() {
        for id in [
            "",
            "../roster",
            "1?token=bad",
            "https://evil.example",
            "123456789012345678901",
        ] {
            assert!(player_url(id, &Provider::Twitch).is_err());
        }
    }

    #[test]
    fn parses_live_list_and_private_offline_submission() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "streams": [{"userId":"1","name":"Guildmate","provider":"twitch","channelId":"guildmate","url":"https://www.twitch.tv/guildmate","title":"Raid night","status":"live","viewerCount":12}],
            "ownStreams": [{"userId":"2","name":"Me","provider":"youtube","channelId":"abcdefghijk","url":"https://www.youtube.com/watch?v=abcdefghijk","status":"offline"}],
            "providers": {"twitch":true,"youtube":true}
        })).unwrap();
        assert_eq!(snapshot.streams[0].status, Status::Live);
        assert_eq!(snapshot.own_streams[0].status, Status::Offline);
    }
}
