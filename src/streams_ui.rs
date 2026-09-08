use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    discord_auth, presence,
    stream_player::StreamPlayer,
    stream_preferences::Preferences,
    streams::{self, Provider, Snapshot, Status, Stream, Vod},
};

const REFRESH: Duration = Duration::from_secs(30);
const MAX_STALE: Duration = Duration::from_secs(90);
const MUTED: Color32 = Color32::from_rgb(159, 169, 184);
const LIVE: Color32 = Color32::from_rgb(69, 211, 127);
const MEMBER_ROW_HEIGHT: f32 = 30.0;

enum Action {
    Refresh,
    Save(Provider, String),
    Remove(Provider),
    Recordings,
}
enum ResultData {
    Snapshot(Snapshot),
    Saved(Provider),
    Removed,
    Recordings(Vec<Vod>),
}
type WorkResult = Result<ResultData, streams::Error>;
type PlayerResult = Result<(String, String, Option<Preferences>), streams::Error>;

pub struct StreamsUi {
    snapshot: Option<Rc<Snapshot>>,
    received_at: Option<Instant>,
    last_attempt: Option<Instant>,
    work: Option<mpsc::Receiver<WorkResult>>,
    selected: Option<Stream>,
    focused: Option<(String, String)>,
    recordings_open: bool,
    recordings: Option<Rc<Vec<Vod>>>,
    player: Option<StreamPlayer>,
    preferences: Option<Preferences>,
    player_work: Option<mpsc::Receiver<PlayerResult>>,
    player_attempted: bool,
    player_rect: Option<egui::Rect>,
    player_error: Option<String>,
    notice: Option<String>,
    notice_provider: Option<Provider>,
    saved_provider: Option<Provider>,
    edit_open: bool,
    drafts: [String; 2],
    confirm_remove: Option<Provider>,
}

impl Default for StreamsUi {
    fn default() -> Self {
        Self {
            snapshot: None,
            received_at: None,
            last_attempt: None,
            work: None,
            selected: None,
            focused: None,
            recordings_open: false,
            recordings: None,
            player: None,
            preferences: None,
            player_work: None,
            player_attempted: false,
            player_rect: None,
            player_error: None,
            notice: None,
            notice_provider: None,
            saved_provider: None,
            edit_open: false,
            drafts: [String::new(), String::new()],
            confirm_remove: None,
        }
    }
}

impl StreamsUi {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn stop_player(&mut self) {
        self.player = None;
        self.player_work = None;
        self.player_attempted = false;
        self.player_rect = None;
    }

