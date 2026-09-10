use std::{
    collections::{HashMap, HashSet},
    rc::{Rc, Weak},
};

use eframe::egui::{self, Color32, RichText};

use crate::{
    profile::{self, RaidRole},
    streams::Vod,
};

const MUTED: Color32 = Color32::from_rgb(159, 169, 184);
const TEXT: Color32 = Color32::from_rgb(239, 242, 247);
const ROW_HEIGHT: f32 = 48.0;
const MEMBER_HEIGHT: f32 = 30.0;
const DATE_HEIGHT: f32 = 28.0;
const ROW_SPACING: f32 = 2.0;

pub enum Action {
    Review(usize),
    OpenBrowser(usize),
    Remove(usize),
}

struct Entry {
    index: usize,
    search: String,
    month: String,
    day: String,
    date: String,
    clock: String,
    detail: String,
}

struct DisplayRow {
    entry: usize,
    top: f32,
    heading: bool,
}

struct Member {
    id: String,
    name: String,
    raid_role: Option<RaidRole>,
    count: usize,
}

#[derive(Default)]
pub struct Library {
    source: Weak<Vec<Vod>>,
    entries: Vec<Entry>,
    members: Vec<Member>,
    months: Vec<(String, String)>,
    member: Option<String>,
    month: Option<String>,
    query: String,
    filtered: Vec<usize>,
    rows: Vec<DisplayRow>,
    rows_height: f32,
    collapsed_days: HashSet<String>,
    dirty: bool,
    reset_scroll: bool,
    #[cfg(test)]
    builds: usize,
    #[cfg(test)]
    row_rects: Vec<egui::Rect>,
    #[cfg(test)]
    scroll_id: Option<egui::Id>,
    #[cfg(test)]
    heading_rects: Vec<egui::Rect>,
    #[cfg(test)]
    viewport_rect: Option<egui::Rect>,
    #[cfg(test)]
    painted_scrollbars: usize,
}

impl Library {
    fn prepare(&mut self, source: &Rc<Vec<Vod>>) {
        let identity = Rc::downgrade(source);
        if Weak::ptr_eq(&self.source, &identity) {
            return;
        }
        self.source = identity;
        self.entries.clear();
        let mut members: HashMap<&str, Member> = HashMap::new();
        let mut months: HashMap<String, String> = HashMap::new();
        for (index, vod) in source.iter().enumerate() {
            let stamp = timestamp(vod);
            let day = stamp
                .get(..10)
                .filter(|day| parse_date(day).is_some())
                .unwrap_or("");
            let month = stamp.get(..7).unwrap_or("").to_owned();
            let date = date_label(day);
            let clock = stamp
                .get(11..16)
                .map(|clock| format!("{clock} UTC"))
                .unwrap_or_default();
            let detail = format!("{} · {}", vod.name, vod.provider.label());
            self.entries.push(Entry {
                index,
                search: format!(
                    "{} {} {} {} {} {}",
                    title(vod),
                    detail,
                    date,
                    day,
                    clock,
                    month_label(&month)
                )
                .to_lowercase(),
                month: month.clone(),
                day: day.to_owned(),
                date,
                clock,
                detail,
            });
            members
                .entry(&vod.user_id)
                .or_insert_with(|| Member {
                    id: vod.user_id.clone(),
                    name: vod.name.clone(),
                    raid_role: vod.raid_role,
                    count: 0,
                })
                .count += 1;
            if !month.is_empty() {
                months
                    .entry(month.clone())
                    .or_insert_with(|| month_label(&month));
            }
        }
        self.entries.sort_by_cached_key(|entry| {
            let vod = &source[entry.index];
            (
                std::cmp::Reverse(entry.day.clone()),
                profile::role_order(vod.raid_role),
                vod.name.to_lowercase(),
                std::cmp::Reverse(timestamp(vod).to_owned()),
                vod.id.clone(),
            )
        });
        let days: HashSet<_> = self
            .entries
            .iter()
            .map(|entry| entry.day.as_str())
            .collect();
        self.collapsed_days
            .retain(|day| days.contains(day.as_str()));
        self.members = members.into_values().collect();
        self.members.sort_by_cached_key(|member| {
            (
                profile::role_order(member.raid_role),
                member.name.to_lowercase(),
                member.id.clone(),
            )
        });
        self.months = months.into_iter().collect();
        self.months.sort_by(|a, b| b.0.cmp(&a.0));
        if self
            .member
            .as_ref()
            .is_some_and(|id| !self.members.iter().any(|member| &member.id == id))
        {
            self.member = None;
        }
        if self
            .month
            .as_ref()
            .is_some_and(|month| !self.months.iter().any(|entry| &entry.0 == month))
        {
            self.month = None;
        }
        self.dirty = true;
        #[cfg(test)]
        {
            self.builds += 1;
        }
    }

