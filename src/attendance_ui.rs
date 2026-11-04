use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText, Stroke};
use serde::{Deserialize, Serialize};
use time::{Date, Month, Weekday};

use crate::{
    discord_auth,
    guild_ui::{
        append, bar_button, clipped, control_row, get, line_color, single, text_at, CONTROL_HEIGHT,
    },
    presence::{self, Roster, RosterMember},
    profile::RaidRole,
    ui::{
        busy_indicator, info_accent, muted_text, panel_frame, primary_text, roster_notice,
        secondary_text,
    },
};

const MAX_VIEW_BYTES: u64 = 1024 * 1024;
const REFRESH_SECS: u64 = 60;
// The grid starts at this week's Wednesday reset and runs twelve weeks.
const WEEKS: i64 = 12;
const SAVE_DELAY: Duration = Duration::from_millis(1200);
const MAX_ABSENCES: usize = 40;
const MAX_NOTE: usize = 60;

pub(crate) const NAME_WIDTH: f32 = 176.0;
const CELL_WIDTH: f32 = 108.0;
const CELL_GAP: f32 = 6.0;
const HEADER_HEIGHT: f32 = 54.0;
const SUMMARY_HEIGHT: f32 = 30.0;
pub(crate) const GROUP_HEIGHT: f32 = 30.0;
pub(crate) const ROW_HEIGHT: f32 = 34.0;
const KIND_WIDTH: f32 = 84.0;
const DATE_WIDTH: f32 = 128.0;

