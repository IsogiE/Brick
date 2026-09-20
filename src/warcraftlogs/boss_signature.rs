//! A complete, boss-only WCL projection for content alignment. No credentials,
//! player identities, targets, damage amounts or inferred timings leave here.
use super::{check_cancelled, report_code, Client, Config, Pull};
use icu_properties::{props::GeneralCategory, CodePointMapData};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::{Duration, Instant},
};
use unicode_normalization::UnicodeNormalization;

pub(crate) const SCHEMA: &str = "brick-boss-signature-1";
const INVALID: &str = "Warcraft Logs returned an invalid boss timeline.";
const CHANGED: &str = "This Warcraft Logs fight changed. Reload its report and try again.";
const TOO_LARGE: &str = "This fight is too large for automatic video alignment.";
const INCOMPLETE: &str = "Warcraft Logs did not return a complete boss timeline.";
const MAX_DURATION_MS: i64 = 3_600_000;
const MAX_TIMESTAMP: i64 = 32_503_680_000_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
// The requested 2000 events are a provider page target, not a response-size
// contract: real WCL responses can exceed that target.
// This independent safety ceiling never truncates a returned page. The 4 MiB
// response cap, total event caps and cursor validation also remain mandatory.
const PAGE_ROWS: usize = 10_000;

const METADATA_QUERY: &str = r#"query($code:String!){reportData{report(code:$code){
 code startTime fights{id encounterID difficulty startTime endTime}
 masterData(translate:false){actors(type:"NPC"){id name type subType} abilities{gameID name}}
}}}"#;
const IDENTITY_QUERY: &str = r#"query($code:String!){reportData{report(code:$code){
 code startTime fights{id encounterID difficulty startTime endTime}
}}}"#;
const EVENTS_QUERY: &str = r#"query($code:String!,$fight:Int!,$type:EventDataType!,$start:Float!,$end:Float!,$resources:Boolean!){
 reportData{report(code:$code){code startTime
 events(fightIDs:[$fight],dataType:$type,hostilityType:Enemies,startTime:$start,endTime:$end,
 translate:false,includeResources:$resources,limit:2000){data nextPageTimestamp}
}}}"#;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BossSignature {
    pub schema: &'static str,
    pub report: String,
    pub pull_id: u64,
    pub encounter_id: u64,
    pub difficulty: u64,
    /// Authenticated WCL UTC; used only as a coarse discovery hint.
    pub report_start_ms: i64,
    /// Report-relative bounds, exactly as returned by WCL.
    pub fight_start_ms: i64,
    pub fight_end_ms: i64,
    pub complete: bool,
    pub actors: Vec<NamedId>,
    pub abilities: Vec<NamedId>,
    pub casts: Vec<BossCast>,
    pub health: Vec<BossHealth>,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct NamedId {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) enum CastType {
    #[serde(rename = "begincast")]
    BeginCast,
    #[serde(rename = "cast")]
    Cast,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BossCast {
    pub seconds: f64,
    #[serde(rename = "type")]
    pub kind: CastType,
    pub actor_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u64>,
    pub ability_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BossHealth {
    pub seconds: f64,
    pub actor_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<u64>,
    pub hit_points: u64,
    pub max_hit_points: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Coverage {
    pub casts: PageCoverage,
    pub health: PageCoverage,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct PageCoverage {
    pub pages: usize,
    pub complete: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    pages: usize,
    casts: usize,
    health: usize,
    bytes: usize,
    page_bytes: usize,
    budget: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 256,
            casts: 12_000,
            health: 120_000,
            bytes: 16 * 1024 * 1024,
            page_bytes: 4 * 1024 * 1024,
            budget: Duration::from_secs(120),
        }
    }
}

impl Client {
    /// Uses the existing viewer authorization and cancellation channel. Callers
    /// must still check their captured guild/account before publishing a result.
    pub(crate) fn boss_signature(
        &mut self,
        access: &crate::guild::Access,
        pull: &Pull,
    ) -> Result<BossSignature, String> {
        access.check()?;
        check_cancelled(&self.cancel)?;
        validate_pull(pull)?;
        self.configure(access, true)?;
        check_scope(self.config.as_ref(), access)?;
        let cancel = self.cancel.clone();
        export_with(
            pull,
            Limits::default(),
            |query, variables, timeout| {
                check_scope(self.config.as_ref(), access)?;
                let value = self.query_with_timeout(query, variables, timeout)?;
                check_scope(self.config.as_ref(), access)?;
                Ok(value)
            },
            || {
                access.check()?;
                check_cancelled(&cancel)
            },
        )
    }
}

fn check_scope(config: Option<&Config>, access: &crate::guild::Access) -> Result<(), String> {
    access.check()?;
    if config.is_some_and(|c| c.user_id == access.user_id && c.discord_guild_id == access.guild_id)
    {
        Ok(())
    } else {
        Err("The selected guild or account changed.".into())
    }
}

struct Reader<F, G> {
    fetch: F,
    current: G,
    limits: Limits,
    started: Instant,
    pages: usize,
}
impl<F, G> Reader<F, G>
where
    F: FnMut(&str, Value, Duration) -> Result<Value, String>,
    G: Fn() -> Result<(), String>,
{
    fn request(&mut self, query: &str, variables: Value) -> Result<Value, String> {
        (self.current)()?;
        let remaining = self
            .limits
            .budget
            .checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(INCOMPLETE)?;
        let value = (self.fetch)(query, variables, remaining.min(Duration::from_secs(15)))?;
        (self.current)()?;
        if self.started.elapsed() >= self.limits.budget {
            return Err(INCOMPLETE.into());
        }
        bounded_json(&value, self.limits.page_bytes, "response_bytes")?;
        Ok(value)
    }
}

fn export_with<F, G>(
    pull: &Pull,
    limits: Limits,
    fetch: F,
    current: G,
) -> Result<BossSignature, String>
where
    F: FnMut(&str, Value, Duration) -> Result<Value, String>,
    G: Fn() -> Result<(), String>,
{
    validate_pull(pull)?;
    let mut reader = Reader {
        fetch,
        current,
        limits,
        started: Instant::now(),
        pages: 0,
    };
    let data = reader.request(METADATA_QUERY, json!({"code":pull.report}))?;
    let report = checked_report(&data, pull)?;
    let (start, end) = checked_fight(report, pull)?;
    let (actors, abilities) = catalog(report)?;
    let mut signature = BossSignature {
        schema: SCHEMA,
        report: pull.report.clone(),
        pull_id: pull.id,
        encounter_id: pull.encounter,
        difficulty: pull.difficulty,
        report_start_ms: pull.report_start_ms,
        fight_start_ms: start,
        fight_end_ms: end,
        complete: false,
        actors: actors
            .iter()
            .map(|(&id, name)| NamedId {
                id,
                name: name.clone(),
            })
            .collect(),
        abilities: Vec::new(),
        casts: Vec::new(),
        health: Vec::new(),
        coverage: Coverage {
            casts: PageCoverage::default(),
            health: PageCoverage::default(),
        },
    };
    let mut used_abilities = BTreeMap::new();
    let mut used_actors = BTreeSet::new();
    let mut seen_casts = BTreeSet::new();
    let mut seen_health = BTreeSet::new();
    for health in [false, true] {
        let mut cursor = start;
        loop {
            if reader.pages >= limits.pages {
                return Err(INCOMPLETE.into());
            }
            let data = reader.request(EVENTS_QUERY, json!({"code":pull.report,"fight":pull.id,
                "type":if health {"DamageTaken"} else {"Casts"},"start":cursor,"end":end,"resources":health}))?;
            reader.pages += 1;
            let report = checked_report(&data, pull)?;
            let page = report
                .get("events")
                .and_then(Value::as_object)
                .ok_or(INCOMPLETE)?;
            let rows = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or(INCOMPLETE)?;
            if rows.len() > PAGE_ROWS {
                return Err(bound_error("event_page_rows", rows.len(), PAGE_ROWS));
            }
            let mut previous = cursor;
            for (row_index, row) in rows.iter().enumerate() {
                let timestamp = integer_ms(&row["timestamp"])
                    .inspect_err(|_| invalid_page(health, reader.pages, row_index))?;
                if timestamp < previous || timestamp > end {
                    invalid_page(health, reader.pages, row_index);
                    return Err(invalid_error(if timestamp < previous {
                        "event_timestamp_order"
                    } else {
                        "event_after_fight_end"
                    }));
                }
                previous = timestamp;
                if let Some(fight) = row.get("fight") {
                    if positive_id(fight)? != pull.id {
                        return Err(CHANGED.into());
                    }
                }
                let seconds = (timestamp - start) as f64 / 1000.0;
                if health {
                    let Some(actor_id) = row["targetID"]
                        .as_u64()
                        .filter(|id| actors.contains_key(id))
                    else {
                        continue;
                    };
                    // DamageTaken has resources on the target only when resourceActor=2.
                    // Missing resources are gaps, never zero HP or interpolated samples.
                    if row.get("resourceActor").is_none_or(Value::is_null) {
                        continue;
                    }
                    let resource_actor = row["resourceActor"]
                        .as_u64()
                        .filter(|id| *id == 1 || *id == 2)
                        .ok_or_else(|| {
                            invalid_page(health, reader.pages, row_index);
                            invalid_error("resource_actor")
                        })?;
                    if resource_actor != 2 {
                        continue;
                    }
                    match (row.get("hitPoints"), row.get("maxHitPoints")) {
                        (None, None) | (Some(Value::Null), Some(Value::Null)) => continue,
                        (Some(hp), Some(max_hp)) => {
                            let hp = safe_integer(hp)
                                .inspect_err(|_| invalid_page(health, reader.pages, row_index))?;
                            let max_hp = safe_integer(max_hp)
                                .inspect_err(|_| invalid_page(health, reader.pages, row_index))?;
                            if max_hp == 0 || hp > max_hp {
                                invalid_page(health, reader.pages, row_index);
                                return Err(invalid_error(if max_hp == 0 {
                                    "health_max_zero"
                                } else {
                                    "health_exceeds_max"
                                }));
                            }
                            let instance = instance(row, "targetInstance")?;
                            if !seen_health.insert((timestamp, actor_id, instance, hp, max_hp)) {
                                continue;
                            }
                            used_actors.insert(actor_id);
                            if signature.health.len() >= limits.health {
                                return Err(bound_error(
                                    "boss_health_records",
                                    signature.health.len() + 1,
                                    limits.health,
                                ));
                            }
                            signature.health.push(BossHealth {
                                seconds,
                                actor_id,
                                instance,
                                hit_points: hp,
                                max_hit_points: max_hp,
                            });
                        }
                        _ => {
                            invalid_page(health, reader.pages, row_index);
                            return Err(invalid_error("health_pair_incomplete"));
                        }
                    }
                } else {
                    let Some(actor_id) = row["sourceID"]
                        .as_u64()
                        .filter(|id| actors.contains_key(id))
                    else {
                        continue;
                    };
                    let kind = match row["type"].as_str() {
                        Some("begincast") => CastType::BeginCast,
                        Some("cast") => CastType::Cast,
                        _ => {
                            invalid_page(health, reader.pages, row_index);
                            return Err(invalid_error("cast_event_type"));
                        }
                    };
                    let ability_id = positive_id(&row["abilityGameID"])?;
                    let name = abilities
                        .get(&ability_id)
                        .ok_or_else(|| invalid_error("cast_ability_not_in_catalog"))?;
                    let instance = instance(row, "sourceInstance")?;
                    if !seen_casts.insert((timestamp, kind as u8, actor_id, instance, ability_id)) {
                        continue;
                    }
                    used_actors.insert(actor_id);
                    used_abilities.insert(ability_id, name.clone());
                    if signature.casts.len() >= limits.casts {
                        return Err(bound_error(
                            "boss_cast_records",
                            signature.casts.len() + 1,
                            limits.casts,
                        ));
                    }
                    signature.casts.push(BossCast {
                        seconds,
                        kind,
                        actor_id,
                        instance,
                        ability_id,
                    });
                }
            }
            let coverage = if health {
                &mut signature.coverage.health
            } else {
                &mut signature.coverage.casts
            };
            coverage.pages += 1;
            match page.get("nextPageTimestamp") {
                Some(Value::Null) => {
                    coverage.complete = true;
                    break;
                }
                Some(next) => {
                    let next = integer_ms(next)?;
                    if next <= cursor || next < previous || next > end {
                        return Err(INCOMPLETE.into());
                    }
                    cursor = next;
                }
                None => return Err(INCOMPLETE.into()),
            }
        }
    }
    if signature.casts.is_empty() {
        return Err("This report has no boss casts for video alignment.".into());
    }
    if used_actors.len() > 64 {
        return Err(bound_error("used_actors", used_actors.len(), 64));
    }
    if used_abilities.len() > 2048 {
        return Err(bound_error("used_abilities", used_abilities.len(), 2048));
    }
    signature
        .actors
        .retain(|actor| used_actors.contains(&actor.id));
    // A live report/fight may have changed while paging. Never attach complete
    // coverage to an obsolete fight duration or an implicitly substituted alias.
    let final_data = reader.request(IDENTITY_QUERY, json!({"code":pull.report}))?;
    checked_fight(checked_report(&final_data, pull)?, pull)?;
    signature.abilities = used_abilities
        .into_iter()
        .map(|(id, name)| NamedId { id, name })
        .collect();
    signature.complete = true;
    bounded_json(&signature, limits.bytes, "signature_bytes")?;
    (reader.current)()?;
    Ok(signature)
}

fn validate_pull(pull: &Pull) -> Result<(), String> {
    if !report_code(&pull.report)
        || pull.id == 0
        || pull.id > 100_000
        || pull.encounter == 0
        || pull.encounter > 100_000
        || !(3..=5).contains(&pull.difficulty)
        || !(1_500_000_000_000..=4_000_000_000_000).contains(&pull.report_start_ms)
        || pull.start_ms < pull.report_start_ms
        || pull.end_ms > MAX_TIMESTAMP
        || pull.end_ms <= pull.start_ms
    {
        return Err(INVALID.into());
    }
    if pull.start_ms - pull.report_start_ms > 7 * 86_400_000
        || pull.end_ms - pull.report_start_ms > 8 * 86_400_000
        || pull.end_ms - pull.start_ms > MAX_DURATION_MS
    {
        return Err(TOO_LARGE.into());
    }
    Ok(())
}

fn checked_report<'a>(data: &'a Value, pull: &Pull) -> Result<&'a Value, String> {
    let report = data
        .pointer("/reportData/report")
        .filter(|r| r.is_object())
        .ok_or(INCOMPLETE)?;
    if report["code"].as_str() != Some(&pull.report)
        || integer_ms(&report["startTime"])? != pull.report_start_ms
    {
        return Err(CHANGED.into());
    }
    Ok(report)
}

