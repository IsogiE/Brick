//! Warm bounded review metadata before starting missing recording alignments.
use crate::{review_ui::ReviewUi, streams::Stream};
use eframe::egui;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Preparation {
    current: Option<Stream>,
    aligning: bool,
    attempted: Vec<(String, bool, Instant, Duration)>,
}
impl Preparation {
    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        candidates: &[Stream],
        visible_count: usize,
        selected: Option<&Stream>,
        peer: &mut ReviewUi,
    ) {
        let now = Instant::now();
        self.attempted
            .retain(|(_, _, at, delay)| now.duration_since(*at) < *delay);
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
            if let Some(stream) = &self.current {
                if let Ok(path) = crate::streams::review_path(stream) {
                    let visible = candidates.iter().take(visible_count).any(|candidate| {
                        crate::streams::review_path(candidate).ok().as_ref() == Some(&path)
                    });
                    if let Some(attempt) = self
                        .attempted
                        .iter_mut()
                        .find(|(key, aligning, _, _)| key == &path && *aligning == self.aligning)
                    {
                        attempt.2 = now;
                        attempt.3 = peer.preparation_retry_delay(visible);
                    }
                }
            }
            self.current = None;
        }
        if peer.metadata_busy() {
            peer.tick(ctx, None);
            return;
        }
        // Prepare the list before doing any potentially slow boss-event export.
        // Both passes share the same eight-entry memory budget.
        let next = [false, true].into_iter().find_map(|aligning| {
            candidates
                .iter()
                .take(8)
                .find(|stream| {
                    let Ok(path) = crate::streams::review_path(stream) else {
                        return false;
                    };
                    let needed = if aligning {
                        peer.prepared_metadata(stream) && !peer.prepared_recording(stream)
                    } else {
                        !peer.prepared_metadata(stream)
                    };
                    needed
                        && !self
                            .attempted
                            .iter()
                            .any(|(key, mode, _, _)| key == &path && *mode == aligning)
                })
                .map(|stream| (stream, aligning))
        });
        if let Some((stream, aligning)) = next {
            if self.attempted.len() >= 128 {
                self.attempted.remove(0);
            }
            self.attempted.push((
                crate::streams::review_path(stream).unwrap(),
                aligning,
                now,
                Duration::from_secs(5 * 60),
            ));
            self.aligning = aligning;
            peer.prepare_alignment(aligning);
            self.current = Some(stream.clone());
            peer.tick(ctx, Some(stream));
        } else {
            peer.tick(ctx, None);
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}
