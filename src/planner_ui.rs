use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText, Stroke};
use serde::{Deserialize, Serialize};
use time::Date;

use crate::{
    art_ui::{self, ArtUi},
    attendance_ui::{
        calendar_popup, draw_name_cell, first_absence, format_date, grouped, month_name,
        paint_cross, parse_date, weekday_short, AttendanceUi, Kind, Line, GROUP_HEIGHT, LATE_TEXT,
        NAME_WIDTH, OUT_TEXT, OWN_ROW, ROW_HEIGHT,
    },
    discord_auth, download,
    guild_ui::{
        append, arrow, bar_button, class_color, clipped, control_row, get, line_color, pill,
        short_raid, single, text_at, token, CharacterRow, CharactersView, MemberCharacters,
        CONTROL_HEIGHT, MAX_ERROR_BYTES,
    },
    presence::{self, Roster, RosterMember},
    ui::{
        busy_indicator, info_accent, muted_text, panel_frame, primary_text, roster_notice,
        secondary_text, success_accent, warning_accent,
    },
};

const MAX_VIEW_BYTES: u64 = 256 * 1024;
const REFRESH_SECS: u64 = 60;
const SAVE_DELAY: Duration = Duration::from_millis(1200);
const MAX_RAIDS: usize = 6;
const MAX_NAME: usize = 32;
// The character picker is wider than its cell to fit saved lockouts.
const PICKER_EXTRA: f32 = 70.0;
const STRIP_CHIP: egui::Vec2 = egui::vec2(112.0, 46.0);
const STRIP_ARROW: egui::Vec2 = egui::vec2(28.0, 46.0);
// The planner keeps resets from this one through twelve ahead.
const WEEKS_AHEAD: i64 = 12;

const RAID_WIDTH: f32 = 210.0;
const RAID_GAP: f32 = 10.0;
const HEADER_HEIGHT: f32 = 80.0;
const EDIT_HEADER_HEIGHT: f32 = 170.0;
const TOTALS_HEIGHT: f32 = 64.0;
const SMALL_HEIGHT: f32 = 26.0;
const FIELD_FILL: Color32 = Color32::from_rgb(38, 42, 50);
const SAVED: Color32 = Color32::from_rgb(232, 112, 112);
const SEND: Color32 = Color32::from_rgb(203, 85, 48);

// Raid buffs and the classes that bring them.
const BUFFS: [(&str, &[&str]); 11] = [
    ("Int", &["Mage"]),
    ("Stam", &["Priest"]),
    ("AP", &["Warrior"]),
    ("Mark", &["Druid"]),
    ("Mystic Touch", &["Monk"]),
    ("Chaos Brand", &["Demon Hunter"]),
    ("Skyfury", &["Shaman"]),
    ("Devo", &["Paladin"]),
    ("Bronze", &["Evoker"]),
    ("Lust", &["Mage", "Shaman", "Hunter", "Evoker"]),
    ("Bres", &["Death Knight", "Warlock", "Druid", "Paladin"]),
];
const TOKENS: [&str; 4] = ["D", "M", "V", "Z"];
// Raid nights in reset order: a week starts on its Wednesday.
const NIGHTS: [&str; 7] = ["wed", "thu", "fri", "sat", "sun", "mon", "tue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Difficulty {
    Normal,
    #[default]
    Heroic,
    Mythic,
}

impl Difficulty {
    const ALL: [Self; 3] = [Self::Normal, Self::Heroic, Self::Mythic];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Heroic => "Heroic",
            Self::Mythic => "Mythic",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Normal => Color32::from_rgb(69, 211, 127),
            Self::Heroic => Color32::from_rgb(94, 168, 224),
            Self::Mythic => Color32::from_rgb(236, 178, 80),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Splits,
    Prog,
    Farm,
}

impl Mode {
    const ALL: [Self; 3] = [Self::Splits, Self::Prog, Self::Farm];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Splits => "splits",
            Self::Prog => "prog",
            Self::Farm => "farm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlannedRaid {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub difficulty: Difficulty,
    pub mode: Mode,
    pub day: String,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(default)]
    pub instances: Vec<u64>,
    #[serde(default)]
    pub picks: BTreeMap<String, Option<String>>,
    /// Per boss, the members slot by slot in groups of five.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Vec<Option<String>>>,
}

impl PlannedRaid {
    pub(crate) fn pick(&self, user_id: &str) -> Option<&str> {
        self.picks.get(user_id).and_then(|key| key.as_deref())
    }

    pub(crate) fn night(&self, week: Date) -> Date {
        let offset = NIGHTS.iter().position(|day| *day == self.day).unwrap_or(0);
        week + time::Duration::days(offset as i64)
    }

    fn title(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Raid {}", index + 1))
    }
}

/// One reset's raids in the strip above the plan.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScheduledWeek {
    pub week: String,
    pub raids: Vec<ScheduledRaid>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScheduledRaid {
    pub difficulty: Difficulty,
    pub day: String,
}

/// The Wednesday reset that holds `day`.
fn reset_of(day: Date) -> Date {
    let since = (day.weekday().number_days_from_monday() + 7
        - time::Weekday::Wednesday.number_days_from_monday())
        % 7;
    day - time::Duration::days(since as i64)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanView {
    pub week: String,
    pub current_week: String,
    pub revision: u64,
    pub raids: Vec<PlannedRaid>,
    pub can_edit: bool,
    pub available: bool,
    pub raid_days: Vec<String>,
    pub raid_time: Option<String>,
    pub schedule: Vec<ScheduledWeek>,
}

type Reply<T> = mpsc::Receiver<Result<T, String>>;

enum SaveError {
    Conflict(String),
    Failed(String),
}

impl SaveError {
    fn message(self) -> String {
        match self {
            Self::Conflict(message) | Self::Failed(message) => message,
        }
    }
}

#[derive(Default)]
pub struct PlannerUi {
    view: Option<PlanView>,
    view_error: Option<String>,
    view_rx: Option<Reply<PlanView>>,
    last_fetch: Option<Instant>,
    week: Option<String>,
    editing: bool,
    raids: Vec<PlannedRaid>,
    dirty_at: Option<Instant>,
    save_rx: Option<mpsc::Receiver<Result<PlanView, SaveError>>>,
    save_error: Option<String>,
    art: Option<ArtUi>,
    // A raid whose date moved it to another reset: its index and new night.
    pending_move: Option<(usize, Date)>,
    move_rx: Option<Reply<String>>,
    strip_week: Option<String>,
    /// Pages to scroll the reset strip by next frame, and how far it can go.
    strip_nudge: f32,
    strip_room: (bool, bool),
}

/// Everything a raid column reads besides the plan itself.
pub(crate) struct Context<'a> {
    characters: HashMap<&'a str, &'a MemberCharacters>,
    raid_names: HashMap<u64, &'a str>,
    attendance: &'a AttendanceUi,
    pub week: Option<Date>,
    current: bool,
    first_week: Option<Date>,
}