    fn filter(&mut self, source: &[Vod]) {
        if !self.dirty {
            return;
        }
        let query = self.query.to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        self.filtered.clear();
        self.filtered.extend(
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let vod = &source[entry.index];
                    (self.member.as_ref().is_none_or(|id| id == &vod.user_id)
                        && self
                            .month
                            .as_ref()
                            .is_none_or(|month| month == &entry.month)
                        && words.iter().all(|word| entry.search.contains(word)))
                    .then_some(index)
                }),
        );
        self.rebuild_rows();
        self.dirty = false;
        self.reset_scroll = true;
    }

    fn toggle_day(&mut self, day: String) {
        if !self.collapsed_days.remove(&day) {
            self.collapsed_days.insert(day);
        }
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        // Prepare only on data/filter/collapse changes. Each frame binary-searches
        // these offsets and paints visible rows without rebuilding the library.
        self.rows.clear();
        let mut previous_day = None;
        let mut top = 0.0;
        for &entry in &self.filtered {
            let day = self.entries[entry].day.as_str();
            if previous_day != Some(day) {
                self.rows.push(DisplayRow {
                    entry,
                    top,
                    heading: true,
                });
                top += DATE_HEIGHT + ROW_SPACING;
                previous_day = Some(day);
            }
            if !self.collapsed_days.contains(day) {
                self.rows.push(DisplayRow {
                    entry,
                    top,
                    heading: false,
                });
                top += ROW_HEIGHT + ROW_SPACING;
            }
        }
        self.rows_height = (top - ROW_SPACING).max(0.0);
    }

    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        source: &Rc<Vec<Vod>>,
        loading: bool,
        can_remove: bool,
        busy: bool,
    ) -> Option<Action> {
        self.prepare(source);
        #[cfg(test)]
        {
            self.row_rects.clear();
            self.heading_rects.clear();
        }
        let mut action = None;
        let height = ui.available_height().max(1.0);
        let sidebar = if ui.available_width() < 1000.0 {
            156.0
        } else {
            176.0
        };
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(sidebar, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(sidebar);
                    ui.spacing_mut().scroll = library_scroll_style();
                    ui.visuals_mut().clip_rect_margin = 0.0;
                    ui.label(RichText::new("PLAYERS").small().strong().color(MUTED));
                    ui.add_space(10.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if member_row(
                        ui,
                        "All players",
                        None,
                        self.members.len(),
                        self.member.is_none(),
                    )
                    .clicked()
                        && self.member.take().is_some()
                    {
                        self.dirty = true;
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("recording-library-members")
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                        )
                        .max_height(ui.available_height())
                        .show_rows(ui, MEMBER_HEIGHT, self.members.len(), |ui, rows| {
                            for index in rows {
                                let member = &self.members[index];
                                if member_row(
                                    ui,
                                    &member.name,
                                    member.raid_role,
                                    member.count,
                                    self.member.as_ref() == Some(&member.id),
                                )
                                .clicked()
                                    && self.member.as_ref() != Some(&member.id)
                                {
                                    self.member = Some(member.id.clone());
                                    self.dirty = true;
                                }
                            }
                        });
                },
            );
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().interact_size.y = 32.0;
                ui.horizontal(|ui| {
                    let filter_width = 154.0;
                    let search_width =
                        (ui.available_width() - filter_width - ui.spacing().item_spacing.x)
                            .max(40.0);
                    self.dirty |= ui
                        .add(
                            egui::TextEdit::singleline(&mut self.query)
                                .desired_width(search_width)
                                .margin(egui::vec2(8.0, 8.0))
                                .hint_text("Search VODs")
                                .char_limit(128),
                        )
                        .changed();
                    let label = self
                        .month
                        .as_ref()
                        .and_then(|month| self.months.iter().find(|entry| &entry.0 == month))
                        .map(|entry| entry.1.as_str())
                        .unwrap_or("All months");
                    egui::ComboBox::from_id_salt("recording-library-month")
                        .selected_text(label)
                        .width(filter_width)
                        .height(280.0)
                        .show_ui(ui, |ui| {
                            self.dirty |= ui
                                .selectable_value(&mut self.month, None, "All months")
                                .changed();
                            for (month, label) in &self.months {
                                self.dirty |= ui
                                    .selectable_value(&mut self.month, Some(month.clone()), label)
                                    .changed();
                            }
                        });
                });
                self.filter(source);
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{} VOD{}",
                        self.filtered.len(),
                        if self.filtered.len() == 1 { "" } else { "s" }
                    ))
                    .small()
                    .color(MUTED),
                );
                ui.add_space(6.0);
                if self.filtered.is_empty() {
                    ui.add_space(20.0);
                    ui.label(
                        RichText::new(if source.is_empty() {
                            if loading {
                                "Loading VODs…"
                            } else {
                                "No VODs yet."
                            }
                        } else {
                            "No VODs match these filters."
                        })
                        .color(MUTED),
                    );
                    if !source.is_empty() && ui.button("Clear filters").clicked() {
                        self.member = None;
                        self.month = None;
                        self.query.clear();
                        self.dirty = true;
                    }
                    return;
                }
                ui.spacing_mut().item_spacing.y = ROW_SPACING;
                // The content margin reserves a permanent gutter. Its fixed-width
                // scrollbar appears only when needed and never covers row actions.
                ui.spacing_mut().scroll = library_scroll_style();
                ui.visuals_mut().clip_rect_margin = 0.0;
                let mut toggled_day = None;
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("recording-library-rows")
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                    )
                    .auto_shrink([false, false])
                    .max_height(ui.available_height());
                if std::mem::take(&mut self.reset_scroll) {
                    scroll = scroll.vertical_scroll_offset(0.0);
                }
                let output = scroll.show_viewport(ui, |ui, viewport| {
                    ui.set_height(self.rows_height);
                    let first = self
                        .rows
                        .partition_point(|row| row.top <= viewport.top())
                        .saturating_sub(1);
                    let end = self
                        .rows
                        .partition_point(|row| row.top <= viewport.bottom());
                    for display in &self.rows[first..end] {
                        let entry = &self.entries[display.entry];
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(ui.max_rect().left(), ui.max_rect().top() + display.top),
                            egui::vec2(
                                ui.available_width(),
                                if display.heading {
                                    DATE_HEIGHT
                                } else {
                                    ROW_HEIGHT
                                },
                            ),
                        );
                        if display.heading {
                            #[cfg(test)]
                            self.heading_rects.push(rect);
                            let collapsed = self.collapsed_days.contains(&entry.day);
                            let mut heading_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(rect)
                                    .id_salt(("recording-date", &entry.day)),
                            );
                            heading_ui.set_clip_rect(ui.clip_rect().intersect(rect));
                            let ui = &mut heading_ui;
                            let response = ui
                                .interact(
                                    rect,
                                    ui.id().with(("recording-date", &entry.day)),
                                    egui::Sense::click(),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    format!(
                                        "{} {}",
                                        if collapsed { "Expand" } else { "Collapse" },
                                        entry.date
                                    ),
                                )
                            });
                            if response.clicked() {
                                toggled_day = Some(entry.day.clone());
                            }
                            let painter = ui.painter_at(rect);
                            if response.hovered() || response.has_focus() {
                                painter.rect_filled(rect, 4.0, Color32::from_rgb(36, 42, 52));
                            }
                            let center = egui::pos2(rect.left() + 12.0, rect.center().y);
                            let points = if collapsed {
                                [
                                    egui::vec2(-2.0, -4.0),
                                    egui::vec2(-2.0, 4.0),
                                    egui::vec2(3.0, 0.0),
                                ]
                            } else {
                                [
                                    egui::vec2(-4.0, -2.0),
                                    egui::vec2(4.0, -2.0),
                                    egui::vec2(0.0, 3.0),
                                ]
                            };
                            painter.add(egui::Shape::convex_polygon(
                                points.into_iter().map(|point| center + point).collect(),
                                MUTED,
                                egui::Stroke::NONE,
                            ));
                            let label = painter.text(
                                egui::pos2(rect.left() + 26.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &entry.date,
                                egui::FontId::proportional(12.0),
                                TEXT,
                            );
                            let line_start = label.right() + 12.0;
                            if line_start < rect.right() {
                                painter.hline(
                                    line_start..=rect.right(),
                                    rect.center().y,
                                    egui::Stroke::new(
                                        1.0_f32,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    ),
                                );
                            }
                            continue;
                        }
                        let vod = &source[entry.index];
                        #[cfg(test)]
                        self.row_rects.push(rect);
                        {
                            let mut row_ui =
                                ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt((
                                    &vod.user_id,
                                    vod.provider.key(),
                                    &vod.id,
                                )));
                            row_ui.set_clip_rect(ui.clip_rect().intersect(rect));
                            let ui = &mut row_ui;
                            let click_rect = egui::Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.right() - 40.0, rect.bottom()),
                            );
                            let response = ui
                                .interact(click_rect, ui.id().with("review"), egui::Sense::click())
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    ui.is_enabled(),
                                    format!(
                                        "Review {} · {} · {}",
                                        title(vod),
                                        vod.name,
                                        entry.date
                                    ),
                                )
                            });
                            if response.clicked() {
                                action = Some(Action::Review(entry.index));
                            }
                            if ui.is_rect_visible(rect) {
                                let painter = ui.painter_at(rect);
                                painter.rect_filled(
                                    rect,
                                    5.0,
                                    if response.hovered() || response.has_focus() {
                                        Color32::from_rgb(36, 42, 52)
                                    } else {
                                        Color32::from_rgb(25, 28, 36)
                                    },
                                );
                                if response.has_focus() {
                                    painter.rect_stroke(
                                        rect,
                                        5.0,
                                        egui::Stroke::new(1.0_f32, MUTED),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                let center = egui::pos2(rect.left() + 17.0, rect.center().y);
                                painter.add(egui::Shape::convex_polygon(
                                    vec![
                                        center + egui::vec2(-3.0, -5.0),
                                        center + egui::vec2(-3.0, 5.0),
                                        center + egui::vec2(5.0, 0.0),
                                    ],
                                    MUTED,
                                    egui::Stroke::NONE,
                                ));
                                let title_left = rect.left() + 34.0;
                                let date_right = click_rect.right() - 10.0;
                                let date_width = 114.0;
                                let text_width =
                                    (date_right - date_width - title_left - 12.0).max(1.0);
                                paint_line(
                                    ui,
                                    title(vod),
                                    egui::pos2(title_left, rect.top() + 6.0),
                                    text_width,
                                    13.0,
                                    TEXT,
                                );
                                paint_line(
                                    ui,
                                    &entry.detail,
                                    egui::pos2(
                                        title_left
                                            + if vod.raid_role.is_some() { 18.0 } else { 0.0 },
                                        rect.top() + 27.0,
                                    ),
                                    (text_width - if vod.raid_role.is_some() { 18.0 } else { 0.0 })
                                        .max(1.0),
                                    10.0,
                                    MUTED,
                                );
                                profile::paint_role_icon(
                                    ui,
                                    vod.raid_role,
                                    egui::Rect::from_min_size(
                                        egui::pos2(title_left, rect.top() + 24.0),
                                        egui::vec2(16.0, 16.0),
                                    ),
                                );
                                painter.text(
                                    egui::pos2(date_right, rect.top() + 14.0),
                                    egui::Align2::RIGHT_CENTER,
                                    &entry.date,
                                    egui::FontId::proportional(11.0),
                                    MUTED,
                                );
                                painter.text(
                                    egui::pos2(date_right, rect.top() + 33.0),
                                    egui::Align2::RIGHT_CENTER,
                                    &entry.clock,
                                    egui::FontId::proportional(10.0),
                                    MUTED,
                                );
                            }
                            response.on_hover_text(format!(
                                "{}\n{} · {}",
                                title(vod),
                                entry.detail,
                                entry.date
                            ));
                            let menu_rect = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - 20.0, rect.center().y),
                                egui::vec2(30.0, 28.0),
                            );
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(menu_rect)
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                |ui| {
                                    ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
                                    ui.menu_button("…", |ui| {
                                        if ui.button("Open in browser").clicked() {
                                            action = Some(Action::OpenBrowser(entry.index));
                                            ui.close();
                                        }
                                        if ui.button("Copy link").clicked() {
                                            ui.ctx().copy_text(vod.url.clone());
                                            ui.close();
                                        }
                                        if can_remove {
                                            ui.separator();
                                            if ui
                                                .add_enabled(
                                                    !busy,
                                                    egui::Button::new("Remove from Brick…"),
                                                )
                                                .clicked()
                                            {
                                                action = Some(Action::Remove(entry.index));
                                                ui.close();
                                            }
                                        }
                                    });
                                },
                            );
                        }
                    }
                });
                #[cfg(test)]
                {
                    self.scroll_id = Some(output.id);
                    self.viewport_rect = Some(output.inner_rect);
                }
                if let Some(day) = toggled_day {
                    self.toggle_day(day);
                    ui.ctx().request_repaint();
                }
                #[cfg(not(test))]
                let _ = output;
            });
        });
        action
    }
}

