//! One cancellable metadata reader warms visible VODs before selection.
use crate::{review_ui::ReviewUi, streams::Stream};
use eframe::egui;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Preparation {
    current: Option<Stream>,
    attempted: Vec<(String, Instant)>,
}
impl Preparation {
    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        candidates: &[Stream],
        selected: Option<&Stream>,
        peer: &mut ReviewUi,
    ) {
        let now = Instant::now();
        self.attempted
            .retain(|(_, at)| now.duration_since(*at) < Duration::from_secs(60));
        if candidates.is_empty() {
            // Clicking the VOD being prepared hands off that same read. Do not
            // cancel it only to queue a duplicate behind the shared WCL client.
            if let Some(stream) = self.current.as_ref().filter(|stream| {
                selected.is_some_and(|selected| {
                    crate::streams::review_path(selected).ok()
                        == crate::streams::review_path(stream).ok()
                })
            }) {
                if peer.metadata_busy() {
                    peer.tick(ctx, Some(stream));
                    return;
                }
            }
            self.current = None;
            peer.tick(ctx, None);
            return;
        }
        if let Some(stream) = &self.current {
            if candidates.iter().any(|candidate| {
                crate::streams::review_path(candidate).ok()
                    == crate::streams::review_path(stream).ok()
            }) {
                peer.tick(ctx, Some(stream));
                if peer.metadata_busy() {
                    return;
                }
            } else {
                peer.tick(ctx, None);
                if peer.metadata_busy() {
                    return;
                }
            }
            self.current = None;
        }
        if peer.metadata_busy() {
            peer.tick(ctx, None);
            return;
        }
        let next = candidates.iter().take(8).find(|stream| {
            let Ok(path) = crate::streams::review_path(stream) else {
                return false;
            };
            !peer.prepared_recording(stream) && !self.attempted.iter().any(|(key, _)| key == &path)
        });
        if let Some(stream) = next {
            if self.attempted.len() >= 32 {
                self.attempted.remove(0);
            }
            self.attempted
                .push((crate::streams::review_path(stream).unwrap(), now));
            self.current = Some(stream.clone());
            peer.tick(ctx, Some(stream));
        } else {
            peer.tick(ctx, None);
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}
