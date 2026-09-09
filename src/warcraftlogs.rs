//! Private reports are fetched directly with the viewer's WCL authorization.
use crate::{
    credential_store::Store,
    defensives::{self, DefensiveGroup},
    download, presence,
    streams::{self, Stream},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{blocking::Client as HttpClient, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const API: &str = "https://www.warcraftlogs.com/api/v2/user";
const TOKEN: &str = "https://www.warcraftlogs.com/oauth/token";
const DAY: i64 = 86_400_000;
const CANCELLED: &str = "The Warcraft Logs request was cancelled.";

#[path = "warcraftlogs_health.rs"]
mod health;
pub(crate) use health::{HealthBand, HealthPoint, HealthTarget, HealthTrace, HealthWindow};

/// Blocking transport has a bounded timeout, but cannot be interrupted here.
/// Check both sides so obsolete requests cannot start another page or publish
/// their response. Token refresh is deliberately handled separately below.
pub(crate) fn while_current<T>(
    cancel: &AtomicBool,
    operation: impl FnOnce() -> T,
) -> Result<T, String> {
    check_cancelled(cancel)?;
    let result = operation();
    check_cancelled(cancel)?;
    Ok(result)
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub client_id: String,
    pub guild_id: u64,
    pub user_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replay {
    pub provider: streams::Provider,
    pub video_id: String,
    pub broadcast_id: String,
    pub started_at: String,
    pub available_seconds: u64,
}

impl Replay {
    pub fn start_ms(&self) -> Result<i64, String> {
        time::OffsetDateTime::parse(
            &self.started_at,
            &time::format_description::well_known::Rfc3339,
        )
        .ok()
        .and_then(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok())
        .filter(|t| *t > 0)
        .ok_or_else(|| "The recording start time is unavailable.".into())
    }

    pub fn public_url(&self, seconds: u64) -> String {
        match self.provider {
            streams::Provider::Twitch => format!(
                "https://www.twitch.tv/videos/{}?t={}s",
                self.video_id, seconds
            ),
            streams::Provider::Youtube => format!(
                "https://www.youtube.com/watch?v={}&t={}s",
                self.video_id, seconds
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pull {
    pub report: String,
    pub id: u64,
    pub encounter: u64,
    pub difficulty: u64,
    pub report_start_ms: i64,
    pub remaining: Option<f64>,
    pub name: String,
    pub kill: bool,
    pub start_ms: i64,
    pub end_ms: i64,
    #[cfg(test)]
    pub seconds: u64,
}

impl Pull {
    pub fn log_url(&self) -> String {
        format!(
            "https://www.warcraftlogs.com/reports/{}#fight={}",
            self.report, self.id
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKind {
    Deaths,
    Defensives,
}
impl EventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Deaths => "Deaths",
            Self::Defensives => "Defensives",
        }
    }
    fn api(self) -> &'static str {
        match self {
            Self::Deaths => "Deaths",
            Self::Defensives => "Casts",
        }
    }
}
#[derive(Clone, Debug)]
pub struct RaidEvent {
    pub at_ms: i64,
    pub actor: String,
    pub class: String,
    pub ability: String,
    pub ability_id: u64,
    pub target: Option<String>,
    pub kind: EventKind,
    pub group: Option<DefensiveGroup>,
}

#[derive(Clone)]
pub struct Review {
    pub replay: Replay,
    pub pulls: Vec<Pull>,
    pub timing: HashMap<String, i64>,
}

#[derive(Serialize, Deserialize)]
struct Session {
    client_id: String,
    user_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: u64,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: String,
}

pub struct Client {
    config: Option<Config>,
    session: Option<Session>,
    http: HttpClient,
    reports: HashMap<String, (Instant, Value)>,
    directory: Option<(i64, Instant, Vec<Value>)>,
    retry_at: Option<Instant>,
    events: HashMap<(String, u64, EventKind), (Instant, Vec<RaidEvent>)>,
    health: HashMap<(String, u64, i64, i64), (Instant, HealthWindow)>,
    health_targets: HashMap<(String, u64, i64, i64), (Instant, Vec<HealthTarget>)>,
    health_bands: HashMap<(String, u64, i64, i64, String), (Instant, HealthWindow)>,
    timing: crate::replay_timing::Store,
    cancel: Arc<AtomicBool>,
}

impl Client {
    pub fn new() -> Result<Self, String> {
        let http = HttpClient::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Brick (+https://github.com/IsogiE/Brick)")
            .build()
            .map_err(|_| "Couldn't initialize Warcraft Logs.")?;
        Ok(Self {
            config: None,
            session: None,
            http,
            reports: HashMap::new(),
            directory: None,
            retry_at: None,
            events: HashMap::new(),
            health: HashMap::new(),
            health_targets: HashMap::new(),
            health_bands: HashMap::new(),
            timing: crate::replay_timing::Store::load(),
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Replace only after the previous operation has returned. The review UI
    /// runs one worker at a time, so a cancelled request never shares a new flag.
    pub(crate) fn set_request_cancellation(&mut self, cancel: Arc<AtomicBool>) {
        self.cancel = cancel;
    }

    fn configure(&mut self, discord_token: &str, restore: bool) -> Result<(), String> {
        let bytes = while_current(&self.cancel, || {
            streams::request(
                Method::GET,
                "/v1/streams/review/config",
                discord_token,
                None,
            )
        })?
        .map_err(|e| {
            if e.access_denied {
                self.session = None;
                self.reports.clear();
                self.events.clear();
                self.health.clear();
                self.health_targets.clear();
                self.health_bands.clear();
                self.directory = None;
            }
            e.message
        })?;
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|_| "Couldn't read the Warcraft Logs configuration.")?;
        if config.client_id.is_empty() {
            return Err("Warcraft Logs is not available on this server yet.".into());
        }
        if config.client_id.len() > 128
            || !config
                .client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || config.guild_id == 0
            || config.guild_id > i32::MAX as u64
            || config.user_id.is_empty()
            || config.user_id.len() > 20
            || !config.user_id.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("Invalid Warcraft Logs configuration.".into());
        }
        if self.config.as_ref() != Some(&config) || !restore {
            self.session = None;
            self.reports.clear();
            self.events.clear();
            self.health.clear();
            self.health_targets.clear();
            self.health_bands.clear();
            self.directory = None;
            if let Some(bytes) = if restore {
                while_current(&self.cancel, || store(&config)?.load())??
            } else {
                None
            } {
                let session: Session = serde_json::from_slice(&bytes)
                    .map_err(|_| "Please reconnect Warcraft Logs.")?;
                if session.client_id == config.client_id
                    && session.user_id == config.user_id
                    && credential(&session.access_token)
                    && session.refresh_token.as_ref().is_none_or(|t| credential(t))
                {
                    self.session = Some(session);
                }
            }
            // A locked keyring must remain retryable after it is unlocked.
            self.config = Some(config);
        }
        check_cancelled(&self.cancel)?;
        Ok(())
    }

    pub fn connected(&self) -> bool {
        self.session.is_some()
    }

    pub fn disconnect(&mut self, discord_token: &str) -> Result<(), String> {
        self.configure(discord_token, false)?;
        store(self.config.as_ref().unwrap())?.remove()?;
        self.session = None;
        self.reports.clear();
        self.events.clear();
        self.health.clear();
        self.health_targets.clear();
        self.health_bands.clear();
        self.directory = None;
        Ok(())
    }

    pub fn login(
        &mut self,
        discord_token: &str,
        ctx: &eframe::egui::Context,
        cancel: &Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.configure(discord_token, false)?;
        let config = self.config.clone().unwrap();
        let state = random();
        let verifier = random();
        let redirect = presence::endpoint_url("/warcraftlogs/callback")?.to_string();
        let mut url = url::Url::parse("https://www.warcraftlogs.com/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", config.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("state", &state),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge(&verifier)),
        ]);
        crate::browser::open(url.as_str())?;
        ctx.request_repaint();
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut code = None;
        while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(750));
            let body = while_current(cancel, || {
                streams::request(
                    Method::GET,
                    &format!("/v1/streams/review/callback?state={state}"),
                    discord_token,
                    None,
                )
            })?
            .map_err(|e| e.message)?;
            let value: Value =
                serde_json::from_slice(&body).map_err(|_| "Invalid Warcraft Logs callback.")?;
            if value.get("error").is_some() {
                return Err("Warcraft Logs sign-in was cancelled.".into());
            }
            if let Some(value) = value.get("code").and_then(Value::as_str) {
                if !credential(value) {
                    return Err("Invalid Warcraft Logs authorization code.".into());
                }
                code = Some(value.to_owned());
                break;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Warcraft Logs sign-in was cancelled.".into());
        }
        let code = code.ok_or("Warcraft Logs sign-in timed out. Try again.")?;
        let token = self.token_request(&[
            ("grant_type", "authorization_code"),
            ("client_id", &config.client_id),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
            ("code", &code),
        ])?;
        if !cancel.load(Ordering::Relaxed) {
            self.save_token(token, None)?;
        }
        Ok(())
    }

    fn token_request(&self, fields: &[(&str, &str)]) -> Result<Token, String> {
        check_cancelled(&self.cancel)?;
        // Once a refresh has started, retain its successful response even if a
        // read is cancelled: providers may invalidate the previous refresh token.
        // access_token saves the replacement before gating further report reads.
        let response = self
            .http
            .post(TOKEN)
            .timeout(Duration::from_secs(15))
            .form(fields)
            .send()
            .map_err(|_| "Couldn't reach Warcraft Logs sign-in.")?;
        if !response.status().is_success() {
            return Err("Warcraft Logs sign-in expired or was rejected. Please reconnect.".into());
        }
        let body = download::read_response(response, 64 * 1024, "Warcraft Logs sign-in")?;
        let token: Token =
            serde_json::from_slice(&body).map_err(|_| "Invalid Warcraft Logs sign-in response.")?;
        if !credential(&token.access_token)
            || token.refresh_token.as_ref().is_some_and(|s| !credential(s))
            || !token.token_type.eq_ignore_ascii_case("bearer")
            || token.expires_in <= 60
            || token.expires_in > 366 * 86400
        {
            return Err("Invalid Warcraft Logs sign-in response.".into());
        }
        Ok(token)
    }

    fn save_token(&mut self, token: Token, previous_refresh: Option<String>) -> Result<(), String> {
        let config = self.config.as_ref().unwrap();
        let session = Session {
            client_id: config.client_id.clone(),
            user_id: config.user_id.clone(),
            access_token: token.access_token,
            refresh_token: token.refresh_token.or(previous_refresh),
            expires_at: now_secs() + token.expires_in - 60,
        };
        store(config)?.save(
            &serde_json::to_vec(&session).map_err(|_| "Couldn't save Warcraft Logs sign-in.")?,
        )?;
        self.session = Some(session);
        Ok(())
    }

    fn access_token(&mut self) -> Result<String, String> {
        check_cancelled(&self.cancel)?;
        let session = self
            .session
            .as_ref()
            .ok_or("Connect Warcraft Logs to see this raid's pulls.")?;
        if session.expires_at <= now_secs() {
            let Some(refresh) = session.refresh_token.clone() else {
                self.session = None;
                self.reports.clear();
                self.events.clear();
                self.health.clear();
                self.health_targets.clear();
                self.health_bands.clear();
                self.directory = None;
                return Err("Please reconnect Warcraft Logs.".into());
            };
            let id = session.client_id.clone();
            let token = match self.token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &id),
                ("refresh_token", &refresh),
            ]) {
                Ok(token) => token,
                Err(error) => {
                    if error != CANCELLED {
                        self.session = None;
                        self.reports.clear();
                        self.events.clear();
                        self.health.clear();
                        self.health_targets.clear();
                        self.health_bands.clear();
                        self.directory = None;
                    }
                    return Err(error);
                }
            };
            self.save_token(token, Some(refresh))?;
        }
        check_cancelled(&self.cancel)?;
        Ok(self.session.as_ref().unwrap().access_token.clone())
    }

    fn query(&mut self, query: &str, variables: Value) -> Result<Value, String> {
        self.query_before(query, variables, None, None)
    }

    fn query_before(
        &mut self,
        query: &str,
        variables: Value,
        deadline: Option<Instant>,
        prepared_token: Option<&str>,
    ) -> Result<Value, String> {
        check_cancelled(&self.cancel)?;
        health::request_timeout(deadline)?;
        if self.retry_at.is_some_and(|until| Instant::now() < until) {
            return Err("Warcraft Logs is busy. Brick will retry shortly.".into());
        }
        let token = match prepared_token {
            Some(token) => token.to_owned(),
            None => self.access_token()?,
        };
        let timeout = health::request_timeout(deadline)?;
        let response = while_current(&self.cancel, || {
            self.http
                .post(API)
                .timeout(timeout)
                .bearer_auth(token)
                .json(&json!({ "query": query, "variables": variables }))
                .send()
        })?
        .map_err(|_| "Couldn't reach Warcraft Logs. Brick will retry shortly.")?;
        health::request_timeout(deadline)?;
        let status = response.status().as_u16();
        if status == 429 {
            let delay = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(30, 3600);
            self.retry_at = Some(Instant::now() + Duration::from_secs(delay));
            return Err("Warcraft Logs is busy. Brick will retry shortly.".into());
        }
        if status == 401 || status == 403 {
            self.session = None;
            self.reports.clear();
            self.events.clear();
            self.health.clear();
            self.health_targets.clear();
            self.health_bands.clear();
            self.directory = None;
        }
        if status == 401 {
            return Err("Please reconnect Warcraft Logs.".into());
        }
        if status == 403 {
            return Err("Your Warcraft Logs account cannot access these reports.".into());
        }
        if !(200..300).contains(&status) {
            return Err("Warcraft Logs is temporarily unavailable.".into());
        }
        let bytes = while_current(&self.cancel, || {
            download::read_response(response, 4 * 1024 * 1024, "Warcraft Logs")
        })??;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Warcraft Logs returned an invalid response.")?;
        check_cancelled(&self.cancel)?;
        health::request_timeout(deadline)?;
        if value
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|e| !e.is_empty())
        {
            // Provider errors may contain sensitive data. Never reflect or log them.
            return Err(
                "Warcraft Logs couldn't load these reports. Check your guild access and try again."
                    .into(),
            );
        }
        value
            .get("data")
            .cloned()
            .ok_or_else(|| "Warcraft Logs returned an invalid response.".into())
    }

    pub fn review(&mut self, discord_token: &str, stream: &Stream) -> Result<Review, String> {
        self.configure(discord_token, true)?;
        self.access_token()?;
        let path = format!(
            "/v1/streams/review/{}/{}",
            stream.user_id,
            stream.provider.key()
        );
        let bytes = while_current(&self.cancel, || {
            streams::request(Method::GET, &path, discord_token, None)
        })?
        .map_err(|e| e.message)?;
        let replay: Replay =
            serde_json::from_slice(&bytes).map_err(|_| "Couldn't read this broadcast's replay.")?;
        if replay.provider != stream.provider || !valid_video(&replay) {
            return Err("The replay does not match this stream.".into());
        }
        self.review_replay(replay)
    }

    pub fn events(
        &mut self,
        discord_token: &str,
        pull: &Pull,
        kind: EventKind,
    ) -> Result<Vec<RaidEvent>, String> {
        self.configure(discord_token, true)?;
        self.access_token()?;
        let key = (pull.report.clone(), pull.id, kind);
        self.events
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        if let Some((_, events)) = self.events.get(&key) {
            return Ok(events.clone());
        }
        if self.events.len() >= 20 {
            self.events.clear();
        }
        let mut start = pull.start_ms - pull.report_start_ms;
        let end = pull.end_ms - pull.report_start_ms;
        let mut result = Vec::new();
        for page in 0..10 {
            check_cancelled(&self.cancel)?;
            let data = self.query("query($code:String!,$fight:Int!,$type:EventDataType!,$start:Float!,$end:Float!,$filter:String){reportData{report(code:$code){masterData{actors{id name type subType} abilities{gameID name}} events(fightIDs:[$fight],dataType:$type,startTime:$start,endTime:$end,filterExpression:$filter,limit:2000){data nextPageTimestamp}}}}", json!({"code":pull.report,"fight":pull.id,"type":kind.api(),"start":start,"end":end,"filter":(kind == EventKind::Defensives).then(defensives::filter_expression)}))?;
            let report = &data["reportData"]["report"];
            result.extend(map_events(
                &report["events"]["data"],
                &report["masterData"],
                pull,
                kind,
            )?);
            let next = &report["events"]["nextPageTimestamp"];
            if next.is_null() {
                break;
            }
            let next = number_ms(next)
                .filter(|n| *n > start && *n <= end)
                .ok_or("Invalid Warcraft Logs event timing.")?;
            if page == 9 {
                return Err("This pull has too many events to display. Try Deaths.".into());
            }
            start = next;
        }
        result.sort_by_key(|e| e.at_ms);
        check_cancelled(&self.cancel)?;
        self.events.insert(key, (Instant::now(), result.clone()));
        Ok(result)
    }

    pub fn align_video(
        &mut self,
        replay: &Replay,
        pull: &Pull,
        seconds: i64,
    ) -> Result<(), String> {
        self.timing.set(
            replay.provider.key(),
            &replay.video_id,
            &pull.report,
            seconds,
        )
    }

    fn review_replay(&mut self, replay: Replay) -> Result<Review, String> {
        check_cancelled(&self.cancel)?;
        let start = replay.start_ms()?;
        if replay.available_seconds == 0 || replay.available_seconds > 7 * 86400 {
            return Err("The replay isn't available yet.".into());
        }
        let window_start = (start - 2 * DAY).max(0) / DAY * DAY;
        if !self.directory.as_ref().is_some_and(|(since, at, _)| {
            *since == window_start && at.elapsed() < Duration::from_secs(60)
        }) {
            let mut reports = Vec::new();
            for page in 1..=5 {
                check_cancelled(&self.cancel)?;
                let data = self.query("query($guild:Int!,$start:Float!,$end:Float!,$page:Int!){reportData{reports(guildID:$guild,startTime:$start,endTime:$end,page:$page,limit:100){data{code startTime endTime} has_more_pages}}}",
                    json!({"guild":self.config.as_ref().unwrap().guild_id,"start":window_start,"end":now_secs()*1000,"page":page}))?;
                let batch = &data["reportData"]["reports"];
                let entries = batch["data"]
                    .as_array()
                    .ok_or("Your Warcraft Logs account could not load the guild reports.")?;
                if entries.len() > 100 {
                    return Err("Warcraft Logs returned too many reports.".into());
                }
                reports.extend(entries.iter().cloned());
                if batch["has_more_pages"].as_bool() == Some(false) {
                    break;
                }
                if page == 5 {
                    return Err(
                        "There are too many reports to match this broadcast automatically.".into(),
                    );
                }
            }
            check_cancelled(&self.cancel)?;
            self.directory = Some((window_start, Instant::now(), reports));
        }
        let end = start + replay.available_seconds as i64 * 1000;
        let reports: Vec<_> = self
            .directory
            .as_ref()
            .unwrap()
            .2
            .iter()
            .filter(|report| {
                number_ms(&report["startTime"]).is_some_and(|s| s < end)
                    && number_ms(&report["endTime"]).is_some_and(|e| e > start)
            })
            .cloned()
            .collect();
        if reports.len() > 20 {
            return Err(
                "Too many overlapping reports to match this broadcast automatically.".into(),
            );
        }
        self.reports
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(120));
        let mut pulls = Vec::new();
        for report in reports {
            check_cancelled(&self.cancel)?;
            let code = report["code"]
                .as_str()
                .filter(|s| report_code(s))
                .ok_or("Invalid Warcraft Logs report.")?;
            if !self
                .reports
                .get(code)
                .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(60))
            {
                let data = self.query("query($code:String!){reportData{report(code:$code){code startTime endTime fights{ id encounterID difficulty name kill fightPercentage startTime endTime }}}}", json!({"code":code}))?;
                let report = data["reportData"]["report"].clone();
                if report.is_null() {
                    continue;
                }
                check_cancelled(&self.cancel)?;
                self.reports
                    .insert(code.to_owned(), (Instant::now(), report));
            }
            pulls.extend(map_pulls(&self.reports[code].1, &replay)?);
        }
        pulls.sort_by_key(|p| (p.start_ms, p.report.clone(), p.id));
        let mut unique: Vec<Pull> = Vec::new();
        for pull in pulls {
            if unique.iter().rev().take(12).any(|p| {
                p.report != pull.report
                    && p.encounter == pull.encounter
                    && p.difficulty == pull.difficulty
                    && (p.start_ms - pull.start_ms).abs() < 3000
                    && (p.end_ms - pull.end_ms).abs() < 3000
            }) {
                continue;
            }
            unique.push(pull);
        }
        let timing = unique
            .iter()
            .map(|pull| {
                (
                    pull.report.clone(),
                    self.timing
                        .get(replay.provider.key(), &replay.video_id, &pull.report),
                )
            })
            .collect();
        check_cancelled(&self.cancel)?;
        Ok(Review {
            replay,
            pulls: unique,
            timing,
        })
    }
}