impl<'a> Context<'a> {
    fn new(
        view: &PlanView,
        characters: Option<&'a CharactersView>,
        attendance: &'a AttendanceUi,
    ) -> Self {
        Self {
            characters: characters
                .map(|view| {
                    view.members
                        .iter()
                        .map(|member| (member.user_id.as_str(), member))
                        .collect()
                })
                .unwrap_or_default(),
            raid_names: characters
                .map(|view| {
                    view.raids
                        .iter()
                        .map(|raid| (raid.id, raid.name.as_str()))
                        .collect()
                })
                .unwrap_or_default(),
            attendance,
            week: parse_date(&view.week),
            current: view.week == view.current_week,
            first_week: parse_date(&view.current_week),
        }
    }

    pub(crate) fn character(&self, user_id: &str, key: &str) -> Option<&CharacterRow> {
        self.characters
            .get(user_id)?
            .characters
            .iter()
            .find(|character| character.key == key)
    }

    pub(crate) fn absence(&self, user_id: &str, raid: &PlannedRaid) -> Option<Kind> {
        let night = format_date(raid.night(self.week?));
        first_absence(self.attendance.absences_of(user_id), &night).map(|absence| absence.kind)
    }

    fn absence_text(&self, user_id: &str, raid: &PlannedRaid) -> Option<String> {
        let night = format_date(raid.night(self.week?));
        first_absence(self.attendance.absences_of(user_id), &night).map(|absence| absence.cell())
    }

    /// Saved to one of the raid's instances on its difficulty this reset.
    pub(crate) fn saved(&self, character: &CharacterRow, raid: &PlannedRaid) -> bool {
        self.current
            && character.saved.iter().any(|saved| {
                raid.instances.contains(&saved.id)
                    && !match raid.difficulty {
                        Difficulty::Normal => &saved.normal,
                        Difficulty::Heroic => &saved.heroic,
                        Difficulty::Mythic => &saved.mythic,
                    }
                    .is_empty()
            })
    }

    /// Bosses killed this reset in the raid's instances, per difficulty.
    fn lockouts(&self, character: &CharacterRow, raid: &PlannedRaid) -> Vec<(String, Color32)> {
        if !self.current {
            return Vec::new();
        }
        let saved: Vec<_> = character
            .saved
            .iter()
            .filter(|saved| raid.instances.contains(&saved.id))
            .collect();
        let total: u32 = saved.iter().map(|saved| saved.total).sum();
        Difficulty::ALL
            .iter()
            .filter_map(|difficulty| {
                let kills: usize = saved
                    .iter()
                    .map(|saved| match difficulty {
                        Difficulty::Normal => saved.normal.len(),
                        Difficulty::Heroic => saved.heroic.len(),
                        Difficulty::Mythic => saved.mythic.len(),
                    })
                    .sum();
                (kills > 0).then(|| {
                    (
                        format!("{} {kills}/{total}", &difficulty.label()[..1]),
                        difficulty.color(),
                    )
                })
            })
            .collect()
    }

    fn instances(&self, raid: &PlannedRaid) -> String {
        let names: Vec<&str> = raid
            .instances
            .iter()
            .filter_map(|id| self.raid_names.get(id).copied())
            .map(short_raid)
            .collect();
        if names.is_empty() {
            "No instance".into()
        } else {
            names.join(", ")
        }
    }
}

struct Totals {
    players: usize,
    ilvl: Option<f32>,
    four_set: usize,
    tokens: [usize; 4],
    missing: Vec<&'static str>,
}

impl PlannerUi {
    pub fn refresh(&mut self, ctx: &egui::Context, force: bool) {
        if self.view_rx.is_some() || !presence::configured() {
            return;
        }
        if !force
            && self
                .last_fetch
                .is_some_and(|at| at.elapsed() < Duration::from_secs(REFRESH_SECS))
        {
            return;
        }
        let path = match &self.week {
            Some(week) if valid_week(week) => format!("/v1/plans?week={week}"),
            _ => "/v1/plans".to_owned(),
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access| get(&access, &path, MAX_VIEW_BYTES, "Planner"))
                .and_then(|body| {
                    serde_json::from_slice::<PlanView>(&body)
                        .map_err(|_| "Planner: the server sent an invalid response.".to_string())
                });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.view_rx = Some(rx);
        self.last_fetch = Some(Instant::now());
    }

    fn settled(&self) -> bool {
        self.dirty_at.is_none()
            && self.save_rx.is_none()
            && self.pending_move.is_none()
            && self.move_rx.is_none()
    }

