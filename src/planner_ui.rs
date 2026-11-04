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
        draw_name_cell, first_absence, format_date, grouped, month_name, paint_cross, parse_date,
        weekday_short, AttendanceUi, Kind, Line, GROUP_HEIGHT, LATE_TEXT, NAME_WIDTH, OUT_TEXT,
        OWN_ROW, ROW_HEIGHT,
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
    pub difficulty: Difficulty,
    pub mode: Mode,
    pub day: String,
    #[serde(default)]
    pub start: Option<String>,
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
}

type Reply<T> = mpsc::Receiver<Result<T, String>>;

enum SaveError {
    Conflict(String),
    Failed(String),
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
}

/// Everything a raid column reads besides the plan itself.
pub(crate) struct Context<'a> {
    characters: HashMap<&'a str, &'a MemberCharacters>,
    raid_names: HashMap<u64, &'a str>,
    attendance: &'a AttendanceUi,
    pub week: Option<Date>,
    current: bool,
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
        self.dirty_at.is_none() && self.save_rx.is_none()
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
        control_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let enabled = settled && self.view_rx.is_none() && self.view.is_some();
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
                if self.view_rx.is_some() || self.save_rx.is_some() {
                    busy_indicator(ui, 16.0, info_accent());
                }
            });
        });
        if step != 0 {
            self.change_week(ui.ctx(), step);
        }
        ui.add_space(10.0);

        let max_height = ui.available_height().max(0.0);
        let mut changed = false;
        let mut open_art = None;
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
                );
            });
        if let Some(raid) = open_art {
            self.art = Some(ArtUi::new(raid));
        }
        if changed {
            self.dirty_at = Some(Instant::now());
            self.save_error = None;
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
        &format!("Raid {}", index + 1),
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
        &night_label(raid),
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
    let half = (cell.width() - 6.0) / 2.0;
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
                    ui.label(
                        RichText::new(format!("Raid {}", index + 1))
                            .size(14.0)
                            .color(primary_text()),
                    );
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
                let labels: Vec<(String, Color32)> = NIGHTS
                    .iter()
                    .map(|day| (night_name(day).to_owned(), primary_text()))
                    .collect();
                if let Some(pick) = choice(
                    ui,
                    "day",
                    half,
                    night_name(&raid.day),
                    primary_text(),
                    &labels,
                ) {
                    raid.day = NIGHTS[pick].to_owned();
                    changed = true;
                }
                let times = start_times();
                let mut labels = vec![("No time".to_owned(), muted_text())];
                labels.extend(times.iter().map(|time| (time.clone(), primary_text())));
                let shown = raid.start.clone().unwrap_or_else(|| "No time".into());
                if let Some(pick) = choice(ui, "start", half, &shown, primary_text(), &labels) {
                    raid.start = (pick > 0).then(|| times[pick - 1].clone());
                    changed = true;
                }
            });
            let button = small_field(
                ui,
                cell.width(),
                &context.instances(raid),
                if raid.instances.is_empty() {
                    muted_text()
                } else {
                    secondary_text()
                },
            );
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
        let rect = text_at(
            ui,
            egui::pos2(*right, field.center().y),
            egui::Align2::RIGHT_CENTER,
            text,
            size,
            color,
        );
        *right = rect.left() - 6.0;
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
    ui.painter().galley(
        egui::pos2(field.left() + 8.0, field.center().y - galley.size().y / 2.0),
        galley,
        color,
    );

    let Some(response) = response else {
        return false;
    };

    let mut changed = false;
    let options = context
        .characters
        .get(member.user_id.as_str())
        .map_or(&[][..], |member| member.characters.as_slice());
    egui::Popup::menu(&response)
        .width(cell.width())
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(cell.width() - 12.0);
            if ui
                .selectable_label(
                    raid.pick(&member.user_id).is_none(),
                    RichText::new("–").color(muted_text()),
                )
                .clicked()
            {
                raid.picks.remove(&member.user_id);
                changed = true;
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
                if context.saved(character, raid) {
                    append(&mut job, "  Saved", 11.0, SAVED);
                }
                let selected = raid.pick(&member.user_id) == Some(character.key.as_str());
                if ui.selectable_label(selected, job).clicked() && !selected {
                    raid.picks
                        .insert(member.user_id.clone(), Some(character.key.clone()));
                    changed = true;
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

fn night_label(raid: &PlannedRaid) -> String {
    match &raid.start {
        Some(start) => format!("{} {start}", night_name(&raid.day)),
        None => night_name(&raid.day).to_owned(),
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
        let raid = PlannedRaid {
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
        assert_eq!(night_label(&raid), "Tue");
        assert_eq!(long_date(parse_date("2026-11-04").unwrap()), "Wed 4 Nov");
    }

    #[test]
    fn start_times_run_in_quarters_through_the_evening() {
        let times = start_times();
        assert_eq!(times.first().map(String::as_str), Some("16:00"));
        assert_eq!(times.last().map(String::as_str), Some("23:45"));
        assert!(times.contains(&"19:15".to_string()));
        assert!(!valid_week("2026-11-4"));
        assert!(valid_week("2026-11-04"));
    }
}