pub(crate) const OUT_FILL: Color32 = Color32::from_rgb(29, 48, 62);
pub(crate) const OUT_TEXT: Color32 = Color32::from_rgb(124, 186, 236);
pub(crate) const LATE_FILL: Color32 = Color32::from_rgb(58, 46, 26);
pub(crate) const LATE_TEXT: Color32 = Color32::from_rgb(236, 178, 80);
pub(crate) const OWN_ROW: Color32 = Color32::from_rgb(33, 37, 44);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Out,
    Late,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Out => "Out",
            Self::Late => "Late",
        }
    }

    fn fill(self) -> Color32 {
        match self {
            Self::Out => OUT_FILL,
            Self::Late => LATE_FILL,
        }
    }

    fn text(self) -> Color32 {
        match self {
            Self::Out => OUT_TEXT,
            Self::Late => LATE_TEXT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Absence {
    pub kind: Kind,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub note: String,
}

impl Absence {
    fn covers(&self, day: &str) -> bool {
        self.from.as_str() <= day && day <= self.to.as_str()
    }

    pub(crate) fn cell(&self) -> String {
        let note = self.note.trim();
        if note.is_empty() {
            self.kind.label().to_owned()
        } else {
            format!("{} · {note}", self.kind.label())
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttendanceView {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub raid_days: Vec<String>,
    #[serde(default)]
    pub raid_time: Option<String>,
    #[serde(default)]
    pub members: Vec<MemberAbsences>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberAbsences {
    pub user_id: String,
    #[serde(default)]
    pub absences: Vec<Absence>,
}

impl AttendanceView {
    fn absences(&self, user_id: &str) -> &[Absence] {
        self.members
            .iter()
            .find(|member| member.user_id == user_id)
            .map_or(&[][..], |member| member.absences.as_slice())
    }
}

type Reply<T> = mpsc::Receiver<Result<T, String>>;

#[derive(Default)]
pub struct AttendanceUi {
    view: Option<AttendanceView>,
    view_error: Option<String>,
    view_rx: Option<Reply<AttendanceView>>,
    last_fetch: Option<Instant>,
    mine: Vec<Absence>,
    seeded: bool,
    own_id: String,
    dirty_at: Option<Instant>,
    save_rx: Option<Reply<Vec<Absence>>>,
    save_error: Option<String>,
}

impl AttendanceUi {
    /// One member's out and late dates; your own come from your edits.
    pub(crate) fn absences_of(&self, user_id: &str) -> &[Absence] {
        if user_id == self.own_id {
            &self.mine
        } else {
            self.view
                .as_ref()
                .map_or(&[][..], |view| view.absences(user_id))
        }
    }

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
        // Your own list reloads only when nothing of yours is waiting to save.
        if force && self.dirty_at.is_none() && self.save_rx.is_none() {
            self.seeded = false;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| fetch_absences(&access_token));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.view_rx = Some(rx);
        self.last_fetch = Some(Instant::now());
    }

    /// Runs every frame, so a change saves even after leaving the tab.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.view_rx {
            match rx.try_recv() {
                Ok(Ok(view)) => {
                    self.view = Some(view);
                    self.view_error = None;
                    self.view_rx = None;
                }
                Ok(Err(error)) => {
                    self.view_error = Some(friendly_problem(&error));
                    self.view_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.view_error = Some("Attendance could not be loaded.".into());
                    self.view_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.save_rx {
            match rx.try_recv() {
                Ok(Ok(saved)) => {
                    if let Some(view) = self.view.as_mut() {
                        view.members.retain(|member| member.user_id != self.own_id);
                        if !saved.is_empty() {
                            view.members.push(MemberAbsences {
                                user_id: self.own_id.clone(),
                                absences: saved,
                            });
                        }
                    }
                    self.save_error = None;
                    self.save_rx = None;
                }
                Ok(Err(error)) => {
                    self.save_error = Some(friendly_problem(&error));
                    self.save_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.save_error = Some("Your absences could not be saved.".into());
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
        self.dirty_at = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let own_id = self.own_id.clone();
        let absences = self.mine.clone();
        crate::guild::spawn(move || {
            let _ = tx.send(save_absences(&own_id, &absences));
            ctx.request_repaint();
        });
        self.save_rx = Some(rx);
    }

    /// The Attendance tab once the roster has loaded. Returns true when the
    /// user asked for a refresh.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        roster: &Roster,
        own_id: &str,
        notice: Option<&str>,
    ) -> bool {
        if self.own_id != own_id {
            self.own_id = own_id.to_owned();
            self.seeded = false;
        }
        if !self.seeded && self.dirty_at.is_none() && self.save_rx.is_none() {
            if let Some(view) = &self.view {
                self.mine = view.absences(own_id).to_vec();
                self.seeded = true;
            }
        }

        let mut refresh = false;
        control_row(ui, |ui| {
            ui.label(
                RichText::new("Attendance")
                    .size(18.0)
                    .strong()
                    .color(primary_text()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if bar_button(ui, "Refresh").clicked() {
                    refresh = true;
                }
                if self.view_rx.is_some() || self.save_rx.is_some() {
                    busy_indicator(ui, 16.0, info_accent());
                }
            });
        });
        ui.add_space(10.0);

        let today = today();
        let max_height = ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("attendance")
            .auto_shrink([false, true])
            .max_height(max_height)
            .min_scrolled_height(0.0)
            .show(ui, |ui| {
                let width = (ui.available_width() - 18.0).max(300.0);
                ui.set_width(width);
                if let Some(notice) = notice {
                    roster_notice(ui, notice);
                    ui.add_space(12.0);
                }
                if let Some(error) = self.view_error.as_deref() {
                    roster_notice(ui, error);
                    ui.add_space(12.0);
                }
                let Some(view) = self.view.clone() else {
                    if self.view_rx.is_some() {
                        busy_indicator(ui, 20.0, info_accent());
                    }
                    return;
                };
                if !view.available {
                    roster_notice(ui, "Attendance is unavailable right now.");
                    return;
                }
                if self.draw_mine(ui, today) {
                    self.dirty_at = Some(Instant::now());
                    self.save_error = None;
                }
                if let Some(error) = self.save_error.as_deref() {
                    ui.add_space(8.0);
                    roster_notice(ui, error);
                }
                ui.add_space(14.0);
                let members: Vec<&RosterMember> =
                    roster.officers.iter().chain(&roster.raiders).collect();
                draw_grid(ui, &view, &members, own_id, &self.mine, today);
            });
        refresh
    }

    /// Your own out and late dates. Returns true when anything changed.
    fn draw_mine(&mut self, ui: &mut egui::Ui, today: Date) -> bool {
        let mut changed = false;
        let mut remove = None;
        panel_frame()
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), CONTROL_HEIGHT),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new("Your absences")
                                .size(15.0)
                                .strong()
                                .color(primary_text()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let full = self.mine.len() >= MAX_ABSENCES;
                            if ui
                                .add_enabled_ui(!full, |ui| bar_button(ui, "+ Add"))
                                .inner
                                .clicked()
                            {
                                let day = format_date(today);
                                self.mine.push(Absence {
                                    kind: Kind::Out,
                                    from: day.clone(),
                                    to: day,
                                    note: String::new(),
                                });
                                changed = true;
                            }
                        });
                    },
                );
                if self.mine.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("None").color(muted_text()));
                    return;
                }
                for (index, entry) in self.mine.iter_mut().enumerate() {
                    ui.add_space(8.0);
                    ui.push_id(index, |ui| {
                        let (edited, removed) = absence_row(ui, entry, today);
                        changed |= edited;
                        if removed {
                            remove = Some(index);
                        }
                    });
                }
            });
        if let Some(index) = remove {
            self.mine.remove(index);
            changed = true;
        }
        changed
    }
}

