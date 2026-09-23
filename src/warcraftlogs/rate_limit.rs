//! WCL reports an hourly points budget, distinct from its HTTP request limiter.
//! Leave the last fifth for interactive review instead of background exports.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const THROTTLED: &str = "Warcraft Logs is busy. Brick will retry shortly.";
pub(super) const FIELD: &str = "_brickBudget";
const MAX_DELAY: u64 = 24 * 60 * 60;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Budget {
    #[serde(default)]
    blocked_until: u64,
    #[serde(default)]
    background_until: u64,
    #[serde(default)]
    reset_at: u64,
}
impl Budget {
    pub fn known(&self, now: u64) -> bool {
        self.reset_at > now
    }

    pub fn permits(&self, now: u64, background: bool) -> bool {
        self.blocked_until <= now && (!background || self.background_until <= now)
    }

    /// Returns true only when a new pause needs persisting with the credentials.
    pub fn observe(&mut self, value: &Value, now: u64) -> bool {
        let Some((limit, spent, reset)) = value["limitPerHour"]
            .as_u64()
            .zip(value["pointsSpentThisHour"].as_f64())
            .zip(value["pointsResetIn"].as_u64())
            .map(|((limit, spent), reset)| (limit, spent, reset))
        else {
            return false;
        };
        if limit == 0
            || limit > i32::MAX as u64
            || !spent.is_finite()
            || spent < 0.0
            || reset > MAX_DELAY
        {
            return false;
        }
        self.reset_at = now.saturating_add(reset.max(1));
        let mut changed = false;
        if spent >= limit as f64 * 0.8 && self.background_until <= now {
            self.background_until = self.reset_at;
            changed = true;
        }
        if spent >= limit as f64 && self.blocked_until <= now {
            self.blocked_until = self.reset_at;
            changed = true;
        }
        changed
    }

    pub fn throttle(&mut self, retry_after: Option<&str>, now: u64) {
        let header = retry_after.and_then(|v| {
            v.trim()
                .parse::<u64>()
                .ok()
                .filter(|v| *v <= MAX_DELAY)
                .or_else(|| {
                    time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc2822)
                        .ok()
                        .and_then(|at| u64::try_from(at.unix_timestamp()).ok())
                        .map(|at| at.saturating_sub(now))
                        .filter(|v| *v <= MAX_DELAY)
                })
        });
        // Without provider guidance, respect the known hourly reset. Avoid
        // repeating the same export every minute throughout an exhausted hour.
        let delay = header.unwrap_or_else(|| {
            self.reset_at
                .checked_sub(now)
                .filter(|v| *v > 0 && *v <= MAX_DELAY)
                .unwrap_or(3600)
        });
        self.blocked_until = self.blocked_until.max(now.saturating_add(delay.max(1)));
    }
}

