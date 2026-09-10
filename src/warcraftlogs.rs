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
    collections::{BTreeSet, HashMap},
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
    pub last_phase: Option<u32>,
    pub last_phase_is_intermission: bool,
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
            Self::Defensives => "Cooldowns",
        }
    }
}
#[derive(Clone, Debug)]
pub struct RaidEvent {
    pub at_ms: i64,
    pub actor_id: u64,
    pub observed_buff: bool,
    pub actor: String,
    pub class: String,
    pub ability: String,
    pub ability_id: u64,
    pub target: Option<String>,
    pub target_actor_id: Option<u64>,
    pub kind: EventKind,
    pub group: Option<DefensiveGroup>,
}

#[derive(Clone)]
pub struct Review {
    pub replay: Replay,
    pub pulls: Vec<Pull>,
    pub marker_timing: HashMap<(String, u64), crate::replay_sync::Alignment>,
}

impl Review {
    pub fn marker_alignment(&self, pull: &Pull) -> Option<crate::replay_sync::Alignment> {
        self.marker_timing
            .get(&(pull.report.clone(), pull.id))
            .copied()
    }
    pub fn pull_video_start(&self, pull: &Pull) -> f64 {
        self.marker_alignment(pull).map_or_else(
            || (pull.start_ms - self.replay.start_ms().unwrap_or(pull.start_ms)) as f64 / 1000.0,
            |alignment| alignment.video_seconds,
        )
    }
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

const EVENT_QUERY: &str = "query($code:String!,$fight:Int!,$type:EventDataType!,$start:Float!,$end:Float!,$filter:String,$master:Boolean!){reportData{report(code:$code){masterData(translate:false) @include(if:$master){actors(type:\"Player\"){id name type subType} abilities{gameID name}} events(fightIDs:[$fight],dataType:$type,startTime:$start,endTime:$end,filterExpression:$filter,translate:false,includeResources:false,limit:2000){data nextPageTimestamp}}}}";
const EVENT_TTL: Duration = Duration::from_secs(600);
const CONFIG_TTL: Duration = Duration::from_secs(60);
const EVENT_FETCH_BUDGET: Duration = Duration::from_secs(30);
fn event_request_timeout(deadline: Instant, now: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return Err("Warcraft Logs took too long to load these events. Try again shortly.".into());
    }
    Ok(remaining.min(Duration::from_secs(15)))
}
const MAX_CACHED_EVENTS: usize = 40_000;
const MAX_PULL_EVENTS: usize = 20_000;
const MAX_COVERAGE_IDS: usize = 512 + defensives::MAX_OVERRIDES;

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct EventCoverage {
    casts: BTreeSet<u64>,
    buffs: BTreeSet<u64>,
}
impl EventCoverage {
    fn selected(preferences: &defensives::Preferences) -> Self {
        let mut coverage = Self::default();
        for id in preferences.ids() {
            if let Some(rule) = preferences.rule(id).filter(|rule| rule.group.is_some()) {
                if rule.observation.accepts("cast") {
                    coverage.casts.insert(id);
                }
                if rule.observation.accepts("applybuff") {
                    coverage.buffs.insert(id);
                }
            }
        }
        coverage
    }
    fn requested(preferences: &defensives::Preferences) -> Self {
        let mut coverage = Self::selected(preferences);
        // The bounded curated catalogue is cheap as Casts and makes enabling a
        // known spell instant. Personal filters still determine every visible event.
        if !coverage.casts.is_empty() || !coverage.buffs.is_empty() {
            coverage
                .casts
                .extend(preferences.catalog.spells().map(|spell| spell.id));
        }
        coverage
    }
    fn accepts(&self, id: u64, event_type: &str) -> bool {
        match event_type {
            "cast" => self.casts.contains(&id),
            "applybuff" => self.buffs.contains(&id),
            _ => false,
        }
    }
    fn missing(&self, have: &Self) -> Self {
        Self {
            casts: self.casts.difference(&have.casts).copied().collect(),
            buffs: self.buffs.difference(&have.buffs).copied().collect(),
        }
    }
    fn merge(&self, other: &Self) -> Self {
        Self {
            casts: self.casts.union(&other.casts).copied().collect(),
            buffs: self.buffs.union(&other.buffs).copied().collect(),
        }
    }
    fn is_bounded(&self) -> bool {
        self.casts.union(&self.buffs).count() <= MAX_COVERAGE_IDS
    }
    fn queries(&self) -> Vec<(&'static str, Option<String>)> {
        [
            ("Casts", "cast", &self.casts),
            ("Buffs", "applybuff", &self.buffs),
        ]
        .into_iter()
        .filter(|(_, _, ids)| !ids.is_empty())
        .map(|(data_type, event_type, ids)| {
            (
                data_type,
                Some(format!(
                    "type = \"{event_type}\" AND ability.id IN ({})",
                    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
                )),
            )
        })
        .collect()
    }
}
#[derive(Clone)]
struct CachedEvents {
    at: Instant,
    bounds: (i64, i64),
    coverage: EventCoverage,
    raw: Arc<Vec<RaidEvent>>,
}
impl CachedEvents {
    fn render(&self, preferences: &defensives::Preferences) -> Vec<RaidEvent> {
        let mut events: Vec<_> = self
            .raw
            .iter()
            .filter_map(|event| {
                let group = if event.kind == EventKind::Defensives {
                    let rule = preferences.rule(event.ability_id)?;
                    let group = rule.group?;
                    if !rule.observation.accepts(if event.observed_buff {
                        "applybuff"
                    } else {
                        "cast"
                    }) {
                        return None;
                    }
                    Some(group)
                } else {
                    event.group
                };
                let mut visible = event.clone();
                visible.group = group;
                Some(visible)
            })
            .collect();
        normalize_cooldown_events(&mut events, preferences);
        events
    }
}
type EventCache = HashMap<(String, u64, EventKind), CachedEvents>;