    /// Runs every frame, so an edit saves even after leaving the tab.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.view_rx {
            match rx.try_recv() {
                Ok(Ok(view)) => {
                    // A week change or a reload replaces the working copy;
                    // unsaved edits to the shown week stay.
                    let same_week = self.view.as_ref().is_some_and(|old| old.week == view.week);
                    if self.settled() || !same_week {
                        self.raids = view.raids.clone();
                        self.dirty_at = None;
                    }
                    if !view.can_edit {
                        self.editing = false;
                    }
                    self.view = Some(view);
                    self.view_error = None;
                    self.view_rx = None;
                    self.close_lost_art();
                }
                Ok(Err(error)) => {
                    self.view_error = Some(friendly_problem(&error));
                    self.view_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.view_error = Some("The plan could not be loaded.".into());
                    self.view_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.save_rx {
            match rx.try_recv() {
                Ok(Ok(view)) => {
                    if self.dirty_at.is_none() {
                        self.raids = view.raids.clone();
                    }
                    self.view = Some(view);
                    self.save_error = None;
                    self.save_rx = None;
                }
                Ok(Err(SaveError::Conflict(message))) => {
                    self.save_rx = None;
                    self.dirty_at = None;
                    self.save_error = Some(message);
                    self.refresh(ctx, true);
                }
                Ok(Err(SaveError::Failed(message))) => {
                    self.save_error = Some(friendly_problem(&message));
                    self.save_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.save_error = Some("The plan could not be saved.".into());
                    self.save_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.move_rx {
            match rx.try_recv() {
                Ok(Ok(week)) => {
                    self.move_rx = None;
                    self.week = Some(week);
                    self.refresh(ctx, true);
                }
                Ok(Err(message)) => {
                    self.move_rx = None;
                    self.save_error = Some(friendly_problem(&message));
                    self.refresh(ctx, true);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.move_rx = None;
                    self.save_error = Some("The raid could not be moved.".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.pending_move.is_some() && self.save_rx.is_none() && self.move_rx.is_none() {
            self.start_move(ctx);
        }
        let Some(changed) = self.dirty_at else {
            return;
        };
        let waited = changed.elapsed();
        if waited < SAVE_DELAY || self.save_rx.is_some() {
            ctx.request_repaint_after(
                SAVE_DELAY
                    .saturating_sub(waited)
                    .max(Duration::from_millis(100)),
            );
            return;
        }
        let Some(view) = &self.view else {
            return;
        };
        self.dirty_at = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let week = view.week.clone();
        let revision = view.revision;
        let raids = self.raids.clone();
        crate::guild::spawn(move || {
            let _ = tx.send(save_plan(&week, revision, &raids));
            ctx.request_repaint();
        });
        self.save_rx = Some(rx);
    }

    /// Every reset the planner keeps, with the nights planned in each, coloured
    /// by difficulty. Returns the week clicked.
    fn draw_strip(&mut self, ui: &mut egui::Ui, enabled: bool) -> Option<String> {
        let view = self.view.as_ref()?;
        let shown = view.week.clone();
        let current = view.current_week.clone();
        let schedule = view.schedule.clone();
        let mut clicked = None;
        let follow = self.strip_week.as_deref() != Some(shown.as_str());
        let nudge = std::mem::take(&mut self.strip_nudge);
        let (can_back, can_forward) = self.strip_room;
        let mut step = 0.0;
        let (row, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), STRIP_CHIP.y),
            egui::Sense::hover(),
        );
        let back = egui::Rect::from_min_size(row.min, STRIP_ARROW);
        let forward = egui::Rect::from_min_size(
            row.right_top() - egui::vec2(STRIP_ARROW.x, 0.0),
            STRIP_ARROW,
        );
        let middle = egui::Rect::from_min_max(
            egui::pos2(back.right() + 6.0, row.top()),
            egui::pos2(forward.left() - 6.0, row.bottom()),
        );
        if strip_arrow(ui, back, -1.0, can_back) {
            step = -1.0;
        }
        if strip_arrow(ui, forward, 1.0, can_forward) {
            step = 1.0;
        }
        {
            let output = ui
                .scope_builder(egui::UiBuilder::new().max_rect(middle), |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("planner-resets")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            if nudge != 0.0 {
                                let page =
                                    (ui.clip_rect().width() - STRIP_CHIP.x).max(STRIP_CHIP.x);
                                ui.scroll_with_delta(egui::vec2(-nudge * page, 0.0));
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                for entry in &schedule {
                                    let Some(week) = parse_date(&entry.week) else {
                                        continue;
                                    };
                                    let (rect, response) =
                                        ui.allocate_exact_size(STRIP_CHIP, egui::Sense::click());
                                    let selected = entry.week == shown;
                                    let fill = if selected {
                                        Color32::from_rgb(48, 54, 64)
                                    } else if response.hovered() && enabled {
                                        Color32::from_rgb(44, 49, 58)
                                    } else {
                                        FIELD_FILL
                                    };
                                    ui.painter().rect_filled(
                                        rect,
                                        egui::CornerRadius::same(8),
                                        fill,
                                    );
                                    if selected {
                                        ui.painter().rect_stroke(
                                            rect,
                                            egui::CornerRadius::same(8),
                                            Stroke::new(1.0_f32, info_accent()),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    let past = entry.week < current;
                                    let color = if entry.week == current {
                                        info_accent()
                                    } else if past {
                                        muted_text()
                                    } else {
                                        primary_text()
                                    };
                                    text_at(
                                        ui,
                                        egui::pos2(rect.left() + 10.0, rect.top() + 8.0),
                                        egui::Align2::LEFT_TOP,
                                        &long_date(week),
                                        13.0,
                                        color,
                                    );
                                    let mut raids: Vec<&ScheduledRaid> =
                                        entry.raids.iter().collect();
                                    raids.sort_by_key(|raid| {
                                        NIGHTS.iter().position(|day| *day == raid.day)
                                    });
                                    let job = if raids.is_empty() {
                                        single("–", 11.0, muted_text())
                                    } else {
                                        let mut job = single("", 11.0, muted_text());
                                        for (index, raid) in raids.iter().enumerate() {
                                            let text = if index == 0 {
                                                night_name(&raid.day).to_owned()
                                            } else {
                                                format!(" {}", night_name(&raid.day))
                                            };
                                            let color = if past {
                                                muted_text()
                                            } else {
                                                raid.difficulty.color()
                                            };
                                            append(&mut job, &text, 11.0, color);
                                        }
                                        job
                                    };
                                    let galley = clipped(ui, job, rect.width() - 20.0);
                                    ui.painter().galley(
                                        egui::pos2(rect.left() + 10.0, rect.top() + 27.0),
                                        galley,
                                        muted_text(),
                                    );
                                    if selected && follow {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if enabled && !selected && response.clicked() {
                                        clicked = Some(entry.week.clone());
                                    }
                                }
                            });
                        })
                })
                .inner;
            let offset = output.state.offset.x;
            let end = (output.content_size.x - output.inner_rect.width()).max(0.0);
            self.strip_room = (offset > 0.5, offset < end - 0.5);
        }
        if step != 0.0 {
            self.strip_nudge = step;
            ui.ctx().request_repaint();
        }
        self.strip_week = Some(shown);
        clicked
    }

    /// Adds the raid to its new reset, then saves this reset without it, so
    /// a failure part way leaves the raid in both rather than in neither.
    fn start_move(&mut self, ctx: &egui::Context) {
        let Some((index, day)) = self.pending_move.take() else {
            return;
        };
        let Some(view) = &self.view else {
            return;
        };
        if index >= self.raids.len() {
            return;
        }
        let mut raid = self.raids.remove(index);
        let target = reset_of(day);
        raid.day = NIGHTS[(day - target).whole_days() as usize].to_owned();
        let (week, revision, remaining) = (view.week.clone(), view.revision, self.raids.clone());
        let target = format_date(target);
        self.dirty_at = None;
        self.art = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let path = format!("/v1/plans?week={target}");
            let result = discord_auth::current_access_token()
                .and_then(|access| get(&access, &path, MAX_VIEW_BYTES, "Planner"))
                .and_then(|body| {
                    serde_json::from_slice::<PlanView>(&body)
                        .map_err(|_| "Planner: the server sent an invalid response.".to_string())
                })
                .and_then(|plan| {
                    let mut raids = plan.raids;
                    if raids.len() >= MAX_RAIDS {
                        return Err(format!(
                            "Planner: that reset already has {MAX_RAIDS} raids."
                        ));
                    }
                    raids.push(raid);
                    save_plan(&target, plan.revision, &raids).map_err(SaveError::message)
                })
                .and_then(|_| save_plan(&week, revision, &remaining).map_err(SaveError::message))
                .map(|_| target);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.move_rx = Some(rx);
    }

    fn close_lost_art(&mut self) {
        let editable = self
            .view
            .as_ref()
            .is_some_and(|view| view.can_edit && view.available);
        if self
            .art
            .as_ref()
            .is_some_and(|art| !editable || art.raid >= self.raids.len())
        {
            self.art = None;
        }
    }

    fn change_week(&mut self, ctx: &egui::Context, delta: i64) {
        let Some(week) = self.view.as_ref().and_then(|view| parse_date(&view.week)) else {
            return;
        };
        self.week = Some(format_date(week + time::Duration::days(7 * delta)));
        self.save_error = None;
        self.art = None;
        self.refresh(ctx, true);
    }

    /// The Raids tab once the roster has loaded. Returns true when the user
    /// asked for a refresh.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        roster: &Roster,
        own_id: &str,
        notice: Option<&str>,
        characters: Option<&CharactersView>,
        attendance: &AttendanceUi,
    ) -> bool {
        let mut refresh = false;
        let mut step = 0;
        let can_edit = self
            .view
            .as_ref()
            .is_some_and(|view| view.can_edit && view.available);
        self.close_lost_art();
        if self.art.is_some() {
            self.draw_art(ui, roster, characters, attendance);
            return false;
        }
        let settled = self.settled();
        let has_strip = self
            .view
            .as_ref()
            .is_some_and(|view| !view.schedule.is_empty());
        control_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let enabled = settled && self.view_rx.is_none() && self.view.is_some();
            if has_strip {
                ui.label(
                    RichText::new("Raids")
                        .size(18.0)
                        .strong()
                        .color(primary_text()),
                );
            } else {
                let previous = ui.add_enabled_ui(enabled, square_button).inner;
                arrow(ui, previous.rect.center(), -1.0);
                if previous.clicked() {
                    step = -1;
                }
                let title = self
                    .view
                    .as_ref()
                    .and_then(|view| parse_date(&view.week))
                    .map_or_else(
                        || "Raids".to_owned(),
                        |week| format!("Reset of {}", long_date(week)),
                    );
                ui.label(
                    RichText::new(title)
                        .size(18.0)
                        .strong()
                        .color(primary_text()),
                );
                let next = ui.add_enabled_ui(enabled, square_button).inner;
                arrow(ui, next.rect.center(), 1.0);
                if next.clicked() {
                    step = 1;
                }
                if self
                    .view
                    .as_ref()
                    .is_some_and(|view| view.week == view.current_week)
                {
                    pill(ui, "This reset");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if bar_button(ui, "Refresh").clicked() {
                    refresh = true;
                }
                if can_edit {
                    if bar_button(ui, if self.editing { "Done" } else { "Edit" }).clicked() {
                        self.editing = !self.editing;
                    }
                    if self.editing
                        && ui
                            .add_enabled_ui(self.raids.len() < MAX_RAIDS, |ui| {
                                bar_button(ui, "+ Raid")
                            })
                            .inner
                            .clicked()
                    {
                        let raid = self.new_raid(characters);
                        self.raids.push(raid);
                        self.dirty_at = Some(Instant::now());
                    }
                }
                if self.view_rx.is_some() || self.save_rx.is_some() || self.move_rx.is_some() {
                    busy_indicator(ui, 16.0, info_accent());
                }
            });
        });
        if step != 0 {
            self.change_week(ui.ctx(), step);
        }
        if has_strip {
            ui.add_space(10.0);
            let enabled = settled && self.view_rx.is_none();
            if let Some(week) = self.draw_strip(ui, enabled) {
                self.week = Some(week);
                self.save_error = None;
                self.art = None;
                self.refresh(ui.ctx(), true);
            }
        }
        ui.add_space(10.0);

        let max_height = ui.available_height().max(0.0);
        let mut changed = false;
        let mut open_art = None;
        let mut moved = None;
        egui::ScrollArea::vertical()
            .id_salt("planner")
            .auto_shrink([false, true])
            .max_height(max_height)
            .min_scrolled_height(0.0)
            .show(ui, |ui| {
                let width = (ui.available_width() - 18.0).max(300.0);
                ui.set_width(width);
                for problem in [
                    notice,
                    self.view_error.as_deref(),
                    self.save_error.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    roster_notice(ui, problem);
                    ui.add_space(12.0);
                }
                let Some(view) = self.view.clone() else {
                    if self.view_rx.is_some() {
                        busy_indicator(ui, 20.0, info_accent());
                    }
                    return;
                };
                if !view.available {
                    roster_notice(ui, "The planner is unavailable right now.");
                    return;
                }
                if self.raids.is_empty() {
                    panel_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("No raids planned").color(muted_text()));
                    });
                    return;
                }
                let context = Context::new(&view, characters, attendance);
                let members: Vec<&RosterMember> =
                    roster.officers.iter().chain(&roster.raiders).collect();
                let editing = self.editing && can_edit;
                changed = draw_plan(
                    ui,
                    &mut self.raids,
                    &members,
                    own_id,
                    &context,
                    characters,
                    editing,
                    &mut open_art,
                    &mut moved,
                );
            });
        if let Some(raid) = open_art {
            self.art = Some(ArtUi::new(raid));
        }
        if changed {
            self.dirty_at = Some(Instant::now());
            self.save_error = None;
        }
        if let Some((index, day)) = moved {
            let last = self
                .view
                .as_ref()
                .and_then(|view| parse_date(&view.current_week))
                .map(|week| week + time::Duration::days(7 * WEEKS_AHEAD + 6));
            if last.is_some_and(|last| day > last) {
                self.save_error = Some("That date is too far ahead to plan.".into());
            } else {
                self.pending_move = Some((index, day));
                self.save_error = None;
            }
        }
        refresh
    }

    fn draw_art(
        &mut self,
        ui: &mut egui::Ui,
        roster: &Roster,
        characters: Option<&CharactersView>,
        attendance: &AttendanceUi,
    ) {
        let (Some(view), Some(art)) = (self.view.clone(), self.art.as_mut()) else {
            return;
        };
        let index = art.raid;
        let context = Context::new(&view, characters, attendance);
        let members: Vec<&RosterMember> = roster.officers.iter().chain(&roster.raiders).collect();
        let mut raid = self.raids[index].clone();
        let others: Vec<&PlannedRaid> = self
            .raids
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, raid)| raid)
            .collect();
        let max_height = ui.available_height().max(0.0);
        let mut outcome = art_ui::Outcome::default();
        egui::ScrollArea::vertical()
            .id_salt("send-to-art")
            .auto_shrink([false, true])
            .max_height(max_height)
            .min_scrolled_height(0.0)
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - 18.0).max(300.0));
                if let Some(problem) = self.save_error.as_deref() {
                    roster_notice(ui, problem);
                    ui.add_space(12.0);
                }
                outcome = art_ui::draw(
                    ui,
                    art,
                    &mut raid,
                    Some(index + 1),
                    &others,
                    &members,
                    &context,
                    characters,
                );
            });
        if outcome.changed {
            self.raids[index] = raid;
            self.dirty_at = Some(Instant::now());
            self.save_error = None;
        }
        if outcome.back {
            self.art = None;
        }
    }

    fn new_raid(&self, characters: Option<&CharactersView>) -> PlannedRaid {
        let view = self.view.as_ref();
        let day = view
            .and_then(|view| {
                view.raid_days
                    .iter()
                    .find(|day| NIGHTS.contains(&day.as_str()))
                    .cloned()
            })
            .unwrap_or_else(|| "wed".into());
        let mut instances: Vec<u64> = characters
            .map(|view| view.tier().into_iter().collect())
            .unwrap_or_default();
        instances.sort_unstable();
        PlannedRaid {
            name: None,
            end: None,
            difficulty: Difficulty::Heroic,
            mode: Mode::Splits,
            day,
            start: view.and_then(|view| view.raid_time.clone()),
            instances,
            picks: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    }
}