/// One editable absence: kind, from, to, note and remove. Returns (changed, removed).
fn absence_row(ui: &mut egui::Ui, entry: &mut Absence, today: Date) -> (bool, bool) {
    let mut changed = false;
    let mut removed = false;
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(width, CONTROL_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let kind = field_button(ui, entry.kind.label(), KIND_WIDTH, entry.kind.text());
            egui::Popup::menu(&kind).width(KIND_WIDTH).show(|ui| {
                for option in [Kind::Out, Kind::Late] {
                    if ui
                        .selectable_label(
                            entry.kind == option,
                            RichText::new(option.label()).color(option.text()),
                        )
                        .clicked()
                        && entry.kind != option
                    {
                        entry.kind = option;
                        changed = true;
                    }
                }
            });

            let from = parse_date(&entry.from).unwrap_or(today);
            if let Some(day) = date_field(ui, "from", from, None) {
                entry.from = format_date(day);
                if entry.to < entry.from {
                    entry.to = entry.from.clone();
                }
                changed = true;
            }
            ui.label(RichText::new("–").color(muted_text()));
            let to = parse_date(&entry.to).unwrap_or(from);
            if let Some(day) = date_field(ui, "to", to, Some(from)) {
                entry.to = format_date(day);
                changed = true;
            }

            let hint = match entry.kind {
                Kind::Out => "Note",
                Kind::Late => "From 19:45",
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let close = ui.add(
                    egui::Button::new("")
                        .corner_radius(egui::CornerRadius::same(8))
                        .fill(Color32::from_rgb(38, 42, 50))
                        .min_size(egui::vec2(CONTROL_HEIGHT, CONTROL_HEIGHT)),
                );
                paint_cross(ui, close.rect.center(), secondary_text());
                if close.on_hover_text("Remove").clicked() {
                    removed = true;
                }
                // The note draws its own fill so it matches the buttons beside it.
                let (field, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width().max(80.0), CONTROL_HEIGHT),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    field,
                    egui::CornerRadius::same(8),
                    Color32::from_rgb(38, 42, 50),
                );
                let inner = field.shrink2(egui::vec2(12.0, 0.0));
                let note = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut entry.note)
                                .char_limit(MAX_NOTE)
                                .hint_text(RichText::new(hint).color(muted_text()))
                                .text_color(primary_text())
                                .frame(egui::Frame::NONE)
                                .vertical_align(egui::Align::Center)
                                .margin(egui::Margin::ZERO)
                                .min_size(egui::vec2(inner.width(), CONTROL_HEIGHT))
                                .desired_width(inner.width()),
                        )
                    })
                    .inner;
                if note.changed() {
                    changed = true;
                }
            });
        },
    );
    (changed, removed)
}

