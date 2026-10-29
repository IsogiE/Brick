//! Brick Server is the only Warcraft Logs client for guild pulls and events.
//! The desktop proves once per window, with its own sign-in, which reports it
//! can see; the server then serves only those reports from its archive.
use super::{check_cancelled, while_current, Client, Pull, DAY};
use crate::{download, presence};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const PROOF_TTL: Duration = Duration::from_secs(25 * 60);
// A proof that still leaves reports hidden is retried at most this often.
const REPROVE_AFTER: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REPORTS: usize = 200;
const UNREACHABLE: &str = "Couldn't reach Brick Server. Brick will retry shortly.";
const UNAVAILABLE: &str =
    "Brick Server couldn't load these Warcraft Logs reports. Try again shortly.";
const DENIED: &str = "Your Warcraft Logs account cannot access these reports.";

#[derive(Default)]
pub(super) struct Proofs {
    entries: Vec<(String, (i64, i64), Instant)>,
}
impl Proofs {
    fn find(&self, id: &str, window: (i64, i64)) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|(owner, covered, at)| {
                owner == id
                    && covered.0 <= window.0
                    && covered.1 >= window.1
                    && at.elapsed() < PROOF_TTL
            })
            .map(|(_, _, at)| *at)
            .max()
    }
    fn record(&mut self, id: String, window: (i64, i64)) {
        self.entries.retain(|(_, _, at)| at.elapsed() < PROOF_TTL);
        if self.entries.len() >= 32 {
            self.entries.remove(0);
        }
        self.entries.push((id, window, Instant::now()));
    }
    fn forget(&mut self, id: &str) {
        self.entries.retain(|(owner, _, _)| owner != id);
    }
}

#[cfg(test)]
pub(super) type Transport =
    Box<dyn FnMut(Method, &str, Option<Value>) -> Result<(u16, Value), String> + Send>;

fn server_message(status: u16, body: &Value) -> String {
    match status {
        403 => DENIED.into(),
        429 | 503 => super::rate_limit::THROTTLED.into(),
        _ => body["error"]
            .as_str()
            .filter(|message| !message.is_empty() && message.len() <= 200)
            .map(str::to_owned)
            .unwrap_or_else(|| UNAVAILABLE.into()),
    }
}

fn proof_required(status: u16, body: &Value) -> bool {
    status == 409 && body["code"] == "proof_required"
}

