//! Quick checks for new addon builds.
//!
//! The Brick API answers which ART and AdvanceWigs builds are current with a
//! few hashes. Brick asks every few seconds and wakes the normal signed sync
//! only when that answer changes, so a published build arrives within seconds
//! without every raider downloading the signed manifest that often. The answer
//! is only a hint: the sync still verifies the signed manifest and every
//! package hash, and the minute-long fallback check runs regardless.

use std::{thread, time::Duration};

use serde::Deserialize;

const CHECK_INTERVAL: Duration = Duration::from_secs(10);
const BUSY_RETRY: Duration = Duration::from_secs(30);
// Servers without the route answer 404; look again rarely.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(5 * 60);
const MAX_RESPONSE_BYTES: u64 = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
// GitHub uploads the manifest, then its signature. A sync that lands between
// the two fails verification, so a change keeps waking the sync for a few
// more checks. Those syncs skip the download once the build is installed.
const FOLLOW_UP_CHECKS: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Build {
    version: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Versions {
    art: Option<Build>,
    advancewigs: Option<Build>,
}

#[derive(Default)]
struct Tracker {
    last: Option<(String, Versions)>,
    follow_ups: u8,
}

impl Tracker {
    /// Whether this answer should wake the sync.
    fn observe(&mut self, scope: String, versions: Versions) -> bool {
        let observed = Some((scope, versions));
        if self.last != observed {
            // The first answer also wakes the sync: a build published between
            // launch and this check must not wait for the fallback minute.
            self.last = observed;
            self.follow_ups = FOLLOW_UP_CHECKS;
            return true;
        }
        if self.follow_ups > 0 {
            self.follow_ups -= 1;
            return true;
        }
        false
    }
}

enum Outcome {
    Answer(String, Versions),
    NotSignedIn,
    Unsupported,
    Busy,
}

pub(super) fn spawn(wake: impl Fn() + Send + 'static) {
    if !crate::presence::configured() {
        return;
    }
    thread::spawn(move || {
        let mut tracker = Tracker::default();
        loop {
            if crate::account_erasure::requests_blocked() {
                return;
            }
            let pause = match check() {
                Ok(Outcome::Answer(scope, versions)) => {
                    if tracker.observe(scope, versions)
                        && !crate::account_erasure::requests_blocked()
                    {
                        wake();
                    }
                    CHECK_INTERVAL
                }
                Ok(Outcome::NotSignedIn) => CHECK_INTERVAL,
                Ok(Outcome::Unsupported) => UNSUPPORTED_RETRY,
                Ok(Outcome::Busy) | Err(_) => BUSY_RETRY,
            };
            thread::sleep(pause);
        }
    });
}

fn check() -> Result<Outcome, String> {
    let Some(access) = crate::discord_auth::current_or_refreshed_access_token()? else {
        return Ok(Outcome::NotSignedIn);
    };
    let response = crate::presence::http_client()?
        .get(access.endpoint("/v1/addons/versions")?)
        .bearer_auth(access.secret())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .map_err(|error| format!("addon version check failed: {error}"))?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => return Ok(Outcome::Unsupported),
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            return Ok(Outcome::Busy)
        }
        status if !status.is_success() => {
            return Err(format!("addon version check failed: HTTP {status}"))
        }
        _ => {}
    }
    let bytes =
        crate::download::read_response(response, MAX_RESPONSE_BYTES, "addon version check")?;
    access.check()?;
    let versions = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid addon version check: {error}"))?;
    Ok(Outcome::Answer(access.cache_id(), versions))
}

#[cfg(test)]
mod tests {
    use super::{Build, Tracker, Versions, FOLLOW_UP_CHECKS};

    fn versions(art: &str, wigs: Option<&str>) -> Versions {
        let build = |version: &str| Build {
            version: version.into(),
            sha256: format!("{version:0>64}"),
        };
        Versions {
            art: Some(build(art)),
            advancewigs: wigs.map(build),
        }
    }

    fn wakes(tracker: &mut Tracker, scope: &str, value: &Versions, checks: usize) -> usize {
        (0..checks)
            .filter(|_| tracker.observe(scope.into(), value.clone()))
            .count()
    }

    #[test]
    fn a_new_build_wakes_the_sync_then_settles_after_a_few_follow_up_checks() {
        let mut tracker = Tracker::default();
        let first = versions("1", Some("7"));
        assert_eq!(
            wakes(&mut tracker, "advance:1", &first, 10),
            1 + FOLLOW_UP_CHECKS as usize
        );
        let wigs = versions("1", Some("8"));
        assert_eq!(
            wakes(&mut tracker, "advance:1", &wigs, 10),
            1 + FOLLOW_UP_CHECKS as usize
        );
        let art = versions("2", Some("8"));
        assert!(tracker.observe("advance:1".into(), art.clone()));
        // A change during the follow-ups restarts them.
        assert!(tracker.observe("advance:1".into(), versions("3", Some("8"))));
        assert_eq!(
            wakes(&mut tracker, "advance:1", &versions("3", Some("8")), 10),
            FOLLOW_UP_CHECKS as usize
        );
    }

    #[test]
    fn switching_guild_or_account_counts_as_a_change() {
        let mut tracker = Tracker::default();
        let same = versions("1", None);
        wakes(&mut tracker, "advance:1", &same, 10);
        assert!(tracker.observe("other:1".into(), same.clone()));
        wakes(&mut tracker, "other:1", &same, 10);
        assert!(tracker.observe("other:2".into(), same));
    }

    #[test]
    fn the_server_answer_parses_with_or_without_advancewigs() {
        let full: Versions = serde_json::from_str(&format!(
            r#"{{"art":{{"version":"v1.8.13","sha256":"{a}"}},"advancewigs":{{"version":"v427.8 AdvanceWigs 8 (3355cf8)","release":8,"sha256":"{b}"}}}}"#,
            a = "a".repeat(64),
            b = "b".repeat(64)
        ))
        .unwrap();
        assert_eq!(
            full.advancewigs.unwrap().version,
            "v427.8 AdvanceWigs 8 (3355cf8)"
        );
        let plain: Versions = serde_json::from_str(r#"{"art":null,"advancewigs":null}"#).unwrap();
        assert_eq!(
            plain,
            Versions {
                art: None,
                advancewigs: None
            }
        );
    }
}