struct ConfigStamp {
    token_hash: [u8; 32],
    at: Instant,
}
impl ConfigStamp {
    fn matches(&self, token: &str) -> bool {
        self.token_hash == <[u8; 32]>::from(Sha256::digest(token.as_bytes()))
    }
}
#[derive(Clone)]
struct Actor {
    name: String,
    class: String,
}
struct MasterData {
    actors: HashMap<u64, Actor>,
    abilities: HashMap<u64, String>,
}
impl MasterData {
    fn parse(value: &Value) -> Result<Self, String> {
        let rows = |name: &str| {
            value[name]
                .as_array()
                .filter(|rows| rows.len() <= 5000)
                .ok_or("Invalid Warcraft Logs actors or abilities.")
        };
        let actors = rows("actors")?
            .iter()
            .filter(|actor| actor["type"] == "Player")
            .filter_map(|actor| {
                Some((
                    actor["id"].as_u64()?,
                    Actor {
                        name: clean_label(actor["name"].as_str()?),
                        class: clean_label(actor["subType"].as_str().unwrap_or("")),
                    },
                ))
            })
            .collect();
        let abilities = rows("abilities")?
            .iter()
            .filter_map(|ability| {
                Some((
                    ability["gameID"].as_u64()?,
                    clean_label(ability["name"].as_str()?),
                ))
            })
            .collect();
        Ok(Self { actors, abilities })
    }
    fn rows(&self) -> usize {
        self.actors.len() + self.abilities.len()
    }
}
struct CachedMaster {
    at: Instant,
    through_ms: i64,
    data: Arc<MasterData>,
}
type MasterCache = HashMap<String, CachedMaster>;
fn cache_master(cache: &mut MasterCache, code: String, through_ms: i64, data: Arc<MasterData>) {
    cache.remove(&code);
    let mut rows: usize = cache.values().map(|entry| entry.data.rows()).sum();
    while cache.len() >= 8 || rows + data.rows() > 10_000 {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(removed) = cache.remove(&oldest) {
            rows -= removed.data.rows();
        }
    }
    cache.insert(
        code,
        CachedMaster {
            at: Instant::now(),
            through_ms,
            data,
        },
    );
}

#[cfg(test)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct RequestCounts {
    config: u32,
    catalogue: u32,
    graphql: u32,
    master: u32,
}

pub struct Client {
    #[cfg(test)]
    requests: RequestCounts,
    config: Option<Config>,
    config_stamp: Option<ConfigStamp>,
    masters: MasterCache,
    session: Option<Session>,
    cooldowns: Option<defensives::Preferences>,
    cooldown_catalog: defensives::CatalogCache,
    http: HttpClient,
    reports: HashMap<String, (Instant, Value)>,
    directory: Option<ReportDirectory>,
    retry_at: Option<Instant>,
    events: EventCache,
    cancel: Arc<AtomicBool>,
}

struct ReportDirectory {
    window: (i64, i64),
    loaded_at: Instant,
    reports: Vec<Value>,
}

impl ReportDirectory {
    fn covers(&self, window: (i64, i64)) -> bool {
        self.window == window && self.loaded_at.elapsed() < Duration::from_secs(15)
    }
}