pub(crate) fn field_button(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    color: Color32,
) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).strong().color(color))
            .corner_radius(egui::CornerRadius::same(8))
            .fill(Color32::from_rgb(38, 42, 50))
            .min_size(egui::vec2(width, CONTROL_HEIGHT)),
    )
}

pub(crate) fn paint_cross(ui: &egui::Ui, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    let r = 4.5;
    ui.painter().line_segment(
        [center - egui::vec2(r, r), center + egui::vec2(r, r)],
        stroke,
    );
    ui.painter().line_segment(
        [center + egui::vec2(-r, r), center + egui::vec2(r, -r)],
        stroke,
    );
}

/// A date button with a month calendar. Returns the picked day.
fn date_field(ui: &mut egui::Ui, id: &str, value: Date, min: Option<Date>) -> Option<Date> {
    let button = field_button(ui, &short_date(value), DATE_WIDTH, primary_text());
    let month_id = ui.id().with(id).with("month");
    if button.clicked() {
        ui.data_mut(|data| data.insert_temp(month_id, first_of_month(value)));
    }
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&button)
        .id(ui.id().with(id).with("calendar"))
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            let shown = ui
                .data(|data| data.get_temp::<Date>(month_id))
                .unwrap_or_else(|| first_of_month(value));
            if let Some(month) = calendar(ui, shown, value, min) {
                match month {
                    Pick::Month(month) => {
                        ui.data_mut(|data| data.insert_temp(month_id, month));
                    }
                    Pick::Day(day) => {
                        picked = Some(day);
                        ui.close();
                    }
                }
            }
        });
    picked
}

enum Pick {
    Month(Date),
    Day(Date),
}

const DAY_SIZE: egui::Vec2 = egui::vec2(32.0, 28.0);

fn calendar(ui: &mut egui::Ui, month: Date, selected: Date, min: Option<Date>) -> Option<Pick> {
    let mut pick = None;
    let width = DAY_SIZE.x * 7.0;
    ui.set_width(width);
    ui.spacing_mut().item_spacing = egui::vec2(0.0, 2.0);
    let (bar, _) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::hover());
    let previous = ui.put(
        egui::Rect::from_min_size(bar.min, egui::vec2(30.0, 28.0)),
        egui::Button::new("").frame(false),
    );
    crate::guild_ui::arrow(ui, previous.rect.center(), -1.0);
    let next = ui.put(
        egui::Rect::from_min_size(
            bar.right_top() - egui::vec2(30.0, 0.0),
            egui::vec2(30.0, 28.0),
        ),
        egui::Button::new("").frame(false),
    );
    crate::guild_ui::arrow(ui, next.rect.center(), 1.0);
    text_at(
        ui,
        bar.center(),
        egui::Align2::CENTER_CENTER,
        &format!("{} {}", month_name(month.month()), month.year()),
        13.0,
        primary_text(),
    );
    if previous.clicked() {
        pick = Some(Pick::Month(add_months(month, -1)));
    }
    if next.clicked() {
        pick = Some(Pick::Month(add_months(month, 1)));
    }

    let (labels, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    for (index, label) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"]
        .iter()
        .enumerate()
    {
        text_at(
            ui,
            egui::pos2(
                labels.left() + DAY_SIZE.x * (index as f32 + 0.5),
                labels.center().y,
            ),
            egui::Align2::CENTER_CENTER,
            label,
            11.0,
            muted_text(),
        );
    }

    let today = today();
    let lead = month.weekday().number_days_from_monday() as i64;
    let start = month - time::Duration::days(lead);
    for week in 0..6 {
        let (row, _) = ui.allocate_exact_size(egui::vec2(width, DAY_SIZE.y), egui::Sense::hover());
        for weekday in 0..7 {
            let day = start + time::Duration::days(week * 7 + weekday);
            if day.month() != month.month() {
                continue;
            }
            let rect = egui::Rect::from_min_size(
                row.min + egui::vec2(DAY_SIZE.x * weekday as f32, 0.0),
                DAY_SIZE,
            )
            .shrink2(egui::vec2(2.0, 1.0));
            let enabled = min.is_none_or(|min| day >= min);
            let response = ui.interact(
                rect,
                ui.id().with(("day", day.to_julian_day())),
                if enabled {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                },
            );
            let painter = ui.painter();
            if day == selected {
                painter.rect_filled(rect, egui::CornerRadius::same(6), info_accent());
            } else if response.hovered() && enabled {
                painter.rect_filled(rect, egui::CornerRadius::same(6), line_color());
            }
            if day == today && day != selected {
                painter.rect_stroke(
                    rect,
                    egui::CornerRadius::same(6),
                    Stroke::new(1.0_f32, info_accent()),
                    egui::StrokeKind::Inside,
                );
            }
            let color = if day == selected {
                Color32::WHITE
            } else if enabled {
                primary_text()
            } else {
                muted_text()
            };
            text_at(
                ui,
                rect.center(),
                egui::Align2::CENTER_CENTER,
                &day.day().to_string(),
                12.0,
                color,
            );
            if response.clicked() {
                pick = Some(Pick::Day(day));
            }
        }
    }
    pick
}

