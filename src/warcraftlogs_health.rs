use super::{check_cancelled, report_code, Client, Pull};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const MAX_PAGES: usize = 8;
const PAGE_SIZE: usize = 2_000;
const MAX_GROUPS: usize = 64;
const DEADLINE_ERROR: &str = "Warcraft Logs health lookup took too long. Try again shortly.";
const INVALID: &str = "Warcraft Logs returned invalid health data.";
const TOO_LARGE: &str = "This part of the pull has too much health data to match safely.";
const METADATA_QUERY: &str = "query($code:String!,$fight:Int!){reportData{report(code:$code){startTime fights(fightIDs:[$fight]){id startTime endTime enemyNPCs{id}} masterData{actors{id name type gameID}}}}}";
// WCL can include additional rows sharing the last event timestamp. Ask for
// fewer rows while retaining the strict 2,000-row response bound.
const HEALTH_FILTER: &str =
    "resources.actor = target AND resources.maxHitPoints > 0 AND target.type = 'NPC'";
const EVENTS_QUERY: &str = "query($code:String!,$fight:Int!,$start:Float!,$end:Float!,$filter:String!){reportData{report(code:$code){events(fightIDs:[$fight],dataType:DamageDone,includeResources:true,startTime:$start,endTime:$end,filterExpression:$filter,limit:1900){data nextPageTimestamp}}}}";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HealthPoint {
    pub elapsed_seconds: f64,
    pub hit_points: u64,
    pub max_hit_points: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HealthTarget {
    pub actor: u64,
    pub game_id: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HealthBand {
    pub game_ids: Vec<u64>,
    pub min_percent: f64,
    pub max_percent: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HealthTrace {
    pub actor: u64,
    pub instance: Option<u64>,
    pub game_id: u64,
    pub name: String,
    pub points: Vec<HealthPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HealthWindow {
    /// Bounds relative to the encounter start, in milliseconds.
    pub start_ms: i64,
    pub end_ms: i64,
    pub traces: Vec<HealthTrace>,
}

pub(super) fn request_timeout(deadline: Option<Instant>) -> Result<Duration, String> {
    match deadline {
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .map(|duration| duration.min(Duration::from_secs(15)))
            .ok_or_else(|| DEADLINE_ERROR.into()),
        None => Ok(Duration::from_secs(15)),
    }
}

fn window_bounds(pull: &Pull, center_ms: i64) -> Result<(i64, i64), String> {
    let duration = pull.end_ms.checked_sub(pull.start_ms).ok_or(INVALID)?;
    if !report_code(&pull.report)
        || pull.id == 0
        || pull.id > i32::MAX as u64
        || pull.report_start_ms < 0
        || pull.start_ms < pull.report_start_ms
        || duration <= 0
        || duration > 86_400_000
    {
        return Err(INVALID.into());
    }
    let center_ms = center_ms.clamp(0, duration);
    let start = (center_ms - 60_000).max(0) / 30_000 * 30_000;
    Ok((start, (start + 120_000).min(duration)))
}

impl Client {
    pub(crate) fn health_targets(
        &mut self,
        discord_token: &str,
        pull: &Pull,
    ) -> Result<Vec<HealthTarget>, String> {
        window_bounds(pull, 0)?;
        self.configure(discord_token, true)?;
        let token = self.access_token()?;
        self.targets_before(pull, &token, Instant::now() + Duration::from_secs(15))
    }

    fn targets_before(
        &mut self,
        pull: &Pull,
        token: &str,
        deadline: Instant,
    ) -> Result<Vec<HealthTarget>, String> {
        check_cancelled(&self.cancel)?;
        let key = (pull.report.clone(), pull.id, pull.start_ms, pull.end_ms);
        self.health_targets
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        if let Some((_, targets)) = self.health_targets.get(&key) {
            return Ok(targets.clone());
        }
        let data = self.query_before(
            METADATA_QUERY,
            json!({"code":pull.report,"fight":pull.id}),
            Some(deadline),
            Some(token),
        )?;
        let (_, actors) = metadata(&data, pull)?;
        let targets = targets(actors)?;
        check_cancelled(&self.cancel)?;
        if self.health_targets.len() >= 6 {
            if let Some(oldest) = self
                .health_targets
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                self.health_targets.remove(&oldest);
            }
        }
        self.health_targets
            .insert(key, (Instant::now(), targets.clone()));
        Ok(targets)
    }

    pub(crate) fn health_band_window(
        &mut self,
        discord_token: &str,
        pull: &Pull,
        bands: &[HealthBand],
    ) -> Result<HealthWindow, String> {
        window_bounds(pull, 0)?;
        let bands = canonical_bands(bands)?;
        self.configure(discord_token, true)?;
        let token = self.access_token()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let targets = self.targets_before(pull, &token, deadline)?;
        let filter = band_filter(&bands, &targets)?;
        let key = (
            pull.report.clone(),
            pull.id,
            pull.start_ms,
            pull.end_ms,
            filter.clone(),
        );
        self.health_bands
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        check_cancelled(&self.cancel)?;
        if let Some((_, window)) = self.health_bands.get(&key) {
            return Ok(window.clone());
        }
        let cancel = self.cancel.clone();
        let mut window = acquire_scope(
            pull,
            Scope {
                start: 0,
                end: pull.end_ms - pull.start_ms,
                filter: &filter,
                targets: Some(&targets),
            },
            &cancel,
            deadline,
            |query, variables| self.query_before(query, variables, Some(deadline), Some(&token)),
        )?;
        // Preserve sparse intervals. Never manufacture health between bands,
        // and verify the numerical filter locally as well as at the provider.
        for trace in &mut window.traces {
            trace
                .points
                .retain(|point| bands.iter().any(|band| band.contains(trace.game_id, point)));
        }
        window.traces.retain(|trace| !trace.points.is_empty());
        check_cancelled(&self.cancel)?;
        request_timeout(Some(deadline))?;
        if self.health_bands.len() >= 6 {
            if let Some(oldest) = self
                .health_bands
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                self.health_bands.remove(&oldest);
            }
        }
        self.health_bands
            .insert(key, (Instant::now(), window.clone()));
        Ok(window)
    }

    pub(crate) fn health_window(
        &mut self,
        discord_token: &str,
        pull: &Pull,
        center_ms: i64,
    ) -> Result<HealthWindow, String> {
        let (start, end) = window_bounds(pull, center_ms)?;
        self.configure(discord_token, true)?;
        let token = self.access_token()?;
        check_cancelled(&self.cancel)?;
        let key = (
            pull.report.clone(),
            pull.id,
            pull.start_ms.checked_add(start).ok_or(INVALID)?,
            pull.start_ms.checked_add(end).ok_or(INVALID)?,
        );
        self.health
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        if let Some((_, window)) = self.health.get(&key) {
            return Ok(window.clone());
        }
        // The deadline covers metadata and all pages. Configuration and secure
        // credential preparation retain their existing separate timeouts. Reuse
        // that token so an intervening expiry cannot start an unbudgeted refresh.
        let deadline = Instant::now() + Duration::from_secs(30);
        let cancel = self.cancel.clone();
        let window = acquire(pull, start, end, &cancel, deadline, |query, variables| {
            self.query_before(query, variables, Some(deadline), Some(&token))
        })?;
        check_cancelled(&self.cancel)?;
        if self.health.len() >= 6 {
            if let Some(oldest) = self
                .health
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                self.health.remove(&oldest);
            }
        }
        self.health.insert(key, (Instant::now(), window.clone()));
        Ok(window)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Actor {
    name: String,
    game_id: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct CanonicalBand {
    game_ids: Vec<u64>,
    min: u32,
    max: u32,
}

impl CanonicalBand {
    fn contains(&self, game_id: u64, point: &HealthPoint) -> bool {
        self.game_ids.binary_search(&game_id).is_ok()
            && (point.hit_points as u128) * 10_000
                >= (self.min as u128) * point.max_hit_points as u128
            && (point.hit_points as u128) * 10_000
                <= (self.max as u128) * point.max_hit_points as u128
    }
}

fn canonical_bands(bands: &[HealthBand]) -> Result<Vec<CanonicalBand>, String> {
    if bands.is_empty() || bands.len() > 64 {
        return Err(INVALID.into());
    }
    let mut grouped: BTreeMap<Vec<u64>, Vec<(u32, u32)>> = BTreeMap::new();
    for band in bands {
        if band.game_ids.is_empty()
            || band.game_ids.len() > 64
            || band
                .game_ids
                .iter()
                .any(|id| *id == 0 || *id > u32::MAX as u64)
            || !band.min_percent.is_finite()
            || !band.max_percent.is_finite()
            || band.min_percent < 0.0
            || band.max_percent > 100.0
            || band.min_percent > band.max_percent
        {
            return Err(INVALID.into());
        }
        let mut ids = band.game_ids.clone();
        ids.sort_unstable();
        ids.dedup();
        grouped.entry(ids).or_default().push((
            (band.min_percent * 100.0).floor() as u32,
            (band.max_percent * 100.0).ceil() as u32,
        ));
    }
    let mut output = Vec::new();
    for (game_ids, mut ranges) in grouped {
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (min, max) in ranges {
            if let Some(last) = merged.last_mut().filter(|last| min <= last.1) {
                last.1 = last.1.max(max);
            } else {
                merged.push((min, max));
            }
        }
        output.extend(merged.into_iter().map(|(min, max)| CanonicalBand {
            game_ids: game_ids.clone(),
            min,
            max,
        }));
    }
    if output.len() > 32 || output.iter().map(|band| band.game_ids.len()).sum::<usize>() > 256 {
        return Err(TOO_LARGE.into());
    }
    Ok(output)
}

fn band_filter(bands: &[CanonicalBand], targets: &[HealthTarget]) -> Result<String, String> {
    let allowed: HashSet<_> = targets.iter().map(|target| target.game_id).collect();
    if bands
        .iter()
        .flat_map(|band| &band.game_ids)
        .any(|id| !allowed.contains(id))
    {
        return Err(INVALID.into());
    }
    // The live API rejects decimal literals with a null paginator. Integer
    // products also avoid division by zero in its expression evaluator.
    let clauses: Vec<_> = bands.iter().map(|band| {
        let ids = band.game_ids.iter().map(|id| format!("target.id = {id}")).collect::<Vec<_>>().join(" OR ");
        format!("(({ids}) AND (10000 * resources.hitPoints) >= ({} * resources.maxHitPoints) AND (10000 * resources.hitPoints) <= ({} * resources.maxHitPoints))", band.min, band.max)
    }).collect();
    let filter = format!("{HEALTH_FILTER} AND ({})", clauses.join(" OR "));
    if filter.len() > 16_384 {
        return Err(TOO_LARGE.into());
    }
    Ok(filter)
}

fn targets(actors: HashMap<u64, Actor>) -> Result<Vec<HealthTarget>, String> {
    if actors.len() > MAX_GROUPS {
        return Err(TOO_LARGE.into());
    }
    let mut targets: Vec<_> = actors
        .into_iter()
        .map(|(actor, data)| HealthTarget {
            actor,
            game_id: data.game_id,
            name: data.name,
        })
        .collect();
    targets.sort_by_key(|target| target.actor);
    Ok(targets)
}

fn unsigned(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|n| {
                n.is_finite() && *n >= 0.0 && *n <= 9_007_199_254_740_991.0 && n.fract() == 0.0
            })
            .map(|n| n as u64)
    })
}

fn milliseconds(value: &Value) -> Option<i64> {
    unsigned(value)
        .filter(|time| *time <= 32_503_680_000_000)
        .map(|time| time as i64)
}

fn metadata(data: &Value, pull: &Pull) -> Result<(i64, HashMap<u64, Actor>), String> {
    let report = &data["reportData"]["report"];
    if milliseconds(&report["startTime"]) != Some(pull.report_start_ms) {
        return Err("The pull changed. Reopen the report and try again.".into());
    }
    let fights = report["fights"].as_array().ok_or(INVALID)?;
    if fights.len() != 1 {
        return Err("This pull is no longer available to your Warcraft Logs account.".into());
    }
    let fight = fights
        .iter()
        .find(|fight| unsigned(&fight["id"]) == Some(pull.id))
        .ok_or("This pull is no longer available to your Warcraft Logs account.")?;
    let start = milliseconds(&fight["startTime"]).ok_or(INVALID)?;
    let end = milliseconds(&fight["endTime"]).ok_or(INVALID)?;
    if start != pull.start_ms - pull.report_start_ms || end != pull.end_ms - pull.report_start_ms {
        return Err("The pull changed. Reopen the report and try again.".into());
    }
    let hostile: HashSet<_> = fight["enemyNPCs"]
        .as_array()
        .ok_or(INVALID)?
        .iter()
        .filter_map(|npc| unsigned(&npc["id"]))
        .collect();
    let mut actors = HashMap::new();
    for value in report["masterData"]["actors"].as_array().ok_or(INVALID)? {
        let Some(id) = unsigned(&value["id"]).filter(|id| hostile.contains(id)) else {
            continue;
        };
        if value["type"].as_str() != Some("NPC") {
            continue;
        }
        let Some(name) = value["name"].as_str().filter(|name| {
            !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
        }) else {
            continue;
        };
        let Some(game_id) = unsigned(&value["gameID"]).filter(|id| *id > 0) else {
            continue;
        };
        let actor = Actor {
            name: name.into(),
            game_id,
        };
        if actors.get(&id).is_some_and(|existing| existing != &actor) {
            return Err(INVALID.into());
        }
        actors.insert(id, actor);
    }
    Ok((start, actors))
}

fn append_page(
    entries: &[Value],
    actors: &HashMap<u64, Actor>,
    fight_start: i64,
    start: i64,
    end: i64,
    traces: &mut BTreeMap<(u64, Option<u64>), HealthTrace>,
) -> Result<(), String> {
    for event in entries {
        // WCL documents damage/healing resources as target-owned; the event
        // discriminator must corroborate that ownership. Never infer an actor
        // from resourceActor itself, nor substitute the largest health pool.
        if event["type"].as_str() != Some("damage") || unsigned(&event["resourceActor"]) != Some(2)
        {
            continue;
        }
        let Some(id) = unsigned(&event["targetID"]) else {
            continue;
        };
        let Some(actor) = actors.get(&id) else {
            continue;
        };
        let Some(timestamp) =
            unsigned(&event["timestamp"]).and_then(|time| i64::try_from(time).ok())
        else {
            continue;
        };
        let Some(elapsed) = timestamp
            .checked_sub(fight_start)
            .filter(|time| *time >= start && *time <= end)
        else {
            continue;
        };
        let (Some(hit_points), Some(max_hit_points)) = (
            unsigned(&event["hitPoints"]),
            unsigned(&event["maxHitPoints"]),
        ) else {
            continue;
        };
        if max_hit_points == 0 || hit_points > max_hit_points {
            continue;
        }
        let instance = match event.get("targetInstance") {
            None | Some(Value::Null) => None,
            Some(value) => match unsigned(value).filter(|instance| *instance > 0) {
                Some(instance) => Some(instance),
                None => continue,
            },
        };
        let key = (id, instance);
        if !traces.contains_key(&key) && traces.len() >= MAX_GROUPS {
            return Err(TOO_LARGE.into());
        }
        traces
            .entry(key)
            .or_insert_with(|| HealthTrace {
                actor: id,
                instance,
                game_id: actor.game_id,
                name: actor.name.clone(),
                points: Vec::new(),
            })
            .points
            .push(HealthPoint {
                elapsed_seconds: elapsed as f64 / 1000.0,
                hit_points,
                max_hit_points,
            });
    }
    Ok(())
}

fn acquire(
    pull: &Pull,
    start: i64,
    end: i64,
    cancel: &AtomicBool,
    deadline: Instant,
    query: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Result<HealthWindow, String> {
    acquire_scope(
        pull,
        Scope {
            start,
            end,
            filter: HEALTH_FILTER,
            targets: None,
        },
        cancel,
        deadline,
        query,
    )
}

struct Scope<'a> {
    start: i64,
    end: i64,
    filter: &'a str,
    targets: Option<&'a [HealthTarget]>,
}

fn acquire_scope(
    pull: &Pull,
    scope: Scope<'_>,
    cancel: &AtomicBool,
    deadline: Instant,
    mut query: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Result<HealthWindow, String> {
    let Scope {
        start,
        end,
        filter,
        targets,
    } = scope;
    let mut fetch = |text, variables| {
        check_cancelled(cancel)?;
        request_timeout(Some(deadline))?;
        let result = query(text, variables);
        check_cancelled(cancel)?;
        request_timeout(Some(deadline))?;
        result
    };
    let (fight_start, actors) = if let Some(targets) = targets {
        (
            pull.start_ms - pull.report_start_ms,
            targets
                .iter()
                .map(|target| {
                    (
                        target.actor,
                        Actor {
                            name: target.name.clone(),
                            game_id: target.game_id,
                        },
                    )
                })
                .collect(),
        )
    } else {
        let data = fetch(METADATA_QUERY, json!({"code":pull.report,"fight":pull.id}))?;
        metadata(&data, pull)?
    };
    let mut traces = BTreeMap::new();
    let mut next_start = fight_start + start;
    let absolute_end = fight_start + end;
    for page in 0..MAX_PAGES {
        let data = fetch(
            EVENTS_QUERY,
            json!({"code":pull.report,"fight":pull.id,"start":next_start,"end":absolute_end,"filter":filter}),
        )?;
        let paginator = &data["reportData"]["report"]["events"];
        let entries = paginator["data"].as_array().ok_or(INVALID)?;
        if entries.len() > PAGE_SIZE {
            return Err(TOO_LARGE.into());
        }
        append_page(entries, &actors, fight_start, start, end, &mut traces)?;
        let next = paginator.get("nextPageTimestamp").ok_or(INVALID)?;
        if next.is_null() {
            break;
        }
        let next = milliseconds(next)
            .filter(|next| *next > next_start && *next <= absolute_end)
            .ok_or(INVALID)?;
        if page + 1 == MAX_PAGES {
            return Err(TOO_LARGE.into());
        }
        next_start = next;
    }
    check_cancelled(cancel)?;
    request_timeout(Some(deadline))?;
    let mut traces: Vec<_> = traces.into_values().collect();
    for trace in &mut traces {
        trace.points.sort_by(|a, b| {
            a.elapsed_seconds
                .total_cmp(&b.elapsed_seconds)
                .then(a.hit_points.cmp(&b.hit_points))
                .then(a.max_hit_points.cmp(&b.max_hit_points))
        });
        trace.points.dedup();
    }
    Ok(HealthWindow {
        start_ms: start,
        end_ms: end,
        traces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn pull() -> Pull {
        Pull {
            report: "abcdefghABCDEFGH".into(),
            id: 7,
            encounter: 10,
            difficulty: 5,
            report_start_ms: 1_000_000,
            remaining: None,
            name: "Encounter".into(),
            kill: false,
            last_phase: None,
            last_phase_is_intermission: false,
            start_ms: 1_010_000,
            end_ms: 1_343_999,
            seconds: 0,
        }
    }

    fn metadata_fixture() -> Value {
        json!({"reportData":{"report":{
            "startTime":1000000,
            "fights":[{"id":7,"startTime":10000,"endTime":343999,
                "enemyNPCs":[{"id":21},{"id":22},{"id":23}]}],
            "masterData":{"actors":[
                {"id":21,"gameID":101.0,"name":"Boss","type":"NPC"},
                {"id":22,"gameID":102,"name":"Boss","type":"NPC"},
                {"id":23,"gameID":103,"name":"Player","type":"Player"},
                {"id":24,"gameID":104,"name":"Friendly NPC","type":"NPC"}]}
        }}})
    }

    fn event(actor: u64, instance: Option<u64>, timestamp: u64, health: u64) -> Value {
        json!({"type":"damage","sourceID":999,"targetID":actor,"targetInstance":instance,
            "timestamp":timestamp,"hitPoints":health,"maxHitPoints":1000,"resourceActor":2})
    }

    fn page(events: Vec<Value>, next: Option<i64>) -> Value {
        json!({"reportData":{"report":{"events":{"data":events,"nextPageTimestamp":next}}}})
    }

    #[test]
    fn health_range_is_quantized_bounded_and_contains_the_clamped_center() {
        let pull = pull();
        for center in [-100, 0, 59999, 60000, 91000, 150000, 333999, i64::MAX] {
            let (start, end) = window_bounds(&pull, center).unwrap();
            assert_eq!(start % 30000, 0);
            assert!(start >= 0 && end <= pull.end_ms - pull.start_ms && end - start <= 120000);
            assert!((start..=end).contains(&center.clamp(0, pull.end_ms - pull.start_ms)));
        }
        assert_eq!(window_bounds(&pull, 150001), window_bounds(&pull, 179999));
        let mut short = pull.clone();
        short.end_ms = short.start_ms + 12345;
        assert_eq!(window_bounds(&short, 90000).unwrap(), (0, 12345));
        short.end_ms = short.start_ms;
        assert!(window_bounds(&short, 0).is_err());
    }

    #[test]
    fn health_parser_keeps_hostile_actors_instances_and_same_time_alternatives_separate() {
        let (fight_start, actors) = metadata(&metadata_fixture(), &pull()).unwrap();
        let mut traces = BTreeMap::new();
        let mut wrong_owner = event(21, Some(1), 11500, 777);
        wrong_owner["resourceActor"] = json!(1);
        let mut unknown_owner = event(21, Some(1), 11500, 888);
        unknown_owner
            .as_object_mut()
            .unwrap()
            .remove("resourceActor");
        let mut bad_hp = event(21, Some(1), 11500, 1001);
        let mut bad_instance = event(21, Some(1), 11500, 777);
        bad_instance["targetInstance"] = json!(1.5);
        let mut not_damage = event(21, Some(1), 11500, 777);
        not_damage["type"] = json!("cast");
        let mut entries = vec![
            event(21, Some(1), 12000, 900),
            event(21, Some(1), 11000, 950),
            event(21, Some(1), 11000, 940),
            event(21, Some(1), 11000, 950),
            event(21, Some(2), 11000, 800),
            event(21, None, 11000, 700),
            event(22, Some(1), 11000, 600),
            event(23, Some(1), 11000, 500),
            event(24, Some(1), 11000, 500),
            event(999, None, 11000, 500),
            event(21, Some(1), 9999, 500),
            event(21, Some(1), 14001, 500),
            wrong_owner,
            unknown_owner,
            bad_hp.clone(),
            bad_instance,
            not_damage,
        ];
        bad_hp["maxHitPoints"] = json!(0);
        entries.push(bad_hp);
        append_page(&entries, &actors, fight_start, 0, 4000, &mut traces).unwrap();
        assert_eq!(traces.len(), 4);
        assert_eq!(traces[&(21, Some(1))].points.len(), 4);
        // The acquisition path sorts and deduplicates only exact repeated values.
        let mut calls = 0;
        let result = acquire(
            &pull(),
            0,
            4000,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(1),
            |_, _| {
                calls += 1;
                Ok(if calls == 1 {
                    metadata_fixture()
                } else {
                    page(entries.clone(), None)
                })
            },
        )
        .unwrap();
        let points = &result
            .traces
            .iter()
            .find(|trace| trace.actor == 21 && trace.instance == Some(1))
            .unwrap()
            .points;
        assert_eq!(points.len(), 3);
        assert_eq!(
            points
                .iter()
                .map(|point| (point.elapsed_seconds, point.hit_points))
                .collect::<Vec<_>>(),
            vec![(1.0, 940), (1.0, 950), (2.0, 900)]
        );
        assert_eq!(
            result
                .traces
                .iter()
                .filter(|trace| trace.name == "Boss")
                .count(),
            4
        );
    }

    #[test]
    fn changed_or_inaccessible_health_metadata_is_rejected() {
        let mut data = metadata_fixture();
        data["reportData"]["report"]["fights"][0]["startTime"] = json!(10001);
        assert!(metadata(&data, &pull()).is_err());
        assert!(metadata(&json!({"reportData":{"report":null}}), &pull()).is_err());
        let mut data = metadata_fixture();
        data["reportData"]["report"]["masterData"]["actors"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":21,"name":"Different NPC","gameID":555,"type":"NPC"}));
        assert!(metadata(&data, &pull()).is_err());
    }

    #[test]
    fn metadata_targets_keep_same_name_actors_and_reject_excessive_counts() {
        let (_, actors) = metadata(&metadata_fixture(), &pull()).unwrap();
        let entries = targets(actors).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, entries[1].name);
        assert_ne!(entries[0].actor, entries[1].actor);
        assert_ne!(entries[0].game_id, entries[1].game_id);
        assert!(targets(
            (1..=65)
                .map(|id| (
                    id,
                    Actor {
                        name: "NPC".into(),
                        game_id: id
                    }
                ))
                .collect()
        )
        .is_err());
    }

    #[test]
    fn percent_bands_preserve_alternatives_gaps_and_all_supplied_game_ids() {
        let input = vec![
            HealthBand {
                game_ids: vec![102, 101, 101],
                min_percent: 74.101,
                max_percent: 78.009,
            },
            HealthBand {
                game_ids: vec![101, 102],
                min_percent: 77.0,
                max_percent: 80.0,
            },
            HealthBand {
                game_ids: vec![101, 102],
                min_percent: 0.0,
                max_percent: 9.0,
            },
        ];
        let bands = canonical_bands(&input).unwrap();
        assert_eq!(
            bands,
            vec![
                CanonicalBand {
                    game_ids: vec![101, 102],
                    min: 0,
                    max: 900
                },
                CanonicalBand {
                    game_ids: vec![101, 102],
                    min: 7410,
                    max: 8000
                }
            ]
        );
        let targets = targets(metadata(&metadata_fixture(), &pull()).unwrap().1).unwrap();
        let filter = band_filter(&bands, &targets).unwrap();
        assert!(filter.contains("target.id = 101 OR target.id = 102"));
        assert!(filter.contains("(7410 * resources.maxHitPoints)"));
        let point = |hp| HealthPoint {
            elapsed_seconds: 10.0,
            hit_points: hp,
            max_hit_points: 10000,
        };
        assert!(bands.iter().any(|band| band.contains(102, &point(7410))));
        assert!(!bands.iter().any(|band| band.contains(102, &point(5000))));
        assert!(!bands.iter().any(|band| band.contains(999, &point(7500))));
        let reversed = input.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(
            band_filter(&canonical_bands(&reversed).unwrap(), &targets).unwrap(),
            filter
        );
    }

    #[test]
    fn invalid_or_unrelated_percent_band_queries_are_rejected() {
        let base = HealthBand {
            game_ids: vec![101],
            min_percent: 70.0,
            max_percent: 80.0,
        };
        for (min, max) in [
            (f64::NAN, 80.0),
            (70.0, f64::INFINITY),
            (-1.0, 80.0),
            (80.0, 70.0),
            (70.0, 101.0),
        ] {
            assert!(canonical_bands(&[HealthBand {
                min_percent: min,
                max_percent: max,
                ..base.clone()
            }])
            .is_err());
        }
        assert!(canonical_bands(&[]).is_err());
        assert!(canonical_bands(&vec![base.clone(); 65]).is_err());
        assert!(canonical_bands(&[HealthBand {
            game_ids: vec![0],
            ..base.clone()
        }])
        .is_err());
        let targets = targets(metadata(&metadata_fixture(), &pull()).unwrap().1).unwrap();
        assert!(band_filter(
            &canonical_bands(&[HealthBand {
                game_ids: vec![999],
                ..base
            }])
            .unwrap(),
            &targets
        )
        .is_err());
    }

    #[test]
    fn sparse_band_acquisition_keeps_time_gaps_and_skips_cached_metadata_network_work() {
        let targets = targets(metadata(&metadata_fixture(), &pull()).unwrap().1).unwrap();
        let scope = Scope {
            start: 0,
            end: 333999,
            filter: HEALTH_FILTER,
            targets: Some(&targets),
        };
        let mut calls = 0;
        let result = acquire_scope(
            &pull(),
            scope,
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(1),
            |query, _| {
                assert_eq!(query, EVENTS_QUERY);
                calls += 1;
                Ok(page(
                    vec![
                        event(21, Some(1), 11000, 750),
                        event(21, Some(1), 210000, 700),
                    ],
                    None,
                ))
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            result.traces[0]
                .points
                .iter()
                .map(|point| point.elapsed_seconds)
                .collect::<Vec<_>>(),
            vec![1.0, 200.0]
        );
    }

    #[test]
    fn health_pagination_never_returns_a_partial_success() {
        for mode in ["limit", "oversized", "loop", "missing"] {
            let mut calls = 0;
            let result = acquire(
                &pull(),
                0,
                120000,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(1),
                |_, variables| {
                    calls += 1;
                    if calls == 1 {
                        return Ok(metadata_fixture());
                    }
                    let start = variables["start"].as_i64().unwrap();
                    Ok(match mode {
                        "oversized" => page(vec![event(21, None, 11000, 900); 2001], None),
                        "loop" => page(vec![], Some(start)),
                        "missing" => json!({"reportData":{"report":{"events":{"data":[]}}}}),
                        _ => page(vec![event(21, None, 11000, 900)], Some(start + 1)),
                    })
                },
            );
            assert!(
                result.is_err(),
                "{mode} unexpectedly accepted incomplete data"
            );
            assert!(calls <= MAX_PAGES + 1);
        }
    }

    #[test]
    fn cancelled_or_expired_health_work_cannot_publish_or_start_another_page() {
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let result = acquire(
            &pull(),
            0,
            120000,
            &cancel,
            Instant::now() + Duration::from_secs(1),
            |_, _| {
                calls += 1;
                if calls == 1 {
                    return Ok(metadata_fixture());
                }
                cancel.store(true, Ordering::Relaxed);
                Ok(page(vec![event(21, None, 11000, 900)], Some(12000)))
            },
        );
        assert_eq!(result.unwrap_err(), super::super::CANCELLED);
        assert_eq!(calls, 2);
        let expired = Instant::now() - Duration::from_secs(1);
        assert!(acquire(
            &pull(),
            0,
            120000,
            &AtomicBool::new(false),
            expired,
            |_, _| panic!("Expired lookup started transport")
        )
        .is_err());
        let deadline = Instant::now() + Duration::from_millis(5);
        assert!(acquire(
            &pull(),
            0,
            120000,
            &AtomicBool::new(false),
            deadline,
            |_, _| {
                std::thread::sleep(Duration::from_millis(10));
                Ok(metadata_fixture())
            }
        )
        .is_err());
        assert_eq!(request_timeout(None).unwrap(), Duration::from_secs(15));
        assert!(
            request_timeout(Some(Instant::now() + Duration::from_millis(50))).unwrap()
                <= Duration::from_millis(50)
        );
    }

    #[test]
    fn excessive_actor_instance_groups_are_rejected() {
        let (start, actors) = metadata(&metadata_fixture(), &pull()).unwrap();
        let entries = (1..=65)
            .map(|instance| event(21, Some(instance), 11000, 900))
            .collect::<Vec<_>>();
        assert!(append_page(&entries, &actors, start, 0, 120000, &mut BTreeMap::new()).is_err());
    }

    #[test]
    fn expired_credentials_without_refresh_discard_cached_health() {
        let mut client = Client::new().unwrap();
        client.health.insert(
            ("abcdefghABCDEFGH".into(), 7, 0, 1000),
            (
                Instant::now(),
                HealthWindow {
                    start_ms: 0,
                    end_ms: 1000,
                    traces: vec![],
                },
            ),
        );
        client.session = Some(super::super::Session {
            client_id: "test-client".into(),
            user_id: "1".into(),
            access_token: "test-token".into(),
            refresh_token: None,
            expires_at: 0,
        });
        client.health_targets.insert(
            ("abcdefghABCDEFGH".into(), 7, 0, 1000),
            (Instant::now(), vec![]),
        );
        assert!(client.access_token().is_err());
        assert!(client.health.is_empty());
        assert!(client.health_targets.is_empty());
    }

    fn private_pull() -> (Pull, i64) {
        assert_eq!(
            crate::presence::endpoint_url("/").unwrap().host_str(),
            Some("127.0.0.1")
        );
        let path = std::env::var("BRICK_HEALTH_PULL_FIXTURE")
            .expect("Set BRICK_HEALTH_PULL_FIXTURE to a private local pull fixture");
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() <= 16384);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let pull = Pull {
            report: value["report"].as_str().unwrap().into(),
            id: value["id"].as_u64().unwrap(),
            encounter: 0,
            difficulty: 0,
            report_start_ms: value["reportStartMs"].as_i64().unwrap(),
            remaining: None,
            name: String::new(),
            kill: false,
            last_phase: None,
            last_phase_is_intermission: false,
            start_ms: value["startMs"].as_i64().unwrap(),
            end_ms: value["endMs"].as_i64().unwrap(),
            seconds: 0,
        };
        let center = value["centerMs"].as_i64().unwrap();
        (pull, center)
    }

    #[test]
    #[ignore = "requires an explicit private pull fixture, localhost service and protected WCL sign-in"]
    fn private_health_targets_fixture() {
        let (pull, _) = private_pull();
        let token = crate::discord_auth::current_or_refreshed_access_token()
            .unwrap()
            .unwrap();
        let mut client = Client::new().unwrap();
        let targets = client.health_targets(&token, &pull).unwrap();
        assert!(!targets.is_empty() && targets.len() <= MAX_GROUPS);
        assert!(targets.windows(2).all(|pair| pair[0].actor < pair[1].actor));
        assert!(targets
            .iter()
            .all(|target| target.game_id > 0 && !target.name.is_empty()));
        assert!(client.health_targets(&token, &pull).unwrap() == targets);
        println!(
            "Protected health metadata passed: {} distinct hostile actors.",
            targets.len()
        );
    }

    #[test]
    #[ignore = "requires explicit private pull and OCR band fixtures, localhost service and protected WCL sign-in"]
    fn private_health_band_fixture() {
        let (pull, _) = private_pull();
        let path = std::env::var("BRICK_HEALTH_BANDS_FIXTURE")
            .expect("Set BRICK_HEALTH_BANDS_FIXTURE to local OCR-derived bands");
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() <= 16384);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let bands: Vec<_> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|band| HealthBand {
                game_ids: band["game_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| id.as_u64().unwrap())
                    .collect(),
                min_percent: band["min_percent"].as_f64().unwrap(),
                max_percent: band["max_percent"].as_f64().unwrap(),
            })
            .collect();
        let token = crate::discord_auth::current_or_refreshed_access_token()
            .unwrap()
            .unwrap();
        let mut client = Client::new().unwrap();
        let window = client.health_band_window(&token, &pull, &bands).unwrap();
        let count: usize = window.traces.iter().map(|trace| trace.points.len()).sum();
        assert!(!window.traces.is_empty() && count > 100 && count <= MAX_PAGES * PAGE_SIZE);
        assert_eq!(
            (window.start_ms, window.end_ms),
            (0, pull.end_ms - pull.start_ms)
        );
        let canonical = canonical_bands(&bands).unwrap();
        assert!(window
            .traces
            .iter()
            .all(|trace| trace.points.iter().all(|point| canonical
                .iter()
                .any(|band| band.contains(trace.game_id, point)))));
        assert!(client.health_band_window(&token, &pull, &bands).unwrap() == window);
        if let Ok(path) = std::env::var("BRICK_HEALTH_OUTPUT") {
            let value = json!({"start_ms":window.start_ms,"end_ms":window.end_ms,"traces":window.traces.iter().map(|trace|json!({
                "actor":trace.actor,"instance":trace.instance,"game_id":trace.game_id,"name":trace.name,
                "points":trace.points.iter().map(|point|json!({"elapsed_seconds":point.elapsed_seconds,"hit_points":point.hit_points,"max_hit_points":point.max_hit_points})).collect::<Vec<_>>()
            })).collect::<Vec<_>>()});
            crate::atomic_file::write(
                std::path::Path::new(&path),
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
        println!(
            "Protected percent-band lookup passed: {} actor/instance traces, {count} points.",
            window.traces.len()
        );
    }

    #[test]
    #[ignore = "requires an explicit private pull fixture, localhost service and protected WCL sign-in"]
    fn private_health_fixture() {
        let (pull, center) = private_pull();
        let token = crate::discord_auth::current_or_refreshed_access_token()
            .unwrap()
            .unwrap();
        let mut client = Client::new().unwrap();
        let window = client.health_window(&token, &pull, center).unwrap();
        let count: usize = window.traces.iter().map(|trace| trace.points.len()).sum();
        assert!(!window.traces.is_empty() && count > 100 && count <= MAX_PAGES * PAGE_SIZE);
        for trace in &window.traces {
            assert!(trace.actor > 0 && trace.game_id > 0 && !trace.name.is_empty());
            assert!(trace
                .points
                .windows(2)
                .all(|points| points[0].elapsed_seconds <= points[1].elapsed_seconds));
            assert!(trace
                .points
                .iter()
                .all(|point| point.hit_points <= point.max_hit_points && point.max_hit_points > 0));
        }
        assert!(client.health_window(&token, &pull, center).unwrap() == window);
        println!(
            "Protected health lookup passed: {} actor/instance traces, {count} health points.",
            window.traces.len()
        );
    }
}
