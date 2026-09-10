#![allow(dead_code)]
#[path = "../../../src/replay_digits.rs"]
mod replay_digits;
#[path = "../../../src/replay_edge.rs"]
mod replay_edge;
use serde::Deserialize;
use std::{fs::File, io::{self, Read}};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request { start_ms: i64, frames: Vec<Frame>, #[serde(default)] locate: bool }
#[derive(Deserialize)]
struct Frame { path: String, seconds: f64 }
fn run() -> Option<serde_json::Value> {
    let mut input = Vec::new();
    io::stdin().take(128 * 1024 + 1).read_to_end(&mut input).ok()?;
    if input.len() > 128 * 1024 { return None; }
    let request: Request = serde_json::from_slice(&input).ok()?;
    if !(1_500_000_000_000..=4_000_000_000_000).contains(&request.start_ms)
        || request.frames.len() > 500 { return None; }
    let mut edge = replay_edge::Edge::default();
    let mut previous = None;
    for frame in request.frames {
        if !frame.seconds.is_finite() || !(0.0..=604800.0).contains(&frame.seconds) { return None; }
        if previous.is_some_and(|p| frame.seconds <= p || (!request.locate && frame.seconds-p > 0.20)) { return None; }
        previous = Some(frame.seconds);
        let mut png = Vec::new();
        File::open(frame.path).ok()?.take(2 * 1024 * 1024 + 1).read_to_end(&mut png).ok()?;
        if png.len() > 2 * 1024 * 1024 { return None; }
        let reading = replay_digits::read(&png, request.start_ms, edge.marker);
        if request.locate {
            if matches!(reading, replay_digits::Reading::Present(_)) {
                return Some(serde_json::json!({"foundSeconds": frame.seconds}));
            }
            continue;
        }
        // Ten source frames per second plus a conservative 50ms sampling margin.
        if let Some(alignment) = edge.observe(replay_edge::Sample { reading,
            before: (frame.seconds - 0.025).max(0.0), after: frame.seconds + 0.025 }).ok()? {
            return serde_json::to_value(alignment).ok();
        }
    }
    None
}
fn main() { println!("{}", serde_json::to_string(&run()).unwrap()); }