pub(crate) enum Line<'a> {
    Header,
    Summary,
    Group(&'static str),
    Member(&'a RosterMember),
}

impl Line<'_> {
    fn height(&self) -> f32 {
        match self {
            Self::Header => HEADER_HEIGHT,
            Self::Summary => SUMMARY_HEIGHT,
            Self::Group(_) => GROUP_HEIGHT,
            Self::Member(_) => ROW_HEIGHT,
        }
    }
}

pub(crate) fn grouped<'a>(members: &[&'a RosterMember]) -> Vec<Line<'a>> {
    let mut lines = vec![Line::Header, Line::Summary];
    for (label, role) in [
        ("TANKS", Some(RaidRole::Tank)),
        ("HEALERS", Some(RaidRole::Healer)),
        ("DPS", Some(RaidRole::Dps)),
        ("NO ROLE", None),
    ] {
        let group: Vec<&RosterMember> = members
            .iter()
            .copied()
            .filter(|member| member.raid_role == role)
            .collect();
        if group.is_empty() {
            continue;
        }
        lines.push(Line::Group(label));
        lines.extend(group.into_iter().map(Line::Member));
    }
    lines
}

/// Raid nights from this week's reset, twelve weeks out.
fn raid_nights(raid_days: &[String], today: Date) -> Vec<Date> {
    let since_reset = (today.weekday().number_days_from_monday() + 7
        - Weekday::Wednesday.number_days_from_monday())
        % 7;
    let start = today - time::Duration::days(since_reset as i64);
    (0..WEEKS * 7)
        .map(|offset| start + time::Duration::days(offset))
        .filter(|day| {
            raid_days
                .iter()
                .any(|name| *name == weekday_key(day.weekday()))
        })
        .collect()
}

pub(crate) fn first_absence<'a>(absences: &'a [Absence], day: &str) -> Option<&'a Absence> {
    absences
        .iter()
        .filter(|absence| absence.covers(day))
        .min_by_key(|absence| absence.kind == Kind::Late)
}