pub fn map_pulls(report: &Value, replay: &Replay) -> Result<Vec<Pull>, String> {
    if replay.available_seconds > 7 * 86400 {
        return Err("Invalid replay duration.".into());
    }
    let code = report["code"]
        .as_str()
        .filter(|s| report_code(s))
        .ok_or("Invalid Warcraft Logs report.")?;
    let start = number_ms(&report["startTime"]).ok_or("Invalid Warcraft Logs report timing.")?;
    let replay_start = replay.start_ms()?;
    let end = replay_start
        .checked_add(replay.available_seconds as i64 * 1000)
        .ok_or("Invalid replay duration.")?;
    let fights = report["fights"]
        .as_array()
        .filter(|f| f.len() <= 5000)
        .ok_or("Invalid Warcraft Logs fight list.")?;
    let mut pulls = Vec::new();
    for fight in fights {
        let Some(encounter) = fight["encounterID"].as_u64().filter(|id| *id > 0) else {
            continue;
        };
        let Some(id) = fight["id"].as_u64().filter(|id| *id > 0) else {
            continue;
        };
        let Some(offset) = number_ms(&fight["startTime"]).filter(|v| *v >= 0) else {
            continue;
        };
        let Some(stop) = number_ms(&fight["endTime"]).filter(|v| *v > offset) else {
            continue;
        };
        let (Some(pull_start), Some(pull_end)) =
            (start.checked_add(offset), start.checked_add(stop))
        else {
            continue;
        };
        // Never clamp an uncovered pull to the start of a different recording.
        if pull_start < replay_start || pull_start >= end {
            continue;
        }
        let Some(name) = fight["name"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 300)
        else {
            continue;
        };
        pulls.push(Pull {
            report: code.to_owned(),
            id,
            encounter,
            difficulty: fight["difficulty"].as_u64().unwrap_or(0),
            report_start_ms: start,
            remaining: fight["fightPercentage"]
                .as_f64()
                .filter(|n| n.is_finite() && (0.0..=100.0).contains(n)),
            name: name.chars().filter(|c| !c.is_control()).collect(),
            kill: fight["kill"].as_bool().unwrap_or(false),
            start_ms: pull_start,
            end_ms: pull_end,
            #[cfg(test)]
            seconds: ((pull_start - replay_start) / 1000) as u64,
        });
    }
    Ok(pulls)
}

