//! Small authenticated sync observations; all image processing stays local.
use crate::{
    replay_sync::{Alignment, Cache, Key},
    warcraftlogs::{Pull, Replay, Review},
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::Duration,
};

const VERSION: u64 = 1;
const BASE: &str = "/v1/streams/review/sync";

fn key(replay: &Replay, pull: &Pull) -> Option<Value> {
    if !crate::replay_marker::supports_pull(pull) {
        return None;
    }
    Some(json!({
        "readerVersion": VERSION, "provider": replay.provider.key(), "videoId": replay.video_id,
        "broadcastId": replay.broadcast_id, "recordingStartMs": replay.start_ms().ok()?,
        "report": pull.report, "pullId": pull.id, "encounter": pull.encounter,
        "difficulty": pull.difficulty, "startMs": pull.start_ms, "endMs": pull.end_ms,
    }))
}
fn valid(alignment: Alignment, replay: &Replay, pull: &Pull, max_uncertainty: f64) -> bool {
    let Ok(start) = replay.start_ms() else {
        return false;
    };
    let estimate = (pull.start_ms - start) as f64 / 1000.0;
    alignment.unix_seconds.abs_diff(pull.start_ms / 1000) <= 3
        && alignment.video_seconds.is_finite()
        && (0.0..replay.available_seconds as f64).contains(&alignment.video_seconds)
        && (alignment.video_seconds - estimate).abs() <= 60.0
        && (0.05..=max_uncertainty).contains(&alignment.uncertainty_seconds)
}
fn request(token: &crate::guild::Access, route: &str, body: Value) -> Option<Value> {
    let url = token.endpoint(&format!("{BASE}/{route}")).ok()?;
    let response = crate::presence::http_client()
        .ok()?
        .post(url)
        .timeout(Duration::from_secs(2))
        .bearer_auth(token.secret())
        .json(&body)
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = crate::download::read_response(response, 128 * 1024, "Timestamp lookup").ok()?;
    serde_json::from_slice(&bytes).ok()
}

// Loading and saving run only on the metadata/submission workers. The UI
// neither opens files nor waits for a network request before accepting a local result.
struct ScopedCache {
    scope: String,
    cache: Cache,
}
fn local_cache(
    token: &crate::guild::Access,
) -> Option<std::sync::MutexGuard<'static, ScopedCache>> {
    token.check().ok()?;
    static CACHE: OnceLock<Mutex<ScopedCache>> = OnceLock::new();
    let mut state = CACHE
        .get_or_init(|| {
            Mutex::new(ScopedCache {
                scope: String::new(),
                cache: Cache::default(),
            })
        })
        .lock()
        .ok()?;
    let scope = token.cache_id();
    if state.scope != scope {
        // Locked vaults and authentication failures are not missing files.
        // Retry on a later worker without overwriting ciphertext or migrating.
        let bytes = crate::protected_cache::load(&scope).ok()?;
        let mut cache = bytes.as_deref().map(Cache::from_bytes).unwrap_or_default();
        // The baseline file belonged to Advance. Ascendance never reads it.
        if bytes.is_none() && token.guild_id == crate::guild::ADVANCE {
            if let Ok(dir) = crate::addon::config_dir() {
                use std::io::Read;
                let legacy = dir.join("raid-timestamps-v1.json");
                let mut bytes = Vec::new();
                if std::fs::File::open(&legacy)
                    .and_then(|file| file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes))
                    .is_ok()
                    && bytes.len() <= 2 * 1024 * 1024
                {
                    cache = Cache::from_bytes(&bytes);
                    if let Some(bytes) = cache.to_bytes() {
                        if crate::protected_cache::save(&scope, &bytes).is_ok() {
                            let _ = std::fs::remove_file(legacy);
                        }
                    }
                }
            }
        }
        *state = ScopedCache { scope, cache };
    }
    Some(state)
}
fn save_local(state: &ScopedCache) {
    if let Some(bytes) = state.cache.to_bytes() {
        let _ = crate::protected_cache::save(&state.scope, &bytes);
    }
}