    pub fn tick(&mut self, ctx: &egui::Context, authorized: bool, active: bool) -> bool {
        if !authorized {
            self.clear();
            return false;
        }
        if !active {
            self.stop_player();
        }
        if self.received_at.is_some_and(|at| at.elapsed() >= MAX_STALE) {
            self.snapshot = None;
            self.selected = None;
            self.stop_player();
            self.received_at = None;
            self.notice = Some("Live status couldn't be verified. Reconnecting…".into());
            self.notice_provider = None;
        }
        if let Some(rx) = &self.work {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Stream request stopped. Try again.".to_string().into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.work = None;
                match result {
                    Ok(ResultData::Snapshot(snapshot)) => {
                        self.saved_provider = None;
                        if let Some(selected) = &self.selected {
                            let current = snapshot
                                .streams
                                .iter()
                                .find(|stream| {
                                    stream.user_id == selected.user_id
                                        && stream.channel_id == selected.channel_id
                                        && stream.provider == selected.provider
                                        && stream.status == Status::Live
                                })
                                .cloned();
                            if current.is_none() {
                                self.stop_player();
                            }
                            self.selected = current;
                        }
                        self.snapshot = Some(Rc::new(snapshot));
                        self.received_at = Some(Instant::now());
                    }
                    Ok(ResultData::Saved(provider)) => {
                        self.saved_provider = Some(provider);
                        self.notice = None;
                        self.start(ctx, Action::Refresh);
                    }
                    Ok(ResultData::Removed) => {
                        self.confirm_remove = None;
                        self.start(ctx, Action::Refresh);
                    }
                    Ok(ResultData::Recordings(vods)) => self.recordings = Some(Rc::new(vods)),
                    Err(error) => {
                        self.saved_provider = None;
                        if error.access_denied {
                            self.clear();
                            return true;
                        }
                        self.notice = Some(error.message);
                    }
                }
            }
        }
        if active
            && presence::configured()
            && self.work.is_none()
            && self.last_attempt.is_none_or(|at| at.elapsed() >= REFRESH)
        {
            self.start(ctx, Action::Refresh);
        }
        if self.player.is_some() {
            crate::stream_player::pump_events();
        }
        if let Some(error) = self.player.as_ref().and_then(StreamPlayer::failure) {
            self.player = None;
            self.player_error = Some(error);
            self.player_attempted = true;
        }
        false
    }

    pub fn repaint_after(&self, active: bool) -> Duration {
        #[cfg(target_os = "linux")]
        if self.player.is_some() {
            return Duration::from_millis(33);
        }
        if active && presence::configured() && self.work.is_none() {
            return self
                .last_attempt
                .map(|at| REFRESH.saturating_sub(at.elapsed()))
                .unwrap_or(Duration::ZERO);
        }
        Duration::from_secs(60)
    }

    fn start(&mut self, ctx: &egui::Context, action: Action) {
        if self.work.is_some() {
            return;
        }
        self.notice = None;
        self.notice_provider = match &action {
            Action::Save(provider, _) | Action::Remove(provider) => Some(provider.clone()),
            _ => None,
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = access_token().and_then(|token| match action {
                Action::Refresh => streams::fetch(&token).map(ResultData::Snapshot),
                Action::Save(provider, url) => {
                    let parsed = url::Url::parse(&url).ok();
                    let matches =
                        parsed
                            .as_ref()
                            .and_then(url::Url::host_str)
                            .is_some_and(|host| match provider {
                                Provider::Twitch => {
                                    ["twitch.tv", "www.twitch.tv", "m.twitch.tv"].contains(&host)
                                }
                                Provider::Youtube => [
                                    "youtube.com",
                                    "www.youtube.com",
                                    "m.youtube.com",
                                    "youtu.be",
                                ]
                                .contains(&host),
                            });
                    if !matches {
                        return Err(format!(
                            "Use a {} link in the {} field.",
                            provider.label(),
                            provider.label()
                        )
                        .into());
                    }
                    streams::save(&token, &url).map(|()| ResultData::Saved(provider))
                }
                Action::Remove(provider) => {
                    streams::remove(&token, &provider).map(|()| ResultData::Removed)
                }
                Action::Recordings => streams::fetch_vods(&token).map(ResultData::Recordings),
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.work = Some(rx);
        self.last_attempt = Some(Instant::now());
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.player_rect = None;
        ui.horizontal(|ui| {
            if self.recordings_open {
                ui.label(
                    RichText::new("Recordings")
                        .size(18.0)
                        .strong()
                        .color(Color32::from_rgb(239, 242, 247)),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        presence::configured() && self.work.is_none(),
                        action_button("Your streams").min_size(egui::vec2(108.0, 32.0)),
                    )
                    .clicked()
                {
                    for (i, provider) in [Provider::Twitch, Provider::Youtube].iter().enumerate() {
                        self.drafts[i] = self
                            .snapshot
                            .as_ref()
                            .and_then(|s| {
                                s.own_streams
                                    .iter()
                                    .find(|stream| &stream.provider == provider)
                            })
                            .map(|s| s.url.clone())
                            .unwrap_or_default();
                    }
                    self.confirm_remove = None;
                    self.edit_open = true;
                    self.stop_player();
                }
                let refresh_button = action_button("Refresh");
                let refresh_button = if self.notice.is_some() && self.notice_provider.is_none() {
                    refresh_button
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(204, 112, 112)))
                } else {
                    refresh_button
                };
                let refresh = ui.add_enabled(
                    self.work.is_none() && presence::configured(),
                    refresh_button,
                );
                let refresh = if self.notice_provider.is_none() {
                    if let Some(error) = &self.notice {
                        refresh.on_hover_text(error)
                    } else {
                        refresh
                    }
                } else {
                    refresh
                };
                if refresh.clicked() {
                    self.notice = None;
                    self.start(
                        ui.ctx(),
                        if self.recordings_open {
                            Action::Recordings
                        } else {
                            Action::Refresh
                        },
                    );
                }
                if ui
                    .add_enabled(
                        self.work.is_none(),
                        action_button(if self.recordings_open {
                            "Live streams"
                        } else {
                            "Recordings"
                        }),
                    )
                    .clicked()
                {
                    self.recordings_open = !self.recordings_open;
                    self.focused = None;
                    self.selected = None;
                    self.notice = None;
                    self.stop_player();
                    if self.recordings_open {
                        self.start(ui.ctx(), Action::Recordings);
                    }
                }
            });
        });
        ui.add_space(12.0);
        if !presence::configured() {
            ui.label("Streams aren't available in this build yet.");
            return;
        }
        let snapshot = self.snapshot.clone();
        let live = snapshot
            .as_ref()
            .map(|s| s.streams.as_slice())
            .unwrap_or_default();
        let empty_message = if (self.notice.is_some() && self.notice_provider.is_none())
            || snapshot.as_ref().is_some_and(|s| s.unverified_count > 0)
        {
            "No streams available"
        } else {
            "No one is live right now."
        };
        let people = live_people(live);
        let recordings = self.recordings.clone();
        let recorded_people =
            recording_people(recordings.as_deref().map(Vec::as_slice).unwrap_or_default());
        let height = ui.available_height().max(330.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(176.0, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(176.0);
                    ui.label(
                        RichText::new(if self.recordings_open {
                            format!("MEMBERS  {}", recorded_people.len())
                        } else {
                            format!("LIVE NOW  {}", people.len())
                        })
                        .small()
                        .strong()
                        .color(MUTED),
                    );
                    ui.add_space(10.0);
                    let count = if self.recordings_open {
                        recorded_people.len()
                    } else {
                        people.len()
                    };
                    if count == 0 {
                        let message = if self.recordings_open {
                            if self.work.is_some() {
                                "Loading recordings…"
                            } else {
                                "No recordings yet."
                            }
                        } else if self.snapshot.is_none() && self.work.is_some() {
                            "Checking who's live…"
                        } else {
                            empty_message
                        };
                        ui.label(RichText::new(message).color(MUTED));
                    }
                    ui.spacing_mut().item_spacing.y = 2.0;
                    egui::ScrollArea::vertical()
                        .id_salt(("stream-members", self.recordings_open))
                        .max_height((height - 36.0).max(80.0))
                        .show_rows(ui, MEMBER_ROW_HEIGHT, count, |ui, rows| {
                            for index in rows {
                                if self.recordings_open {
                                    let person = &recorded_people[index];
                                    let selected = self
                                        .focused
                                        .as_ref()
                                        .is_some_and(|(id, _)| id == person.user_id);
                                    if member_row(
                                        ui,
                                        person.name,
                                        Some(person.count),
                                        selected,
                                        false,
                                    )
                                    .clicked()
                                    {
                                        self.focused = Some((
                                            person.user_id.to_owned(),
                                            person.name.to_owned(),
                                        ));
                                    }
                                } else {
                                    let stream = people[index];
                                    let selected = self
                                        .focused
                                        .as_ref()
                                        .is_some_and(|(id, _)| id == &stream.user_id);
                                    if member_row(ui, &stream.name, None, selected, true).clicked()
                                    {
                                        self.stop_player();
                                        self.player_error = None;
                                        self.focused =
                                            Some((stream.user_id.clone(), stream.name.clone()));
                                        self.selected = Some(stream.clone());
                                    }
                                }
                            }
                        });
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if self.recordings_open {
                    self.draw_recordings(ui);
                } else if let Some(stream) = self.selected.clone() {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let platforms: Vec<_> = live
                                .iter()
                                .filter(|s| s.user_id == stream.user_id)
                                .collect();
                            let reserved =
                                platforms.len() as f32 * (80.0 + ui.spacing().item_spacing.x);
                            let name_width = (ui.available_width() - reserved).max(40.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(name_width, 32.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&stream.name)
                                                .strong()
                                                .color(Color32::from_rgb(239, 242, 247)),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&stream.name);
                                },
                            );
                            for platform in platforms {
                                let selected = platform.provider == stream.provider;
                                if ui
                                    .add(
                                        action_button(platform.provider.label())
                                            .fill(if selected {
                                                Color32::from_rgb(41, 47, 60)
                                            } else {
                                                Color32::from_rgb(36, 41, 50)
                                            })
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                if selected {
                                                    Color32::from_rgb(84, 99, 122)
                                                } else {
                                                    Color32::from_rgb(51, 58, 70)
                                                },
                                            )),
                                    )
                                    .clicked()
                                {
                                    self.stop_player();
                                    self.player_error = None;
                                    self.selected = Some(platform.clone());
                                }
                            }
                        },
                    );
                    ui.add_space(8.0);
                    let size = egui::vec2(
                        ui.available_width(),
                        (ui.available_width() * 9.0 / 16.0)
                            .max(300.0)
                            .min((height - 120.0).max(300.0)),
                    );
                    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                    ui.painter()
                        .rect_filled(rect, 8.0, Color32::from_rgb(12, 14, 19));
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        if self.player_error.is_some() {
                            "Player unavailable"
                        } else {
                            "Opening stream…"
                        },
                        egui::FontId::proportional(16.0),
                        MUTED,
                    );
                    self.player_rect = Some(rect);
                    ui.add_space(if ui.ctx().content_rect().height() < 640.0 {
                        4.0
                    } else {
                        10.0
                    });
                    let mut caption = egui::text::LayoutJob::default();
                    caption.append(
                        "LIVE  ·  ",
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(11.0),
                            color: LIVE,
                            ..Default::default()
                        },
                    );
                    caption.append(
                        &stream.name,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(14.0),
                            color: Color32::from_rgb(239, 242, 247),
                            ..Default::default()
                        },
                    );
                    caption.append(
                        &format!("  ·  {}", stream.provider.label()),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(11.0),
                            color: MUTED,
                            ..Default::default()
                        },
                    );
                    ui.add_sized(
                        egui::vec2(ui.available_width(), 22.0),
                        egui::Label::new(caption)
                            .halign(egui::Align::Center)
                            .truncate(),
                    );
                    if let Some(error) = &self.player_error {
                        ui.label(RichText::new(error).small().color(MUTED));
                        if ui.button("Retry player").clicked() {
                            self.player_attempted = false;
                            self.player_error = None;
                        }
                    }
                } else {
                    empty_view(
                        ui,
                        if live.is_empty() {
                            empty_message
                        } else {
                            "Select a player"
                        },
                    );
                }
            });
        });
        self.draw_editor(ui.ctx());
    }

    fn draw_recordings(&mut self, ui: &mut egui::Ui) {
        let Some((member_id, name)) = &self.focused else {
            empty_view(
                ui,
                if self.recordings.as_ref().is_some_and(|v| v.is_empty()) {
                    "No recordings yet."
                } else {
                    "Select a player"
                },
            );
            return;
        };
        ui.add_sized(
            egui::vec2(ui.available_width(), 32.0),
            egui::Label::new(RichText::new(name).size(18.0).strong()).truncate(),
        );
        ui.add_space(8.0);
        let vods = self
            .recordings
            .as_deref()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut filtered: Vec<_> = vods
            .iter()
            .filter(|vod| &vod.user_id == member_id)
            .collect();
        filtered.sort_by(|a, b| recording_time(b).cmp(recording_time(a)));
        if filtered.is_empty() {
            ui.label(
                RichText::new(if self.work.is_some() {
                    "Loading recordings…"
                } else {
                    "No recordings yet."
                })
                .color(MUTED),
            );
        }
        egui::ScrollArea::vertical()
            .id_salt(("stream-recordings", member_id))
            .max_height(ui.available_height())
            .show_rows(ui, 152.0, filtered.len(), |ui, range| {
                for index in range {
                    let vod = filtered[index];
                    let day = recording_day(vod);
                    ui.allocate_ui(egui::vec2(ui.available_width(), 24.0), |ui| {
                        if index == 0 || recording_day(filtered[index - 1]) != day {
                            ui.label(
                                RichText::new(recording_day_label(day))
                                    .strong()
                                    .color(MUTED),
                            );
                        }
                    });
                    egui::Frame::new()
                        .fill(Color32::from_rgb(25, 28, 36))
                        .corner_radius(8)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            ui.set_min_height(100.0);
                            ui.set_width((ui.available_width() - 1.0).max(200.0));
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(vod.provider.label()).strong());
                                let time = recording_time(vod).get(11..16).unwrap_or("");
                                if !time.is_empty() {
                                    ui.label(
                                        RichText::new(format!("{time} UTC")).small().color(MUTED),
                                    );
                                }
                            });
                            ui.hyperlink_to("Open recording", &vod.url);
                            ui.add(
                                egui::Label::new(RichText::new(&vod.url).small().color(MUTED))
                                    .truncate(),
                            );
                            if ui.small_button("Copy link").clicked() {
                                ui.ctx().copy_text(vod.url.clone());
                            }
                        });
                }
            });
    }

    fn draw_editor(&mut self, ctx: &egui::Context) {
        if !self.edit_open {
            return;
        }
        let mut open = true;
        let mut action = None;
        let mut done = false;
        egui::Window::new("Your streams")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .collapsible(false).resizable(false).default_width(470.0)
            .frame(egui::Frame::new().fill(Color32::from_rgb(29, 33, 41)).stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(51, 58, 70))).corner_radius(10).inner_margin(18))
            .open(&mut open).show(ctx, |ui| {
                ui.label(RichText::new("Add one or both platforms.").strong());
                ui.add_space(10.0);
                egui::ScrollArea::vertical().id_salt("stream-setup-help").max_height((ctx.content_rect().height() - 220.0).max(220.0)).show(ui, |ui| {
                    for (index, provider) in [Provider::Twitch, Provider::Youtube].into_iter().enumerate() {
                        let twitch = provider == Provider::Twitch;
                        let own = self.snapshot.as_ref().and_then(|s| s.own_streams.iter().find(|stream| stream.provider == provider));
                        let ended = own.is_some_and(|stream| stream.broadcast_state.as_deref() == Some("ended"));
                        egui::Frame::new().fill(Color32::from_rgb(22, 25, 32)).corner_radius(8).inner_margin(14).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(if twitch { "Twitch channel" } else { "YouTube broadcast" }).strong().size(15.0));
                                ui.label(RichText::new(if twitch { "SET UP ONCE" } else { "NEW LINK EACH BROADCAST" }).size(10.0).color(if twitch { LIVE } else { Color32::from_rgb(231, 190, 108) }));
                            });
                            ui.add_space(6.0);
                            ui.label(RichText::new(if twitch {
                                "Paste your channel link once. Brick will find your future broadcasts automatically."
                            } else {
                                "Start or schedule your stream in YouTube, choose Share, and paste that broadcast's video link here."
                            }).small().color(MUTED));
                            ui.add_space(8.0);
                            let hint = if twitch { "https://twitch.tv/yourchannel" } else { "https://youtube.com/live/your-video-id" };
                            ui.add_enabled(self.work.is_none(), egui::TextEdit::singleline(&mut self.drafts[index]).hint_text(hint).desired_width(f32::INFINITY).char_limit(512));
                            if self.notice_provider.as_ref() == Some(&provider) {
                                if let Some(error) = &self.notice {
                                    ui.label(RichText::new(error).small().color(Color32::from_rgb(244, 144, 144)));
                                }
                            }
                            if !twitch {
                                ui.collapsing("Where do I find the right link?", |ui| {
                                    ui.label(RichText::new("1. Open the live or scheduled broadcast on YouTube.\n2. Choose Share, then Copy link.\n3. Paste it above. Use a new link for your next broadcast.").small().color(MUTED));
                                    ui.label(RichText::new("Use the video's link, rather than your YouTube channel page.").small().color(MUTED));
                                });
                            }
                            if self.saved_provider.as_ref() == Some(&provider) {
                                ui.add_space(6.0);
                                ui.label(RichText::new("Link saved. Checking live status…").small().color(MUTED));
                            } else if let Some(own) = own {
                                ui.add_space(6.0);
                                ui.label(RichText::new(submission_status(own)).small().color(if own.status == Status::Live { LIVE } else { MUTED }));
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if own.is_some() {
                                    let confirming = self.confirm_remove.as_ref() == Some(&provider);
                                    if ui.add_enabled(self.work.is_none(), action_button(if confirming { "Confirm removal" } else { "Remove" })).clicked() {
                                        if confirming { action = Some(Action::Remove(provider.clone())); self.drafts[index].clear(); }
                                        else { self.confirm_remove = Some(provider.clone()); }
                                    }
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let label = if twitch { "Save channel" } else if ended { "Add next broadcast" } else if own.is_some() { "Replace broadcast" } else { "Add broadcast" };
                                    let changed = own.is_none_or(|stream| stream.url != self.drafts[index].trim());
                                    if ui.add_enabled(self.work.is_none() && changed && !self.drafts[index].trim().is_empty(), action_button(label)).clicked() {
                                        action = Some(Action::Save(provider.clone(), self.drafts[index].trim().to_owned()));
                                    }
                                });
                            });
                        });
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { done = ui.add(action_button("Done")).clicked(); });
            });
        if let Some(action) = action {
            self.notice = None;
            self.start(ctx, action);
        }
        if !open || done {
            self.edit_open = false;
            self.drafts = [String::new(), String::new()];
            self.confirm_remove = None;
            if self.notice_provider.take().is_some() {
                self.notice = None;
            }
        }
    }

    pub fn update_player(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        allowed: bool,
    ) -> bool {
        if !allowed || self.edit_open || self.recordings_open {
            self.stop_player();
            return false;
        }
        let Some(rect) = self.player_rect else {
            self.stop_player();
            return false;
        };
        if let Some(player) = &mut self.player {
            if let Err(error) = player.set_bounds(rect, ctx.pixels_per_point()) {
                self.player_error = Some(error);
                self.player = None;
            }
            return false;
        }
        if let Some(rx) = &self.player_work {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("The player stopped loading. Try again."
                        .to_string()
                        .into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.player_work = None;
                match result {
                    Ok((url, token, preferences)) => {
                        self.preferences = preferences;
                        match StreamPlayer::new(
                            frame,
                            &url,
                            &token,
                            rect,
                            ctx.pixels_per_point(),
                            self.preferences.clone(),
                        ) {
                            Ok(player) => self.player = Some(player),
                            Err(error) => self.player_error = Some(error),
                        }
                    }
                    Err(error) => {
                        if error.access_denied {
                            self.clear();
                            return true;
                        }
                        self.player_error = Some(error.message);
                    }
                }
            }
        } else if !self.player_attempted {
            let Some(stream) = &self.selected else {
                return false;
            };
            let user_id = stream.user_id.clone();
            let provider = stream.provider.clone();
            let preferences = self.preferences.clone();
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let result = access_token().and_then(|token| {
                    streams::player_url(&user_id, &provider).map(|url| {
                        let preferences = if provider == Provider::Twitch {
                            Some(preferences.unwrap_or_else(Preferences::load))
                        } else {
                            preferences
                        };
                        (url, token, preferences)
                    })
                });
                let _ = tx.send(result);
                ctx.request_repaint();
            });
            self.player_work = Some(rx);
            self.player_attempted = true;
        }
        false
    }
}