pub(super) fn query(query: &str) -> String {
    // All callers supply one static GraphQL operation; insert in its root
    // selection without changing any report fields, variables or projection.
    let end = query.rfind('}').expect("static GraphQL selection");
    format!(
        "{} {FIELD}:rateLimitData{{limitPerHour pointsSpentThisHour pointsResetIn}} {}",
        &query[..end],
        &query[end..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hourly_points_reserve_interactive_requests_and_survive_restart() {
        let mut budget = Budget::default();
        assert!(budget.observe(
            &json!({"limitPerHour":3600,"pointsSpentThisHour":2880.5,"pointsResetIn":167}),
            1000
        ));
        assert!(!budget.permits(1000, true));
        assert!(budget.permits(1000, false));
        // Updating identical usage never asks for another credential-store write.
        assert!(!budget.observe(
            &json!({"limitPerHour":3600,"pointsSpentThisHour":2881,"pointsResetIn":166}),
            1001
        ));
        let restored: Budget =
            serde_json::from_slice(&serde_json::to_vec(&budget).unwrap()).unwrap();
        assert!(!restored.permits(1166, true));
        assert!(restored.permits(1167, true));
        assert!(budget.observe(
            &json!({"limitPerHour":3600,"pointsSpentThisHour":3600,"pointsResetIn":100}),
            1002
        ));
        assert!(!budget.permits(1003, false));
    }

    #[test]
    fn retry_after_delta_date_and_missing_header_do_not_poll_an_exhausted_hour() {
        let mut budget = Budget::default();
        budget.throttle(Some("167"), 1000);
        assert!(!budget.permits(1166, false));
        assert!(budget.permits(1167, false));
        let now = 1_790_198_553;
        budget.throttle(Some("Wed, 23 Sep 2026 21:25:20 GMT"), now);
        assert!(!budget.permits(1_790_198_719, false));
        assert!(budget.permits(1_790_198_720, false));
        budget = Budget::default();
        budget.observe(
            &json!({"limitPerHour":3600,"pointsSpentThisHour":3599,"pointsResetIn":900}),
            1000,
        );
        budget.throttle(None, 1000);
        assert!(!budget.permits(1899, false));
        assert!(budget.permits(1900, false));
        let mut unknown = Budget::default();
        unknown.throttle(Some("18446744073709551615"), 1000);
        assert!(!unknown.permits(4599, false));
        assert!(unknown.permits(4600, false));
    }

    #[test]
    fn malformed_usage_is_ignored_and_deadlines_cannot_overflow() {
        let mut budget = Budget::default();
        for value in [
            json!({}),
            json!({"limitPerHour":0,"pointsSpentThisHour":0,"pointsResetIn":1}),
            json!({"limitPerHour":10,"pointsSpentThisHour":-1,"pointsResetIn":1}),
            json!({"limitPerHour":10,"pointsSpentThisHour":9,"pointsResetIn":u64::MAX}),
        ] {
            assert!(!budget.observe(&value, 1000));
        }
        assert!(budget.permits(1000, true));
        budget.throttle(Some("3600"), u64::MAX - 10);
        assert!(!budget.permits(u64::MAX - 1, false));
        assert!(budget.permits(u64::MAX, false));
    }

    #[test]
    fn budget_selection_does_not_change_report_operation_or_variables() {
        let original = "query($code:String!){reportData{report(code:$code){code fights{id}}}}";
        let augmented = query(original);
        assert!(augmented.starts_with(&original[..original.len() - 1]));
        assert!(augmented.ends_with("} }"));
        assert_eq!(augmented.matches("rateLimitData").count(), 1);
    }
}

#[cfg(test)]
mod transport_tests {
    use super::super::{Client, Session};
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    fn server(
        responses: Vec<(u16, &'static str, Value)>,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut queries = Vec::new();
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = Vec::new();
                let mut byte = [0];
                while !input.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    input.push(byte[0]);
                    assert!(input.len() < 8192);
                }
                let header = String::from_utf8(input).unwrap();
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 8192);
                let mut request = vec![0; length];
                stream.read_exact(&mut request).unwrap();
                let request: Value = serde_json::from_slice(&request).unwrap();
                queries.push(request["query"].as_str().unwrap().to_owned());
                let body = serde_json::to_vec(&body).unwrap();
                write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
            queries
        });
        (url, worker)
    }
    fn client(url: String) -> Client {
        let mut client = Client::new().unwrap();
        client.query_endpoint = Some(url);
        // No configuration/store: these are isolated fake credentials and never
        // access the real vault or provider. The endpoint is test-only.
        client.session = Some(
            serde_json::from_value::<Session>(json!({
                "cache_id":"fixture", "client_id":"fixture", "user_id":"fixture",
                "access_token":"fixture-token", "refresh_token":null,
                "expires_at":super::super::now_secs()+3600
            }))
            .unwrap(),
        );
        client
    }

    #[test]
    fn cold_background_export_checks_budget_then_yields_to_interactive_review() {
        let usage = json!({"limitPerHour":3600,"pointsSpentThisHour":3300,"pointsResetIn":600});
        let (url, worker) = server(vec![
            (
                200,
                "",
                json!({"data":{"_brickBudget":usage,"__typename":"Query"}}),
            ),
            (
                200,
                "",
                json!({"data":{"_brickBudget":usage,"reportData":{"reports":[]}}}),
            ),
        ]);
        let mut client = client(url);
        client.set_background_requests(true);
        assert_eq!(
            client.query("{reportData{events}}", json!({})).unwrap_err(),
            THROTTLED
        );
        assert_eq!(
            client.requests.graphql, 1,
            "Only the small budget query is sent"
        );
        assert_eq!(
            client.query("{reportData{events}}", json!({})).unwrap_err(),
            THROTTLED
        );
        assert_eq!(
            client.requests.graphql, 1,
            "Background retries send no requests"
        );
        client.set_background_requests(false);
        let data = client.query("{reportData{reports}}", json!({})).unwrap();
        assert_eq!(data, json!({"reportData":{"reports":[]}}));
        assert_eq!(client.requests.graphql, 2);
        let queries = worker.join().unwrap();
        assert!(queries[0].contains("__typename"));
        assert!(queries.iter().all(|q| !q.contains("events")));
    }

    #[test]
    fn provider_throttle_prevents_requests_from_other_features_and_restored_sessions() {
        let (url, worker) = server(vec![(429, "Retry-After: 167\r\n", json!({}))]);
        let mut first = client(url.clone());
        assert_eq!(
            first.query("{reportData{reports}}", json!({})).unwrap_err(),
            THROTTLED
        );
        let serialized = serde_json::to_vec(first.session.as_ref().unwrap()).unwrap();
        let mut restored = client(url);
        restored.session = Some(serde_json::from_slice(&serialized).unwrap());
        assert_eq!(
            restored
                .query("{reportData{events}}", json!({}))
                .unwrap_err(),
            THROTTLED
        );
        assert_eq!(restored.requests.graphql, 0);
        assert_eq!(
            first.query("{reportData{events}}", json!({})).unwrap_err(),
            THROTTLED
        );
        assert_eq!(first.requests.graphql, 1);
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