fn report_window(start: i64, end: i64) -> Option<(i64, i64)> {
    if start <= 0 || end <= start || end.checked_sub(start)? > 7 * DAY {
        return None;
    }
    // Retain the lookback for reports opened before the video, while excluding
    // all unrelated raids after this archive. Day boundaries let nearby POVs
    // share the same directory without depending on today's calendar date.
    Some((
        start.saturating_sub(2 * DAY) / DAY * DAY,
        end.checked_add(DAY - 1)? / DAY * DAY,
    ))
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
            #[cfg(test)]
            requests: Default::default(),
            config: None,
            config_stamp: None,
            masters: HashMap::new(),
            session: None,
            cooldowns: None,
            cooldown_catalog: Default::default(),
            http,
            reports: HashMap::new(),
            directory: None,
            retry_at: None,
            events: HashMap::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Replace only after the previous operation has returned. The review UI
    /// runs one worker at a time, so a cancelled request never shares a new flag.
    pub(crate) fn set_request_cancellation(&mut self, cancel: Arc<AtomicBool>) {
        self.cancel = cancel;
    }

    fn configure(&mut self, discord_token: &str, restore: bool) -> Result<(), String> {
        check_cancelled(&self.cancel)?;
        if restore
            && self.config.is_some()
            && self.config_stamp.as_ref().is_some_and(|stamp| {
                stamp.matches(discord_token) && stamp.at.elapsed() < CONFIG_TTL
            })
        {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.requests.config += 1;
        }
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
                self.masters.clear();
                self.config_stamp = None;
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
            self.cooldowns = None;
            self.session = None;
            self.reports.clear();
            self.events.clear();
            self.masters.clear();
            self.config_stamp = None;
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
        self.config_stamp = Some(ConfigStamp {
            token_hash: Sha256::digest(discord_token.as_bytes()).into(),
            at: Instant::now(),
        });
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
        self.masters.clear();
        self.config_stamp = None;
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
                self.masters.clear();
                self.config_stamp = None;
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
                        self.masters.clear();
                        self.config_stamp = None;
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
        self.query_with_timeout(query, variables, Duration::from_secs(15))
    }
    fn query_with_timeout(
        &mut self,
        query: &str,
        variables: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        check_cancelled(&self.cancel)?;
        if self.retry_at.is_some_and(|until| Instant::now() < until) {
            return Err("Warcraft Logs is busy. Brick will retry shortly.".into());
        }
        let token = self.access_token()?;
        #[cfg(test)]
        {
            self.requests.graphql += 1;
            if query.contains("masterData")
                && variables.get("master").is_none_or(|value| value == true)
            {
                self.requests.master += 1;
            }
        }
        let response = while_current(&self.cancel, || {
            self.http
                .post(API)
                .timeout(timeout)
                .bearer_auth(token)
                .json(&json!({ "query": query, "variables": variables }))
                .send()
        })?
        .map_err(|_| "Couldn't reach Warcraft Logs. Brick will retry shortly.")?;
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
            self.masters.clear();
            self.config_stamp = None;
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
        self.load_cooldown_preferences(discord_token)?;
        self.access_token()?;
        let path = streams::review_path(stream).map_err(|error| error.message)?;
        let bytes = while_current(&self.cancel, || {
            streams::request(Method::GET, &path, discord_token, None)
        })?
        .map_err(|e| e.message)?;
        let replay: Replay =
            serde_json::from_slice(&bytes).map_err(|_| "Couldn't read this broadcast's replay.")?;
        if replay.provider != stream.provider
            || !valid_video(&replay)
            || stream
                .recording_id
                .as_ref()
                .is_some_and(|id| id != &replay.video_id)
        {
            return Err("The replay does not match this stream.".into());
        }
        let mut review = self.review_replay(replay)?;
        while_current(&self.cancel, || {
            crate::replay_library::lookup(discord_token, &mut review)
        })?;
        Ok(review)
    }

    pub fn cooldown_preferences(&self) -> defensives::Preferences {
        let mut preferences = self.cooldowns.clone().unwrap_or_default();
        preferences.catalog = self.cooldown_catalog.snapshot();
        preferences
    }

    fn load_cooldown_preferences(&mut self, discord_token: &str) -> Result<(), String> {
        check_cancelled(&self.cancel)?;
        let http = &self.http;
        let cancel = &self.cancel;
        #[cfg(test)]
        let requests = &mut self.requests;
        self.cooldown_catalog.refresh(Instant::now(), || {
            let endpoint = presence::endpoint_url("/v1/cooldowns/catalog")?;
            #[cfg(test)]
            {
                requests.catalogue += 1;
            }
            let response = while_current(cancel, || {
                http.get(endpoint)
                    .bearer_auth(discord_token)
                    .timeout(Duration::from_secs(5))
                    .send()
            })?
            .map_err(|_| "The cooldown catalogue is unavailable.")?;
            if !response.status().is_success() {
                return Err("The cooldown catalogue is unavailable.".into());
            }
            let bytes = while_current(cancel, || {
                download::read_response(
                    response,
                    defensives::MAX_CATALOG_BYTES,
                    "Cooldown catalogue",
                )
            })??;
            defensives::Catalog::parse(&bytes)
        });
        check_cancelled(&self.cancel)?;
        if self.cooldowns.is_none() {
            let account = &self
                .config
                .as_ref()
                .ok_or("Sign in to load cooldown filters.")?
                .user_id;
            self.cooldowns = Some(while_current(&self.cancel, || {
                defensives::Preferences::load(account)
            })??);
        }
        self.cooldowns.as_mut().unwrap().catalog = self.cooldown_catalog.snapshot();
        Ok(())
    }

    pub fn save_cooldown_preferences(
        &mut self,
        discord_token: &str,
        mut preferences: defensives::Preferences,
    ) -> Result<defensives::Preferences, String> {
        check_cancelled(&self.cancel)?;
        let expected_account = self.config.as_ref().map(|config| config.user_id.clone());
        // The caller has validated this current Discord token. If it is the same
        // account-bound token used to load the editor, a protected local save
        // needs no server, catalogue, or WCL request.
        if self.cooldowns.is_none()
            || self.config.is_none()
            || !self
                .config_stamp
                .as_ref()
                .is_some_and(|stamp| stamp.matches(discord_token))
        {
            self.configure(discord_token, true)?;
            if expected_account.as_ref().is_some_and(|account| {
                self.config
                    .as_ref()
                    .is_none_or(|config| &config.user_id != account)
            }) {
                return Err("Your account changed. Reopen the cooldown editor.".into());
            }
            self.load_cooldown_preferences(discord_token)?;
        }
        preferences.catalog = self.cooldown_catalog.snapshot();
        let account = &self
            .config
            .as_ref()
            .ok_or("Sign in to save cooldown filters.")?
            .user_id;
        while_current(&self.cancel, || preferences.save(account))??;
        // Raw cached coverage survives visibility, category and tracking edits.
        self.cooldowns = Some(preferences.clone());
        Ok(preferences)
    }

    pub fn events(
        &mut self,
        discord_token: &str,
        pull: &Pull,
        kind: EventKind,
    ) -> Result<Vec<RaidEvent>, String> {
        self.configure(discord_token, true)?;
        self.access_token()?;
        if self.cooldowns.is_none() {
            self.load_cooldown_preferences(discord_token)?;
        }
        let preferences = self.cooldown_preferences();
        let wanted = if kind == EventKind::Defensives {
            EventCoverage::requested(&preferences)
        } else {
            EventCoverage::default()
        };
        let key = (pull.report.clone(), pull.id, kind);
        let bounds = (pull.start_ms, pull.end_ms);
        self.events
            .retain(|_, entry| entry.at.elapsed() < EVENT_TTL);
        let cached = self
            .events
            .get(&key)
            .filter(|entry| entry.bounds == bounds && entry.coverage.merge(&wanted).is_bounded())
            .cloned();
        let missing = wanted.missing(
            &cached
                .as_ref()
                .map(|entry| entry.coverage.clone())
                .unwrap_or_default(),
        );
        let queries = if kind == EventKind::Deaths && cached.is_none() {
            vec![("Deaths", None)]
        } else {
            missing.queries()
        };
        if queries.is_empty() {
            return Ok(cached.map_or_else(Vec::new, |entry| entry.render(&preferences)));
        }
        // Metadata follows the fast cache path, never precedes a cache hit.
        self.masters
            .retain(|_, entry| entry.at.elapsed() < EVENT_TTL);
        let mut master = self
            .masters
            .get(&pull.report)
            .filter(|entry| entry.through_ms >= pull.end_ms)
            .map(|entry| entry.data.clone());
        let end = pull.end_ms - pull.report_start_ms;
        let mut added = Vec::new();
        let mut pages = 0;
        let deadline = Instant::now() + EVENT_FETCH_BUDGET;
        for (data_type, filter) in queries {
            let mut start = pull.start_ms - pull.report_start_ms;
            loop {
                check_cancelled(&self.cancel)?;
                if pages >= 10 {
                    return Err("This pull has too many events to display. Try Deaths.".into());
                }
                pages += 1;
                let include_master = master.is_none();
                let timeout = event_request_timeout(deadline, Instant::now())?;
                let data = self.query_with_timeout(EVENT_QUERY, json!({"code":pull.report,"fight":pull.id,"type":data_type,"start":start,"end":end,"filter":filter,"master":include_master}), timeout)?;
                let report = &data["reportData"]["report"];
                if include_master {
                    let loaded = Arc::new(MasterData::parse(&report["masterData"])?);
                    let through = self
                        .reports
                        .get(&pull.report)
                        .and_then(|(_, report)| number_ms(&report["endTime"]))
                        .unwrap_or(pull.end_ms)
                        .max(pull.end_ms);
                    cache_master(
                        &mut self.masters,
                        pull.report.clone(),
                        through,
                        loaded.clone(),
                    );
                    master = Some(loaded);
                }
                added.extend(map_event_page(
                    &report["events"]["data"],
                    master.as_ref().unwrap(),
                    pull,
                    kind,
                    &missing,
                    &preferences,
                )?);
                if added.len() + cached.as_ref().map_or(0, |entry| entry.raw.len())
                    > MAX_PULL_EVENTS
                {
                    return Err("This pull has too many events to display. Try Deaths.".into());
                }
                let next = &report["events"]["nextPageTimestamp"];
                if next.is_null() {
                    break;
                }
                start = number_ms(next)
                    .filter(|next| *next > start && *next <= end)
                    .ok_or("Invalid Warcraft Logs event timing.")?;
            }
        }
        check_cancelled(&self.cancel)?;
        let mut raw = cached
            .as_ref()
            .map_or_else(Vec::new, |entry| (*entry.raw).clone());
        raw.extend(added);
        let entry = CachedEvents {
            at: cached.as_ref().map_or_else(Instant::now, |entry| entry.at),
            bounds,
            coverage: cached
                .as_ref()
                .map_or(wanted.clone(), |entry| entry.coverage.merge(&wanted)),
            raw: Arc::new(raw),
        };
        let visible = entry.render(&preferences);
        // Publish only complete successful extensions; failures retain old coverage.
        cache_events(&mut self.events, key, entry);
        Ok(visible)
    }

    fn review_replay(&mut self, replay: Replay) -> Result<Review, String> {
        check_cancelled(&self.cancel)?;
        let start = replay.start_ms()?;
        if replay.available_seconds == 0 || replay.available_seconds > 7 * 86400 {
            return Err("The replay isn't available yet.".into());
        }
        let end = start
            .checked_add(replay.available_seconds as i64 * 1000)
            .ok_or("The recording's date range is unavailable.")?;
        let window =
            report_window(start, end).ok_or("The recording's date range is unavailable.")?;
        if !self
            .directory
            .as_ref()
            .is_some_and(|directory| directory.covers(window))
        {
            let mut reports = Vec::new();
            for page in 1..=5 {
                check_cancelled(&self.cancel)?;
                let data = self.query("query($guild:Int!,$start:Float!,$end:Float!,$page:Int!){reportData{reports(guildID:$guild,startTime:$start,endTime:$end,page:$page,limit:100){data{code startTime endTime} has_more_pages}}}",
                    json!({"guild":self.config.as_ref().unwrap().guild_id,"start":window.0,"end":window.1,"page":page}))?;
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
            self.directory = Some(ReportDirectory {
                window,
                loaded_at: Instant::now(),
                reports,
            });
        }
        let reports: Vec<_> = self
            .directory
            .as_ref()
            .unwrap()
            .reports
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
                .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(15))
            {
                let data = self.query("query($code:String!){reportData{report(code:$code){code startTime endTime fights{ id encounterID difficulty name kill lastPhase lastPhaseIsIntermission fightPercentage startTime endTime }}}}", json!({"code":code}))?;
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
        check_cancelled(&self.cancel)?;
        Ok(Review {
            marker_timing: HashMap::new(),
            replay,
            pulls: unique,
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
            last_phase: fight["lastPhase"]
                .as_u64()
                .filter(|phase| *phase > 0)
                .and_then(|phase| u32::try_from(phase).ok()),
            last_phase_is_intermission: fight["lastPhaseIsIntermission"].as_bool().unwrap_or(false),
            start_ms: pull_start,
            end_ms: pull_end,
            #[cfg(test)]
            seconds: ((pull_start - replay_start) / 1000) as u64,
        });
    }
    Ok(pulls)
}

#[cfg(test)]
fn map_events(
    events: &Value,
    master: &Value,
    pull: &Pull,
    kind: EventKind,
    preferences: &defensives::Preferences,
) -> Result<Vec<RaidEvent>, String> {
    map_event_page(
        events,
        &MasterData::parse(master)?,
        pull,
        kind,
        &EventCoverage::selected(preferences),
        preferences,
    )
}
fn map_event_page(
    events: &Value,
    master: &MasterData,
    pull: &Pull,
    kind: EventKind,
    coverage: &EventCoverage,
    preferences: &defensives::Preferences,
) -> Result<Vec<RaidEvent>, String> {
    let entries = events
        .as_array()
        .filter(|events| events.len() <= 2000)
        .ok_or("Invalid Warcraft Logs events.")?;
    let actors = &master.actors;
    let abilities = &master.abilities;
    Ok(entries
        .iter()
        .filter_map(|event| {
            let event_type = event["type"].as_str()?;
            let ability_id = event["abilityGameID"].as_u64().unwrap_or(0);
            if kind == EventKind::Deaths {
                if event_type != "death" {
                    return None;
                }
            } else if !coverage.accepts(ability_id, event_type) {
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
            let actor_id = event[actor_key].as_u64()?;
            let actor = actors.get(&actor_id)?;
            let name = actor.name.clone();
            if name.is_empty() {
                return None;
            }
            let group = (kind == EventKind::Defensives)
                .then(|| {
                    preferences
                        .classify(ability_id)
                        .or_else(|| {
                            preferences
                                .catalog
                                .spell(ability_id)
                                .map(|spell| spell.group)
                        })
                        .or(Some(DefensiveGroup::Healing))
                })
                .flatten();
            if kind == EventKind::Defensives && group.is_none() {
                return None;
            }
            let target = event["targetID"]
                .as_u64()
                .filter(|id| Some(*id) != event["sourceID"].as_u64())
                .and_then(|id| actors.get(&id))
                .map(|actor| actor.name.clone());
            let ability = abilities
                .get(&ability_id)
                .cloned()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| {
                    preferences
                        .spell_name(ability_id)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Spell {ability_id}"))
                });
            Some(RaidEvent {
                at_ms,
                actor_id,
                observed_buff: event_type == "applybuff",
                actor: name,
                class: actor.class.clone(),
                ability,
                ability_id,
                target,
                target_actor_id: event["targetID"].as_u64(),
                kind,
                group,
            })
        })
        .collect())
}
// Count both entries and events: aura-heavy pulls must not multiply the cache
// into hundreds of thousands of independently allocated player/spell labels.
fn cache_events(cache: &mut EventCache, key: (String, u64, EventKind), entry: CachedEvents) {
    if entry.raw.len() > MAX_CACHED_EVENTS {
        return;
    }
    cache.remove(&key);
    let mut rows: usize = cache.values().map(|entry| entry.raw.len()).sum();
    while cache.len() >= 20 || rows.saturating_add(entry.raw.len()) > MAX_CACHED_EVENTS {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(removed) = cache.remove(&oldest) {
            rows = rows.saturating_sub(removed.raw.len());
        }
    }
    cache.insert(key, entry);
}

/// WCL often records a cast and its aura application together. Keep the cast
/// once; a buff without a nearby cast stays explicitly identified as an observed
/// application, never as an inferred cast or remaining cooldown. Process after
/// pagination so a cast/application pair split across pages is still collapsed.
fn normalize_cooldown_events(events: &mut Vec<RaidEvent>, preferences: &defensives::Preferences) {
    events.sort_by_key(|event| event.at_ms);
    let mut casts: HashMap<(u64, u64), Vec<i64>> = HashMap::new();
    for event in events
        .iter()
        .filter(|event| event.kind == EventKind::Defensives && !event.observed_buff)
    {
        casts
            .entry((event.actor_id, event.ability_id))
            .or_default()
            .push(event.at_ms);
    }
    events.retain(|event| {
        if !event.observed_buff
            || preferences
                .rule(event.ability_id)
                .is_none_or(|rule| rule.observation != defensives::Observation::CastOrBuff)
        {
            return true;
        }
        let Some(times) = casts.get(&(event.actor_id, event.ability_id)) else {
            return true;
        };
        let index = times.partition_point(|at| *at < event.at_ms.saturating_sub(1500));
        times
            .get(index)
            .is_none_or(|at| *at > event.at_ms.saturating_add(1500))
    });
    let mut seen = std::collections::HashSet::new();
    events.retain(|event| {
        seen.insert((
            event.at_ms,
            event.actor_id,
            event.ability_id,
            event.kind,
            event.observed_buff,
            event.target_actor_id,
        ))
    });
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
    fn metadata_cache_expires_before_the_next_current_raid_refresh() {
        let mut directory = ReportDirectory {
            window: (1, 2),
            loaded_at: Instant::now(),
            reports: Vec::new(),
        };
        assert!(directory.covers((1, 2)));
        directory.loaded_at = Instant::now() - Duration::from_secs(15);
        assert!(
            !directory.covers((1, 2)),
            "New uploads must not stay cached for a minute"
        );
    }

    #[test]
    fn historical_recording_only_searches_its_own_dates_and_cache_includes_end() {
        let at = |text| {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .unix_timestamp()
                * 1000
        };
        let start = at("2022-05-04T08:00:00Z");
        let end = at("2022-05-04T23:30:00Z");
        let window = report_window(start, end).unwrap();
        assert_eq!(
            window,
            (at("2022-05-02T00:00:00Z"), at("2022-05-05T00:00:00Z"))
        );
        // More recent reports cannot consume the pagination budget for this archive.
        assert!(at("2022-06-01T18:00:00Z") > window.1);
        let directory = ReportDirectory {
            window,
            loaded_at: Instant::now(),
            reports: Vec::new(),
        };
        assert!(directory.covers(report_window(start + 3_600_000, end - 3_600_000).unwrap()));
        let crossing_midnight = report_window(start, at("2022-05-05T01:00:00Z")).unwrap();
        assert_eq!(crossing_midnight.0, window.0);
        assert!(!directory.covers(crossing_midnight));
        for (start, end) in [
            (0, DAY),
            (DAY, DAY),
            (DAY, 9 * DAY),
            (i64::MAX - 1, i64::MAX),
        ] {
            assert!(report_window(start, end).is_none());
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
    fn pull_phase_uses_wcl_numbering_and_preserves_unknown_and_intermission() {
        let video = replay();
        let fights: Vec<_> = [json!(2), json!(1), Value::Null, json!(0), json!(-1)]
            .into_iter()
            .enumerate()
            .map(|(index, phase)| {
                json!({
                    "id":index + 1,"encounterID":123,"name":"Boss",
                    "startTime":index * 180000,"endTime":index * 180000 + 120000,
                    "kill":index == 4,"lastPhase":phase,
                    "lastPhaseIsIntermission":index == 1,"lastPhaseAsAbsoluteIndex":4
                })
            })
            .collect();
        let report = json!({"code":"abcdefghABCDEFGH","startTime":video.start_ms().unwrap(),"fights":fights});
        let pulls = map_pulls(&report, &video).unwrap();
        assert_eq!(
            pulls.iter().map(|pull| pull.last_phase).collect::<Vec<_>>(),
            vec![Some(2), Some(1), None, None, None]
        );
        assert!(!pulls[0].last_phase_is_intermission);
        assert!(pulls[1].last_phase_is_intermission);
        assert!(pulls[4].kill);
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
    #[ignore = "opt-in live timing: protected sessions, selected live POV, timings/counts only"]
    fn live_event_query_timing() {
        assert_eq!(std::env::var("BRICK_WCL_TIMING").as_deref(), Ok("1"));
        let token = crate::discord_auth::current_or_refreshed_access_token()
            .expect("Protected Discord session unavailable")
            .expect("Sign in to Discord");
        let name = std::env::var("BRICK_WCL_TIMING_POV")
            .expect("Set the explicit live POV filter locally")
            .to_lowercase();
        let mut snapshot = streams::fetch(&token)
            .map_err(|_| "Stream list unavailable")
            .unwrap();
        snapshot.streams.extend(snapshot.own_streams);
        let stream = snapshot
            .streams
            .iter()
            .find(|stream| {
                stream.name.to_lowercase().contains(&name)
                    && stream.provider == streams::Provider::Twitch
            })
            .expect("Requested live POV unavailable");
        let mut client = Client::new().unwrap();
        let started = Instant::now();
        let review = client
            .review(&token, stream)
            .expect("Review metadata unavailable");
        eprintln!(
            "wcl_timing stage=metadata ms={} pulls={}",
            started.elapsed().as_millis(),
            review.pulls.len()
        );
        let pull = review.pulls.first().expect("No raid pull available");
        let filter = client.cooldown_preferences().filter_expression().unwrap();
        let legacy = "query($code:String!,$fight:Int!,$type:EventDataType!,$start:Float!,$end:Float!,$filter:String){reportData{report(code:$code){masterData{actors{id name type subType} abilities{gameID name}} events(fightIDs:[$fight],dataType:$type,startTime:$start,endTime:$end,filterExpression:$filter,limit:2000){data nextPageTimestamp}}}}";
        let narrowed = "query($code:String!,$fight:Int!,$type:EventDataType!,$start:Float!,$end:Float!,$filter:String){reportData{report(code:$code){masterData(translate:false){actors(type:\"Player\"){id name type subType} abilities{gameID name}} events(fightIDs:[$fight],dataType:$type,startTime:$start,endTime:$end,filterExpression:$filter,translate:false,includeResources:false,limit:2000){data nextPageTimestamp}}}}";
        for (label, query, kind, filter) in [
            ("legacy_deaths", legacy, "Deaths", None),
            ("narrow_deaths", narrowed, "Deaths", None),
            ("legacy_cooldowns", legacy, "All", Some(filter.as_str())),
            ("narrow_cooldowns", narrowed, "Casts", Some(filter.as_str())),
        ] {
            let started = Instant::now();
            let result = client.query(query, json!({"code":pull.report,"fight":pull.id,"type":kind,
                "start":pull.start_ms-pull.report_start_ms,"end":pull.end_ms-pull.report_start_ms,"filter":filter}));
            match result {
                Ok(value) => {
                    let page = &value["reportData"]["report"]["events"];
                    eprintln!(
                        "wcl_timing stage={label} ms={} entries={} next_page={}",
                        started.elapsed().as_millis(),
                        page["data"].as_array().map_or(0, Vec::len),
                        !page["nextPageTimestamp"].is_null()
                    );
                }
                Err(_) => eprintln!(
                    "wcl_timing stage={label} ms={} status=failed",
                    started.elapsed().as_millis()
                ),
            }
        }
        let measure = |client: &mut Client, label: &str, kind| {
            let before = client.requests;
            let started = Instant::now();
            let events = client
                .events(&token, pull, kind)
                .expect("Event query failed");
            let after = client.requests;
            eprintln!("wcl_timing stage={label} us={} visible={} config_requests={} catalogue_requests={} graphql_requests={} master_requests={}",
                started.elapsed().as_micros(), events.len(), after.config-before.config,
                after.catalogue-before.catalogue, after.graphql-before.graphql, after.master-before.master);
            after.graphql - before.graphql
        };
        assert_eq!(measure(&mut client, "cold_deaths", EventKind::Deaths), 1);
        assert!(measure(&mut client, "cold_cooldowns", EventKind::Defensives) <= 2);
        assert_eq!(measure(&mut client, "warm_deaths", EventKind::Deaths), 0);
        assert_eq!(
            measure(&mut client, "warm_cooldowns", EventKind::Defensives),
            0
        );
        let saved = client.cooldowns.clone().unwrap();
        let optional = saved
            .catalog
            .spells()
            .find(|spell| !spell.default_enabled)
            .unwrap()
            .clone();
        client.cooldowns.as_mut().unwrap().overrides.insert(
            optional.id,
            defensives::Rule {
                group: Some(optional.group),
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(
            measure(&mut client, "enable_known_cast", EventKind::Defensives),
            0
        );
        client.cooldowns.as_mut().unwrap().overrides.insert(
            optional.id,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(
            measure(&mut client, "recategorize", EventKind::Defensives),
            0
        );
        client.cooldowns.as_mut().unwrap().overrides.insert(
            optional.id,
            defensives::Rule {
                group: None,
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(
            measure(&mut client, "remove_cast", EventKind::Defensives),
            0
        );
        client.cooldowns.as_mut().unwrap().hidden_groups = DefensiveGroup::ALL.to_vec();
        assert_eq!(
            measure(&mut client, "hide_timeline", EventKind::Defensives),
            0
        );
        // A valid bounded ID absent from this catalogue exercises just the missing
        // cast coverage. No preference is written to the user's protected store.
        let new_id = (9_999_000..=9_999_999)
            .find(|id| saved.catalog.spell(*id).is_none())
            .unwrap();
        client.cooldowns.as_mut().unwrap().overrides.insert(
            new_id,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(measure(&mut client, "add_new_id", EventKind::Defensives), 1);
        assert_eq!(
            measure(&mut client, "repeat_new_id", EventKind::Defensives),
            0
        );
        client.cooldowns = Some(saved);
        eprintln!("wcl_timing total_graphql_requests={} total_config_requests={} total_catalogue_requests={}", client.requests.graphql, client.requests.config, client.requests.catalogue);
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
                    "lastPhase":pull.last_phase,"lastPhaseIsIntermission":pull.last_phase_is_intermission,
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
            last_phase: None,
            last_phase_is_intermission: false,
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
        let casts = map_events(
            &events,
            &master,
            &pull,
            EventKind::Defensives,
            &defensives::Preferences::default(),
        )
        .unwrap();
        assert_eq!(casts.len(), 1);
        assert_eq!(casts[0].at_ms, 1_006_000);
        assert_eq!(casts[0].actor, "Healer");
        assert_eq!(casts[0].target.as_deref(), Some("Tank"));
        assert_eq!(casts[0].group, Some(DefensiveGroup::External));
        let deaths = map_events(
            &events,
            &master,
            &pull,
            EventKind::Deaths,
            &defensives::Preferences::default(),
        )
        .unwrap();
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].actor, "Tank");
        assert_eq!(deaths[0].group, None);
    }

    #[test]
    fn cooldown_buff_fallbacks_do_not_duplicate_casts_or_cross_player_identities() {
        let video = replay();
        let report = json!({"code":"abcdefghABCDEFGH","startTime":video.start_ms().unwrap(),"fights":[
            {"id":1,"encounterID":123,"difficulty":5,"name":"Boss","kill":false,"startTime":0,"endTime":120000}
        ]});
        let pull = map_pulls(&report, &video).unwrap().remove(0);
        let master = json!({"actors":[
            {"id":1,"type":"Player","name":"SameName","subType":"Priest"},
            {"id":2,"type":"Player","name":"SameName","subType":"Priest"},
            {"id":3,"type":"NPC","name":"Pet","subType":"Pet"}],
            "abilities":[{"gameID":200183,"name":"Apotheosis"}]});
        let mut preferences = defensives::Preferences::default();
        // Buff fallback is an explicit choice; defaults keep a sparse cast timeline.
        preferences.overrides.insert(
            200183,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::CastOrBuff,
            },
        );
        preferences.overrides.insert(
            1234567,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Buff,
            },
        );
        let page_one = json!([
            {"type":"cast","timestamp":6000,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"cast","timestamp":6001,"sourceID":1,"targetID":1,"abilityGameID":1234567}
        ]);
        let page_two = json!([
            {"type":"applybuff","timestamp":6020,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"applybuff","timestamp":6020,"sourceID":2,"targetID":2,"abilityGameID":200183},
            {"type":"applybuff","timestamp":6020,"sourceID":3,"targetID":3,"abilityGameID":200183},
            {"type":"refreshbuff","timestamp":6500,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"removebuff","timestamp":26000,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"applybuff","timestamp":7000,"sourceID":1,"targetID":2,"abilityGameID":1234567},
            {"type":"applybuff","timestamp":7000,"sourceID":1,"targetID":2,"abilityGameID":1234567}
        ]);
        let mut events = map_events(
            &page_one,
            &master,
            &pull,
            EventKind::Defensives,
            &preferences,
        )
        .unwrap();
        events.extend(
            map_events(
                &page_two,
                &master,
                &pull,
                EventKind::Defensives,
                &preferences,
            )
            .unwrap(),
        );
        normalize_cooldown_events(&mut events, &preferences);
        assert_eq!(events.len(), 3);
        assert_eq!(
            (events[0].actor_id, events[0].observed_buff, events[0].group),
            (1, false, Some(DefensiveGroup::Healing))
        );
        assert_eq!((events[1].actor_id, events[1].observed_buff), (2, true));
        assert_eq!(events[2].ability, "Spell 1234567");
        assert!(events[2].observed_buff);
        assert_eq!(events[2].target.as_deref(), Some("SameName"));
    }

    #[test]
    fn server_catalogue_classifies_new_log_ids_and_personal_rules_take_precedence() {
        let video = replay();
        let report = json!({"code":"abcdefghABCDEFGH","startTime":video.start_ms().unwrap(),"fights":[
            {"id":1,"encounterID":123,"difficulty":5,"name":"Boss","kill":false,"startTime":0,"endTime":120000}
        ]});
        let pull = map_pulls(&report, &video).unwrap().remove(0);
        let master = json!({"actors":[{"id":1,"type":"Player","name":"Healer","subType":"Priest"}],"abilities":[]});
        let events = json!([{"type":"cast","timestamp":6000,"sourceID":1,"abilityGameID":1234567}]);
        let mut preferences = defensives::Preferences::default();
        assert!(
            map_events(&events, &master, &pull, EventKind::Defensives, &preferences)
                .unwrap()
                .is_empty()
        );
        preferences.catalog = Arc::new(defensives::Catalog::parse(br#"{"schemaVersion":1,"revision":"new-id","spells":[{"id":1234567,"name":"Server healing cooldown","category":"healing","defaultEnabled":true}]}"#).unwrap());
        let mapped =
            map_events(&events, &master, &pull, EventKind::Defensives, &preferences).unwrap();
        assert_eq!(mapped[0].group, Some(DefensiveGroup::Healing));
        assert_eq!(mapped[0].ability, "Server healing cooldown");
        preferences.overrides.insert(
            1234567,
            defensives::Rule {
                group: Some(DefensiveGroup::External),
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(
            map_events(&events, &master, &pull, EventKind::Defensives, &preferences).unwrap()[0]
                .group,
            Some(DefensiveGroup::External)
        );
        preferences.overrides.get_mut(&1234567).unwrap().group = None;
        assert!(
            map_events(&events, &master, &pull, EventKind::Defensives, &preferences)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn additional_event_pages_share_one_deadline_and_keep_the_normal_request_limit() {
        let now = Instant::now();
        let deadline = now + EVENT_FETCH_BUDGET;
        assert_eq!(
            event_request_timeout(deadline, now).unwrap(),
            Duration::from_secs(15)
        );
        assert_eq!(
            event_request_timeout(deadline, now + Duration::from_secs(27)).unwrap(),
            Duration::from_secs(3)
        );
        assert!(event_request_timeout(deadline, deadline).is_err());
        assert!(event_request_timeout(deadline, deadline + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn typed_coverage_prefetches_catalogue_casts_and_only_queries_missing_evidence() {
        let mut preferences = defensives::Preferences::default();
        let have = EventCoverage::requested(&preferences);
        assert_eq!(have.casts.len(), 99);
        assert!(have.buffs.is_empty());
        let queries = have.queries();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "Casts");
        assert!(queries[0]
            .1
            .as_ref()
            .unwrap()
            .starts_with("type = \"cast\" AND ability.id IN ("));
        preferences.overrides.insert(
            197908,
            defensives::Rule {
                group: Some(DefensiveGroup::Utility),
                observation: defensives::Observation::Cast,
            },
        );
        assert!(EventCoverage::requested(&preferences)
            .missing(&have)
            .queries()
            .is_empty());
        preferences.overrides.insert(
            200183,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::CastOrBuff,
            },
        );
        let missing = EventCoverage::requested(&preferences).missing(&have);
        assert!(missing.casts.is_empty());
        assert_eq!(
            missing.queries(),
            vec![(
                "Buffs",
                Some("type = \"applybuff\" AND ability.id IN (200183)".into())
            )]
        );
        preferences.overrides.insert(
            1234567,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Cast,
            },
        );
        let missing = EventCoverage::requested(&preferences).missing(&have.merge(&missing));
        assert_eq!(
            missing.queries(),
            vec![(
                "Casts",
                Some("type = \"cast\" AND ability.id IN (1234567)".into())
            )]
        );
        let oversized = EventCoverage {
            casts: (1..=MAX_COVERAGE_IDS as u64 + 1).collect(),
            buffs: BTreeSet::new(),
        };
        assert!(!oversized.is_bounded());
    }

    #[test]
    fn cached_raw_events_survive_tracking_categories_and_cast_buff_switches_without_transport() {
        let video = replay();
        let report = json!({"code":"abcdefghABCDEFGH","startTime":video.start_ms().unwrap(),"fights":[
            {"id":1,"encounterID":123,"difficulty":5,"name":"Boss","kill":false,"startTime":0,"endTime":120000}
        ]});
        let pull = map_pulls(&report, &video).unwrap().remove(0);
        let master = MasterData::parse(&json!({"actors":[{"id":1,"type":"Player","name":"Healer","subType":"Priest"}],"abilities":[]})).unwrap();
        let data = json!([
            {"type":"cast","timestamp":1000,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"applybuff","timestamp":1020,"sourceID":1,"targetID":1,"abilityGameID":200183},
            {"type":"cast","timestamp":2000,"sourceID":1,"targetID":1,"abilityGameID":197908}
        ]);
        let prefs = defensives::Preferences::default();
        let mut coverage = EventCoverage::requested(&prefs);
        coverage.buffs.insert(200183);
        let raw = Arc::new(
            map_event_page(
                &data,
                &master,
                &pull,
                EventKind::Defensives,
                &coverage,
                &prefs,
            )
            .unwrap(),
        );
        assert_eq!(
            raw.len(),
            3,
            "Raw storage must retain optional casts and cast/buff pairs"
        );
        let mut client = Client::new().unwrap();
        client.config = Some(Config {
            client_id: "fixture".into(),
            guild_id: 1,
            user_id: "123".into(),
        });
        client.config_stamp = Some(ConfigStamp {
            token_hash: Sha256::digest(b"fixture-discord-token").into(),
            at: Instant::now(),
        });
        client.session = Some(Session {
            client_id: "fixture".into(),
            user_id: "123".into(),
            access_token: "fixture-never-sent".into(),
            refresh_token: None,
            expires_at: now_secs() + 3600,
        });
        client.cooldowns = Some(prefs);
        let key = (pull.report.clone(), pull.id, EventKind::Defensives);
        client.events.insert(
            key.clone(),
            CachedEvents {
                at: Instant::now(),
                bounds: (pull.start_ms, pull.end_ms),
                coverage,
                raw: raw.clone(),
            },
        );
        let token = "fixture-discord-token";
        assert_eq!(
            client
                .events(token, &pull, EventKind::Defensives)
                .unwrap()
                .len(),
            1
        );
        client.cooldowns.as_mut().unwrap().overrides.insert(
            197908,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Cast,
            },
        );
        assert_eq!(
            client
                .events(token, &pull, EventKind::Defensives)
                .unwrap()
                .len(),
            2
        );
        client.cooldowns.as_mut().unwrap().overrides.insert(
            200183,
            defensives::Rule {
                group: Some(DefensiveGroup::External),
                observation: defensives::Observation::CastOrBuff,
            },
        );
        let events = client.events(token, &pull, EventKind::Defensives).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].group, Some(DefensiveGroup::External));
        client
            .cooldowns
            .as_mut()
            .unwrap()
            .overrides
            .get_mut(&200183)
            .unwrap()
            .observation = defensives::Observation::Buff;
        assert!(
            client.events(token, &pull, EventKind::Defensives).unwrap()[0].observed_buff,
            "A previous deduplicated view must not destroy the raw buff evidence"
        );
        client
            .cooldowns
            .as_mut()
            .unwrap()
            .overrides
            .get_mut(&200183)
            .unwrap()
            .group = None;
        assert_eq!(
            client
                .events(token, &pull, EventKind::Defensives)
                .unwrap()
                .len(),
            1
        );
        client.cooldowns.as_mut().unwrap().hidden_groups = DefensiveGroup::ALL.to_vec();
        assert_eq!(
            client
                .events(token, &pull, EventKind::Defensives)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            client.requests,
            RequestCounts::default(),
            "All cached edits must avoid transport"
        );
        client.cooldowns.as_mut().unwrap().overrides.insert(
            1234567,
            defensives::Rule {
                group: Some(DefensiveGroup::Healing),
                observation: defensives::Observation::Cast,
            },
        );
        client.cancel.store(true, Ordering::Relaxed);
        assert!(client.events(token, &pull, EventKind::Defensives).is_err());
        assert!(
            Arc::ptr_eq(&raw, &client.events[&key].raw),
            "Failed extension must preserve previous raw events"
        );
    }

    #[test]
    fn account_config_cache_never_matches_a_different_token_and_master_cache_is_bounded() {
        let stamp = ConfigStamp {
            token_hash: Sha256::digest(b"account-one").into(),
            at: Instant::now(),
        };
        assert!(stamp.matches("account-one"));
        assert!(!stamp.matches("account-two"));
        let data = Arc::new(
            MasterData::parse(&json!({"actors":[
            {"id":1,"type":"Player","name":"Player","subType":"Priest"},
            {"id":2,"type":"NPC","name":"Summon","subType":"Pet"}],"abilities":[]}))
            .unwrap(),
        );
        assert_eq!(data.actors.len(), 1);
        let mut cache = MasterCache::new();
        for index in 0..20 {
            cache_master(&mut cache, index.to_string(), index, data.clone());
        }
        assert_eq!(cache.len(), 8);
        assert!(cache.values().map(|entry| entry.data.rows()).sum::<usize>() <= 10_000);
        assert!(!cache.contains_key("0"));
    }

    #[test]
    fn event_cache_evicts_oldest_pulls_under_aggregate_memory_budget() {
        let event = RaidEvent {
            at_ms: 0,
            actor_id: 1,
            observed_buff: false,
            actor: String::new(),
            class: String::new(),
            ability: String::new(),
            ability_id: 1,
            target: None,
            target_actor_id: None,
            kind: EventKind::Defensives,
            group: Some(DefensiveGroup::Healing),
        };
        let entry = |at, raw| CachedEvents {
            at,
            bounds: (0, 1),
            coverage: Default::default(),
            raw: Arc::new(raw),
        };
        let mut cache = EventCache::new();
        let key = |id| ("abcdefghABCDEFGH".to_string(), id, EventKind::Defensives);
        cache.insert(
            key(1),
            entry(
                Instant::now() - Duration::from_secs(2),
                vec![event.clone(); 20_000],
            ),
        );
        cache.insert(
            key(2),
            entry(
                Instant::now() - Duration::from_secs(1),
                vec![event.clone(); 20_000],
            ),
        );
        cache_events(&mut cache, key(3), entry(Instant::now(), vec![event]));
        assert!(!cache.contains_key(&key(1)));
        assert!(cache.contains_key(&key(2)) && cache.contains_key(&key(3)));
        assert!(cache.values().map(|entry| entry.raw.len()).sum::<usize>() <= MAX_CACHED_EVENTS);
        for id in 4..30 {
            cache_events(&mut cache, key(id), entry(Instant::now(), Vec::new()));
        }
        assert_eq!(cache.len(), 20);
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