fn checked_fight(report: &Value, pull: &Pull) -> Result<(i64, i64), String> {
    let fights = report["fights"]
        .as_array()
        .filter(|rows| rows.len() <= 5000)
        .ok_or_else(|| invalid_error("fight_catalog"))?;
    let mut matches = fights.iter().filter(|f| f["id"].as_u64() == Some(pull.id));
    let fight = matches.next().ok_or(CHANGED)?;
    if matches.next().is_some()
        || positive_id(&fight["encounterID"])? != pull.encounter
        || positive_id(&fight["difficulty"])? != pull.difficulty
    {
        return Err(CHANGED.into());
    }
    let start = integer_ms(&fight["startTime"])?;
    let end = integer_ms(&fight["endTime"])?;
    if start != pull.start_ms - pull.report_start_ms || end != pull.end_ms - pull.report_start_ms {
        return Err(CHANGED.into());
    }
    Ok((start, end))
}

type Names = BTreeMap<u64, String>;
fn catalog(report: &Value) -> Result<(Names, Names), String> {
    let master = &report["masterData"];
    let actor_rows = master["actors"]
        .as_array()
        .filter(|rows| rows.len() <= 20_000)
        .ok_or_else(|| invalid_error("actor_catalog"))?;
    let ability_rows = master["abilities"]
        .as_array()
        .filter(|rows| rows.len() <= 50_000)
        .ok_or_else(|| invalid_error("ability_catalog"))?;
    let mut actors = Names::new();
    for row in actor_rows {
        if row["type"] != "NPC" || row["subType"] != "Boss" {
            continue;
        }
        // WCL's reserved World actor is labelled NPC/Boss but also owns
        // player ground effects. It is not a boss identity.
        if row["id"].as_i64() == Some(-1) {
            continue;
        }
        let id = positive_id(&row["id"])?;
        if actors.insert(id, name(&row["name"])?).is_some() {
            return Err(invalid_error("duplicate_actor_id"));
        }
    }
    if actors.is_empty() {
        return Err("This report has no boss identities for video alignment.".into());
    }
    let mut abilities = Names::new();
    for row in ability_rows {
        // WCL includes reserved 0 (unknown) and negative melee entries in its
        // report-wide master catalog. They are not named boss spell IDs.
        if row["gameID"].as_i64().is_some_and(|id| id <= 0) {
            continue;
        }
        let id = positive_id(&row["gameID"])?;
        if abilities.insert(id, name(&row["name"])?).is_some() {
            return Err(invalid_error("duplicate_ability_id"));
        }
    }
    Ok((actors, abilities))
}