fn empty_view(ui: &mut egui::Ui, message: &str) {
    let size = egui::vec2(ui.available_width(), ui.available_height().max(240.0));
    egui::Frame::new()
        .fill(Color32::from_rgb(25, 28, 36))
        .corner_radius(10)
        .show(ui, |ui| {
            ui.allocate_ui_with_layout(
                size,
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.label(
                        RichText::new(message)
                            .size(20.0)
                            .strong()
                            .color(Color32::from_rgb(239, 242, 247)),
                    );
                },
            );
        });
}

fn member_row(
    ui: &mut egui::Ui,
    name: &str,
    recordings: Option<usize>,
    selected: bool,
    live: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), MEMBER_ROW_HEIGHT),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            name,
        )
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        if selected || response.hovered() || response.has_focus() {
            painter.rect_filled(
                rect,
                4.0,
                if selected {
                    Color32::from_rgb(36, 42, 52)
                } else {
                    Color32::from_rgb(29, 34, 42)
                },
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0_f32, MUTED),
                egui::StrokeKind::Inside,
            );
        }
        if live {
            painter.circle_filled(egui::pos2(rect.left() + 10.0, rect.center().y), 3.0, LIVE);
        }
        let count_width = recordings
            .map(|count| {
                painter
                    .text(
                        egui::pos2(rect.right() - 8.0, rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        count.to_string(),
                        egui::FontId::proportional(11.0),
                        MUTED,
                    )
                    .width()
                    + 12.0
            })
            .unwrap_or(0.0);
        let inset = if live { 22.0 } else { 10.0 };
        let color = if selected {
            Color32::from_rgb(239, 242, 247)
        } else {
            Color32::from_rgb(209, 217, 229)
        };
        let mut text = egui::text::LayoutJob::simple_singleline(
            name.to_owned(),
            egui::FontId::proportional(13.0),
            color,
        );
        text.wrap.max_width = (rect.width() - inset - count_width - 8.0).max(1.0);
        text.wrap.max_rows = 1;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(text));
        painter.galley(
            egui::pos2(rect.left() + inset, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    response
        .on_hover_text(name)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

struct RecordingMember<'a> {
    user_id: &'a str,
    name: &'a str,
    count: usize,
}

fn recording_people(vods: &[Vod]) -> Vec<RecordingMember<'_>> {
    let mut members = HashMap::new();
    for vod in vods {
        let member = members
            .entry(vod.user_id.as_str())
            .or_insert(RecordingMember {
                user_id: &vod.user_id,
                name: &vod.name,
                count: 0,
            });
        member.count += 1;
    }
    let mut members: Vec<_> = members.into_values().collect();
    members.sort_by_cached_key(|member| (member.name.to_lowercase(), member.user_id));
    members
}