fn draw_grid(
    ui: &mut egui::Ui,
    view: &AttendanceView,
    members: &[&RosterMember],
    own_id: &str,
    mine: &[Absence],
    today: Date,
) {
    let nights = raid_nights(&view.raid_days, today);
    let keys: Vec<String> = nights.iter().copied().map(format_date).collect();
    let lines = grouped(members);
    let absences_of = |member: &RosterMember| -> &[Absence] {
        if member.user_id == own_id {
            mine
        } else {
            view.absences(&member.user_id)
        }
    };
    let totals: Vec<(usize, usize)> = keys
        .iter()
        .map(|key| {
            members.iter().fold((0, 0), |(out, late), member| {
                match first_absence(absences_of(member), key).map(|absence| absence.kind) {
                    Some(Kind::Out) => (out + 1, late),
                    Some(Kind::Late) => (out, late + 1),
                    None => (out, late),
                }
            })
        })
        .collect();
    let height: f32 = lines.iter().map(Line::height).sum();

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
                        egui::vec2(NAME_WIDTH, line.height()),
                    );
                    draw_name_cell(ui, rect, line, own_id);
                    y += line.height();
                }

                egui::ScrollArea::horizontal()
                    .id_salt("attendance-grid")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let width = nights.len() as f32 * (CELL_WIDTH + CELL_GAP);
                        let (grid, _) =
                            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
                        let mut y = grid.top();
                        for line in &lines {
                            let row = egui::Rect::from_min_size(
                                egui::pos2(grid.left(), y),
                                egui::vec2(width, line.height()),
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
                            let mut previous_month = None;
                            for (index, night) in nights.iter().enumerate() {
                                let cell = egui::Rect::from_min_size(
                                    egui::pos2(
                                        grid.left() + index as f32 * (CELL_WIDTH + CELL_GAP),
                                        y,
                                    ),
                                    egui::vec2(CELL_WIDTH, line.height()),
                                );
                                match line {
                                    Line::Header => {
                                        let first = previous_month != Some(night.month());
                                        previous_month = Some(night.month());
                                        draw_night_header(
                                            ui,
                                            cell,
                                            *night,
                                            first,
                                            view.raid_time.as_deref(),
                                            today,
                                        );
                                    }
                                    Line::Summary => draw_total(ui, cell, totals[index]),
                                    Line::Group(_) => {}
                                    Line::Member(member) => {
                                        if let Some(absence) =
                                            first_absence(absences_of(member), &keys[index])
                                        {
                                            draw_absence_cell(ui, cell, absence, &member.user_id);
                                        }
                                    }
                                }
                            }
                            y += line.height();
                        }
                    });
            });
        });
}

pub(crate) fn draw_name_cell(ui: &egui::Ui, rect: egui::Rect, line: &Line, own_id: &str) {
    let y = rect.center().y;
    match line {
        Line::Header => {}
        Line::Summary => {
            let mut job = single("Out", 12.0, OUT_TEXT);
            append(&mut job, " · ", 12.0, muted_text());
            append(&mut job, "late", 12.0, LATE_TEXT);
            let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
            ui.painter().galley(
                egui::pos2(rect.left() + 4.0, y - galley.size().y / 2.0),
                galley,
                secondary_text(),
            );
        }
        Line::Group(label) => {
            text_at(
                ui,
                egui::pos2(rect.left() + 4.0, rect.bottom() - 8.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                11.0,
                Color32::from_rgb(164, 176, 193),
            );
        }
        Line::Member(member) => {
            if member.user_id == own_id {
                ui.painter().rect_filled(rect, 0.0, OWN_ROW);
            }
            ui.painter().hline(
                rect.x_range(),
                rect.top(),
                Stroke::new(1.0_f32, line_color()),
            );
            let hue = member
                .name
                .chars()
                .fold(7u32, |hash, ch| (hash * 31 + ch as u32) % 360);
            let avatar =
                Color32::from(egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.55, 0.62, 1.0));
            let center = egui::pos2(rect.left() + 16.0, y);
            ui.painter().circle_filled(center, 12.0, avatar);
            let initials: String = member
                .name
                .chars()
                .take(2)
                .collect::<String>()
                .to_uppercase();
            text_at(
                ui,
                center,
                egui::Align2::CENTER_CENTER,
                &initials,
                10.0,
                Color32::WHITE,
            );
            let galley = clipped(
                ui,
                single(&member.name, 13.0, primary_text()),
                NAME_WIDTH - 46.0,
            );
            ui.painter().galley(
                egui::pos2(rect.left() + 36.0, y - galley.size().y / 2.0),
                galley,
                primary_text(),
            );
        }
    }
}