/// Columns of raids beside a fixed column of names. Returns true when an
/// officer changed anything.
#[allow(clippy::too_many_arguments)]
fn draw_plan(
    ui: &mut egui::Ui,
    raids: &mut Vec<PlannedRaid>,
    members: &[&RosterMember],
    own_id: &str,
    context: &Context,
    characters: Option<&CharactersView>,
    editing: bool,
    open_art: &mut Option<usize>,
    moved: &mut Option<(usize, Date)>,
) -> bool {
    let header = if editing {
        EDIT_HEADER_HEIGHT
    } else {
        HEADER_HEIGHT
    };
    let lines = grouped(members);
    let height_of = |line: &Line| match line {
        Line::Header => header,
        Line::Summary => TOTALS_HEIGHT,
        Line::Group(_) => GROUP_HEIGHT,
        Line::Member(_) => ROW_HEIGHT,
    };
    let height: f32 = lines.iter().map(height_of).sum();
    let totals: Vec<Totals> = raids
        .iter()
        .map(|raid| totals(raid, members, context))
        .collect();
    let mut changed = false;
    let mut remove = None;

    panel_frame()
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (names, _) =
                    ui.allocate_exact_size(egui::vec2(NAME_WIDTH, height), egui::Sense::hover());
                let mut y = names.top();
                for line in &lines {
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(names.left(), y),
                        egui::vec2(NAME_WIDTH, height_of(line)),
                    );
                    match line {
                        Line::Summary => {
                            text_at(
                                ui,
                                egui::pos2(rect.left() + 4.0, rect.top() + 6.0),
                                egui::Align2::LEFT_TOP,
                                "Totals",
                                12.0,
                                secondary_text(),
                            );
                        }
                        _ => draw_name_cell(ui, rect, line, own_id),
                    }
                    y += height_of(line);
                }

                egui::ScrollArea::horizontal()
                    .id_salt("planner-grid")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // Rows run the panel's full width even with one raid.
                        let width = (raids.len() as f32 * (RAID_WIDTH + RAID_GAP))
                            .max(ui.available_width());
                        let (grid, _) =
                            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
                        let mut y = grid.top();
                        for line in &lines {
                            let row = egui::Rect::from_min_size(
                                egui::pos2(grid.left(), y),
                                egui::vec2(width, height_of(line)),
                            );
                            if let Line::Member(member) = line {
                                if member.user_id == own_id {
                                    ui.painter().rect_filled(row, 0.0, OWN_ROW);
                                }
                                ui.painter().hline(
                                    row.x_range(),
                                    row.top(),
                                    Stroke::new(1.0_f32, line_color()),
                                );
                            }
                            for (index, raid) in raids.iter_mut().enumerate() {
                                let cell = egui::Rect::from_min_size(
                                    egui::pos2(
                                        grid.left() + index as f32 * (RAID_WIDTH + RAID_GAP),
                                        y,
                                    ),
                                    egui::vec2(RAID_WIDTH, height_of(line)),
                                );
                                ui.push_id(("raid", index), |ui| match line {
                                    Line::Header if editing => {
                                        let header =
                                            edit_header(ui, cell, index, raid, context, characters);
                                        changed |= header.changed;
                                        if header.removed {
                                            remove = Some(index);
                                        }
                                        if header.send {
                                            *open_art = Some(index);
                                        }
                                        if let Some(day) = header.moved {
                                            *moved = Some((index, day));
                                        }
                                    }
                                    Line::Header => draw_header(ui, cell, index, raid, context),
                                    Line::Summary => draw_totals(ui, cell, &totals[index]),
                                    Line::Group(_) => {}
                                    Line::Member(member) => {
                                        changed |=
                                            draw_pick(ui, cell, raid, member, context, editing);
                                    }
                                });
                            }
                            y += height_of(line);
                        }
                    });
            });
        });
    if let Some(index) = remove {
        raids.remove(index);
        changed = true;
    }
    changed
}

