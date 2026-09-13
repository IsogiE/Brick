//! Viewer-local VOD filtering from complete, authenticated guild report reads.
//! Server history and timestamp measurements are never removed by this filter.
use crate::{streams::Vod, warcraftlogs::Client};
use eframe::egui;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

const GRACE_MS: i64 = 48 * 60 * 60 * 1000;
const MARGIN_MS: i64 = 5 * 60 * 1000;
const REFRESH: Duration = Duration::from_secs(5 * 60);
const CATALOG_REFRESH_MS: i64 = 60 * 60 * 1000;
const RETRY: Duration = Duration::from_secs(5 * 60);
const DIRECTORY_QUERY: &str = "query($guild:Int!,$end:Float!,$page:Int!){reportData{reports(guildID:$guild,startTime:0,endTime:$end,page:$page,limit:100){total current_page last_page has_more_pages data{code startTime endTime guild{id}}}}}";
const FIGHTS_QUERY: &str = "query($code:String!){reportData{report(code:$code){code startTime endTime guild{id} fights{encounterID difficulty startTime endTime}}}}";
const INCOMPLETE: &str = "The recording raid check is incomplete.";

fn milliseconds(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .filter(|value| {
            value.is_finite()
                && value.fract() == 0.0
                && *value >= 0.0
                && *value <= 9_007_199_254_740_991.0
        })
        .map(|value| value as i64)
}

fn bounds(vod: &Vod) -> Option<(i64, i64)> {
    let stream = vod.as_stream();
    let start = stream.replay_start_ms?;
    let end = stream.replay_end_ms?;
    (start > 0 && end > start).then_some((start, end))
}

fn key(vod: &Vod) -> String {
    format!(
        "{}:{}:{}:{}",
        vod.provider.key(),
        vod.id,
        vod.started_at.as_deref().unwrap_or(""),
        vod.ended_at.as_deref().unwrap_or("")
    )
}

fn merged_ranges(ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    merge_ranges(ranges, MARGIN_MS)
}