fn name(value: &Value) -> Result<String, String> {
    let raw = value
        .as_str()
        .ok_or_else(|| invalid_error("name_not_text"))?;
    // Reject controls before trimming so hidden directional/format characters
    // cannot enter evidence labels. NFC is also the server hashing contract.
    let categories = CodePointMapData::<GeneralCategory>::new();
    if raw.chars().any(|c| {
        matches!(
            categories.get(c),
            GeneralCategory::Control | GeneralCategory::Format
        )
    }) {
        return Err(invalid_error("name_control_character"));
    }
    let text: String = raw.trim().nfc().collect();
    if text.is_empty() || text.len() > 256 {
        return Err(invalid_error("name_length"));
    }
    Ok(text)
}
fn integer_ms(value: &Value) -> Result<i64, String> {
    let number = value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= MAX_TIMESTAMP as f64 && n.fract() == 0.0)
        .ok_or_else(|| invalid_error("integer_milliseconds"))?;
    Ok(number as i64)
}
fn safe_integer(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|n| *n <= MAX_SAFE_INTEGER)
        .ok_or_else(|| invalid_error("safe_integer"))
}
fn positive_id(value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .filter(|n| *n > 0 && *n <= i32::MAX as u64)
        .ok_or_else(|| invalid_error("positive_id"))
}
fn instance(row: &Value, key: &str) -> Result<Option<u64>, String> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= i32::MAX as u64)
            .map(Some)
            .ok_or_else(|| invalid_error("actor_instance")),
    }
}