fn totals(raid: &PlannedRaid, members: &[&RosterMember], context: &Context) -> Totals {
    let mut players = 0;
    let mut levels = Vec::new();
    let mut four_set = 0;
    let mut tokens = [0; 4];
    let mut classes = HashSet::new();
    for member in members {
        if context.absence(&member.user_id, raid) == Some(Kind::Out) {
            continue;
        }
        let Some(key) = raid.pick(&member.user_id) else {
            continue;
        };
        players += 1;
        let Some(character) = context.character(&member.user_id, key) else {
            continue;
        };
        if let Some(ilvl) = character.ilvl {
            levels.push(ilvl as f32);
        }
        if character.tier >= 4 {
            four_set += 1;
        }
        if let Some(class) = character.class.as_deref() {
            if let Some(index) = token(class) {
                tokens[index] += 1;
            }
            classes.insert(class.to_owned());
        }
    }
    let missing = BUFFS
        .iter()
        .filter(|(_, by)| !by.iter().any(|class| classes.contains(*class)))
        .map(|(name, _)| *name)
        .collect();
    Totals {
        players,
        ilvl: (!levels.is_empty()).then(|| levels.iter().sum::<f32>() / levels.len() as f32),
        four_set,
        tokens,
        missing,
    }
}

fn draw_header(
    ui: &egui::Ui,
    cell: egui::Rect,
    index: usize,
    raid: &PlannedRaid,
    context: &Context,
) {
    let x = cell.left() + 8.0;
    text_at(
        ui,
        egui::pos2(x, cell.top() + 4.0),
        egui::Align2::LEFT_TOP,
        &raid.title(index),
        14.0,
        primary_text(),
    );
    let mut job = single(raid.difficulty.label(), 12.0, raid.difficulty.color());
    append(
        &mut job,
        &format!(" {}", raid.mode.label()),
        12.0,
        raid.difficulty.color(),
    );
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter()
        .galley(egui::pos2(x, cell.top() + 24.0), galley, primary_text());
    text_at(
        ui,
        egui::pos2(x, cell.top() + 42.0),
        egui::Align2::LEFT_TOP,
        &night_label(raid, context.week),
        12.0,
        secondary_text(),
    );
    let galley = clipped(
        ui,
        single(&context.instances(raid), 11.0, muted_text()),
        cell.width() - 12.0,
    );
    ui.painter()
        .galley(egui::pos2(x, cell.top() + 60.0), galley, muted_text());
}

#[derive(Default)]
struct HeaderEdit {
    changed: bool,
    removed: bool,
    send: bool,
    moved: Option<Date>,
}