impl Client {
    fn live_send(
        &mut self,
        access: &crate::guild::Access,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), String> {
        access.check()?;
        check_cancelled(&self.cancel)?;
        #[cfg(test)]
        if let Some(transport) = self.live_transport.as_mut() {
            return transport(method, path, body);
        }
        let mut request = presence::http_client()?
            .request(method, access.endpoint(path)?)
            .timeout(Duration::from_secs(30))
            .bearer_auth(access.secret());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = while_current(&self.cancel, || request.send())?.map_err(|_| UNREACHABLE)?;
        let status = response.status().as_u16();
        let bytes = while_current(&self.cancel, || {
            download::read_response(response, MAX_RESPONSE_BYTES, "Brick Server")
        })??;
        access.check()?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    /// Proves this member's Warcraft Logs access for a window. The access token is
    /// sent once for the server's own report-list check; the server keeps only codes.
    fn live_prove(
        &mut self,
        access: &crate::guild::Access,
        window: (i64, i64),
    ) -> Result<(), String> {
        let token = self.access_token()?;
        let (status, body) = self.live_send(
            access,
            Method::POST,
            "/v1/warcraftlogs/live/proof",
            Some(json!({ "accessToken": token, "start": window.0, "end": window.1 })),
        )?;
        if status != 200 {
            return Err(server_message(status, &body));
        }
        self.live_proofs.record(access.cache_id(), window);
        Ok(())
    }

    fn live_proven(
        &mut self,
        access: &crate::guild::Access,
        window: (i64, i64),
        force: bool,
    ) -> Result<(), String> {
        let proved = self.live_proofs.find(&access.cache_id(), window);
        if proved.is_some_and(|at| !force || at.elapsed() < REPROVE_AFTER) {
            return Ok(());
        }
        self.live_prove(access, window)
    }

    /// Every guild report overlapping `window`, each with its full fight list.
    pub(super) fn live_reports(
        &mut self,
        access: &crate::guild::Access,
        window: (i64, i64),
    ) -> Result<Vec<Value>, String> {
        let path = format!(
            "/v1/warcraftlogs/live/reports?start={}&end={}",
            window.0, window.1
        );
        for attempt in 0..2 {
            self.live_proven(access, window, attempt > 0)?;
            let (status, body) = self.live_send(access, Method::GET, &path, None)?;
            if proof_required(status, &body) && attempt == 0 {
                self.live_proofs.forget(&access.cache_id());
                continue;
            }
            if status != 200 {
                return Err(server_message(status, &body));
            }
            // Reports this sign-in didn't list yet: one fresh proof, then accept.
            if attempt == 0 && body["unproven"].as_u64().is_some_and(|count| count > 0) {
                let proved = self.live_proofs.find(&access.cache_id(), window);
                if proved.is_some_and(|at| at.elapsed() >= REPROVE_AFTER) {
                    continue;
                }
            }
            let reports = body["reports"]
                .as_array()
                .filter(|reports| reports.len() <= MAX_REPORTS)
                .ok_or(UNAVAILABLE)?;
            return Ok(reports.clone());
        }
        Err(UNAVAILABLE.into())
    }

    /// One pull's archived deaths, casts and buffs with the report's player metadata.
    /// `spells` adds personal spells outside the shared catalogue.
    pub(super) fn live_events(
        &mut self,
        access: &crate::guild::Access,
        pull: &Pull,
        spells: &[u64],
    ) -> Result<Value, String> {
        let mut path = format!(
            "/v1/warcraftlogs/live/events?report={}&fight={}",
            pull.report, pull.id
        );
        if !spells.is_empty() {
            let list: Vec<_> = spells.iter().take(256).map(u64::to_string).collect();
            path.push_str(&format!("&spells={}", list.join(",")));
        }
        let window = (
            pull.report_start_ms.saturating_sub(DAY),
            pull.end_ms.saturating_add(DAY),
        );
        for attempt in 0..2 {
            self.live_proven(access, window, false)?;
            let (status, body) = self.live_send(access, Method::GET, &path, None)?;
            if proof_required(status, &body) && attempt == 0 {
                self.live_proofs.forget(&access.cache_id());
                continue;
            }
            if status != 200 {
                return Err(server_message(status, &body));
            }
            if body["report"] != pull.report.as_str() || body["fight"].as_u64() != Some(pull.id) {
                return Err(UNAVAILABLE.into());
            }
            return Ok(body);
        }
        Err(UNAVAILABLE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(Method, String, Option<Value>)>>>;

    fn access() -> crate::guild::Access {
        crate::guild::Access::new(
            "discord".into(),
            "1".into(),
            "2".into(),
            crate::guild::generation(),
        )
    }

    fn client(responses: Vec<(u16, Value)>) -> (Client, Calls) {
        let mut client = Client::new().unwrap();
        client.set_test_session("member-token");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let mut responses = responses.into_iter();
        client.live_transport = Some(Box::new(move |method, path, body| {
            seen.lock().unwrap().push((method, path.to_owned(), body));
            Ok(responses.next().expect("unexpected request"))
        }));
        (client, calls)
    }

    #[test]
    fn reports_prove_once_then_reuse_the_proof() {
        let report = json!({"code":"AbCdEfGh12345678","startTime":1,"endTime":2,"fights":[]});
        let (mut client, calls) = client(vec![
            (200, json!({"reports":1})),
            (200, json!({"unproven":0,"reports":[report]})),
            (200, json!({"unproven":0,"reports":[]})),
        ]);
        let window = (1_000, 2_000);
        assert_eq!(client.live_reports(&access(), window).unwrap().len(), 1);
        assert!(client
            .live_reports(&access(), (1_100, 1_900))
            .unwrap()
            .is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1, "/v1/warcraftlogs/live/proof");
        assert_eq!(
            calls[0].2,
            Some(json!({"accessToken":"member-token","start":1_000,"end":2_000}))
        );
        assert_eq!(
            calls[1].1,
            "/v1/warcraftlogs/live/reports?start=1000&end=2000"
        );
        assert!(calls[1].2.is_none() && calls[2].2.is_none());
    }

    #[test]
    fn an_expired_proof_on_the_server_is_renewed_once() {
        let (mut client, calls) = client(vec![
            (200, json!({"reports":0})),
            (409, json!({"code":"proof_required"})),
            (200, json!({"reports":1})),
            (200, json!({"unproven":0,"reports":[]})),
        ]);
        assert!(client.live_reports(&access(), (1, 2)).unwrap().is_empty());
        assert_eq!(calls.lock().unwrap().len(), 4);
        let (mut client, _) = client_with_denial();
        assert_eq!(client.live_reports(&access(), (1, 2)).unwrap_err(), DENIED);
    }

    fn client_with_denial() -> (Client, Calls) {
        client(vec![(
            403,
            json!({"error":"Warcraft Logs did not accept this sign-in."}),
        )])
    }

    #[test]
    fn events_are_matched_to_the_requested_pull_and_name_personal_spells() {
        let pull = Pull {
            report: "AbCdEfGh12345678".into(),
            id: 4,
            encounter: 3001,
            difficulty: 5,
            friendly_players: None,
            report_start_ms: 10 * DAY,
            remaining: None,
            name: "Boss".into(),
            kill: false,
            last_phase: None,
            last_phase_is_intermission: false,
            start_ms: 10 * DAY + 1,
            end_ms: 10 * DAY + 2,
            seconds: 0,
        };
        let (mut client, calls) = client(vec![
            (200, json!({"reports":1})),
            (
                200,
                json!({"report":"AbCdEfGh12345678","fight":4,"deaths":[]}),
            ),
            (200, json!({"report":"AbCdEfGh12345678","fight":5})),
        ]);
        assert_eq!(
            client.live_events(&access(), &pull, &[7, 8]).unwrap()["fight"],
            4
        );
        assert_eq!(
            calls.lock().unwrap()[1].1,
            "/v1/warcraftlogs/live/events?report=AbCdEfGh12345678&fight=4&spells=7,8"
        );
        assert_eq!(
            client.live_events(&access(), &pull, &[]).unwrap_err(),
            UNAVAILABLE
        );
    }
}