// Private live-fixture diagnostics identify only the violated invariant and
// bounded page position. No report/actor/spell identity, row values or tokens.
fn invalid_page(_health: bool, _page: usize, _row: usize) {
    #[cfg(test)]
    if std::env::var("BRICK_BOSS_SIGNATURE_ALLOW_LIVE").as_deref() == Ok("1") {
        eprintln!("boss_signature_invalid_position health={_health} page={_page} row={_row}");
    }
}
fn invalid_error(_kind: &str) -> String {
    #[cfg(test)]
    if std::env::var("BRICK_BOSS_SIGNATURE_ALLOW_LIVE").as_deref() == Ok("1") {
        eprintln!("boss_signature_invalid kind={_kind}");
    }
    INVALID.into()
}

/// Bound serialization without allocating a second multi-megabyte JSON buffer.
fn bound_error(_kind: &str, _actual: usize, _maximum: usize) -> String {
    // Only the explicitly enabled private fixture emits numeric diagnostics.
    // Report codes, actor/spell names, raw rows and credentials are never logged.
    #[cfg(test)]
    if std::env::var("BRICK_BOSS_SIGNATURE_ALLOW_LIVE").as_deref() == Ok("1") {
        eprintln!("boss_signature_bound kind={_kind} actual={_actual} maximum={_maximum}");
    }
    TOO_LARGE.into()
}
fn bounded_json(value: &impl Serialize, maximum: usize, kind: &str) -> Result<(), String> {
    struct Counter {
        used: usize,
        maximum: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.used = self.used.saturating_add(bytes.len());
            if self.used > self.maximum {
                Err(io::Error::other("bounded JSON limit"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, maximum };
    serde_json::to_writer(&mut counter, value).map_err(|_| bound_error(kind, counter.used, maximum))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    fn pull() -> Pull {
        Pull {
            report: "AbCdEfGhIjKlMnOp".into(),
            id: 21,
            encounter: 100,
            difficulty: 5,
            report_start_ms: 1_700_000_000_000,
            remaining: None,
            name: "Boss".into(),
            kill: true,
            last_phase: None,
            last_phase_is_intermission: false,
            start_ms: 1_700_000_001_000,
            end_ms: 1_700_000_011_000,
            seconds: 10,
        }
    }
    fn metadata() -> Value {
        json!({"reportData":{"report":{"code":pull().report,"startTime":pull().report_start_ms,
            "fights":[{"id":21,"encounterID":100,"difficulty":5,"startTime":1000,"endTime":11000}],
            "masterData":{"actors":[
                {"id":10,"name":"Boss","type":"NPC","subType":"Boss"},
                    {"id":-1,"name":"World","type":"NPC","subType":"Boss"},
                {"id":11,"name":"Other boss","type":"NPC","subType":"Boss"},
                {"id":12,"name":"Add","type":"NPC","subType":"Unknown"},
                {"id":7,"name":"Private player","type":"Player","subType":"Mage"}],
                "abilities":[{"gameID":100,"name":"Boss spell"},{"gameID":101,"name":"Player spell"},
                    {"gameID":0,"name":"Unknown Ability"},{"gameID":-32,"name":"Melee"}]}
        }}})
    }
    fn cast(at: i64, kind: &str) -> Value {
        json!({"timestamp":at,"type":kind,"sourceID":10,"sourceInstance":0,
            "targetID":7,"abilityGameID":100,"fight":21,"playerName":"Private player"})
    }
    fn health(at: i64, hp: u64) -> Value {
        json!({"timestamp":at,"type":"damage","sourceID":7,"targetID":10,"targetInstance":1,
            "resourceActor":2,"hitPoints":hp,"maxHitPoints":1000,"fight":21,
            "amount":12345,"playerName":"Private player"})
    }
    fn page(rows: Vec<Value>, next: Value) -> Value {
        json!({"reportData":{"report":{"code":pull().report,"startTime":pull().report_start_ms,
            "events":{"data":rows,"nextPageTimestamp":next}}}})
    }
    fn pages() -> Vec<Value> {
        vec![
            metadata(),
            page(
                vec![cast(1100, "begincast"), cast(3100, "cast")],
                Value::Null,
            ),
            page(
                vec![health(1200, 900), health(1200, 890), health(11000, 0)],
                Value::Null,
            ),
            metadata(),
        ]
    }
    fn run(rows: Vec<Value>, limits: Limits) -> Result<BossSignature, String> {
        let mut rows: VecDeque<_> = rows.into();
        export_with(
            &pull(),
            limits,
            |_, _, _| Ok(rows.pop_front().expect("unexpected extra WCL request")),
            || Ok(()),
        )
    }

    #[test]
    fn projects_real_flat_resource_schema_without_player_data_or_inferred_times() {
        let mut rows = pages();
        let mut add = cast(3200, "cast");
        add["sourceID"] = json!(12);
        let mut player = cast(3200, "cast");
        player["sourceID"] = json!(7);
        let mut world = cast(3200, "cast");
        world["sourceID"] = json!(-1);
        world["abilityGameID"] = json!(101);
        rows[1]["reportData"]["report"]["events"]["data"]
            .as_array_mut()
            .unwrap()
            .extend([add, player, world]);
        let signature = run(rows, Limits::default()).unwrap();
        assert_eq!(signature.report_start_ms, pull().report_start_ms);
        assert_eq!(
            (signature.fight_start_ms, signature.fight_end_ms),
            (1000, 11000)
        );
        assert_eq!(
            signature
                .casts
                .iter()
                .map(|c| c.seconds)
                .collect::<Vec<_>>(),
            vec![0.1, 2.1]
        );
        assert_eq!(signature.casts[0].instance, Some(0));
        assert_eq!(
            signature
                .health
                .iter()
                .map(|h| h.hit_points)
                .collect::<Vec<_>>(),
            vec![900, 890, 0]
        );
        assert_eq!(signature.health[2].seconds, 10.0);
        assert_eq!(
            signature.actors,
            vec![NamedId {
                id: 10,
                name: "Boss".into()
            }]
        );
        assert_eq!(
            signature.abilities,
            vec![NamedId {
                id: 100,
                name: "Boss spell".into()
            }]
        );
        let exported = serde_json::to_string(&signature).unwrap();
        for private in [
            "Private player",
            "Player spell",
            "targetID",
            "sourceID",
            "amount",
            "credential",
            "token",
        ] {
            assert!(
                !exported.contains(private),
                "unexpected private field {private}"
            );
        }
        assert!(
            signature.complete
                && signature.coverage.casts.complete
                && signature.coverage.health.complete
        );
    }

    #[test]
    fn follows_every_cursor_and_deduplicates_only_identical_boundary_events() {
        let mut rows = VecDeque::from([
            metadata(),
            page(
                vec![cast(1100, "begincast"), cast(3100, "cast")],
                json!(3100),
            ),
            page(
                vec![cast(3100, "cast"), cast(6000, "begincast")],
                Value::Null,
            ),
            page(vec![health(1200, 900), health(2500, 880)], json!(2500)),
            page(
                vec![health(2500, 880), health(2500, 870), health(5000, 850)],
                Value::Null,
            ),
            metadata(),
        ]);
        let mut observed = Vec::new();
        let signature = export_with(
            &pull(),
            Limits::default(),
            |query, variables, timeout| {
                assert!(timeout <= Duration::from_secs(15));
                if query == EVENTS_QUERY {
                    observed.push((
                        variables["type"].clone(),
                        variables["start"].clone(),
                        variables["end"].clone(),
                    ));
                    assert_eq!(variables["resources"], variables["type"] == "DamageTaken");
                    assert_eq!(variables["fight"], 21);
                }
                Ok(rows.pop_front().unwrap())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(
            observed,
            vec![
                (json!("Casts"), json!(1000), json!(11000)),
                (json!("Casts"), json!(3100), json!(11000)),
                (json!("DamageTaken"), json!(1000), json!(11000)),
                (json!("DamageTaken"), json!(2500), json!(11000))
            ]
        );
        assert_eq!(signature.casts.len(), 3);
        assert_eq!(signature.health.len(), 4);
        assert_eq!(signature.coverage.casts.pages, 2);
        assert_eq!(signature.coverage.health.pages, 2);
        assert!(rows.is_empty());
    }

    #[test]
    fn missing_resources_stay_gaps_and_empty_complete_health_is_allowed() {
        let mut absent = health(5000, 500);
        absent.as_object_mut().unwrap().remove("hitPoints");
        absent.as_object_mut().unwrap().remove("maxHitPoints");
        let mut source = health(8000, 100);
        source["resourceActor"] = json!(1);
        let mut rows = pages();
        rows[2] = page(vec![absent, source], Value::Null);
        let signature = run(rows, Limits::default()).unwrap();
        assert!(signature.health.is_empty());
        assert!(signature.coverage.health.complete);
    }

    #[test]
    fn incomplete_or_nonadvancing_pagination_never_returns_partial_success() {
        for next in [
            json!(1000),
            json!(900),
            json!(2000),
            json!(11001),
            json!(3100.5),
            json!("3100"),
        ] {
            let mut rows = pages();
            rows[1]["reportData"]["report"]["events"]["nextPageTimestamp"] = next;
            assert!(run(rows, Limits::default()).is_err());
        }
        let mut rows = pages();
        rows[1]["reportData"]["report"]["events"]
            .as_object_mut()
            .unwrap()
            .remove("nextPageTimestamp");
        assert!(run(rows, Limits::default()).is_err());
        let limits = Limits {
            pages: 1,
            ..Limits::default()
        };
        assert_eq!(run(pages(), limits).unwrap_err(), INCOMPLETE);
    }

    #[test]
    fn exact_report_identity_and_fight_bounds_are_checked_again_after_pagination() {
        for at in [0, 1, 3] {
            let mut rows = pages();
            rows[at]["reportData"]["report"]["code"] = json!("ZbCdEfGhIjKlMnOp");
            assert_eq!(run(rows, Limits::default()).unwrap_err(), CHANGED);
        }
        for field in ["startTime", "endTime", "encounterID", "difficulty"] {
            let mut rows = pages();
            rows[3]["reportData"]["report"]["fights"][0][field] = json!(999);
            assert_eq!(run(rows, Limits::default()).unwrap_err(), CHANGED);
        }
        let mut rows = pages();
        rows[3]["reportData"]["report"]["startTime"] = json!(pull().report_start_ms + 1);
        assert_eq!(run(rows, Limits::default()).unwrap_err(), CHANGED);
        let mut rows = pages();
        let duplicate = rows[0]["reportData"]["report"]["fights"][0].clone();
        rows[0]["reportData"]["report"]["fights"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert_eq!(run(rows, Limits::default()).unwrap_err(), CHANGED);
    }

    #[test]
    fn missing_report_and_partial_graphql_data_are_rejected() {
        for first in [
            Value::Null,
            json!({}),
            json!({"reportData":{"report":null}}),
        ] {
            let mut rows = pages();
            rows[0] = first;
            assert_eq!(run(rows, Limits::default()).unwrap_err(), INCOMPLETE);
        }
        let mut rows = pages();
        rows[1]["reportData"]["report"]["events"] = Value::Null;
        assert_eq!(run(rows, Limits::default()).unwrap_err(), INCOMPLETE);
    }

    #[test]
    fn rejects_malformed_event_numbers_order_and_references() {
        for time in [
            json!(-1),
            json!(999),
            json!(11001),
            json!(1000.5),
            json!("1100"),
            Value::Null,
            json!(1e300),
        ] {
            let mut rows = pages();
            rows[1]["reportData"]["report"]["events"]["data"][0]["timestamp"] = time;
            assert!(run(rows, Limits::default()).is_err());
        }
        for (field, value) in [
            ("fight", json!(22)),
            ("abilityGameID", json!(999)),
            ("sourceInstance", json!(-1)),
            ("type", json!("unknown")),
        ] {
            let mut rows = pages();
            rows[1]["reportData"]["report"]["events"]["data"][0][field] = value;
            assert!(run(rows, Limits::default()).is_err());
        }
        let mut rows = pages();
        rows[1] = page(
            vec![cast(3100, "cast"), cast(1100, "begincast")],
            Value::Null,
        );
        assert_eq!(run(rows, Limits::default()).unwrap_err(), INVALID);
    }

    #[test]
    fn malformed_resources_are_not_silently_dropped() {
        for (field, value) in [
            ("hitPoints", json!(-1)),
            ("hitPoints", json!(1001)),
            ("hitPoints", json!(1.5)),
            ("maxHitPoints", json!(0)),
            ("maxHitPoints", json!(MAX_SAFE_INTEGER + 1)),
            ("resourceActor", json!(3)),
            ("targetInstance", json!(i32::MAX as u64 + 1)),
        ] {
            let mut rows = pages();
            rows[2]["reportData"]["report"]["events"]["data"][0][field] = value;
            assert_eq!(run(rows, Limits::default()).unwrap_err(), INVALID);
        }
        let mut rows = pages();
        rows[2]["reportData"]["report"]["events"]["data"][0]
            .as_object_mut()
            .unwrap()
            .remove("hitPoints");
        assert_eq!(run(rows, Limits::default()).unwrap_err(), INVALID);
    }

    #[test]
    fn oversized_provider_target_pages_keep_all_casts_health_and_boundary_alternatives() {
        let mut casts: Vec<Value> = (0..2000).map(|i| cast(1100 + i, "cast")).collect();
        for instance in [1, 2] {
            let mut extra = cast(3099, "cast");
            extra["sourceInstance"] = json!(instance);
            casts.push(extra);
        }
        let health_rows: Vec<Value> = (0..2002)
            .map(|i| {
                let mut row = health(5000, 9000 - i);
                row["maxHitPoints"] = json!(10000);
                row
            })
            .collect();
        let boundary = health_rows.last().unwrap().clone();
        let mut alternative = boundary.clone();
        alternative["hitPoints"] = json!(6998);
        let mut final_row = alternative.clone();
        final_row["timestamp"] = json!(11000);
        let mut input = VecDeque::from([
            metadata(),
            page(casts, Value::Null),
            page(health_rows, json!(5000)),
            page(vec![boundary, alternative, final_row], Value::Null),
            metadata(),
        ]);
        let mut health_cursors = Vec::new();
        let signature = export_with(
            &pull(),
            Limits::default(),
            |query, variables, _| {
                if query == EVENTS_QUERY {
                    assert!(query.contains("limit:2000"));
                    if variables["type"] == "DamageTaken" {
                        health_cursors.push(variables["start"].clone());
                    }
                }
                Ok(input.pop_front().unwrap())
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(signature.casts.len(), 2002);
        assert_eq!(signature.health.len(), 2004);
        assert_eq!(signature.health[2001].hit_points, 6999);
        assert_eq!(signature.health[2002].hit_points, 6998);
        assert_eq!(signature.health.last().unwrap().seconds, 10.0);
        assert_eq!(health_cursors, [json!(1000), json!(5000)]);
        assert_eq!(signature.coverage.health.pages, 2);
        assert!(signature.complete && signature.coverage.health.complete && input.is_empty());
    }

    #[test]
    fn provider_target_overflow_rows_still_require_valid_fight_and_time() {
        for wrong_fight in [false, true] {
            let mut rows = pages();
            let mut events: Vec<Value> = (0..2002).map(|i| cast(1100 + i, "cast")).collect();
            if wrong_fight {
                events.last_mut().unwrap()["fight"] = json!(22);
            } else {
                events.last_mut().unwrap()["timestamp"] = json!(11001);
            }
            rows[1] = page(events, Value::Null);
            assert_eq!(
                run(rows, Limits::default()).unwrap_err(),
                if wrong_fight { CHANGED } else { INVALID }
            );
        }
    }

    #[test]
    fn record_and_byte_limits_fail_without_truncating() {
        for limits in [
            Limits {
                casts: 1,
                ..Limits::default()
            },
            Limits {
                health: 2,
                ..Limits::default()
            },
            Limits {
                bytes: 100,
                ..Limits::default()
            },
            Limits {
                page_bytes: 100,
                ..Limits::default()
            },
        ] {
            assert_eq!(run(pages(), limits).unwrap_err(), TOO_LARGE);
        }
        let mut rows = pages();
        rows[1] = page(vec![cast(1100, "cast"); PAGE_ROWS + 1], Value::Null);
        assert_eq!(run(rows, Limits::default()).unwrap_err(), TOO_LARGE);
        assert!(run(
            pages(),
            Limits {
                budget: Duration::ZERO,
                ..Limits::default()
            }
        )
        .is_err());
    }

    #[test]
    fn duration_and_alias_validation_precedes_transport() {
        for mutation in 0..5 {
            let mut selected = pull();
            match mutation {
                0 => selected.end_ms = selected.start_ms + MAX_DURATION_MS + 1,
                1 => {
                    selected.report = "https://www.warcraftlogs.com/reports/AbCdEfGhIjKlMnOp".into()
                }
                2 => selected.start_ms = selected.report_start_ms - 1,
                3 => selected.end_ms = i64::MAX,
                _ => selected.id = 0,
            }
            let result = export_with(
                &selected,
                Limits::default(),
                |_, _, _| panic!("invalid input reached transport"),
                || Ok(()),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn unicode_labels_are_normalized_and_hidden_controls_rejected() {
        let mut rows = pages();
        rows[0]["reportData"]["report"]["masterData"]["actors"][0]["name"] =
            json!("  Ame\u{301}lie  ");
        assert_eq!(
            run(rows, Limits::default()).unwrap().actors[0].name,
            "Amélie"
        );
        for label in [
            "\u{202e}Boss".to_string(),
            "Boss\n".into(),
            "\u{200d}Boss".into(),
            " ".into(),
            "é".repeat(129),
        ] {
            let mut rows = pages();
            rows[0]["reportData"]["report"]["masterData"]["actors"][0]["name"] = json!(label);
            assert_eq!(run(rows, Limits::default()).unwrap_err(), INVALID);
        }
    }

    #[test]
    fn duplicate_catalog_id_or_missing_boss_casts_is_not_complete_alignment_input() {
        let mut rows = pages();
        let duplicate = rows[0]["reportData"]["report"]["masterData"]["actors"][0].clone();
        rows[0]["reportData"]["report"]["masterData"]["actors"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert_eq!(run(rows, Limits::default()).unwrap_err(), INVALID);
        let mut rows = pages();
        rows[1] = page(vec![], Value::Null);
        assert!(run(rows, Limits::default())
            .unwrap_err()
            .contains("no boss casts"));
    }

    #[test]
    fn cancelled_request_never_starts_or_publishes_another_page() {
        let cancel = Arc::new(AtomicBool::new(false));
        let calls = Cell::new(0);
        let mut rows: VecDeque<_> = pages().into();
        let result = export_with(
            &pull(),
            Limits::default(),
            |_, _, _| {
                calls.set(calls.get() + 1);
                let response = rows.pop_front().unwrap();
                if calls.get() == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
                Ok(response)
            },
            || check_cancelled(&cancel),
        );
        assert_eq!(result.unwrap_err(), super::super::CANCELLED);
        assert_eq!(calls.get(), 2);
        assert!(export_with(
            &pull(),
            Limits::default(),
            |_, _, _| panic!("cancelled request reached transport"),
            || check_cancelled(&cancel)
        )
        .is_err());
    }

    #[test]
    fn changed_scope_during_response_stops_before_health_and_final_publish() {
        let active = Cell::new(true);
        let calls = Cell::new(0);
        let mut rows: VecDeque<_> = pages().into();
        let result = export_with(
            &pull(),
            Limits::default(),
            |_, _, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    active.set(false);
                }
                Ok(rows.pop_front().unwrap())
            },
            || {
                if active.get() {
                    Ok(())
                } else {
                    Err("The selected guild or account changed.".into())
                }
            },
        );
        assert_eq!(
            result.unwrap_err(),
            "The selected guild or account changed."
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn account_guild_and_generation_are_bound_to_authorized_config() {
        let access = crate::guild::Access::new(
            "private-secret".into(),
            crate::guild::ADVANCE.into(),
            "123".into(),
            crate::guild::generation(),
        );
        let config = Config {
            client_id: "wcl-client".into(),
            guild_id: 100,
            user_id: "123".into(),
            discord_guild_id: crate::guild::ADVANCE.into(),
            content_alignment: None,
        };
        assert!(check_scope(Some(&config), &access).is_ok());
        for (guild, user) in [("999", "123"), (crate::guild::ADVANCE, "456")] {
            let other = crate::guild::Access::new(
                "private-secret".into(),
                guild.into(),
                user.into(),
                crate::guild::generation(),
            );
            assert!(check_scope(Some(&config), &other).is_err());
        }
        let stale = crate::guild::Access::new(
            "private-secret".into(),
            crate::guild::ADVANCE.into(),
            "123".into(),
            crate::guild::generation().wrapping_sub(1),
        );
        let mut client = Client::new().unwrap();
        assert!(client.boss_signature(&stale, &pull()).is_err());
        assert_eq!(client.requests.config, 0);
        assert_eq!(client.requests.graphql, 0);
    }
}

#[cfg(test)]
mod private_fixture {
    use super::*;
    #[test]
    #[ignore = "explicit local service, protected sign-in, exact private fight fixture and private artifact directory required"]
    fn private_boss_signature_fixture() {
        assert_eq!(
            std::env::var("BRICK_BOSS_SIGNATURE_ALLOW_LIVE").as_deref(),
            Ok("1")
        );
        assert_eq!(
            crate::presence::endpoint_url("/").unwrap().host_str(),
            Some("127.0.0.1")
        );
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Fixture {
            report: String,
            pull_id: u64,
        }
        let fixture_path = std::path::PathBuf::from(
            std::env::var("BRICK_BOSS_SIGNATURE_FIXTURE").expect("Explicit fixture required"),
        );
        let directory = std::path::PathBuf::from(
            std::env::var("BRICK_BOSS_SIGNATURE_ARTIFACT_DIR")
                .expect("Private artifact directory required"),
        );
        assert!(directory.is_absolute() && directory.is_dir());
        assert_eq!(
            directory.canonicalize().unwrap(),
            directory,
            "Artifact path must be canonical without symlinks"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o077,
                0,
                "Artifact directory must be private"
            );
        }
        assert!(std::fs::metadata(&fixture_path).unwrap().len() <= 4096);
        let fixture: Fixture = serde_json::from_slice(&std::fs::read(fixture_path).unwrap())
            .expect("Invalid private fixture");
        assert!(
            report_code(&fixture.report)
                && fixture.pull_id > 0
                && fixture.pull_id <= i32::MAX as u64
        );
        let output = directory.join("signature.json");
        assert!(!output.exists(), "Refusing to replace existing evidence");
        // Publish only the normal protected session's selected guild/account,
        // exactly as UI startup does. The fixture supplies no identity override.
        let user = match crate::discord_auth::saved_session_status()
            .expect("Protected sign-in unavailable")
        {
            crate::discord_auth::SessionStatus::Authorized(user) => user,
            crate::discord_auth::SessionStatus::NeedsRefresh => {
                crate::discord_auth::refresh_saved_session().expect("Sign-in refresh unavailable")
            }
            _ => panic!("A protected sign-in is required"),
        };
        crate::guild::activate(&user.guild_id, &user.user_id);
        let access = crate::discord_auth::current_or_refreshed_access_token()
            .expect("Protected sign-in unavailable")
            .expect("Sign-in required");
        let mut client = Client::new().unwrap();
        client
            .configure(&access, true)
            .expect("WCL configuration unavailable");
        let data=client.query("query($code:String!){reportData{report(code:$code){code startTime fights{id encounterID difficulty name kill lastPhase lastPhaseIsIntermission fightPercentage startTime endTime}}}}",json!({"code":fixture.report})).expect("Private report unavailable");
        let report = &data["reportData"]["report"];
        assert!(
            report["code"].as_str() == Some(fixture.report.as_str()),
            "Report identity changed"
        );
        assert!(
            super::super::complete_fight_list(report),
            "Incomplete fight metadata"
        );
        let pull = super::super::map_report_pulls(report, None)
            .expect("Invalid fight metadata")
            .into_iter()
            .find(|p| p.id == fixture.pull_id)
            .expect("Exact fight absent");
        let signature = client
            .boss_signature(&access, &pull)
            .expect("Complete boss signature export failed");
        crate::atomic_file::write(&output, &serde_json::to_vec(&signature).unwrap())
            .expect("Private artifact write failed");
        // The harness emits no report, media, credentials, raw responses or tokens.
    }
}