fn library_scroll_style() -> egui::style::ScrollStyle {
    egui::style::ScrollStyle {
        floating: true,
        floating_width: 6.0,
        // egui draws floating scrollbars over this margin, outside row content.
        content_margin: egui::Margin {
            right: 10,
            ..egui::Margin::ZERO
        },
        dormant_handle_opacity: 1.0,
        active_handle_opacity: 1.0,
        interact_handle_opacity: 1.0,
        ..egui::style::ScrollStyle::solid()
    }
}

fn member_row(
    ui: &mut egui::Ui,
    name: &str,
    role: Option<RaidRole>,
    count: usize,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), MEMBER_HEIGHT),
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
            painter.rect_filled(rect, 4.0, Color32::from_rgb(36, 42, 52));
        }
        let count_rect = painter.text(
            egui::pos2(rect.right() - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            count.to_string(),
            egui::FontId::proportional(10.0),
            MUTED,
        );
        let icon_width = if role.is_some() { 22.0 } else { 0.0 };
        profile::paint_role_icon(
            ui,
            role,
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 18.0, rect.center().y),
                egui::vec2(18.0, 18.0),
            ),
        );
        paint_line(
            ui,
            name,
            egui::pos2(rect.left() + 10.0 + icon_width, rect.center().y - 7.0),
            (count_rect.left() - rect.left() - 22.0 - icon_width).max(1.0),
            13.0,
            TEXT,
        );
    }
    response
        .on_hover_text(name)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn paint_line(ui: &egui::Ui, text: &str, pos: egui::Pos2, width: f32, size: f32, color: Color32) {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::FontId::proportional(size),
        color,
    );
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter().galley(pos, galley, color);
}

