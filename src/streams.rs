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
    #[serde(default)]
    pub raid_role: Option<crate::profile::RaidRole>,
    pub provider: Provider,
    pub channel_id: String,
    pub url: String,
    pub status: Status,
    pub broadcast_state: Option<String>,
    #[serde(default)]
    pub recording_id: Option<String>,
    #[serde(default)]
    pub replay_start_ms: Option<i64>,
    #[serde(default)]
    pub replay_end_ms: Option<i64>,
}

impl Stream {
    pub fn replay_range(&self) -> Option<(i64, i64)> {
        let (start, end) = (self.replay_start_ms?, self.replay_end_ms?);
        let duration = end.checked_sub(start)?;
        (start > 0 && (1..=604_800_000).contains(&duration)).then_some((start, end))
    }
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
    #[serde(default)]
    pub id: String,
    pub user_id: String,
    pub name: String,
    #[serde(default)]
    pub raid_role: Option<crate::profile::RaidRole>,
    pub provider: Provider,
    pub url: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    #[serde(default)]
    pub title: String,
}

impl Vod {
    pub fn as_stream(&self) -> Stream {
        Stream {
            user_id: self.user_id.clone(),
            name: self.name.clone(),
            raid_role: self.raid_role,
            provider: self.provider.clone(),
            channel_id: self.id.clone(),
            url: self.url.clone(),
            status: Status::Offline,
            broadcast_state: Some("ended".into()),
            recording_id: Some(self.id.clone()),
            replay_start_ms: self.started_at.as_deref().and_then(timestamp_ms),
            replay_end_ms: self.ended_at.as_deref().and_then(timestamp_ms),
        }
    }
}

fn timestamp_ms(text: &str) -> Option<i64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .and_then(|time| i64::try_from(time.unix_timestamp_nanos() / 1_000_000).ok())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recordings {
    pub vods: Vec<Vod>,
    #[serde(default)]
    pub can_delete_recordings: bool,
}

pub fn fetch_recordings(access_token: &crate::guild::Access) -> Result<Recordings, Error> {
    let body = request(Method::GET, "/v1/streams/vods", access_token, None)?;
    let mut recordings: Recordings = serde_json::from_slice(&body)
        .map_err(|_| Error::from("The recording history could not be read.".to_string()))?;
    recordings.vods.retain(valid_vod_url);
    for vod in &mut recordings.vods {
        let url = url::Url::parse(&vod.url).expect("Validated recording URL");
        let id = match vod.provider {
            Provider::Twitch => url.path().trim_start_matches("/videos/").to_owned(),
            Provider::Youtube => url
                .query_pairs()
                .find(|(key, _)| key == "v")
                .unwrap()
                .1
                .into_owned(),
        };
        if !vod.id.is_empty() && vod.id != id {
            return Err("The recording identity could not be verified."
                .to_string()
                .into());
        }
        vod.id = id;
    }
    Ok(recordings)
}

pub fn remove_recording(
    access_token: &crate::guild::Access,
    provider: &Provider,
    id: &str,
) -> Result<(), Error> {
    validate_recording_id(provider, id)?;
    request(
        Method::DELETE,
        &format!("/v1/streams/vods/{}/{id}", provider.key()),
        access_token,
        None,
    )?;
    Ok(())
}