/// An officer's header: difficulty, mode, night, start, instances and the way
/// to Send to ART.
fn edit_header(
    ui: &mut egui::Ui,
    cell: egui::Rect,
    index: usize,
    raid: &mut PlannedRaid,
    context: &Context,
    characters: Option<&CharactersView>,
) -> HeaderEdit {
    let mut changed = false;
    let mut removed = false;
    let mut send = false;
    let mut moved = None;
    let half = (cell.width() - 6.0) / 2.0;
    let quarter = (half - 6.0) / 2.0;
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(cell.shrink2(egui::vec2(0.0, 4.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            ui.allocate_ui_with_layout(
                egui::vec2(cell.width(), 22.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let mut name = raid.name.clone().unwrap_or_default();
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .id_salt("name")
                            .frame(egui::Frame::NONE)
                            .char_limit(MAX_NAME)
                            .font(egui::FontId::proportional(14.0))
                            .text_color(primary_text())
                            .hint_text(
                                RichText::new(format!("Raid {}", index + 1))
                                    .color(secondary_text()),
                            )
                            .desired_width(cell.width() - 34.0),
                    );
                    if edit.changed() {
                        raid.name = (!name.trim().is_empty()).then_some(name);
                        changed = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close = ui.add(
                            egui::Button::new("")
                                .corner_radius(egui::CornerRadius::same(6))
                                .fill(FIELD_FILL)
                                .min_size(egui::vec2(22.0, 22.0)),
                        );
                        paint_cross(ui, close.rect.center(), secondary_text());
                        if close.on_hover_text("Remove").clicked() {
                            removed = true;
                        }
                    });
                },
            );
            ui.horizontal(|ui| {
                let labels: Vec<(String, Color32)> = Difficulty::ALL
                    .iter()
                    .map(|d| (d.label().to_owned(), d.color()))
                    .collect();
                if let Some(pick) = choice(
                    ui,
                    "difficulty",
                    half,
                    raid.difficulty.label(),
                    raid.difficulty.color(),
                    &labels,
                ) {
                    raid.difficulty = Difficulty::ALL[pick];
                    changed = true;
                }
                let labels: Vec<(String, Color32)> = Mode::ALL
                    .iter()
                    .map(|m| (m.label().to_owned(), primary_text()))
                    .collect();
                if let Some(pick) =
                    choice(ui, "mode", half, raid.mode.label(), primary_text(), &labels)
                {
                    let mode = Mode::ALL[pick];
                    if mode != Mode::Splits && raid.picks.is_empty() {
                        fill_mains(raid, characters);
                    }
                    raid.mode = mode;
                    changed = true;
                }
            });
            ui.horizontal(|ui| {
                if let Some(week) = context.week {
                    let night = raid.night(week);
                    let button = small_field(ui, half, &long_date(night), primary_text());
                    if let Some(day) =
                        calendar_popup(ui, &button, "night", night, context.first_week)
                    {
                        if reset_of(day) == week {
                            raid.day = NIGHTS[(day - week).whole_days() as usize].to_owned();
                            changed = true;
                        } else {
                            moved = Some(day);
                        }
                    }
                }
                let times = start_times();
                let mut labels = vec![("Start".to_owned(), muted_text())];
                labels.extend(times.iter().map(|time| (time.clone(), primary_text())));
                let shown = raid.start.clone().unwrap_or_else(|| "Start".into());
                let color = if raid.start.is_some() {
                    primary_text()
                } else {
                    muted_text()
                };
                if let Some(pick) = choice(ui, "start", quarter, &shown, color, &labels) {
                    raid.start = (pick > 0).then(|| times[pick - 1].clone());
                    changed = true;
                }
                let ends = end_times();
                let mut labels = vec![("End".to_owned(), muted_text())];
                labels.extend(ends.iter().map(|time| (time.clone(), primary_text())));
                let shown = raid.end.clone().unwrap_or_else(|| "End".into());
                let color = if raid.end.is_some() {
                    primary_text()
                } else {
                    muted_text()
                };
                if let Some(pick) = choice(ui, "end", quarter, &shown, color, &labels) {
                    raid.end = (pick > 0).then(|| ends[pick - 1].clone());
                    changed = true;
                }
            });
            let button = if raid.instances.is_empty() {
                small_field(
                    ui,
                    cell.width(),
                    "Pick instances to see who is saved",
                    warning_accent(),
                )
            } else {
                small_field(ui, cell.width(), &context.instances(raid), secondary_text())
            };
            egui::Popup::menu(&button)
                .width(cell.width())
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_min_width(cell.width() - 12.0);
                    for instance in characters.map_or(&[][..], |view| view.raids.as_slice()) {
                        let mut on = raid.instances.contains(&instance.id);
                        if ui.checkbox(&mut on, short_raid(&instance.name)).changed() {
                            if on {
                                raid.instances.push(instance.id);
                            } else {
                                raid.instances.retain(|id| *id != instance.id);
                            }
                            changed = true;
                        }
                    }
                });
            send = ui
                .add_sized(
                    [cell.width(), SMALL_HEIGHT],
                    egui::Button::new(
                        RichText::new("Send to ART")
                            .size(12.0)
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .corner_radius(egui::CornerRadius::same(6))
                    .fill(SEND),
                )
                .clicked();
        },
    );
    HeaderEdit {
        changed,
        removed,
        send,
        moved,
    }
}

fn draw_totals(ui: &egui::Ui, cell: egui::Rect, totals: &Totals) {
    let x = cell.left() + 8.0;
    let mut job = single(&totals.players.to_string(), 12.0, primary_text());
    append(&mut job, " players · ", 12.0, muted_text());
    append(
        &mut job,
        &totals
            .ilvl
            .map_or_else(|| "–".to_owned(), |ilvl| format!("{ilvl:.1}")),
        12.0,
        primary_text(),
    );
    append(&mut job, " ilvl", 12.0, muted_text());
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter()
        .galley(egui::pos2(x, cell.top() + 6.0), galley, primary_text());

    let mut job = single("4-set ", 12.0, muted_text());
    append(&mut job, &totals.four_set.to_string(), 12.0, primary_text());
    for (label, count) in TOKENS.iter().zip(totals.tokens) {
        append(&mut job, &format!("  {label} "), 12.0, muted_text());
        append(&mut job, &count.to_string(), 12.0, primary_text());
    }
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter()
        .galley(egui::pos2(x, cell.top() + 24.0), galley, primary_text());

    let (text, color) = if totals.missing.is_empty() {
        ("All buffs".to_owned(), success_accent())
    } else {
        (
            format!("Missing {}", totals.missing.join(", ")),
            warning_accent(),
        )
    };
    let galley = clipped(ui, single(&text, 12.0, color), cell.width() - 12.0);
    let rect = egui::Rect::from_min_size(egui::pos2(x, cell.top() + 42.0), galley.size());
    let cut = galley.size().x >= cell.width() - 13.0;
    ui.painter().galley(rect.min, galley, color);
    if cut {
        ui.interact(rect, ui.id().with("buffs"), egui::Sense::hover())
            .on_hover_text(text);
    }
}