fn draw_night_header(
    ui: &egui::Ui,
    cell: egui::Rect,
    night: Date,
    first_of_month: bool,
    raid_time: Option<&str>,
    today: Date,
) {
    let x = cell.left() + 7.0;
    if first_of_month {
        text_at(
            ui,
            egui::pos2(x, cell.top() + 4.0),
            egui::Align2::LEFT_TOP,
            &month_name(night.month())[..3],
            11.0,
            muted_text(),
        );
    }
    let color = if night == today {
        info_accent()
    } else if night < today {
        muted_text()
    } else {
        primary_text()
    };
    text_at(
        ui,
        egui::pos2(x, cell.top() + 19.0),
        egui::Align2::LEFT_TOP,
        &format!("{} {}", weekday_short(night.weekday()), night.day()),
        13.0,
        color,
    );
    if let Some(time) = raid_time {
        text_at(
            ui,
            egui::pos2(x, cell.top() + 37.0),
            egui::Align2::LEFT_TOP,
            time,
            11.0,
            muted_text(),
        );
    }
}

fn draw_total(ui: &egui::Ui, cell: egui::Rect, (out, late): (usize, usize)) {
    let job = if out + late == 0 {
        single("–", 12.0, muted_text())
    } else {
        let mut job = single(&out.to_string(), 12.0, OUT_TEXT);
        append(&mut job, " · ", 12.0, muted_text());
        append(&mut job, &late.to_string(), 12.0, LATE_TEXT);
        job
    };
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter().galley(
        egui::pos2(cell.left() + 7.0, cell.center().y - galley.size().y / 2.0),
        galley,
        secondary_text(),
    );
}

fn draw_absence_cell(ui: &egui::Ui, cell: egui::Rect, absence: &Absence, user_id: &str) {
    let rect = egui::Rect::from_center_size(cell.center(), egui::vec2(cell.width(), 26.0));
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(4), absence.kind.fill());
    let text = absence.cell();
    let galley = clipped(
        ui,
        single(&text, 12.0, absence.kind.text()),
        rect.width() - 14.0,
    );
    let cut = galley.size().x >= rect.width() - 15.0;
    ui.painter().galley(
        egui::pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0),
        galley,
        absence.kind.text(),
    );
    if cut {
        let id = ui.id().with(("absence", user_id, cell.left() as i32));
        ui.interact(rect, id, egui::Sense::hover())
            .on_hover_text(text);
    }
}

fn fetch_absences(access: &crate::guild::Access) -> Result<AttendanceView, String> {
    let body = get(access, "/v1/absences", MAX_VIEW_BYTES, "Attendance")?;
    serde_json::from_slice(&body)
        .map_err(|_| "Attendance: the server sent an invalid response.".to_string())
}

fn save_absences(own_id: &str, absences: &[Absence]) -> Result<Vec<Absence>, String> {
    #[derive(Deserialize)]
    struct Saved {
        absences: Vec<Absence>,
    }
    let token = discord_auth::current_or_refreshed_access_token()?
        .ok_or_else(|| "Sign in again to save your absences.".to_string())?;
    let (status, body) = presence::profile_call(
        reqwest::Method::PUT,
        "/v1/absences/me",
        &token,
        own_id,
        Some(&serde_json::json!({ "absences": absences })),
    )?;
    if !status.is_success() {
        return Err(presence::profile_error("Absences", status, &body));
    }
    serde_json::from_slice::<Saved>(&body)
        .map(|saved| saved.absences)
        .map_err(|_| "Absences: the server sent an invalid response.".to_string())
}

fn friendly_problem(error: &str) -> String {
    let message = error.split_once(": ").map_or(error, |(_, rest)| rest);
    if message.contains("HTTP 404") || message == "Not found." {
        "Attendance is not available on this server yet.".into()
    } else if message.contains("HTTP") || message.trim().is_empty() {
        "Attendance could not be loaded. Brick will try again.".into()
    } else {
        message.to_owned()
    }
}

pub(crate) fn today() -> Date {
    time::OffsetDateTime::now_utc().date()
}

pub(crate) fn format_date(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        day.month() as u8,
        day.day()
    )
}