fn map_events(
    events: &Value,
    master: &Value,
    pull: &Pull,
    kind: EventKind,
) -> Result<Vec<RaidEvent>, String> {
    let entries = events
        .as_array()
        .filter(|events| events.len() <= 2000)
        .ok_or("Invalid Warcraft Logs events.")?;
    let actors = master["actors"]
        .as_array()
        .ok_or("Warcraft Logs player names are unavailable.")?;
    let abilities = master["abilities"]
        .as_array()
        .ok_or("Warcraft Logs spell names are unavailable.")?;
    let actors: HashMap<_, _> = actors
        .iter()
        .filter_map(|a| a["id"].as_u64().map(|id| (id, a)))
        .collect();
    let abilities: HashMap<_, _> = abilities
        .iter()
        .filter_map(|a| a["gameID"].as_u64().map(|id| (id, a)))
        .collect();
    Ok(entries
        .iter()
        .filter_map(|event| {
            let expected = if kind == EventKind::Deaths {
                "death"
            } else {
                "cast"
            };
            if event["type"].as_str() != Some(expected) {
                return None;
            }
            let at_ms = pull
                .report_start_ms
                .checked_add(number_ms(&event["timestamp"])?)?;
            if at_ms < pull.start_ms || at_ms > pull.end_ms {
                return None;
            }
            let actor_key = if kind == EventKind::Deaths {
                "targetID"
            } else {
                "sourceID"
            };
            let actor = actors.get(&event[actor_key].as_u64()?)?;
            if actor["type"].as_str() != Some("Player") {
                return None;
            }
            let name = clean_label(actor["name"].as_str()?);
            if name.is_empty() {
                return None;
            }
            let ability_id = event["abilityGameID"].as_u64().unwrap_or(0);
            let group = (kind == EventKind::Defensives)
                .then(|| defensives::classify(ability_id))
                .flatten();
            if kind == EventKind::Defensives && group.is_none() {
                return None;
            }
            let target = event["targetID"]
                .as_u64()
                .filter(|id| Some(*id) != event["sourceID"].as_u64())
                .and_then(|id| actors.get(&id))
                .filter(|actor| actor["type"].as_str() == Some("Player"))
                .and_then(|actor| actor["name"].as_str())
                .map(clean_label);
            let ability = abilities
                .get(&ability_id)
                .and_then(|a| a["name"].as_str())
                .map(clean_label)
                .unwrap_or_default();
            Some(RaidEvent {
                at_ms,
                actor: name,
                class: clean_label(actor["subType"].as_str().unwrap_or("")),
                ability,
                ability_id,
                target,
                kind,
                group,
            })
        })
        .collect())
}
fn clean_label(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect()
}