fn title(vod: &Vod) -> &str {
    if vod.title.trim().is_empty() {
        "VOD"
    } else {
        &vod.title
    }
}
fn timestamp(vod: &Vod) -> &str {
    vod.started_at
        .as_deref()
        .or(vod.ended_at.as_deref())
        .unwrap_or("")
}

fn month_parts(stamp: &str) -> Option<(i32, time::Month)> {
    let year = stamp.get(..4)?.parse().ok()?;
    let month = time::Month::try_from(stamp.get(5..7)?.parse::<u8>().ok()?).ok()?;
    Some((year, month))
}
fn month_label(stamp: &str) -> String {
    month_parts(stamp)
        .map(|(year, month)| format!("{month} {year}"))
        .unwrap_or_else(|| "Date unavailable".into())
}
fn parse_date(stamp: &str) -> Option<time::Date> {
    let (year, month) = month_parts(stamp)?;
    let day = stamp.get(8..10)?.parse::<u8>().ok()?;
    time::Date::from_calendar_date(year, month, day).ok()
}
fn date_label(stamp: &str) -> String {
    parse_date(stamp)
        .map(|date| {
            let month = date.month().to_string();
            format!("{} {} {}", date.day(), &month[..3], date.year())
        })
        .unwrap_or_else(|| "Date unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure_context(ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());
        ctx.global_style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(14.0, 8.0);
            style.visuals.panel_fill = Color32::from_rgb(21, 23, 28);
        });
    }

    fn archive(index: usize) -> Vod {
        Vod {
            id: format!("{}", index + 1),
            user_id: format!("{}", index % 30 + 1),
            name: format!("Player {:02}", index % 30 + 1),
            raid_role: None,
            provider: crate::streams::Provider::Twitch,
            url: format!("https://www.twitch.tv/videos/{}", index + 1),
            started_at: Some(format!(
                "2026-{:02}-{:02}T18:10:00Z",
                6 + index % 4,
                1 + index % 28
            )),
            ended_at: None,
            title: format!(
                "Mythic raid night {} — a long recording title for checking truncation",
                index + 1
            ),
        }
    }

    #[test]
    fn search_combines_player_month_provider_and_title_without_fetches() {
        let mut vods: Vec<_> = (0..120).map(archive).collect();
        vods[3].provider = crate::streams::Provider::Youtube;
        let source = Rc::new(vods);
        let mut library = Library::default();
        library.prepare(&source);
        library.filter(&source);
        assert_eq!(library.filtered.len(), 120);
        assert_eq!(library.members.len(), 30);
        assert_eq!(library.months.len(), 4);
        assert_eq!(library.members[3].count, 4);
        assert_eq!(library.months[0].1, "September 2026");
        library.member = Some("4".into());
        library.month = Some("2026-09".into());
        library.query = "   PLAYER 04  SEPTEMBER  Youtube   ".into();
        library.dirty = true;
        library.filter(&source);
        assert_eq!(
            library
                .filtered
                .iter()
                .map(|i| library.entries[*i].index)
                .collect::<Vec<_>>(),
            [3]
        );
        library.query.push_str(" missing");
        library.dirty = true;
        library.filter(&source);
        assert!(library.filtered.is_empty());
        assert_eq!(library.builds, 1);
    }

    #[test]
    fn library_players_and_vods_use_role_then_case_insensitive_name_order() {
        let mut items = vec![archive(0), archive(1), archive(2), archive(3), archive(4)];
        for (vod, (name, role)) in items.iter_mut().zip([
            ("Alpha", None),
            ("zulu", Some(RaidRole::Tank)),
            ("Bravo", Some(RaidRole::Dps)),
            ("alpha", Some(RaidRole::Tank)),
            ("Healer", Some(RaidRole::Healer)),
        ]) {
            vod.started_at = Some("2026-09-10T18:10:00Z".into());
            vod.name = name.into();
            vod.raid_role = role;
        }
        let source = Rc::new(items);
        let mut library = Library::default();
        library.prepare(&source);
        assert_eq!(
            library
                .members
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zulu", "Healer", "Bravo", "Alpha"]
        );
        assert_eq!(
            library
                .entries
                .iter()
                .map(|entry| source[entry.index].name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zulu", "Healer", "Bravo", "Alpha"]
        );
    }

    #[test]
    fn date_groups_are_newest_first_with_roles_inside_each_day_and_no_empty_headers() {
        let mut items: Vec<_> = (0..7).map(archive).collect();
        for (vod, (stamp, name, role)) in items.iter_mut().zip([
            ("2026-09-09T18:00:00Z", "Old tank", Some(RaidRole::Tank)),
            ("2026-09-10T18:00:00Z", "DPS", Some(RaidRole::Dps)),
            ("2026-09-10T18:00:00Z", "Zulu", Some(RaidRole::Tank)),
            ("2026-09-10T18:00:00Z", "alpha", Some(RaidRole::Tank)),
            ("2026-09-10T18:00:00Z", "Healer", Some(RaidRole::Healer)),
            ("", "Undated", None),
            ("2026-02-30T18:00:00Z", "Invalid date", Some(RaidRole::Tank)),
        ]) {
            vod.started_at = Some(stamp.into());
            vod.name = name.into();
            vod.raid_role = role;
        }
        let source = Rc::new(items);
        let mut library = Library::default();
        library.prepare(&source);
        library.filter(&source);
        assert_eq!(
            library
                .filtered
                .iter()
                .map(|i| library.entries[*i].index)
                .collect::<Vec<_>>(),
            [3, 2, 4, 1, 0, 6, 5]
        );
        assert_eq!(
            library
                .rows
                .iter()
                .filter(|row| row.heading)
                .map(|row| library.entries[row.entry].date.as_str())
                .collect::<Vec<_>>(),
            ["10 Sep 2026", "9 Sep 2026", "Date unavailable"]
        );
        assert_eq!(library.rows_height, 7.0 * 50.0 + 3.0 * 30.0 - 2.0);
        library.query = "Old tank".into();
        library.dirty = true;
        library.filter(&source);
        assert_eq!(library.rows.len(), 2);
        assert!(library.rows[0].heading);
        assert_eq!(library.entries[library.rows[0].entry].date, "9 Sep 2026");
        assert_eq!(library.entries[library.rows[1].entry].index, 0);
        library.query = "no match".into();
        library.dirty = true;
        library.filter(&source);
        assert!(library.rows.is_empty());
        assert_eq!(library.rows_height, 0.0);
        assert_eq!(library.builds, 1);
    }

    #[test]
    fn clicking_a_grouped_vod_opens_the_original_source_entry() {
        let source = Rc::new(vec![archive(0), archive(3)]);
        let ctx = egui::Context::default();
        configure_context(&ctx);
        let mut library = Library::default();
        let size = egui::vec2(980.0, 600.0);
        frame(&ctx, &mut library, &source, size);
        frame(&ctx, &mut library, &source, size);
        let position = library.row_rects[0].center();
        let mut selected = None;
        for pressed in [true, false] {
            let _ = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show_inside(ui, |ui| {
                        if let Some(Action::Review(index)) =
                            library.draw(ui, &source, false, true, false)
                        {
                            selected = Some(index);
                        }
                    });
                },
            );
        }
        assert_eq!(selected, Some(1));
    }

    #[test]
    fn cached_library_updates_after_removal_and_clears_invalid_filters() {
        let mut source = Rc::new(vec![archive(0), archive(3)]);
        let mut library = Library::default();
        library.prepare(&source);
        library.filter(&source);
        for _ in 0..100 {
            library.prepare(&source);
            library.filter(&source);
        }
        assert_eq!(library.builds, 1);
        library.member = Some("4".into());
        library.month = Some("2026-09".into());
        Rc::make_mut(&mut source).remove(1);
        library.prepare(&source);
        library.filter(&source);
        assert_eq!(library.builds, 2);
        assert!(library.member.is_none());
        assert!(library.month.is_none());
        assert_eq!(library.filtered.len(), 1);
        assert_eq!(library.members[0].count, 1);
        assert_eq!(date_label("2026-02-30"), "Date unavailable");
    }

    fn frame(
        ctx: &egui::Context,
        library: &mut Library,
        source: &Rc<Vec<Vod>>,
        size: egui::Vec2,
    ) -> egui::Rect {
        frame_input(ctx, library, source, size, Vec::new()).0
    }

    fn frame_input(
        ctx: &egui::Context,
        library: &mut Library,
        source: &Rc<Vec<Vod>>,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> (egui::Rect, Option<Action>) {
        let mut bounds = egui::Rect::NOTHING;
        let mut action = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    action = library.draw(ui, source, false, true, false);
                    bounds = ui.min_rect();
                });
            },
        );
        library.painted_scrollbars = output
            .shapes
            .iter()
            .filter(|clipped| {
                if let egui::Shape::Rect(shape) = &clipped.shape {
                    library.viewport_rect.is_some_and(|viewport| {
                        shape.rect.left() >= viewport.right() - 6.1
                            && shape.rect.right() <= viewport.right() + 0.1
                            && shape.rect.height() > 10.0
                            && shape.fill.a() > 0
                    })
                } else {
                    false
                }
            })
            .count();
        (bounds, action)
    }

    fn click(
        ctx: &egui::Context,
        library: &mut Library,
        source: &Rc<Vec<Vod>>,
        size: egui::Vec2,
        pos: egui::Pos2,
    ) -> Option<Action> {
        frame_input(
            ctx,
            library,
            source,
            size,
            vec![egui::Event::PointerMoved(pos)],
        );
        let mut action = None;
        for pressed in [true, false] {
            let next = frame_input(
                ctx,
                library,
                source,
                size,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            )
            .1;
            if next.is_some() {
                action = next;
            }
        }
        action
    }

    #[test]
    fn day_headers_collapse_locally_and_scrollbar_has_a_fixed_separate_gutter() {
        let mut items: Vec<_> = (0..24).map(archive).collect();
        for (i, vod) in items.iter_mut().enumerate() {
            vod.started_at = Some(format!(
                "2026-09-{}T18:00:00Z",
                if i < 12 { "10" } else { "09" }
            ));
        }
        let source = Rc::new(items);
        let ctx = egui::Context::default();
        configure_context(&ctx);
        let mut library = Library::default();
        let size = egui::vec2(980.0, 600.0);
        for _ in 0..3 {
            frame(&ctx, &mut library, &source, size);
        }
        let width = library.row_rects[0].width();
        let viewport = library.viewport_rect.unwrap();
        let gutter = egui::pos2(viewport.right() - 3.0, viewport.center().y);
        for _ in 0..4 {
            frame_input(
                &ctx,
                &mut library,
                &source,
                size,
                vec![egui::Event::PointerMoved(gutter)],
            );
            assert_eq!(library.row_rects[0].width(), width);
            assert!(library
                .row_rects
                .iter()
                .all(|rect| rect.right() <= viewport.right() - 10.0));
        }
        assert!(library.painted_scrollbars > 0);
        assert!(click(&ctx, &mut library, &source, size, gutter).is_none());
        let id = library.scroll_id.unwrap();
        // Cancel the gutter click's animated scroll before testing header clicks.
        egui::scroll_area::State::default().store(&ctx, id);
        frame(&ctx, &mut library, &source, size);
        frame(&ctx, &mut library, &source, size);
        let header = library.heading_rects[0].center();
        assert!(click(&ctx, &mut library, &source, size, header).is_none());
        frame(&ctx, &mut library, &source, size);
        assert!(library.collapsed_days.contains("2026-09-10"));
        assert_eq!(library.rows.len(), 14);
        assert_eq!(library.filtered.len(), 24);
        assert_eq!(library.row_rects[0].width(), width);
        assert_eq!(library.rows_height, 2.0 * 30.0 + 12.0 * 50.0 - 2.0);
        assert!(click(&ctx, &mut library, &source, size, header).is_none());
        frame(&ctx, &mut library, &source, size);
        assert!(library.collapsed_days.is_empty());
        assert_eq!(library.rows.len(), 26);
        // No overflow after collapsing both days still reserves exactly the same gutter.
        library.toggle_day("2026-09-10".into());
        library.toggle_day("2026-09-09".into());
        for _ in 0..20 {
            frame(&ctx, &mut library, &source, size);
        }
        assert_eq!(library.painted_scrollbars, 0);
        assert_eq!(library.rows.len(), 2);
        assert!(library.row_rects.is_empty());
        assert_eq!(library.heading_rects[0].width(), width);
        assert_eq!(library.viewport_rect.unwrap().right(), viewport.right());
        assert_eq!(library.builds, 1);
        assert!(!library.dirty);
    }

    #[test]
    fn collapsed_days_survive_filtering_and_refresh_but_removed_dates_are_pruned() {
        let mut source = Rc::new(vec![archive(0), archive(3)]);
        let mut library = Library::default();
        library.prepare(&source);
        library.filter(&source);
        library.toggle_day("2026-09-04".into());
        library.query = "Player 04".into();
        library.dirty = true;
        library.filter(&source);
        assert_eq!(library.rows.len(), 1);
        assert!(library.rows[0].heading);
        Rc::make_mut(&mut source)[1].title = "Updated title".into();
        library.prepare(&source);
        library.filter(&source);
        assert_eq!(library.rows.len(), 1);
        assert!(library.collapsed_days.contains("2026-09-04"));
        Rc::make_mut(&mut source).remove(1);
        library.prepare(&source);
        library.filter(&source);
        assert!(library.collapsed_days.is_empty());
    }

    #[test]
    fn ten_thousand_recordings_only_draw_visible_rows_and_fit_small_windows() {
        let source = Rc::new((0..10_000).map(archive).collect::<Vec<_>>());
        for size in [
            egui::vec2(640.0, 400.0),
            egui::vec2(980.0, 600.0),
            egui::vec2(1440.0, 800.0),
        ] {
            for dpi in [1.0, 1.5, 2.0] {
                let ctx = egui::Context::default();
                configure_context(&ctx);
                ctx.set_pixels_per_point(dpi);
                let mut library = Library::default();
                frame(&ctx, &mut library, &source, size);
                for offset in [0.0, 200_000.0, 499_900.0] {
                    let id = library.scroll_id.unwrap();
                    let mut state = egui::scroll_area::State::load(&ctx, id).unwrap();
                    state.offset.y = offset;
                    state.store(&ctx, id);
                    let bounds = frame(&ctx, &mut library, &source, size);
                    assert!(bounds.right() <= size.x + 0.1, "{size:?} {dpi}: {bounds:?}");
                    assert!(
                        bounds.bottom() <= size.y + 0.1,
                        "{size:?} {dpi}: {bounds:?}"
                    );
                    assert!(library.row_rects.len() <= (size.y / 50.0).ceil() as usize + 2);
                    assert!(!library.row_rects.is_empty());
                    for rows in library.row_rects.windows(2) {
                        assert!(
                            [50.0, 80.0]
                                .iter()
                                .any(|gap| (rows[1].top() - rows[0].top() - gap).abs() < 0.1),
                            "row spacing: {rows:?}"
                        );
                    }
                    for row in &library.row_rects {
                        assert!(row.right() <= size.x);
                    }
                }
                assert_eq!(library.builds, 1);
            }
        }
    }

    #[test]
    #[ignore = "opt-in recordings library layout and scroll benchmark, no provider decoding"]
    fn recordings_library_drawing_benchmark() {
        let source = Rc::new((0..10_000).map(archive).collect::<Vec<_>>());
        for size in [egui::vec2(980.0, 600.0), egui::vec2(1440.0, 800.0)] {
            let ctx = egui::Context::default();
            configure_context(&ctx);
            let mut library = Library::default();
            let initial = std::time::Instant::now();
            frame(&ctx, &mut library, &source, size);
            let initial_ms = initial.elapsed().as_secs_f64() * 1000.0;
            let mut samples = Vec::new();
            let mut max_rows = 0;
            for frame_index in 0..630 {
                let id = library.scroll_id.unwrap();
                let mut state = egui::scroll_area::State::load(&ctx, id).unwrap();
                state.offset.y = (frame_index * 673 % 490_000) as f32;
                state.store(&ctx, id);
                let before = std::time::Instant::now();
                frame(&ctx, &mut library, &source, size);
                if frame_index >= 30 {
                    samples.push(before.elapsed().as_secs_f64() * 1000.0);
                }
                max_rows = max_rows.max(library.row_rects.len());
            }
            samples.sort_by(f64::total_cmp);
            println!("recordings_library_benchmark size={size:?} records={} initial_ms={initial_ms:.3} median_ms={:.3} p95_ms={:.3} worst_ms={:.3} max_rows={max_rows} builds={}", source.len(), samples[300], samples[570], samples[599], library.builds);
            assert_eq!(library.builds, 1);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "opt-in isolated X11 recordings library visual check, generated data only"]
    fn recordings_library_native_preview() {
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        struct Preview {
            library: Library,
            source: Rc<Vec<Vod>>,
            started: std::time::Instant,
            expanded: bool,
        }
        impl eframe::App for Preview {
            fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
                let elapsed = self.started.elapsed();
                if elapsed.as_secs() >= 5 && !self.expanded {
                    self.expanded = true;
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                            1440.0, 800.0,
                        )));
                }
                if elapsed.as_secs() >= 10 {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    ui.heading("VODs");
                    ui.add_space(12.0);
                    self.library.draw(ui, &self.source, false, true, false);
                });
            }
        }
        eframe::run_native(
            "Brick · VOD library check",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([980.0, 600.0])
                    .with_position([0.0, 0.0]),
                event_loop_builder: Some(Box::new(|builder| {
                    builder.with_x11().with_any_thread(true);
                })),
                ..Default::default()
            },
            Box::new(|cc| {
                configure_context(&cc.egui_ctx);
                Ok(Box::new(Preview {
                    library: Library::default(),
                    source: Rc::new((0..10_000).map(archive).collect()),
                    started: std::time::Instant::now(),
                    expanded: false,
                }))
            }),
        )
        .unwrap();
    }
}
