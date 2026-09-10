#[cfg(not(test))]
use super::PlaybackState;
#[cfg(not(test))]
use std::sync::{mpsc, OnceLock};

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Stats {
    pub source: Option<String>,
    pub buffer_seconds: Option<f64>,
    pub fps: Option<f64>,
    pub ready_state: Option<u8>,
    pub decoded_frames: Option<u64>,
    pub media_seeking: Option<bool>,
}

/// Bounded transition-only logging, never provider URLs, tokens or raw events.
#[cfg(not(test))]
pub fn record(state: &PlaybackState) {
    static SENDER: OnceLock<mpsc::SyncSender<String>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<String>(16);
        std::thread::spawn(move || {
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            for message in receiver {
                if last.elapsed() < std::time::Duration::from_secs(1) {
                    continue;
                }
                last = std::time::Instant::now();
                let _ = crate::addon::record_log(crate::addon::LogLevel::Info, message);
            }
        });
        sender
    });
    let number = |n: Option<f64>| {
        n.filter(|n| n.is_finite() && (0.0..=604800.0).contains(n))
            .map(|n| format!("{n:.2}"))
            .unwrap_or_else(|| "unknown".into())
    };
    let source = state
        .diagnostics
        .source
        .as_deref()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 25
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        })
        .unwrap_or("unknown");
    let _ = sender.try_send(format!(
        "Player {source}: ready={} playing={} buffering={} blocked={} buffer_seconds={} fps={} media_ready={:?} decoded_frames={:?} media_seeking={:?}",
        state.ready,
        state.playing,
        state.buffering,
        state.blocked,
        number(state.diagnostics.buffer_seconds),
        number(state.diagnostics.fps),
        state.diagnostics.ready_state,
        state.diagnostics.decoded_frames,
        state.diagnostics.media_seeking
    ));
}