fn store(config: &Config) -> Result<Store, String> {
    Store::new(&format!("{}:{}", config.client_id, config.user_id))
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn credential(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
fn report_code(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_video(replay: &Replay) -> bool {
    let id = &replay.video_id;
    let broadcast = &replay.broadcast_id;
    match replay.provider {
        streams::Provider::Twitch => {
            !id.is_empty()
                && id.len() <= 30
                && id.bytes().all(|b| b.is_ascii_digit())
                && !broadcast.is_empty()
                && broadcast.len() <= 30
                && broadcast.bytes().all(|b| b.is_ascii_digit())
        }
        streams::Provider::Youtube => {
            id.len() == 11
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                && broadcast == id
        }
    }
}
fn number_ms(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 32_503_680_000_000.0)
        .map(|n| n as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_request_does_not_start_transport() {
        let cancel = AtomicBool::new(true);
        let result = while_current(&cancel, || panic!("Obsolete transport started"));
        assert_eq!(result.unwrap_err(), CANCELLED);
    }

    #[test]
    fn cancelling_stalled_transport_discards_response_and_stops_pagination() {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || -> Result<usize, String> {
            let mut accepted_pages = 0;
            for page in 0..10 {
                while_current(&worker_cancel, || {
                    started_tx.send(page).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                })?;
                accepted_pages += 1;
            }
            Ok(accepted_pages)
        });
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
        cancel.store(true, Ordering::Relaxed);
        // The blocking transport still owns this request until it returns.
        assert!(!worker.is_finished());
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err(), CANCELLED);
        assert!(started_rx.try_recv().is_err());
    }

    fn replay() -> Replay {
        Replay {
            provider: streams::Provider::Youtube,
            video_id: "abcDEF_12-3".into(),
            broadcast_id: "abcDEF_12-3".into(),
            started_at: "2026-09-08T12:00:00Z".into(),
            available_seconds: 8 * 3600,
        }
    }
    #[test]
    fn five_hours_of_streaming_before_raid_and_late_discovery_do_not_change_pull_positions() {
        let video = replay();
        let report_start = video.start_ms().unwrap() + 5 * 3_600_000;
        let report = json!({"code":"abcdefghABCDEFGH","startTime":report_start,"fights":[
            {"id":1,"encounterID":123,"difficulty":5,"name":"Boss","kill":false,"startTime":0,"endTime":120000},
            {"id":2,"encounterID":123,"difficulty":5,"name":"Boss","kill":true,"startTime":1800000,"endTime":2100000}]});
        let pulls = map_pulls(&report, &video).unwrap();
        assert_eq!(pulls[0].seconds, 5 * 3600);
        assert_eq!(pulls[1].seconds, 5 * 3600 + 30 * 60);
        assert!(pulls[1].kill);
    }
    #[test]
    fn stream_starting_mid_raid_only_offers_pulls_present_in_the_recording() {
        let mut video = replay();
        video.available_seconds = 600;
        let report = json!({"code":"abcdefghABCDEFGH","startTime":video.start_ms().unwrap()-1_200_000,"fights":[
            {"id":1,"encounterID":1,"name":"Before","startTime":0,"endTime":120000},
            {"id":2,"encounterID":1,"name":"Recorded","startTime":1200000,"endTime":1400000},
            {"id":3,"encounterID":1,"name":"Not available yet","startTime":1800000,"endTime":1900000}]});
        let pulls = map_pulls(&report, &video).unwrap();
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].id, 2);
        assert_eq!(pulls[0].seconds, 0);
    }
    #[test]
    #[ignore = "requires a local service, protected WCL sign-in and an explicit private replay fixture"]
    fn private_replay_fixture() {
        let path = std::path::PathBuf::from(
            std::env::var("BRICK_REPLAY_FIXTURE")
                .expect("Set BRICK_REPLAY_FIXTURE to a local replay metadata file"),
        );
        assert_eq!(
            presence::endpoint_url("/").unwrap().host_str(),
            Some("127.0.0.1")
        );
        let token = crate::discord_auth::current_or_refreshed_access_token()
            .unwrap()
            .unwrap();
        let replay: Replay = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(valid_video(&replay));
        let mut client = Client::new().unwrap();
        client.configure(&token, true).unwrap();
        let review = client.review_replay(replay).unwrap();
        assert!(
            !review.pulls.is_empty(),
            "No overlapping private raid pulls found"
        );
        let pulls: Vec<_> = review
            .pulls
            .iter()
            .map(|pull| {
                json!({
                    "report":pull.report,"fightId":pull.id,"name":pull.name,"kill":pull.kill,
                    "startMs":pull.start_ms,"endMs":pull.end_ms,"seconds":pull.seconds,
                    "url":review.replay.public_url(pull.seconds.saturating_sub(5))
                })
            })
            .collect();
        let mut event_samples = Vec::new();
        for index in [0, review.pulls.len() / 2, review.pulls.len() - 1] {
            let pull = &review.pulls[index];
            let deaths = client.events(&token, pull, EventKind::Deaths).unwrap();
            let defensives = client.events(&token, pull, EventKind::Defensives).unwrap();
            assert!(
                !deaths.is_empty(),
                "Expected player deaths in the selected wipe fixture"
            );
            assert!(
                !defensives.is_empty(),
                "Expected major defensive casts in the selected raid fixture"
            );
            let encode = |events: &[RaidEvent]| {
                events.iter().map(|e|json!({"atMs":e.at_ms,"actor":e.actor,"class":e.class,"ability":e.ability,"abilityId":e.ability_id,"target":e.target,"group":format!("{:?}",e.group)})).collect::<Vec<_>>()
            };
            event_samples.push(json!({"pull":index+1,"remaining":pull.remaining,"deaths":encode(&deaths),"defensives":encode(&defensives)}));
        }
        crate::atomic_file::write(
            &path.with_extension("events.json"),
            &serde_json::to_vec(&event_samples).unwrap(),
        )
        .unwrap();
        // Private validation evidence stays in the caller's local fixture directory.
        crate::atomic_file::write(
            &path.with_extension("review.json"),
            &serde_json::to_vec(&pulls).unwrap(),
        )
        .unwrap();
        eprintln!(
            "Matched {} private raid pulls to the supplied recording.",
            pulls.len()
        );
    }

    #[test]
    fn review_events_exclude_rotation_npcs_and_other_pulls_but_keep_external_context() {
        let pull = Pull {
            report: "abcdefghABCDEFGH".into(),
            id: 1,
            encounter: 1,
            difficulty: 5,
            report_start_ms: 1_000_000,
            remaining: Some(50.0),
            name: "Boss".into(),
            kill: false,
            start_ms: 1_005_000,
            end_ms: 1_015_000,
            seconds: 5,
        };
        let master = json!({"actors":[
            {"id":1,"type":"Player","name":"Healer","subType":"Priest"},
            {"id":2,"type":"Player","name":"Tank","subType":"Warrior"},
            {"id":3,"type":"NPC","name":"Enemy","subType":"Boss"}],
            "abilities":[{"gameID":33206,"name":"Pain Suppression"},{"gameID":123,"name":"Rotation"}]});
        let events = json!([
            {"type":"cast","timestamp":4000,"sourceID":1,"targetID":2,"abilityGameID":33206},
            {"type":"cast","timestamp":6000,"sourceID":1,"targetID":2,"abilityGameID":33206},
            {"type":"cast","timestamp":7000,"sourceID":3,"targetID":2,"abilityGameID":33206},
            {"type":"cast","timestamp":8000,"sourceID":1,"targetID":2,"abilityGameID":123},
            {"type":"cast","timestamp":16000,"sourceID":1,"targetID":2,"abilityGameID":33206},
            {"type":"death","timestamp":12000,"sourceID":3,"targetID":2},
            {"type":"death","timestamp":13000,"sourceID":2,"targetID":3}]);
        let casts = map_events(&events, &master, &pull, EventKind::Defensives).unwrap();
        assert_eq!(casts.len(), 1);
        assert_eq!(casts[0].at_ms, 1_006_000);
        assert_eq!(casts[0].actor, "Healer");
        assert_eq!(casts[0].target.as_deref(), Some("Tank"));
        assert_eq!(casts[0].group, Some(DefensiveGroup::External));
        let deaths = map_events(&events, &master, &pull, EventKind::Deaths).unwrap();
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].actor, "Tank");
        assert_eq!(deaths[0].group, None);
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