fn recording_time(vod: &Vod) -> &str {
    vod.started_at
        .as_deref()
        .or(vod.ended_at.as_deref())
        .unwrap_or("")
}

fn recording_day(vod: &Vod) -> &str {
    recording_time(vod).get(..10).unwrap_or("")
}

fn recording_day_label(day: &str) -> String {
    let parts: Vec<_> = day.split('-').collect();
    if parts.len() == 3 {
        if let (Ok(year), Ok(month), Ok(day)) = (
            parts[0].parse::<i32>(),
            parts[1].parse::<u8>(),
            parts[2].parse::<u8>(),
        ) {
            if let Ok(month) = time::Month::try_from(month) {
                if time::Date::from_calendar_date(year, month, day).is_ok() {
                    return format!("{day} {month} {year}");
                }
            }
        }
    }
    "Date unavailable".into()
}

fn action_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::from_rgb(239, 242, 247)))
        .fill(Color32::from_rgb(36, 41, 50))
        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(51, 58, 70)))
        .corner_radius(8)
        .min_size(egui::vec2(80.0, 32.0))
}

fn submission_status(stream: &Stream) -> &'static str {
    match stream.status {
        Status::Checking => "Link saved. Checking live status…",
        Status::Unknown => "Link saved. Live status couldn't be checked. Brick will retry.",
        Status::Live => "Live now.",
        Status::Offline => match stream.broadcast_state.as_deref() {
            Some("ended") => {
                "This broadcast has ended. Add your next broadcast's link to go live here again."
            }
            Some("upcoming") => "Scheduled. Brick will show you when this broadcast starts.",
            _ if stream.provider == Provider::Twitch => {
                "Channel saved. You'll appear automatically when you go live."
            }
            _ => "Broadcast saved. Brick will check when it goes live.",
        },
    }
}