fn merge_ranges(mut ranges: Vec<(i64, i64)>, margin: i64) -> Vec<(i64, i64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in ranges {
        let start = start.saturating_sub(margin);
        let end = end.saturating_add(margin);
        if let Some(previous) = merged.last_mut().filter(|previous| previous.1 >= start) {
            previous.1 = previous.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn has_overlap(ranges: &[(i64, i64)], range: (i64, i64)) -> bool {
    let index = ranges.partition_point(|candidate| candidate.1 < range.0);
    ranges
        .get(index)
        .is_some_and(|candidate| candidate.0 <= range.1)
}

#[derive(Default)]
pub(crate) struct Catalog {
    complete: Option<(u64, i64, HashMap<String, (i64, i64)>)>,
    pending: Option<Sweep>,
    classified: HashMap<String, Classified>,
}
struct Classified {
    bounds: (i64, i64),
    checked_at: i64,
    ranges: Vec<(i64, i64)>,
}
struct Sweep {
    guild: u64,
    end: i64,
    page: u64,
    total: Option<u64>,
    first: Option<Value>,
    reports: HashMap<String, (i64, i64)>,
}

impl Catalog {
    fn refresh(
        &mut self,
        guild: u64,
        now: i64,
        query: &mut impl FnMut(&str, Value) -> Result<Value, String>,
    ) -> Result<(), String> {
        if self
            .complete
            .as_ref()
            .is_some_and(|(owner, _, _)| *owner != guild)
            || self
                .pending
                .as_ref()
                .is_some_and(|sweep| sweep.guild != guild)
        {
            *self = Self::default();
        }
        if self.pending.is_none()
            && self
                .complete
                .as_ref()
                .is_some_and(|(_, at, _)| now.saturating_sub(*at) < CATALOG_REFRESH_MS)
        {
            return Ok(());
        }
        self.pending.get_or_insert_with(|| Sweep {
            guild,
            end: now,
            page: 1,
            total: None,
            first: None,
            reports: HashMap::new(),
        });
        // Initial history makes bounded progress over as many passes as needed.
        // Refreshes amortize old history one page at a time while retaining the
        // last complete catalog. No partial sweep can establish absence.
        let budget = if self.complete.is_none() { 16 } else { 1 };
        for _ in 0..budget {
            let sweep = self.pending.as_mut().unwrap();
            let page = sweep.page;
            let data = query(
                DIRECTORY_QUERY,
                json!({"guild":guild,"end":sweep.end,"page":page}),
            )?;
            let batch = &data["reportData"]["reports"];
            let parsed = (|| {
                let total = batch["total"]
                    .as_u64()
                    .filter(|total| *total <= 100_000)
                    .ok_or(INCOMPLETE)?;
                let last = total.div_ceil(100).max(1);
                let rows = batch["data"].as_array().ok_or(INCOMPLETE)?;
                if batch["current_page"].as_u64() != Some(page)
                    || batch["last_page"].as_u64() != Some(last)
                    || batch["has_more_pages"].as_bool() != Some(page < last)
                    || rows.len() as u64 != total.saturating_sub((page - 1) * 100).min(100)
                    || sweep.total.is_some_and(|previous| previous != total)
                {
                    return Err(INCOMPLETE);
                }
                let mut additions = HashMap::new();
                for row in rows {
                    let code = row["code"]
                        .as_str()
                        .filter(|code| {
                            code.len() == 16 && code.bytes().all(|c| c.is_ascii_alphanumeric())
                        })
                        .ok_or(INCOMPLETE)?;
                    let start = milliseconds(&row["startTime"]).ok_or(INCOMPLETE)?;
                    let stop = milliseconds(&row["endTime"])
                        .filter(|stop| *stop >= start)
                        .ok_or(INCOMPLETE)?;
                    if row["guild"]["id"].as_u64() != Some(guild)
                        || start > sweep.end
                        || sweep.reports.contains_key(code)
                        || additions.insert(code.to_owned(), (start, stop)).is_some()
                    {
                        return Err(INCOMPLETE);
                    }
                }
                Ok((total, last, additions))
            })();
            let (total, last, additions) = match parsed {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.pending = None;
                    return Err(error.into());
                }
            };
            if page == last && page > 1 {
                let check = query(
                    DIRECTORY_QUERY,
                    json!({"guild":guild,"end":sweep.end,"page":1}),
                )?;
                if Some(&check["reportData"]["reports"]) != sweep.first.as_ref() {
                    self.pending = None;
                    return Err(INCOMPLETE.into());
                }
            }
            if page == 1 {
                sweep.first = Some(batch.clone());
            }
            sweep.total = Some(total);
            sweep.reports.extend(additions);
            sweep.page += 1;
            if page == last {
                let sweep = self.pending.take().unwrap();
                if sweep.reports.len() as u64 != total {
                    return Err(INCOMPLETE.into());
                }
                self.complete = Some((guild, sweep.end, sweep.reports));
                return Ok(());
            }
        }
        if self.complete.is_some() {
            Ok(())
        } else {
            Err(INCOMPLETE.into())
        }
    }
}

#[cfg(test)]
fn load_hidden(
    vods: &[Vod],
    guild_id: u64,
    now_ms: i64,
    query: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Result<HashSet<String>, String> {
    load_hidden_with_catalog(vods, guild_id, now_ms, &mut Catalog::default(), query)
}

/// Only complete metadata coverage and successful fight reads yield decisions.
/// Cached coverage is limited to VODs already past upload grace when collected.
pub(crate) fn load_hidden_with_catalog(
    vods: &[Vod],
    guild_id: u64,
    now_ms: i64,
    catalog: &mut Catalog,
    mut query: impl FnMut(&str, Value) -> Result<Value, String>,
) -> Result<HashSet<String>, String> {
    if vods.len() > 10_000 || guild_id == 0 || guild_id > i32::MAX as u64 {
        return Err(INCOMPLETE.into());
    }
    let mut eligible: Vec<_> = vods
        .iter()
        .filter_map(|vod| {
            let range = bounds(vod)?;
            (range.1 <= now_ms.saturating_sub(GRACE_MS)).then(|| (key(vod), range))
        })
        .collect();
    if eligible.is_empty() {
        return Ok(HashSet::new());
    }
    catalog.refresh(guild_id, now_ms, &mut query)?;
    let (_, captured_at, reports) = catalog.complete.as_ref().ok_or(INCOMPLETE)?;
    eligible.retain(|(_, range)| range.1 <= captured_at.saturating_sub(GRACE_MS));
    let vod_ranges = merged_ranges(eligible.iter().map(|(_, range)| *range).collect());
    let relevant: Vec<_> = reports
        .iter()
        .filter(|(_, range)| has_overlap(&vod_ranges, **range))
        .collect();
    catalog
        .classified
        .retain(|code, _| reports.contains_key(code));
    let mut raid_ranges = Vec::new();
    let mut requests = 0;
    for (code, range) in relevant {
        let cached = catalog.classified.get(code).is_some_and(|entry| {
            entry.bounds == *range
                && now_ms.saturating_sub(entry.checked_at) < 6 * CATALOG_REFRESH_MS
        });
        if !cached {
            if requests >= 32 {
                return Err(INCOMPLETE.into());
            }
            requests += 1;
            let data = query(FIGHTS_QUERY, json!({"code":code}))?;
            let ranges = classify_report(&data["reportData"]["report"], code, range, guild_id)?;
            let stored: usize = catalog
                .classified
                .iter()
                .filter(|(other, _)| *other != code)
                .map(|(_, entry)| entry.ranges.len())
                .sum();
            if stored + ranges.len() > 200_000 {
                return Err(INCOMPLETE.into());
            }
            catalog.classified.insert(
                code.clone(),
                Classified {
                    bounds: *range,
                    checked_at: now_ms,
                    ranges,
                },
            );
        }
        raid_ranges.extend_from_slice(&catalog.classified[code].ranges);
    }
    let raid_ranges = merged_ranges(raid_ranges);
    Ok(eligible
        .into_iter()
        .filter(|(_, range)| !has_overlap(&raid_ranges, *range))
        .map(|(key, _)| key)
        .collect())
}

fn classify_report(
    report: &Value,
    code: &str,
    range: &(i64, i64),
    guild_id: u64,
) -> Result<Vec<(i64, i64)>, String> {
    let mut raid_ranges = Vec::new();

    if report["code"].as_str() != Some(code)
        || report["guild"]["id"].as_u64() != Some(guild_id)
        || milliseconds(&report["startTime"]) != Some(range.0)
        || milliseconds(&report["endTime"]) != Some(range.1)
    {
        return Err(INCOMPLETE.into());
    }
    let fights = report["fights"]
        .as_array()
        .filter(|fights| fights.len() <= 5000)
        .ok_or(INCOMPLETE)?;
    // Empty or unknown-format logs are kept conservatively. WCL difficulty
    // 10 identifies dungeon/M+; 1 through 5 are the raid difficulties.
    if fights.is_empty() {
        raid_ranges.push(*range);
    }
    for fight in fights {
        if fight["encounterID"].as_u64() == Some(0) {
            continue;
        }
        let Some(difficulty) = fight["difficulty"].as_u64() else {
            // A boss with an unknown difficulty is uncertain, so retain its
            // report range. Aggregate/trash rows were excluded above.
            raid_ranges.push(*range);
            continue;
        };
        if difficulty == 10 {
            continue;
        }
        if !(1..=5).contains(&difficulty) {
            raid_ranges.push(*range);
            continue;
        }
        let start = milliseconds(&fight["startTime"])
            .and_then(|v| range.0.checked_add(v))
            .ok_or(INCOMPLETE)?;
        let end = milliseconds(&fight["endTime"])
            .and_then(|v| range.0.checked_add(v))
            .filter(|end| *end >= start && *end <= range.1)
            .ok_or(INCOMPLETE)?;
        raid_ranges.push((start, end));
    }
    Ok(merge_ranges(raid_ranges, 0))
}

#[derive(Default)]
pub(crate) struct Filter {
    hidden: HashSet<String>,
    source: Weak<Vec<Vod>>,
    visible: Option<Rc<Vec<Vod>>>,
    work: Option<mpsc::Receiver<Result<(u64, HashSet<String>), String>>>,
    cancel: Arc<AtomicBool>,
    next_at: Option<Instant>,
    active: bool,
    last_attempt: Option<Instant>,
    auth_epoch: Option<u64>,
}

impl Drop for Filter {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Filter {
    pub(crate) fn visible(&mut self, source: &Rc<Vec<Vod>>) -> Rc<Vec<Vod>> {
        if !Weak::ptr_eq(&self.source, &Rc::downgrade(source)) || self.visible.is_none() {
            self.source = Rc::downgrade(source);
            self.visible = Some(if self.hidden.is_empty() {
                source.clone()
            } else {
                Rc::new(
                    source
                        .iter()
                        .filter(|vod| !self.hidden.contains(&key(vod)))
                        .cloned()
                        .collect(),
                )
            });
        }
        self.visible.as_ref().unwrap().clone()
    }

    pub(crate) fn tick(
        &mut self,
        ctx: &egui::Context,
        source: Option<&Rc<Vec<Vod>>>,
        active: bool,
        client: Arc<Mutex<Option<Client>>>,
    ) {
        if let Some(epoch) = client
            .try_lock()
            .ok()
            .and_then(|lock| lock.as_ref().map(Client::recording_auth_epoch))
        {
            if self.auth_epoch != Some(epoch) {
                self.auth_epoch = Some(epoch);
                self.hidden.clear();
                self.visible = None;
                self.next_at = None;
            }
        }
        if active && !self.active && self.last_attempt.is_none_or(|at| at.elapsed() >= RETRY) {
            self.next_at = None;
        }
        self.active = active;
        if !active {
            self.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(result) = self.work.as_ref().and_then(|work| match work.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(INCOMPLETE.into())),
            Err(mpsc::TryRecvError::Empty) => None,
        }) {
            self.work = None;
            match result {
                Ok((epoch, hidden)) if self.auth_epoch == Some(epoch) => {
                    self.next_at = Some(Instant::now() + REFRESH);
                    self.hidden = hidden;
                    self.visible = None;
                }
                Ok(_) => self.next_at = None,
                Err(_) => self.next_at = Some(Instant::now() + RETRY),
            }
        }
        let Some(source) = source.filter(|source| !source.is_empty()) else {
            return;
        };
        if !active || self.work.is_some() || self.next_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        let recordings = (**source).clone();
        self.last_attempt = Some(Instant::now());
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = (|| {
                // Foreground review owns this client first. Cleanup never waits
                // behind it or starts a second WCL session/token refresh.
                let mut lock = client.try_lock().map_err(|_| INCOMPLETE)?;
                let token = crate::warcraftlogs::while_current(
                    &cancel,
                    crate::discord_auth::current_or_refreshed_access_token,
                )??
                .ok_or(INCOMPLETE)?;
                if lock.is_none() {
                    *lock = Some(Client::new()?);
                }
                let client = lock.as_mut().unwrap();
                client.set_request_cancellation(cancel.clone());
                client
                    .unrelated_recordings(&token, &recordings)
                    .map(|hidden| (client.recording_auth_epoch(), hidden))
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.work = Some(rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GUILD: u64 = 580482;
    const START: i64 = 1_789_000_000_000;
    const END: i64 = START + 7_200_000;
    const NOW: i64 = END + GRACE_MS + 1;
    fn date(value: i64) -> String {
        time::OffsetDateTime::from_unix_timestamp(value / 1000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }
    fn vod(id: &str, provider: &str, start: i64, end: i64) -> Vod {
        serde_json::from_value(
            json!({"id":id,"userId":"11","name":"Raider","provider":provider,
            "url":"https://www.twitch.tv/videos/123","startedAt":date(start),"endedAt":date(end)}),
        )
        .unwrap()
    }
    fn report(index: usize, start: i64, end: i64) -> Value {
        json!({"code":format!("{index:016}"),"startTime":start,"endTime":end,"guild":{"id":GUILD}})
    }
    fn page(reports: &[Value], number: u64) -> Value {
        let last = (reports.len() as u64).div_ceil(100).max(1);
        let from = (number as usize - 1) * 100;
        let to = (from + 100).min(reports.len());
        json!({"reportData":{"reports":{"total":reports.len(),"current_page":number,"last_page":last,
            "has_more_pages":number < last,"data":reports[from..to]}}})
    }
    fn details(mut report: Value, difficulty: Value, start: i64, end: i64) -> Value {
        report["fights"] =
            json!([{"difficulty":difficulty,"encounterID":1,"startTime":start,"endTime":end}]);
        json!({"reportData":{"report":report}})
    }
    #[test]
    fn recent_live_and_unknown_timing_vods_need_no_wcl_query() {
        let mut unknown = vod("123", "twitch", START, END);
        unknown.started_at = None;
        let mut live = vod("abcDEF12345", "youtube", START, END);
        live.ended_at = None;
        let recent = vod("456", "twitch", NOW - 100_000, NOW - 1000);
        let invalid = vod("789", "twitch", END, START);
        assert!(load_hidden(
            &[unknown, live, recent, invalid],
            GUILD,
            NOW,
            |_, _| panic!("No complete VOD is old enough")
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn raid_overlap_preserves_both_providers_without_markers_and_covers_reports_started_earlier() {
        let vods = vec![
            vod("123", "twitch", START, END),
            vod("abcDEF12345", "youtube", START, END),
        ];
        let row = report(1, START - 7 * GRACE_MS, END);
        let hidden = load_hidden(&vods, GUILD, NOW, |query, variables| {
            if query == DIRECTORY_QUERY {
                assert!(query.contains("startTime:0"));
                assert_eq!(variables["guild"], GUILD);
                Ok(page(std::slice::from_ref(&row), 1))
            } else {
                Ok(details(
                    row.clone(),
                    json!(5),
                    7 * GRACE_MS,
                    END - START + 7 * GRACE_MS,
                ))
            }
        })
        .unwrap();
        assert!(hidden.is_empty());
    }
    #[test]
    fn dungeon_only_recordings_are_hidden_and_late_raid_uploads_restore_original_metadata() {
        let item = vod("123", "twitch", START, END);
        let row = report(1, START, END);
        let scan = |difficulty| {
            load_hidden(std::slice::from_ref(&item), GUILD, NOW, |query, _| {
                Ok(if query == DIRECTORY_QUERY {
                    page(std::slice::from_ref(&row), 1)
                } else {
                    details(row.clone(), json!(difficulty), 0, END - START)
                })
            })
            .unwrap()
        };
        let original = Rc::new(vec![item.clone()]);
        let mut filter = Filter::default();
        filter.hidden = scan(10);
        assert!(filter.visible(&original).is_empty());
        assert_eq!(original.len(), 1);
        filter.hidden = scan(5);
        filter.visible = None;
        assert_eq!(filter.visible(&original).len(), 1);
    }
    #[test]
    fn a_raid_log_keeps_only_recordings_overlapping_its_raid_fights() {
        let old = vod("123", "twitch", START, START + 100_000);
        let raid = vod("456", "twitch", END - 100_000, END);
        let row = report(1, START, END);
        let hidden = load_hidden(&[old.clone(), raid], GUILD, NOW, |query, _| {
            Ok(if query == DIRECTORY_QUERY {
                page(std::slice::from_ref(&row), 1)
            } else {
                details(row.clone(), json!(5), END - START - 50_000, END - START)
            })
        })
        .unwrap();
        assert_eq!(hidden, HashSet::from([key(&old)]));
    }
    #[test]
    fn complete_pagination_includes_matching_last_page_and_rechecks_first_page() {
        let item = vod("123", "twitch", START, END);
        let mut reports: Vec<_> = (0..100)
            .map(|i| report(i, START - GRACE_MS, START - GRACE_MS + 1000))
            .collect();
        let relevant = report(100, START, END);
        reports.push(relevant.clone());
        let mut pages = Vec::new();
        let hidden = load_hidden(&[item], GUILD, NOW, |query, vars| {
            if query == DIRECTORY_QUERY {
                let number = vars["page"].as_u64().unwrap();
                pages.push(number);
                Ok(page(&reports, number))
            } else {
                Ok(details(relevant.clone(), json!(5), 0, END - START))
            }
        })
        .unwrap();
        assert_eq!(pages, [1, 2, 1]);
        assert!(hidden.is_empty());
    }
    #[test]
    fn failed_partial_changed_and_cross_guild_pages_publish_no_filter_decisions() {
        let item = vod("123", "twitch", START, END);
        for mode in [
            "outage",
            "duplicate",
            "insert",
            "guild",
            "missing-time",
            "partial",
            "limit",
        ] {
            let mut reports: Vec<_> = (0..101)
                .map(|i| report(i, START - GRACE_MS, START - GRACE_MS + 1000))
                .collect();
            if mode == "duplicate" {
                reports[100] = reports[0].clone();
            }
            if mode == "guild" {
                reports[0]["guild"]["id"] = json!(198);
            }
            if mode == "missing-time" {
                reports[0]["startTime"] = Value::Null;
            }
            let mut calls = 0;
            assert!(
                load_hidden(std::slice::from_ref(&item), GUILD, NOW, |_, vars| {
                    calls += 1;
                    if mode == "outage" && calls == 2 {
                        return Err("Temporary API failure".into());
                    }
                    if mode == "insert" && calls == 3 {
                        reports[0] = report(999, START, END);
                    }
                    let mut data = page(&reports, vars["page"].as_u64().unwrap());
                    if mode == "partial" {
                        data["reportData"]["reports"]["has_more_pages"] = Value::Null;
                    }
                    if mode == "limit" {
                        data["reportData"]["reports"]["total"] = json!(100_001);
                    }
                    Ok(data)
                })
                .is_err(),
                "{mode}"
            );
        }
    }
    #[test]
    fn inaccessible_changed_and_malformed_report_details_never_hide_vods() {
        let item = vod("123", "twitch", START, END);
        let row = report(1, START, END);
        for detail in [
            json!({"reportData":{"report":null}}),
            details(report(2, START, END), json!(10), 0, END - START),
            details(row.clone(), json!(5), -1, END - START),
        ] {
            assert!(
                load_hidden(std::slice::from_ref(&item), GUILD, NOW, |query, _| {
                    Ok(if query == DIRECTORY_QUERY {
                        page(std::slice::from_ref(&row), 1)
                    } else {
                        detail.clone()
                    })
                })
                .is_err()
            );
        }
    }
    #[test]
    fn unknown_difficulty_formats_remain_visible() {
        let item = vod("123", "twitch", START, END);
        let row = report(1, START, END);
        for difficulty in [json!(0), json!(100)] {
            assert!(
                load_hidden(std::slice::from_ref(&item), GUILD, NOW, |query, _| {
                    Ok(if query == DIRECTORY_QUERY {
                        page(std::slice::from_ref(&row), 1)
                    } else {
                        details(row.clone(), difficulty.clone(), 0, END - START)
                    })
                })
                .unwrap()
                .is_empty()
            );
        }
    }
    #[test]
    fn interval_lookup_retains_clock_margin_and_changed_vod_bounds_require_a_new_check() {
        let ranges = merged_ranges(vec![(2000, 3000), (1000, 1500), (1_000_000, 1_001_000)]);
        assert_eq!(ranges.len(), 2);
        assert!(has_overlap(&ranges, (303_000, 304_000)));
        assert!(!has_overlap(&ranges, (303_001, 600_000)));
        let first = vod("123", "twitch", START, END);
        let changed = vod("123", "twitch", START, END + 1000);
        let mut filter = Filter::default();
        filter.hidden = HashSet::from([key(&first)]);
        assert!(filter.visible(&Rc::new(vec![first])).is_empty());
        assert_eq!(filter.visible(&Rc::new(vec![changed])).len(), 1);
    }
    #[test]
    fn list_cache_reuses_allocations_and_guild_reset_drops_filter_decisions() {
        let source = Rc::new(vec![vod("123", "twitch", START, END)]);
        let mut filter = Filter::default();
        let first = filter.visible(&source);
        assert!(Rc::ptr_eq(&first, &source));
        assert!(Rc::ptr_eq(&first, &filter.visible(&source)));
        filter.hidden.insert(key(&source[0]));
        filter.visible = None;
        assert!(filter.visible(&source).is_empty());
        filter = Filter::default();
        assert_eq!(filter.visible(&source).len(), 1);
    }

    #[test]
    fn catalogs_larger_than_3200_reports_resume_without_restarting_or_publishing_partial_absence() {
        let item = vod("123", "twitch", START, END);
        let reports: Vec<_> = (0..4001)
            .map(|i| report(i, START - GRACE_MS, START - GRACE_MS + 1000))
            .collect();
        let mut catalog = Catalog::default();
        let mut pages = Vec::new();
        for pass in 0..3 {
            let result = load_hidden_with_catalog(
                std::slice::from_ref(&item),
                GUILD,
                NOW,
                &mut catalog,
                |query, vars| {
                    assert_eq!(query, DIRECTORY_QUERY);
                    let number = vars["page"].as_u64().unwrap();
                    pages.push(number);
                    Ok(page(&reports, number))
                },
            );
            if pass < 2 {
                assert!(result.is_err());
                assert!(catalog.complete.is_none());
            } else {
                assert_eq!(result.unwrap(), HashSet::from([key(&item)]));
            }
        }
        assert_eq!(pages, (1..=41).chain([1]).collect::<Vec<_>>());
        assert_eq!(catalog.complete.as_ref().unwrap().2.len(), 4001);
        assert!(catalog.pending.is_none());
        assert_eq!(
            load_hidden_with_catalog(
                &[item.clone()],
                GUILD,
                NOW + 1,
                &mut catalog,
                |_, _| panic!("A complete fresh catalog is reused")
            )
            .unwrap(),
            HashSet::from([key(&item)])
        );
    }

    #[test]
    fn detail_budget_resumes_larger_recording_histories_and_cached_checks_make_no_requests() {
        let item = vod("123", "twitch", START, END);
        let reports: Vec<_> = (0..40).map(|i| report(i, START, END)).collect();
        let mut catalog = Catalog::default();
        let mut detail_calls = 0;
        for pass in 0..2 {
            let result = load_hidden_with_catalog(
                std::slice::from_ref(&item),
                GUILD,
                NOW,
                &mut catalog,
                |query, vars| {
                    if query == DIRECTORY_QUERY {
                        return Ok(page(&reports, 1));
                    }
                    detail_calls += 1;
                    let index = vars["code"].as_str().unwrap().parse::<usize>().unwrap();
                    Ok(details(reports[index].clone(), json!(10), 0, END - START))
                },
            );
            if pass == 0 {
                assert!(result.is_err());
                assert_eq!(detail_calls, 32);
            } else {
                assert_eq!(result.unwrap(), HashSet::from([key(&item)]));
            }
        }
        assert_eq!(detail_calls, 40);
        assert_eq!(
            load_hidden_with_catalog(
                &[item.clone()],
                GUILD,
                NOW + 1,
                &mut catalog,
                |_, _| panic!("All completed detail checks are cached")
            )
            .unwrap(),
            HashSet::from([key(&item)])
        );
    }

    #[test]
    fn completed_catalog_reaudits_one_page_per_pass_and_late_upload_restores_the_vod() {
        let item = vod("123", "twitch", START, END);
        let old: Vec<_> = (0..101)
            .map(|i| report(i, START - GRACE_MS, START - GRACE_MS + 1000))
            .collect();
        let mut catalog = Catalog::default();
        assert!(load_hidden_with_catalog(
            std::slice::from_ref(&item),
            GUILD,
            NOW,
            &mut catalog,
            |_, vars| Ok(page(&old, vars["page"].as_u64().unwrap()))
        )
        .unwrap()
        .contains(&key(&item)));
        let mut updated = old;
        updated[0] = report(999, START, END);
        let mut pages = Vec::new();
        for pass in 0..2 {
            let result = load_hidden_with_catalog(
                std::slice::from_ref(&item),
                GUILD,
                NOW + CATALOG_REFRESH_MS + pass,
                &mut catalog,
                |query, vars| {
                    if query == DIRECTORY_QUERY {
                        let number = vars["page"].as_u64().unwrap();
                        pages.push(number);
                        Ok(page(&updated, number))
                    } else {
                        Ok(details(updated[0].clone(), json!(5), 0, END - START))
                    }
                },
            )
            .unwrap();
            assert_eq!(result.contains(&key(&item)), pass == 0);
        }
        assert_eq!(pages, [1, 2, 1]);
    }

    #[test]
    fn trash_aggregate_rows_do_not_block_or_turn_a_dungeon_into_a_raid() {
        let item = vod("123", "twitch", START, END);
        let row = report(1, START, END);
        let hidden = load_hidden(std::slice::from_ref(&item), GUILD, NOW, |query, _| {
            if query == DIRECTORY_QUERY {
                return Ok(page(std::slice::from_ref(&row), 1));
            }
            let mut data = details(row.clone(), json!(10), 0, END - START);
            data["reportData"]["report"]["fights"]
                .as_array_mut()
                .unwrap()
                .push(json!({"encounterID":0,"difficulty":null,"startTime":-1,"endTime":0}));
            Ok(data)
        })
        .unwrap();
        assert!(hidden.contains(&key(&item)));
    }

    #[test]
    fn changing_wcl_identity_discards_hidden_decisions_and_old_worker_results() {
        let source = Rc::new(vec![vod("123", "twitch", START, END)]);
        let mut filter = Filter::default();
        filter.auth_epoch = Some(7);
        filter.hidden.insert(key(&source[0]));
        let (tx, rx) = mpsc::channel();
        tx.send(Ok((7, filter.hidden.clone()))).unwrap();
        filter.work = Some(rx);
        let client = Arc::new(Mutex::new(Some(Client::new().unwrap())));
        filter.tick(&egui::Context::default(), None, false, client);
        assert!(filter.hidden.is_empty());
        assert_eq!(filter.visible(&source).len(), 1);
    }
}