/// One bounded lookup alongside metadata loading. An older/offline server is
/// optional: it cannot prevent reviewing a VOD or starting a local scan.
pub fn lookup(token: &crate::guild::Access, review: &mut Review) {
    if let Some(state) = local_cache(token) {
        for pull in &review.pulls {
            if let Some(alignment) = state
                .cache
                .get(&review.replay, pull)
                .filter(|a| valid(*a, &review.replay, pull, 0.35))
            {
                review
                    .marker_timing
                    .insert((pull.report.clone(), pull.id), alignment);
            }
        }
    }
    let keys: Vec<_> = review
        .pulls
        .iter()
        .rev()
        .filter_map(|pull| key(&review.replay, pull))
        .take(64)
        .collect();
    if keys.is_empty() {
        return;
    }
    let Some(response) = request(token, "lookup", json!({ "keys": keys })) else {
        return;
    };
    apply_response(review, &keys, &response);
    if !review.marker_timing.is_empty() {
        if let Some(mut state) = local_cache(token) {
            for pull in &review.pulls {
                if let Some(alignment) = review.marker_alignment(pull) {
                    state
                        .cache
                        .insert(Key::new(&review.replay, pull), alignment);
                }
            }
            save_local(&state);
        }
    }
}
fn apply_response(review: &mut Review, requested: &[Value], response: &Value) {
    let Some(results) = response["results"]
        .as_array()
        .filter(|rows| rows.len() <= 64)
    else {
        return;
    };
    for result in results {
        if !requested.contains(&result["key"]) {
            continue;
        }
        let shared = &result["alignment"];
        if shared["verified"] != true
            || (shared["verifiedBy"] != "server"
                && !shared["confirmations"]
                    .as_u64()
                    .is_some_and(|n| (2..=10).contains(&n)))
        {
            continue;
        }
        let Ok(alignment) = serde_json::from_value::<Alignment>(shared.clone()) else {
            continue;
        };
        if let Some(pull) = review
            .pulls
            .iter()
            .find(|pull| key(&review.replay, pull).as_ref() == Some(&result["key"]))
        {
            if valid(alignment, &review.replay, pull, 0.35) {
                review
                    .marker_timing
                    .insert((pull.report.clone(), pull.id), alignment);
            }
        }
    }
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<(Value, Key, Alignment, u64)>,
    running: bool,
}
fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Queue::default()))
}
/// Called only for a fresh local measurement, never for a downloaded consensus.
/// There is one bounded worker for the whole app, including comparison peers.
pub fn submit(replay: &Replay, pull: &Pull, alignment: Alignment) {
    if !valid(alignment, replay, pull, 0.225) {
        return;
    }
    let Some(key) = key(replay, pull) else {
        return;
    };
    let Ok(mut state) = queue().lock() else {
        return;
    };
    let generation = crate::guild::request_generation();
    state
        .pending
        .retain(|pending| pending.0["key"] != key || pending.3 != generation);
    if state.pending.len() == 32 {
        state.pending.pop_front();
    }
    state.pending.push_back((
        json!({ "key": key, "alignment": alignment }),
        Key::new(replay, pull),
        alignment,
        generation,
    ));
    if state.running {
        return;
    }
    state.running = true;
    if std::thread::Builder::new()
        .name("raid-timestamp-library".into())
        .spawn(|| loop {
            let item = {
                let Ok(mut state) = queue().lock() else {
                    return;
                };
                let Some(item) = state.pending.pop_front() else {
                    state.running = false;
                    return;
                };
                item
            };
            let (item, key, alignment, generation) = item;
            crate::guild::with_generation(generation, || {
                if let Ok(Some(token)) = crate::discord_auth::current_or_refreshed_access_token() {
                    if let Some(mut state) = local_cache(&token) {
                        state.cache.insert(key, alignment);
                        save_local(&state);
                    }
                    let _ = request(&token, "observations", item);
                }
            });
        })
        .is_err()
    {
        state.running = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Review, Pull, Value, Alignment) {
        let mut pull = crate::replay_marker::tests::pull();
        let replay = Replay {
            provider: crate::streams::Provider::Twitch,
            video_id: "12345".into(),
            broadcast_id: "broadcast".into(),
            started_at: "2026-09-09T15:57:38Z".into(),
            available_seconds: 7200,
        };
        pull.start_ms = replay.start_ms().unwrap() + 123_375;
        pull.end_ms = pull.start_ms + 300_000;
        let key = key(&replay, &pull).unwrap();
        let alignment = Alignment {
            unix_seconds: pull.start_ms / 1000,
            video_seconds: 124.12,
            uncertainty_seconds: 0.1,
        };
        (
            Review {
                replay,
                pulls: vec![pull.clone()],
                marker_timing: Default::default(),
            },
            pull,
            key,
            alignment,
        )
    }
    #[test]
    fn shared_results_require_exact_keys_quorum_and_plausible_measurements() {
        let (mut review, pull, key, alignment) = fixture();
        let mut shared = serde_json::to_value(alignment).unwrap();
        shared["verified"] = json!(true);
        shared["confirmations"] = json!(2);
        for (field, value) in [
            ("verified", json!(false)),
            ("confirmations", json!(1)),
            ("videoSeconds", json!(7000)),
            ("unixSeconds", json!(42)),
            ("uncertaintySeconds", json!(0.5)),
        ] {
            let mut invalid = shared.clone();
            invalid[field] = value;
            apply_response(
                &mut review,
                &[key.clone()],
                &json!({"results": [{"key": key, "alignment": invalid}]}),
            );
            assert!(review.marker_alignment(&pull).is_none());
        }
        let mut wrong = key.clone();
        wrong["endMs"] = json!(pull.end_ms + 1);
        apply_response(
            &mut review,
            &[key.clone()],
            &json!({"results": [{"key": wrong, "alignment": shared}]}),
        );
        assert!(review.marker_alignment(&pull).is_none());
        apply_response(
            &mut review,
            &[key.clone()],
            &json!({"results": [{"key": key, "alignment": shared}]}),
        );
        assert_eq!(
            review.marker_alignment(&pull).unwrap().video_seconds,
            alignment.video_seconds
        );
    }
}