fn live_people(streams: &[Stream]) -> Vec<&Stream> {
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    for stream in streams {
        if seen.insert(&stream.user_id) {
            people.push(stream);
        }
    }
    people
}

fn access_token() -> Result<String, streams::Error> {
    discord_auth::current_access_token().map_err(|message| streams::Error {
        message,
        access_denied: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        serde_json::from_value(serde_json::json!({"streams":[{"userId":"1","name":"Guildmate","provider":"twitch","channelId":"guildmate","url":"https://www.twitch.tv/guildmate","status":"live"}],"ownStream":null,"providers":{"twitch":true,"youtube":true}})).unwrap()
    }

    #[test]
    fn losing_authorization_discards_private_data_and_late_results() {
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.saved_provider = Some(Provider::Twitch);
        ui.drafts[0] = "https://twitch.tv/privateguildmate".into();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        ui.tick(&egui::Context::default(), false, false);
        assert!(ui.snapshot.is_none());
        assert!(ui.saved_provider.is_none());
        assert!(ui.drafts.iter().all(String::is_empty));
        assert!(tx.send(Ok(ResultData::Snapshot(snapshot()))).is_err());
    }

    #[test]
    fn saved_submission_waits_for_verification_without_reporting_failure() {
        let mut ui = StreamsUi::default();
        ui.saved_provider = Some(Provider::Twitch);
        let ctx = egui::Context::default();
        for (status, unverified, expected) in [
            ("checking", 0, "Checking live status"),
            ("offline", 0, "automatically when you go live"),
            ("live", 0, "Live now"),
            ("unknown", 1, "couldn't be checked"),
        ] {
            let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
                "streams": [],
                "ownStreams": [{"userId":"1","name":"Me","provider":"twitch",
                    "channelId":"mychannel","url":"https://www.twitch.tv/mychannel","status":status}],
                "providers": {"twitch":true,"youtube":true},
                "unverifiedCount": unverified
            })).unwrap();
            let (tx, rx) = mpsc::channel();
            ui.work = Some(rx);
            tx.send(Ok(ResultData::Snapshot(snapshot))).unwrap();
            ui.tick(&ctx, true, false);
            assert!(ui.saved_provider.is_none());
            assert!(ui.notice.is_none());
            let current = ui.snapshot.as_ref().unwrap();
            assert!(submission_status(&current.own_streams[0]).contains(expected));
            assert_eq!(current.unverified_count, unverified);
            assert!(ui.selected.is_none());
        }
    }

    #[test]
    fn waiting_for_stream_requests_does_not_animate_the_native_window() {
        let mut streams = StreamsUi::default();
        let (_tx, rx) = mpsc::channel();
        streams.work = Some(rx);
        streams.snapshot = Some(Rc::new(snapshot()));
        let ctx = egui::Context::default();
        for _ in 0..5 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| streams.draw(ui));
        }
        let output = ctx.run_ui(egui::RawInput::default(), |ui| streams.draw(ui));
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay > Duration::from_secs(1)
        );
    }

    #[test]
    fn ended_or_changed_stream_clears_selection_and_player_work() {
        let mut ui = StreamsUi::default();
        ui.selected = Some(snapshot().streams.remove(0));
        let mut ended = snapshot();
        ended.streams.clear();
        let (tx, rx) = mpsc::channel();
        ui.work = Some(rx);
        tx.send(Ok(ResultData::Snapshot(ended))).unwrap();
        ui.tick(&egui::Context::default(), true, false);
        assert!(ui.selected.is_none());
        assert!(ui.notice.is_none());
    }

    #[test]
    fn unverified_live_data_expires_even_when_hidden() {
        let mut ui = StreamsUi::default();
        ui.snapshot = Some(Rc::new(snapshot()));
        ui.selected = Some(snapshot().streams.remove(0));
        ui.received_at = Some(Instant::now() - MAX_STALE);
        ui.tick(&egui::Context::default(), true, false);
        assert!(ui.snapshot.is_none());
        assert!(ui.selected.is_none());
    }

    #[test]
    fn dual_platform_streams_share_one_player_sidebar_entry() {
        let mut snapshot = snapshot();
        let mut youtube = snapshot.streams[0].clone();
        youtube.provider = Provider::Youtube;
        youtube.channel_id = "abcDEF_12-3".into();
        snapshot.streams.push(youtube);
        assert_eq!(live_people(&snapshot.streams).len(), 1);
    }

    #[test]
    fn recording_members_are_independent_of_live_status_and_group_both_platforms() {
        let vods: Vec<Vod> = serde_json::from_value(serde_json::json!([
            {"userId":"2", "name":"Zed", "provider":"youtube", "url":"https://www.youtube.com/watch?v=abcDEF_12-3", "title":"Raid", "startedAt":"2026-09-08T18:00:00Z"},
            {"userId":"1", "name":"Andy", "provider":"twitch", "url":"https://www.twitch.tv/videos/1", "title":"Raid", "startedAt":"2026-09-07T18:00:00Z"},
            {"userId":"1", "name":"Andy", "provider":"youtube", "url":"https://www.youtube.com/watch?v=abcDEF_12-3", "title":"Raid", "startedAt":"2026-09-07T18:00:00Z"}
        ])).unwrap();
        let people = recording_people(&vods);
        assert_eq!(
            people
                .iter()
                .map(|person| (person.user_id, person.count))
                .collect::<Vec<_>>(),
            [("1", 2), ("2", 1)]
        );
        assert_eq!(recording_day(&vods[1]), recording_day(&vods[2]));
        assert_eq!(
            recording_day_label(recording_day(&vods[0])),
            "8 September 2026"
        );
        assert_eq!(recording_day_label("2026-02-30"), "Date unavailable");
        let mut ui = StreamsUi::default();
        ui.recordings = Some(Rc::new(vods));
        ui.recordings_open = true;
        ui.tick(&egui::Context::default(), false, true);
        assert!(ui.recordings.is_none());
        assert!(!ui.recordings_open);
    }

    // Explicitly opt in with the local fixture and its isolated Secret Service
    // profile. This renders the actual native media child without starting the
    // add-on watcher or changing the production application/session.
    #[cfg(target_os = "linux")]
    mod native_smoke {
        use super::*;
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        use std::{
            collections::HashSet,
            sync::{Arc, Mutex},
        };
        use x11rb::{
            connection::Connection as _,
            protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, MapState},
        };

        #[derive(Default)]
        struct Outcome {
            complete: bool,
            failure: Option<String>,
            captures: Vec<String>,
        }

        struct Driver {
            streams: StreamsUi,
            outcome: Arc<Mutex<Outcome>>,
            started: Instant,
            phase_started: Instant,
            phase: u8,
            baseline: Option<HashSet<u32>>,
            media_children: Vec<u32>,
            sampled_media: bool,
            clicked_media: bool,
            cycles: u8,
            drop_probe: Option<Box<dyn Fn() -> bool>>,
        }

        fn resource_sample(label: &str) -> (usize, usize) {
            let mut processes = Vec::new();
            for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                let Some(end) = stat.rfind(')') else {
                    continue;
                };
                let fields: Vec<_> = stat[end + 2..].split_whitespace().collect();
                if fields.len() < 13 {
                    continue;
                }
                let Some(parent) = fields[1].parse::<u32>().ok() else {
                    continue;
                };
                let ticks =
                    fields[11].parse::<u64>().unwrap_or(0) + fields[12].parse::<u64>().unwrap_or(0);
                let rss = std::fs::read_to_string(entry.path().join("status"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find(|line| line.starts_with("VmRSS:"))
                            .and_then(|line| line.split_whitespace().nth(1))
                            .and_then(|value| value.parse::<u64>().ok())
                    })
                    .unwrap_or(0);
                let name = stat[stat.find('(').unwrap_or(0) + 1..end].to_owned();
                processes.push((pid, parent, ticks, rss, name));
            }
            let mut selected = HashSet::from([std::process::id()]);
            loop {
                let before = selected.len();
                for (pid, parent, ..) in &processes {
                    if selected.contains(parent) {
                        selected.insert(*pid);
                    }
                }
                if before == selected.len() {
                    break;
                }
            }
            let mut rows: Vec<_> = processes.into_iter().filter(|(pid, ..)| selected.contains(pid)).map(|(pid, parent, ticks, rss, name)| {
                let pss = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find(|line| line.starts_with("Pss:"))
                            .and_then(|line| line.split_whitespace().nth(1))
                            .and_then(|value| value.parse::<u64>().ok())
                    });
                serde_json::json!({"pid":pid,"parent":parent,"cpuTicks":ticks,"rssKiB":rss,"pssKiB":pss,"name":name})
            }).collect();
            rows.sort_by_key(|row| row["pid"].as_u64());
            let total: u64 = rows.iter().filter_map(|row| row["rssKiB"].as_u64()).sum();
            let total_pss = rows
                .iter()
                .map(|row| row["pssKiB"].as_u64())
                .collect::<Option<Vec<_>>>()
                .map(|values| values.into_iter().sum::<u64>());
            let network_processes = rows
                .iter()
                .filter(|row| row["name"] == "WebKitNetworkPr")
                .count();
            let renderer_processes = rows
                .iter()
                .filter(|row| row["name"] == "WebKitWebProces")
                .count();
            eprintln!(
                "Native resource sample {}",
                serde_json::json!({"label":label,"atMillis":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64,"totalRssKiB":total,"totalPssKiB":total_pss,"processes":rows})
            );
            (network_processes, renderer_processes)
        }

        fn window_id(frame: &eframe::Frame) -> Result<u32, String> {
            match frame
                .window_handle()
                .map_err(|error| error.to_string())?
                .as_raw()
            {
                RawWindowHandle::Xlib(handle) => handle
                    .window
                    .try_into()
                    .map_err(|_| "Invalid X11 window".into()),
                RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
                _ => Err("The native smoke test requires X11/XWayland.".into()),
            }
        }

        fn mapped_children(window: u32) -> Result<HashSet<u32>, String> {
            let operation = || -> Result<HashSet<u32>, Box<dyn std::error::Error>> {
                let (connection, _) = x11rb::connect(None)?;
                let mut children = HashSet::new();
                let mut parents = vec![window];
                for _ in 0..8 {
                    let mut next = Vec::new();
                    for parent in parents {
                        if let Ok(tree) = connection.query_tree(parent)?.reply() {
                            for child in tree.children {
                                if connection.get_window_attributes(child)?.reply().is_ok_and(
                                    |attributes| attributes.map_state == MapState::VIEWABLE,
                                ) {
                                    children.insert(child);
                                    next.push(child);
                                }
                            }
                        }
                    }
                    if next.is_empty() {
                        break;
                    }
                    parents = next;
                }
                Ok(children)
            };
            operation().map_err(|error| error.to_string())
        }

        fn capture(window: u32, name: &str) -> Result<String, String> {
            let operation = || -> Result<String, Box<dyn std::error::Error>> {
                let (connection, screen) = x11rb::connect(None)?;
                let setup = connection.setup();
                let root = &setup.roots[screen];
                let geometry = connection.get_geometry(window)?.reply()?;
                let shot = connection
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        window,
                        0,
                        0,
                        geometry.width,
                        geometry.height,
                        u32::MAX,
                    )?
                    .reply()?;
                let format = setup
                    .pixmap_formats
                    .iter()
                    .find(|format| format.depth == shot.depth)
                    .ok_or("Missing image format")?;
                if format.bits_per_pixel != 32 {
                    return Err("Expected a 32-bit X11 screenshot".into());
                }
                let visual = root
                    .allowed_depths
                    .iter()
                    .flat_map(|depth| &depth.visuals)
                    .find(|visual| visual.visual_id == shot.visual)
                    .ok_or("Missing root visual")?;
                let stride = (geometry.width as usize * 32).div_ceil(format.scanline_pad as usize)
                    * format.scanline_pad as usize
                    / 8;
                let channel = |pixel: u32, mask: u32| -> u8 {
                    if mask == 0 {
                        return 0;
                    }
                    let shift = mask.trailing_zeros();
                    (((pixel & mask) >> shift) * 255 / (mask >> shift)) as u8
                };
                let mut image =
                    image::RgbaImage::new(geometry.width as u32, geometry.height as u32);
                for y in 0..geometry.height as usize {
                    for x in 0..geometry.width as usize {
                        let bytes: [u8; 4] =
                            shot.data[y * stride + x * 4..y * stride + x * 4 + 4].try_into()?;
                        let pixel = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                            u32::from_le_bytes(bytes)
                        } else {
                            u32::from_be_bytes(bytes)
                        };
                        image.put_pixel(
                            x as u32,
                            y as u32,
                            image::Rgba([
                                channel(pixel, visual.red_mask),
                                channel(pixel, visual.green_mask),
                                channel(pixel, visual.blue_mask),
                                255,
                            ]),
                        );
                    }
                }
                let directory = std::path::Path::new("/tmp/brick-streams-smoke/results");
                std::fs::create_dir_all(directory)?;
                let path = directory.join(format!("native-{name}.png"));
                image.save(&path)?;
                Ok(path.to_string_lossy().into_owned())
            };
            operation().map_err(|error| error.to_string())
        }

        impl Driver {
            fn select(&mut self, provider: Provider) -> Result<(), String> {
                let member = if provider == Provider::Twitch {
                    "103"
                } else {
                    "102"
                };
                let stream =
                    self.streams
                        .snapshot
                        .as_ref()
                        .and_then(|snapshot| {
                            snapshot.streams.iter().find(|stream| {
                                stream.user_id == member && stream.provider == provider
                            })
                        })
                        .cloned()
                        .ok_or_else(|| {
                            format!(
                                "Fixture member {member} has no live {} stream",
                                provider.label()
                            )
                        })?;
                self.streams.stop_player();
                self.streams.player_error = None;
                self.streams.focused = Some((stream.user_id.clone(), stream.name.clone()));
                self.streams.selected = Some(stream);
                Ok(())
            }

            fn next(&mut self, phase: u8) {
                self.phase = phase;
                self.phase_started = Instant::now();
                self.sampled_media = false;
                self.clicked_media = false;
                eprintln!("Native stream smoke phase {phase}");
            }

            fn save_capture(&self, window: u32, name: &str) -> Result<(), String> {
                let path = capture(window, name)?;
                eprintln!("Native stream smoke screenshot: {path}");
                self.outcome.lock().unwrap().captures.push(path);
                Ok(())
            }

            fn assert_media_gone(&self, window: u32) -> Result<(), String> {
                if self.streams.player.is_some() {
                    return Err("Player still exists after closing its view".into());
                }
                let mapped = mapped_children(window)?;
                if self
                    .media_children
                    .iter()
                    .any(|child| mapped.contains(child))
                {
                    return Err("A native media child remains mapped after player shutdown".into());
                }
                if self.drop_probe.as_ref().is_some_and(|probe| !probe()) {
                    return Err(
                        "The native WebKit widget remains alive after player shutdown".into(),
                    );
                }
                Ok(())
            }

            fn track_media(&mut self, window: u32) -> Result<(), String> {
                self.media_children = mapped_children(window)?
                    .difference(self.baseline.as_ref().unwrap())
                    .copied()
                    .collect();
                self.drop_probe = self
                    .streams
                    .player
                    .as_ref()
                    .and_then(|player| player.diagnostic_drop_probe());
                if self.media_children.is_empty() {
                    return Err("The real media player created no mapped native child".into());
                }
                Ok(())
            }

            fn advance(
                &mut self,
                ctx: &egui::Context,
                frame: &eframe::Frame,
            ) -> Result<(), String> {
                if self.started.elapsed() > Duration::from_secs(105) {
                    return Err("Native stream smoke timed out".into());
                }
                let window = window_id(frame)?;
                if self.baseline.is_none() {
                    self.baseline = Some(mapped_children(window)?);
                    resource_sample("idle-before-streams");
                }
                let elapsed = self.phase_started.elapsed();
                match self.phase {
                    0 if self.streams.snapshot.is_some() => {
                        self.select(Provider::Youtube)?;
                        self.next(1);
                    }
                    1 | 3 => {
                        let expected = if self.phase == 1 {
                            Provider::Youtube
                        } else {
                            Provider::Twitch
                        };
                        if self
                            .streams
                            .selected
                            .as_ref()
                            .is_none_or(|stream| stream.provider != expected)
                        {
                            return Err("The selected platform changed during the automated capture sequence".into());
                        }
                        if let Some(error) = &self.streams.player_error {
                            return Err(error.clone());
                        }
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(5)
                            && !self.clicked_media
                        {
                            let rect = self.streams.player_rect.ok_or("Player has no bounds")?;
                            let scale = ctx.pixels_per_point() as f64;
                            let (x, y) = if self.phase == 1 {
                                (
                                    rect.width() as f64 * scale / 2.0,
                                    rect.height() as f64 * scale / 2.0,
                                )
                            } else {
                                (24.0, rect.height() as f64 * scale - 24.0)
                            };
                            self.streams
                                .player
                                .as_ref()
                                .unwrap()
                                .diagnostic_click(x, y)?;
                            self.clicked_media = true;
                        }
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(10)
                            && !self.sampled_media
                        {
                            self.streams.player.as_ref().unwrap().diagnostic_details();
                            for child in
                                mapped_children(window)?.difference(self.baseline.as_ref().unwrap())
                            {
                                let provider = if self.phase == 1 { "youtube" } else { "twitch" };
                                match capture(*child, &format!("{provider}-early-child-{child}")) {
                                    Ok(path) => eprintln!("Native media child screenshot: {path}"),
                                    Err(error) => eprintln!(
                                        "Native child {child} has no readable drawable: {error}"
                                    ),
                                }
                            }
                            if std::env::var_os("BRICK_STREAM_DIAGNOSTIC").is_some() {
                                self.streams.player.as_ref().unwrap().diagnostic_html();
                            }
                            self.sampled_media = true;
                        }
                        let capture_after = if self.phase == 1 { 30 } else { 15 };
                        if self.streams.player.is_some()
                            && elapsed > Duration::from_secs(capture_after)
                        {
                            self.track_media(window)?;
                            resource_sample(if self.phase == 1 {
                                "youtube-open"
                            } else {
                                "twitch-open"
                            });
                            self.media_children = mapped_children(window)?
                                .difference(self.baseline.as_ref().unwrap())
                                .copied()
                                .collect();
                            if self.media_children.is_empty() {
                                return Err(
                                    "The real media player created no mapped native child".into()
                                );
                            }
                            for child in &self.media_children {
                                let provider = if self.phase == 1 { "youtube" } else { "twitch" };
                                if let Ok(path) =
                                    capture(*child, &format!("{provider}-late-child-{child}"))
                                {
                                    eprintln!("Native media child screenshot: {path}");
                                }
                            }
                            self.save_capture(
                                window,
                                if self.phase == 1 { "youtube" } else { "twitch" },
                            )?;
                            if self.phase == 1 {
                                self.streams.tick(ctx, true, false);
                                self.next(2);
                            } else {
                                self.streams.edit_open = true;
                                self.streams.stop_player();
                                self.next(4);
                            }
                        }
                    }
                    2 if elapsed > Duration::from_secs(2) => {
                        self.assert_media_gone(window)?;
                        self.save_capture(window, "updates-cleanup")?;
                        resource_sample("after-switch-to-updates");
                        if std::env::var_os("BRICK_STREAM_YOUTUBE_ONLY").is_some() {
                            self.outcome.lock().unwrap().complete = true;
                            self.next(10);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            return Ok(());
                        }
                        self.select(Provider::Twitch)?;
                        self.next(3);
                    }
                    4 if elapsed > Duration::from_secs(2) => {
                        self.assert_media_gone(window)?;
                        self.save_capture(window, "dialog-cleanup")?;
                        resource_sample("after-dialog-open");
                        self.streams.tick(ctx, false, false);
                        if self.streams.snapshot.is_some()
                            || self.streams.selected.is_some()
                            || self.streams.player_work.is_some()
                            || self.streams.recordings.is_some()
                        {
                            return Err("Logout retained stream data or work".into());
                        }
                        self.next(5);
                    }
                    5 if elapsed > Duration::from_secs(2) => {
                        self.assert_media_gone(window)?;
                        self.save_capture(window, "logout-cleanup")?;
                        resource_sample("after-logout");
                        self.next(6);
                    }
                    6 if self.streams.snapshot.is_some() => {
                        self.select(Provider::Twitch)?;
                        self.next(7);
                    }
                    7 if self.streams.player.is_some() && elapsed > Duration::from_secs(3) => {
                        self.track_media(window)?;
                        resource_sample(&format!("repeat-{}-open", self.cycles + 1));
                        self.streams.stop_player();
                        self.next(8);
                    }
                    8 if elapsed > Duration::from_secs(1) => {
                        self.assert_media_gone(window)?;
                        self.cycles += 1;
                        resource_sample(&format!("repeat-{}-closed", self.cycles));
                        if self.cycles < 5 {
                            self.select(Provider::Twitch)?;
                            self.next(7);
                        } else {
                            self.streams.clear();
                            resource_sample("final-idle-start");
                            self.next(9);
                        }
                    }
                    9 if elapsed > Duration::from_secs(5) => {
                        self.assert_media_gone(window)?;
                        let (network_processes, renderer_processes) =
                            resource_sample("final-idle-end");
                        if network_processes > 1 || renderer_processes != 0 {
                            return Err(format!(
                                "Player shutdown retained {network_processes} network processes and {renderer_processes} renderer processes"
                            ));
                        }
                        self.outcome.lock().unwrap().complete = true;
                        self.next(10);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                }
                Ok(())
            }
        }

        impl eframe::App for Driver {
            fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
                if self.phase >= 10 {
                    return;
                }
                let authorized = self.phase != 5 && self.phase < 9;
                let active = !matches!(self.phase, 2 | 8) && authorized;
                if self.streams.tick(ctx, authorized, active) {
                    self.outcome.lock().unwrap().failure =
                        Some("The fixture rejected the secure test session".into());
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                if let Err(error) = self.advance(ctx, frame) {
                    let _ =
                        window_id(frame).and_then(|window| self.save_capture(window, "failure"));
                    self.outcome.lock().unwrap().failure = Some(error);
                    self.streams.clear();
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ctx.request_repaint_after(Duration::from_millis(if self.phase == 9 {
                    250
                } else {
                    33
                }));
            }

            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                let ctx = ui.ctx().clone();
                egui::Frame::central_panel(ui.style())
                    .fill(Color32::from_rgb(18, 21, 27))
                    .inner_margin(20)
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        ui.label(RichText::new("Brick · Native stream lifecycle test").strong());
                        ui.add_space(16.0);
                        if matches!(self.phase, 2 | 8) {
                            ui.heading("Updates");
                            ui.label(
                                "The stream player has closed. This native view remains usable.",
                            );
                        } else if self.phase == 5 || self.phase >= 9 {
                            ui.heading("Sign in to Discord");
                            ui.label(
                                "Private stream data and the native player have been cleared.",
                            );
                        } else {
                            ui.add_enabled_ui(false, |ui| self.streams.draw(ui));
                        }
                    });
                if self.streams.update_player(
                    frame,
                    &ctx,
                    !matches!(self.phase, 2 | 5 | 8) && self.phase < 9,
                ) {
                    self.outcome.lock().unwrap().failure =
                        Some("Player authorization failed".into());
                    self.phase = 10;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        #[test]
        #[ignore = "requires an isolated secure test session, localhost fixture and an X11 desktop"]
        fn player_lifecycle() {
            use winit::platform::x11::EventLoopBuilderExtX11 as _;
            assert_eq!(
                option_env!("BRICK_PRESENCE_API_URL"),
                Some("http://127.0.0.1:18080")
            );
            assert_eq!(option_env!("BRICK_DISCORD_CLIENT_ID"), Some("brick-test"));
            assert!(
                discord_auth::current_access_token().is_ok(),
                "Unlock the isolated test profile's Secret Service first"
            );
            let outcome = Arc::new(Mutex::new(Outcome::default()));
            let app_outcome = outcome.clone();
            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_title("Brick · Native stream smoke")
                    .with_inner_size([1080.0, 820.0])
                    .with_position([120.0, 80.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    builder.with_x11().with_any_thread(true);
                })),
                ..Default::default()
            };
            eframe::run_native(
                "Brick native stream smoke",
                options,
                Box::new(move |cc| {
                    cc.egui_ctx.set_visuals(egui::Visuals::dark());
                    Ok(Box::new(Driver {
                        streams: StreamsUi::default(),
                        outcome: app_outcome,
                        started: Instant::now(),
                        phase_started: Instant::now(),
                        phase: 0,
                        baseline: None,
                        media_children: Vec::new(),
                        sampled_media: false,
                        clicked_media: false,
                        cycles: 0,
                        drop_probe: None,
                    }))
                }),
            )
            .expect("Native lifecycle test window failed");
            let outcome = outcome.lock().unwrap();
            assert!(
                outcome.failure.is_none(),
                "{}",
                outcome.failure.as_deref().unwrap_or_default()
            );
            assert!(outcome.complete, "The lifecycle sequence did not complete");
            assert_eq!(
                outcome.captures.len(),
                if std::env::var_os("BRICK_STREAM_YOUTUBE_ONLY").is_some() {
                    2
                } else {
                    5
                }
            );
        }
    }
}