fn validate_recording_id(provider: &Provider, id: &str) -> Result<(), Error> {
    let valid = match provider {
        Provider::Twitch => {
            !id.is_empty() && id.len() <= 30 && id.bytes().all(|b| b.is_ascii_digit())
        }
        Provider::Youtube => {
            id.len() == 11
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
    };
    if valid {
        Ok(())
    } else {
        Err("Invalid recording.".to_string().into())
    }
}

pub fn review_path(stream: &Stream) -> Result<String, Error> {
    player_path(&stream.user_id, &stream.provider)?;
    if let Some(id) = &stream.recording_id {
        validate_recording_id(&stream.provider, id)?;
        Ok(format!(
            "/v1/streams/vods/{}/{}/{id}/review",
            stream.user_id,
            stream.provider.key()
        ))
    } else {
        Ok(format!(
            "/v1/streams/review/{}/{}",
            stream.user_id,
            stream.provider.key()
        ))
    }
}

pub fn player_url_for_stream(
    stream: &Stream,
    token: &crate::guild::Access,
) -> Result<String, Error> {
    player_url_for_stream_using(stream, |path| token.endpoint(path))
}

fn player_url_for_stream_using(
    stream: &Stream,
    endpoint: impl FnOnce(&str) -> Result<url::Url, String>,
) -> Result<String, Error> {
    let mut url = endpoint(&player_path(&stream.user_id, &stream.provider)?)?;
    if let Some(id) = &stream.recording_id {
        validate_recording_id(&stream.provider, id)?;
        url.query_pairs_mut().append_pair("recording", id);
    }
    Ok(url.into())
}

fn valid_vod_url(vod: &Vod) -> bool {
    let Ok(url) = url::Url::parse(&vod.url) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || vod.user_id.is_empty()
        || vod.user_id.len() > 20
        || !vod.user_id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    match vod.provider {
        Provider::Twitch => {
            url.host_str() == Some("www.twitch.tv")
                && url.query().is_none()
                && url
                    .path()
                    .strip_prefix("/videos/")
                    .is_some_and(|id| validate_recording_id(&Provider::Twitch, id).is_ok())
        }
        Provider::Youtube => {
            url.host_str() == Some("www.youtube.com")
                && url.path() == "/watch"
                && url.query_pairs().count() == 1
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

pub fn fetch(access_token: &crate::guild::Access) -> Result<Snapshot, Error> {
    let body = request(Method::GET, "/v1/streams", access_token, None)?;
    serde_json::from_slice(&body).map_err(|_| {
        "The stream service returned an invalid response."
            .to_string()
            .into()
    })
}

pub fn save(access_token: &crate::guild::Access, url: &str) -> Result<(), Error> {
    request(
        Method::PUT,
        "/v1/streams/me",
        access_token,
        Some(serde_json::json!({"url": url})),
    )?;
    Ok(())
}

pub fn remove(access_token: &crate::guild::Access, provider: &Provider) -> Result<(), Error> {
    request(
        Method::DELETE,
        "/v1/streams/me",
        access_token,
        Some(serde_json::json!({"provider":provider})),
    )?;
    Ok(())
}

fn player_path(user_id: &str, provider: &Provider) -> Result<String, Error> {
    if user_id.is_empty() || user_id.len() > 20 || !user_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("This stream could not be opened.".to_string().into());
    }
    Ok(format!("/v1/streams/player/{user_id}/{}", provider.key()))
}

pub(crate) fn request(
    method: Method,
    path: &str,
    token: &crate::guild::Access,
    body: Option<serde_json::Value>,
) -> Result<Vec<u8>, Error> {
    let recording_removal = method == Method::DELETE && path.starts_with("/v1/streams/vods/");
    let mut request = presence::http_client()?
        .request(method, token.endpoint(path)?)
        .timeout(std::time::Duration::from_secs(20))
        .bearer_auth(token.secret());
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
        let message = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .filter(|s| !s.is_empty() && s.len() <= 512)
            .unwrap_or_else(|| format!("Stream request failed ({}).", status.as_u16()));
        return Err(Error {
            message,
            access_denied: status.as_u16() == 401 || (status.as_u16() == 403 && !recording_removal),
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_bounds_are_optional_and_validated_without_losing_milliseconds() {
        let vod: Vod = serde_json::from_value(serde_json::json!({
            "id":"abcDEF_12-3", "userId":"11", "name":"Guildmate", "provider":"youtube",
            "url":"https://www.youtube.com/watch?v=abcDEF_12-3",
            "startedAt":"2026-09-08T18:10:42.125Z", "endedAt":"2026-09-08T21:10:42.250Z"
        }))
        .unwrap();
        let mut stream = vod.as_stream();
        let (start, end) = stream.replay_range().unwrap();
        assert_eq!(end - start, 10_800_125);
        assert_eq!(start % 1000, 125);
        for (a, b) in [
            (Some(start), None),
            (None, Some(end)),
            (Some(start), Some(start)),
            (Some(end), Some(start)),
            (Some(-1), Some(end)),
            (Some(start), Some(start + 604_800_001)),
            (Some(i64::MIN), Some(i64::MAX)),
        ] {
            stream.replay_start_ms = a;
            stream.replay_end_ms = b;
            assert_eq!(stream.replay_range(), None);
        }
    }

    #[test]
    fn player_paths_cannot_escape_authenticated_endpoint() {
        for id in [
            "",
            "../roster",
            "1?token=bad",
            "https://evil.example",
            "123456789012345678901",
        ] {
            assert!(player_path(id, &Provider::Twitch).is_err());
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
    #[test]
    fn saved_recording_keeps_its_identity_in_review_and_player_routes() {
        let endpoint = |path: &str| {
            url::Url::parse("https://brick.example.test")
                .unwrap()
                .join(path)
                .map_err(|error| error.to_string())
        };
        let vod: Vod = serde_json::from_value(serde_json::json!({
            "id":"abcDEF_12-3", "userId":"11", "name":"Guildmate", "provider":"youtube",
            "url":"https://www.youtube.com/watch?v=abcDEF_12-3", "title":"Saved raid"
        }))
        .unwrap();
        assert!(valid_vod_url(&vod));
        let stream = vod.as_stream();
        assert_eq!(stream.status, Status::Offline);
        assert_eq!(
            review_path(&stream).unwrap(),
            "/v1/streams/vods/11/youtube/abcDEF_12-3/review"
        );
        let url =
            url::Url::parse(&player_url_for_stream_using(&stream, endpoint).unwrap()).unwrap();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://brick.example.test"
        );
        assert_eq!(url.path(), "/v1/streams/player/11/youtube");
        assert_eq!(url.query(), Some("recording=abcDEF_12-3"));
        for bad in [
            "../secret",
            "a&at=1",
            "https://evil.example",
            "",
            "x".repeat(32).as_str(),
        ] {
            let mut invalid = stream.clone();
            invalid.recording_id = Some(bad.into());
            assert!(review_path(&invalid).is_err());
            assert!(player_url_for_stream_using(&invalid, endpoint).is_err());
        }
    }
}