pub(crate) fn parse_date(text: &str) -> Option<Date> {
    let mut parts = text.splitn(3, '-');
    let year = parts.next()?.parse().ok()?;
    let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
    let day = parts.next()?.parse().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn first_of_month(day: Date) -> Date {
    day.replace_day(1).unwrap_or(day)
}

fn add_months(month: Date, delta: i32) -> Date {
    let index = month.year() * 12 + month.month() as i32 - 1 + delta;
    let year = index.div_euclid(12);
    let number = (index.rem_euclid(12) + 1) as u8;
    Month::try_from(number)
        .ok()
        .and_then(|m| Date::from_calendar_date(year, m, 1).ok())
        .unwrap_or(month)
}

fn short_date(day: Date) -> String {
    format!(
        "{} {} {}",
        weekday_short(day.weekday()),
        day.day(),
        &month_name(day.month())[..3]
    )
}

pub(crate) fn month_name(month: Month) -> &'static str {
    match month {
        Month::January => "January",
        Month::February => "February",
        Month::March => "March",
        Month::April => "April",
        Month::May => "May",
        Month::June => "June",
        Month::July => "July",
        Month::August => "August",
        Month::September => "September",
        Month::October => "October",
        Month::November => "November",
        Month::December => "December",
    }
}

pub(crate) fn weekday_short(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

pub(crate) fn weekday_key(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "mon",
        Weekday::Tuesday => "tue",
        Weekday::Wednesday => "wed",
        Weekday::Thursday => "thu",
        Weekday::Friday => "fri",
        Weekday::Saturday => "sat",
        Weekday::Sunday => "sun",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> Date {
        parse_date(text).unwrap()
    }

    #[test]
    fn the_view_reads_the_server_shape() {
        let view: AttendanceView = serde_json::from_str(
            r#"{"generatedAt":"2026-11-04T18:00:00.000Z","available":true,"raidDays":["wed","thu"],"raidTime":"19:30",
            "members":[{"userId":"1","absences":[{"kind":"late","from":"2026-11-05","to":"2026-11-05","note":"20:00"}]}]}"#,
        )
        .unwrap();
        assert!(view.available);
        assert_eq!(view.raid_time.as_deref(), Some("19:30"));
        assert_eq!(view.absences("1")[0].cell(), "Late · 20:00");
        assert!(view.absences("2").is_empty());
        let saved = serde_json::to_value(&view.absences("1")[0]).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({"kind":"late","from":"2026-11-05","to":"2026-11-05","note":"20:00"})
        );
    }

    #[test]
    fn raid_nights_start_at_this_weeks_reset() {
        let days = vec!["wed".to_string(), "thu".to_string()];
        let nights = raid_nights(&days, day("2026-11-10"));
        assert_eq!(format_date(nights[0]), "2026-11-04");
        assert_eq!(format_date(nights[1]), "2026-11-05");
        assert_eq!(nights.len(), 24);
        let nights = raid_nights(&days, day("2026-11-04"));
        assert_eq!(format_date(nights[0]), "2026-11-04");
    }

    #[test]
    fn out_wins_over_late_on_the_same_night() {
        let absences = vec![
            Absence {
                kind: Kind::Late,
                from: "2026-11-05".into(),
                to: "2026-11-05".into(),
                note: String::new(),
            },
            Absence {
                kind: Kind::Out,
                from: "2026-11-01".into(),
                to: "2026-11-12".into(),
                note: "Holiday".into(),
            },
        ];
        assert_eq!(
            first_absence(&absences, "2026-11-05").unwrap().kind,
            Kind::Out
        );
        assert!(first_absence(&absences, "2026-11-13").is_none());
        assert_eq!(absences[1].cell(), "Out · Holiday");
    }

    #[test]
    fn dates_round_trip_and_months_wrap() {
        assert_eq!(format_date(day("2026-02-28")), "2026-02-28");
        assert!(parse_date("2026-02-30").is_none());
        assert_eq!(format_date(add_months(day("2026-12-01"), 1)), "2027-01-01");
        assert_eq!(format_date(add_months(day("2026-01-01"), -1)), "2025-12-01");
        assert_eq!(short_date(day("2026-11-04")), "Wed 4 Nov");
    }
}