/// One member's character for one raid. Returns true when an officer picked.
fn draw_pick(
    ui: &mut egui::Ui,
    cell: egui::Rect,
    raid: &mut PlannedRaid,
    member: &RosterMember,
    context: &Context,
    editing: bool,
) -> bool {
    let absence = context.absence(&member.user_id, raid);
    if absence == Some(Kind::Out) {
        let text = context
            .absence_text(&member.user_id, raid)
            .unwrap_or_else(|| "Out".into());
        let galley = clipped(ui, single(&text, 12.0, OUT_TEXT), cell.width() - 12.0);
        ui.painter().galley(
            egui::pos2(cell.left() + 8.0, cell.center().y - galley.size().y / 2.0),
            galley,
            OUT_TEXT,
        );
        return false;
    }
    let field = egui::Rect::from_center_size(cell.center(), egui::vec2(cell.width(), 26.0));
    let pick = raid
        .pick(&member.user_id)
        .and_then(|key| context.character(&member.user_id, key));
    let saved = pick.is_some_and(|character| context.saved(character, raid));
    let response = if editing {
        let response = ui.interact(
            field,
            ui.id().with(("pick", &member.user_id)),
            egui::Sense::click(),
        );
        let fill = if response.hovered() {
            Color32::from_rgb(46, 51, 60)
        } else {
            FIELD_FILL
        };
        ui.painter()
            .rect_filled(field, egui::CornerRadius::same(6), fill);
        Some(response)
    } else {
        None
    };
    if saved {
        ui.painter().rect_stroke(
            field,
            egui::CornerRadius::same(6),
            Stroke::new(1.0_f32, SAVED),
            egui::StrokeKind::Inside,
        );
    }

    // Right side first, so the name is cut to what is left.
    let mut right = field.right() - 8.0;
    let tag = |ui: &egui::Ui, right: &mut f32, text: &str, size: f32, color: Color32| {
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(single(text, size, color)));
        let pos = egui::pos2(*right - galley.size().x, cap_centred(&galley, size, field));
        *right = pos.x - 6.0;
        ui.painter().galley(pos, galley, color);
    };
    if let Some(ilvl) = pick.and_then(|character| character.ilvl) {
        tag(ui, &mut right, &ilvl.to_string(), 12.0, muted_text());
    }
    if saved {
        tag(ui, &mut right, "Saved", 11.0, SAVED);
    }
    if absence == Some(Kind::Late) {
        tag(ui, &mut right, "Late", 11.0, LATE_TEXT);
    }
    let (name, color) = match (raid.pick(&member.user_id), pick) {
        (_, Some(character)) => (
            character.name.clone(),
            character
                .class
                .as_deref()
                .map_or_else(secondary_text, class_color),
        ),
        (Some(_), None) => ("Unavailable".to_owned(), muted_text()),
        (None, None) => ("–".to_owned(), muted_text()),
    };
    let galley = clipped(
        ui,
        single(&name, 13.0, color),
        (right - field.left() - 10.0).max(20.0),
    );
    let top = cap_centred(&galley, 13.0, field);
    ui.painter()
        .galley(egui::pos2(field.left() + 8.0, top), galley, color);

    let Some(response) = response else {
        return false;
    };

    let mut changed = false;
    let options = context
        .characters
        .get(member.user_id.as_str())
        .map_or(&[][..], |member| member.characters.as_slice());
    egui::Popup::menu(&response)
        .width(cell.width() + PICKER_EXTRA)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(cell.width() + PICKER_EXTRA - 12.0);
            if ui
                .selectable_label(
                    raid.pick(&member.user_id).is_none(),
                    RichText::new("–").color(muted_text()),
                )
                .clicked()
            {
                raid.picks.remove(&member.user_id);
                changed = true;
                ui.close();
            }
            for character in options {
                let color = character
                    .class
                    .as_deref()
                    .map_or_else(secondary_text, class_color);
                let mut job = single(&character.name, 13.0, color);
                if let Some(ilvl) = character.ilvl {
                    append(&mut job, &format!("  {ilvl}"), 12.0, muted_text());
                }
                for (text, color) in context.lockouts(character, raid) {
                    append(&mut job, &format!("  {text}"), 11.0, color);
                }
                let selected = raid.pick(&member.user_id) == Some(character.key.as_str());
                if ui.selectable_label(selected, job).clicked() {
                    if !selected {
                        raid.picks
                            .insert(member.user_id.clone(), Some(character.key.clone()));
                        changed = true;
                    }
                    ui.close();
                }
            }
        });
    changed
}

fn fill_mains(raid: &mut PlannedRaid, characters: Option<&CharactersView>) {
    for member in characters.map_or(&[][..], |view| view.members.as_slice()) {
        if let Some(main) = &member.main {
            raid.picks
                .insert(member.user_id.clone(), Some(main.clone()));
        }
    }
}

/// A dropdown of fixed choices. Returns the picked index.
fn choice(
    ui: &mut egui::Ui,
    id: &str,
    width: f32,
    selected: &str,
    color: Color32,
    options: &[(String, Color32)],
) -> Option<usize> {
    let mut picked = None;
    ui.push_id(id, |ui| {
        let button = small_field(ui, width, selected, color);
        egui::Popup::menu(&button).width(width).show(|ui| {
            ui.set_min_width(width - 12.0);
            for (index, (label, color)) in options.iter().enumerate() {
                if ui
                    .selectable_label(label == selected, RichText::new(label).color(*color))
                    .clicked()
                {
                    picked = Some(index);
                }
            }
        });
    });
    picked.filter(|index| options[*index].0 != selected)
}

fn small_field(ui: &mut egui::Ui, width: f32, text: &str, color: Color32) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, SMALL_HEIGHT), egui::Sense::click());
    let fill = if response.hovered() {
        Color32::from_rgb(46, 51, 60)
    } else {
        FIELD_FILL
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(6), fill);
    let galley = clipped(ui, single(text, 12.0, color), width - 16.0);
    ui.painter().galley(
        egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    response
}

fn square_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(
        egui::Button::new("")
            .corner_radius(egui::CornerRadius::same(8))
            .fill(FIELD_FILL)
            .min_size(egui::vec2(CONTROL_HEIGHT, CONTROL_HEIGHT)),
    )
}

fn start_times() -> Vec<String> {
    (16 * 4..24 * 4)
        .map(|quarter| format!("{:02}:{:02}", quarter / 4, quarter % 4 * 15))
        .collect()
}

/// Start times through the evening, then past midnight until 02:00.
fn end_times() -> Vec<String> {
    (16 * 4..26 * 4 + 1)
        .map(|quarter| format!("{:02}:{:02}", quarter / 4 % 24, quarter % 4 * 15))
        .collect()
}

pub(crate) fn night_name(day: &str) -> &'static str {
    match day {
        "wed" => "Wed",
        "thu" => "Thu",
        "fri" => "Fri",
        "sat" => "Sat",
        "sun" => "Sun",
        "mon" => "Mon",
        "tue" => "Tue",
        _ => "–",
    }
}

