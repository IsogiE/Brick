//! Share a public YouTube channel; no Google account authorization is involved.
use crate::{streams, streams_ui::action_button, youtube_account::Channel};
use eframe::egui::{self, RichText};
use std::sync::mpsc;

#[derive(Default)]
pub struct YoutubeUi {
    draft: String,
    work: Option<mpsc::Receiver<Result<bool, String>>>,
    initialized: bool,
    cleanup: Option<mpsc::Receiver<Result<(), String>>>,
    cleanup_notice: Option<String>,
    notice: Option<String>,
}

impl YoutubeUi {
    pub fn busy(&self) -> bool {
        self.work.is_some()
    }

    pub fn tick(&mut self, ctx: &egui::Context, _shared: Option<&Channel>) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.work {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("YouTube sharing stopped. Try again.".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.work = None;
                match result {
                    Ok(shared) => {
                        changed = shared;
                        self.notice = None;
                        if shared {
                            self.draft.clear();
                        }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
        }
        if let Some(rx) = &self.cleanup {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                    "Old YouTube connection cleanup stopped. Try again.".into(),
                )),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.cleanup = None;
                self.cleanup_notice = result.err();
            }
        }
        if !self.initialized {
            self.initialized = true;
            self.start_cleanup(ctx);
        }
        changed
    }

    fn start_cleanup(&mut self, ctx: &egui::Context) {
        if self.cleanup.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.cleanup = Some(rx);
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = crate::discord_auth::current_access_token().and_then(|access| {
                crate::youtube_account::forget_legacy_connection(&access.user_id)
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn start(&mut self, ctx: &egui::Context, channel: String) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.work = Some(rx);
        self.notice = None;
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = crate::discord_auth::current_access_token().and_then(|access| {
                streams::share_youtube_channel(&access, &channel).map_err(|error| error.message)?;
                Ok(true)
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn draw_channel(&mut self, ui: &mut egui::Ui, shared: Option<&Channel>, enabled: bool) {
        ui.label(RichText::new("YouTube channel").strong().size(15.0));
        if let Some(channel) = shared {
            ui.hyperlink_to(format!("Shared here: {}", channel.title), &channel.url);
        }
        if shared.is_some() {
            ui.collapsing("Change YouTube channel", |ui| self.draw_input(ui, enabled));
        } else {
            ui.small("Share a public channel without connecting your Google account.");
            self.draw_input(ui, enabled);
        }
        if self.busy() {
            ui.small("Working…");
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        if let Some(notice) = &self.cleanup_notice {
            ui.small(notice);
            if ui
                .add_enabled(
                    self.cleanup.is_none(),
                    action_button("Retry old connection cleanup"),
                )
                .clicked()
            {
                self.start_cleanup(ui.ctx());
            }
        }
    }

    fn draw_input(&mut self, ui: &mut egui::Ui, enabled: bool) {
        ui.add_enabled(
            enabled && !self.busy(),
            egui::TextEdit::singleline(&mut self.draft)
                .hint_text("YouTube channel link or @handle")
                .desired_width(f32::INFINITY)
                .char_limit(512),
        );
        if ui
            .add_enabled(
                enabled && !self.busy() && !self.draft.trim().is_empty(),
                action_button("Share channel with this guild"),
            )
            .clicked()
        {
            self.start(ui.ctx(), self.draft.trim().to_owned());
        }
        ui.small("Live streams may take time to appear. Add a video link below if one is missing or unlisted.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_cleanup_failure_does_not_block_sharing_or_get_hidden_by_success() {
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let (share_tx, share_rx) = mpsc::channel();
        let mut panel = YoutubeUi {
            initialized: true,
            cleanup: Some(cleanup_rx),
            ..Default::default()
        };
        assert!(!panel.busy());
        cleanup_tx.send(Err("Unlock your keyring".into())).unwrap();
        panel.tick(&egui::Context::default(), None);
        panel.work = Some(share_rx);
        share_tx.send(Ok(true)).unwrap();
        assert!(panel.tick(&egui::Context::default(), None));
        assert_eq!(panel.cleanup_notice.as_deref(), Some("Unlock your keyring"));
        assert!(panel.cleanup.is_none());
    }

    #[test]
    fn channel_share_completion_refreshes_the_guild_and_clears_the_draft() {
        let (tx, rx) = mpsc::channel();
        let mut panel = YoutubeUi {
            initialized: true,
            draft: "@advance".into(),
            work: Some(rx),
            ..Default::default()
        };
        tx.send(Ok(true)).unwrap();
        assert!(panel.tick(&egui::Context::default(), None));
        assert!(!panel.busy());
        assert!(panel.draft.is_empty());
        assert!(!panel.tick(&egui::Context::default(), None));
    }

    #[test]
    fn failed_share_keeps_input_and_releases_controls_for_retry() {
        let (tx, rx) = mpsc::channel();
        let mut panel = YoutubeUi {
            initialized: true,
            draft: "@advance".into(),
            work: Some(rx),
            ..Default::default()
        };
        drop(tx);
        assert!(!panel.tick(&egui::Context::default(), None));
        assert!(!panel.busy());
        assert_eq!(panel.draft, "@advance");
        assert!(panel.notice.is_some());
    }
}