/// The top for a single line whose capitals sit in the middle of `rect`, so
/// text of different sizes lines up.
fn cap_centred(galley: &egui::Galley, size: f32, rect: egui::Rect) -> f32 {
    let baseline = galley
        .rows
        .first()
        .and_then(|row| row.glyphs.first().map(|glyph| row.pos.y + glyph.pos.y))
        .unwrap_or(galley.size().y * 0.8);
    rect.center().y + size * 0.35 - baseline
}

/// One of the arrows either side of the reset strip.
fn strip_arrow(ui: &egui::Ui, rect: egui::Rect, direction: f32, active: bool) -> bool {
    let sense = if active {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with(("strip-arrow", direction as i32)), sense);
    let fill = if active && response.hovered() {
        Color32::from_rgb(44, 49, 58)
    } else {
        FIELD_FILL
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), fill);
    let color = if active { primary_text() } else { muted_text() };
    let stroke = Stroke::new(1.6_f32, color);
    let tip = rect.center() + egui::vec2(3.0 * direction, 0.0);
    ui.painter()
        .line_segment([tip, tip + egui::vec2(-5.0 * direction, -5.0)], stroke);
    ui.painter()
        .line_segment([tip, tip + egui::vec2(-5.0 * direction, 5.0)], stroke);
    active && response.clicked()
}

fn night_label(raid: &PlannedRaid, week: Option<Date>) -> String {
    let night = week.map_or_else(
        || night_name(&raid.day).to_owned(),
        |week| long_date(raid.night(week)),
    );
    match (&raid.start, &raid.end) {
        (Some(start), Some(end)) => format!("{night} · {start}–{end}"),
        (Some(start), None) => format!("{night} · {start}"),
        (None, Some(end)) => format!("{night} · until {end}"),
        (None, None) => night,
    }
}

pub(crate) fn long_date(day: Date) -> String {
    format!(
        "{} {} {}",
        weekday_short(day.weekday()),
        day.day(),
        &month_name(day.month())[..3]
    )
}

fn valid_week(week: &str) -> bool {
    parse_date(week).is_some_and(|day| format_date(day) == week)
}

fn save_plan(week: &str, revision: u64, raids: &[PlannedRaid]) -> Result<PlanView, SaveError> {
    if !valid_week(week) {
        return Err(SaveError::Failed("Planner: invalid week.".into()));
    }
    let token = discord_auth::current_or_refreshed_access_token()
        .map_err(SaveError::Failed)?
        .ok_or_else(|| SaveError::Failed("Sign in again to change the plan.".into()))?;
    let response = presence::http_client()
        .map_err(SaveError::Failed)?
        .put(
            token
                .endpoint(&format!("/v1/plans/{week}"))
                .map_err(SaveError::Failed)?,
        )
        .bearer_auth(token.secret())
        .json(&serde_json::json!({ "revision": revision, "raids": raids }))
        .send()
        .map_err(|_| SaveError::Failed("Planner: the server could not be reached.".into()))?;
    let status = response.status();
    let limit = if status.is_success() {
        MAX_VIEW_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let body = download::read_response(response, limit, "Planner").map_err(SaveError::Failed)?;
    if status == reqwest::StatusCode::CONFLICT {
        let message = presence::profile_error("Planner", status, &body);
        let message = message
            .split_once(": ")
            .map_or(message.as_str(), |(_, rest)| rest)
            .to_owned();
        return Err(SaveError::Conflict(message));
    }
    if !status.is_success() {
        return Err(SaveError::Failed(presence::profile_error(
            "Planner", status, &body,
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| SaveError::Failed("Planner: the server sent an invalid response.".into()))
}

fn friendly_problem(error: &str) -> String {
    let message = error.split_once(": ").map_or(error, |(_, rest)| rest);
    if message.contains("HTTP 404") || message == "Not found." {
        "The planner is not available on this server yet.".into()
    } else if message.contains("HTTP") || message.trim().is_empty() {
        "The plan could not be loaded. Brick will try again.".into()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_reads_and_writes_the_server_shape() {
        let view: PlanView = serde_json::from_str(
            r#"{"generatedAt":"2026-11-06T18:00:00.000Z","week":"2026-11-04","currentWeek":"2026-11-04","revision":3,
            "canEdit":true,"available":true,"raidDays":["wed","thu"],"raidTime":"19:30",
            "raids":[{"difficulty":"mythic","mode":"prog","day":"thu","start":"19:30","instances":[1324],
            "picks":{"222":"eu:draenor:lucas","111":null}}]}"#,
        )
        .unwrap();
        let raid = &view.raids[0];
        assert_eq!(
            (raid.difficulty, raid.mode),
            (Difficulty::Mythic, Mode::Prog)
        );
        assert_eq!(raid.pick("222"), Some("eu:draenor:lucas"));
        assert_eq!(raid.pick("111"), None);
        assert_eq!(
            format_date(raid.night(parse_date(&view.week).unwrap())),
            "2026-11-05"
        );
        assert_eq!(
            serde_json::to_value(raid).unwrap(),
            serde_json::json!({"difficulty":"mythic","mode":"prog","day":"thu","start":"19:30",
                "instances":[1324],"picks":{"111":null,"222":"eu:draenor:lucas"}})
        );
    }

    #[test]
    fn a_tuesday_raid_falls_on_the_last_night_of_the_reset() {
        let mut raid = PlannedRaid {
            name: None,
            end: None,
            difficulty: Difficulty::Heroic,
            mode: Mode::Farm,
            day: "tue".into(),
            start: None,
            instances: Vec::new(),
            picks: BTreeMap::new(),
            groups: BTreeMap::new(),
        };
        assert_eq!(
            format_date(raid.night(parse_date("2026-11-04").unwrap())),
            "2026-11-10"
        );
        assert_eq!(night_label(&raid, None), "Tue");
        let week = parse_date("2026-11-04");
        assert_eq!(night_label(&raid, week), "Tue 10 Nov");
        raid.start = Some("19:30".into());
        raid.end = Some("23:00".into());
        assert_eq!(night_label(&raid, week), "Tue 10 Nov · 19:30–23:00");
        assert_eq!(raid.title(1), "Raid 2");
        raid.name = Some("Prog".into());
        assert_eq!(raid.title(1), "Prog");
        for (day, reset) in [
            ("2026-11-04", "2026-11-04"),
            ("2026-11-10", "2026-11-04"),
            ("2026-11-11", "2026-11-11"),
        ] {
            assert_eq!(format_date(reset_of(parse_date(day).unwrap())), reset);
        }
        assert_eq!(long_date(parse_date("2026-11-04").unwrap()), "Wed 4 Nov");
    }

    #[test]
    fn start_times_run_in_quarters_through_the_evening() {
        let times = start_times();
        assert_eq!(times.first().map(String::as_str), Some("16:00"));
        assert_eq!(times.last().map(String::as_str), Some("23:45"));
        assert!(times.contains(&"19:15".to_string()));
        let ends = end_times();
        assert_eq!(ends.last().map(String::as_str), Some("02:00"));
        assert!(ends.contains(&"00:30".to_string()));
        assert!(!valid_week("2026-11-4"));
        assert!(valid_week("2026-11-04"));
    }
}
