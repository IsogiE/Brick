//! Send to ART: a planned raid's groups, boss by boss, as an ART Raid Groups
//! import. ART reads `ART_RG2:` and the base64 of `preset,group,slot,character`
//! rows; each boss becomes one preset, and importing again replaces it.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use base64::Engine as _;
use eframe::egui::{self, Color32, RichText, Stroke};

use crate::{
    attendance_ui::Kind,
    guild_ui::{
        arrow, bar_button, class_color, clipped, control_row, line_color, single, text_at,
        CharacterRow, CharactersView, CONTROL_HEIGHT,
    },
    planner_ui::{night_name, Context, Difficulty, PlannedRaid},
    presence::RosterMember,
    profile::{role_order, RaidRole},
    ui::{muted_text, panel_frame, primary_text, secondary_text, success_accent, warning_accent},
};

const SLOTS_PER_GROUP: usize = 5;
const SLOT_HEIGHT: f32 = 22.0;
const GROUP_GAP: f32 = 8.0;
const SIDE_WIDTH: f32 = 320.0;
const SEND: Color32 = Color32::from_rgb(203, 85, 48);
const SELECTED: Color32 = Color32::from_rgb(94, 168, 224);
const FIELD_FILL: Color32 = Color32::from_rgb(38, 42, 50);
const HOVER_FILL: Color32 = Color32::from_rgb(46, 51, 60);
const BOX_FILL: Color32 = Color32::from_rgb(24, 27, 33);
const COPIED_FOR: Duration = Duration::from_secs(4);
const MAX_LISTED: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Group(usize),
    Bench(String),
}

/// The Send to ART screen for one raid of the shown week.
pub(crate) struct ArtUi {
    pub raid: usize,
    boss: usize,
    selected: Option<Slot>,
    copied: Option<Instant>,
}

impl ArtUi {
    pub fn new(raid: usize) -> Self {
        Self {
            raid,
            boss: 0,
            selected: None,
            copied: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Boss {
    pub id: u64,
    pub name: String,
}

/// What the screen asks of the planner after a frame.
#[derive(Default)]
pub(crate) struct Outcome {
    pub changed: bool,
    pub back: bool,
}

/// The raid's bosses in journal order, instance by instance.
pub(crate) fn bosses(raid: &PlannedRaid, characters: Option<&CharactersView>) -> Vec<Boss> {
    characters
        .map_or(&[][..], |view| view.raids.as_slice())
        .iter()
        .filter(|instance| raid.instances.contains(&instance.id))
        .flat_map(|instance| &instance.encounters)
        .filter_map(|encounter| {
            Some(Boss {
                id: encounter.get("id")?.as_u64().filter(|id| *id > 0)?,
                name: encounter.get("name")?.as_str()?.trim().to_owned(),
            })
        })
        .filter(|boss| !boss.name.is_empty())
        .collect()
}

pub(crate) fn capacity(raid: &PlannedRaid) -> usize {
    if raid.difficulty == Difficulty::Mythic {
        20
    } else {
        30
    }
}

/// Tank and healer specs; everything else is damage.
fn spec_role(spec: &str) -> Option<RaidRole> {
    match spec {
        "Protection" | "Blood" | "Guardian" | "Brewmaster" | "Vengeance" => Some(RaidRole::Tank),
        "Restoration" | "Holy" | "Discipline" | "Mistweaver" | "Preservation" => {
            Some(RaidRole::Healer)
        }
        "" => None,
        _ => Some(RaidRole::Dps),
    }
}

/// Members the raid brings, tanks first, then healers, then damage by class.
fn going<'a>(
    raid: &PlannedRaid,
    members: &[&'a RosterMember],
    context: &'a Context,
) -> Vec<(&'a str, Option<&'a CharacterRow>)> {
    let mut going: Vec<(&str, Option<&CharacterRow>, Option<RaidRole>)> = members
        .iter()
        .filter(|member| context.absence(&member.user_id, raid) != Some(Kind::Out))
        .filter_map(|member| {
            let key = raid.pick(&member.user_id)?;
            let character = context.character(&member.user_id, key);
            let role = character
                .and_then(|character| character.spec.as_deref())
                .and_then(spec_role)
                .or(member.raid_role);
            Some((member.user_id.as_str(), character, role))
        })
        .collect();
    going.sort_by(|a, b| {
        let key = |entry: &(&str, Option<&CharacterRow>, Option<RaidRole>)| {
            (
                role_order(entry.2),
                entry
                    .1
                    .and_then(|character| character.class.clone())
                    .unwrap_or_default(),
                entry
                    .1
                    .map(|character| character.name.to_lowercase())
                    .unwrap_or_default(),
            )
        };
        key(a).cmp(&key(b))
    });
    going
        .into_iter()
        .map(|(id, character, _)| (id, character))
        .collect()
}

/// One boss's groups: the officer's layout where there is one, with everyone
/// else filling the open slots in order. Whoever does not fit is on the bench.
fn layout(
    stored: Option<&Vec<Option<String>>>,
    going: &[&str],
    capacity: usize,
) -> (Vec<Option<String>>, Vec<String>) {
    let mut slots: Vec<Option<String>> = vec![None; capacity];
    let mut placed = HashSet::new();
    for (index, member) in stored.into_iter().flatten().take(capacity).enumerate() {
        if let Some(member) = member {
            if going.contains(&member.as_str()) && placed.insert(member.clone()) {
                slots[index] = Some(member.clone());
            }
        }
    }
    let mut bench = Vec::new();
    for member in going {
        if placed.contains(*member) {
            continue;
        }
        match slots.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some((*member).to_owned()),
            None => bench.push((*member).to_owned()),
        }
    }
    (slots, bench)
}

fn swap(slots: &mut [Option<String>], a: &Slot, b: &Slot) {
    match (a, b) {
        (Slot::Group(a), Slot::Group(b)) => slots.swap(*a, *b),
        (Slot::Group(index), Slot::Bench(member)) | (Slot::Bench(member), Slot::Group(index)) => {
            slots[*index] = Some(member.clone());
        }
        (Slot::Bench(_), Slot::Bench(_)) => {}
    }
}

/// How ART is told who a character is: Name-Realm, the realm without spaces.
fn art_name(character: &CharacterRow) -> Option<String> {
    let name = character.name.trim();
    if name.is_empty() || name.contains([',', '\n', '\r', '\t', '|', ';']) {
        return None;
    }
    let realm: String = character
        .realm
        .as_deref()?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if realm.is_empty() || realm.contains([',', '|', ';']) {
        return None;
    }
    Some(format!("{name}-{realm}"))
}

/// The import ART's Raid Groups takes: one preset per boss.
pub(crate) fn art_import<'a>(
    presets: impl IntoIterator<Item = (String, Vec<Option<&'a CharacterRow>>)>,
) -> String {
    let mut rows = vec!["preset,group,slot,character".to_owned()];
    for (name, slots) in presets {
        let name: String = name
            .chars()
            .filter(|c| !matches!(c, ',' | '\n' | '\r' | '\t' | '|' | ';'))
            .collect();
        for (index, character) in slots.into_iter().enumerate() {
            if let Some(character) = character.and_then(art_name) {
                rows.push(format!(
                    "{},{},{},{character}",
                    name.trim(),
                    index / SLOTS_PER_GROUP + 1,
                    index % SLOTS_PER_GROUP + 1
                ));
            }
        }
    }
    format!(
        "ART_RG2:{}",
        base64::engine::general_purpose::STANDARD.encode(rows.join("\n"))
    )
}

fn preset_name(raid: &PlannedRaid, number: Option<usize>, boss: &Boss) -> String {
    match number {
        Some(number) => format!("{} {} {number}", night_name(&raid.day), boss.name),
        None => format!("{} {}", night_name(&raid.day), boss.name),
    }
}

struct Check {
    ok: bool,
    text: String,
}

fn listed(names: &[String]) -> String {
    if names.len() > MAX_LISTED {
        format!(
            "{} and {} more",
            names[..MAX_LISTED].join(", "),
            names.len() - MAX_LISTED
        )
    } else {
        names.join(", ")
    }
}

fn checks(
    raid: &PlannedRaid,
    others: &[&PlannedRaid],
    members: &[&RosterMember],
    context: &Context,
    filled: usize,
    capacity: usize,
) -> Vec<Check> {
    let mut checks = vec![Check {
        ok: filled == capacity || (raid.difficulty != Difficulty::Mythic && filled >= 10),
        text: format!("{filled} of {capacity} slots filled"),
    }];
    let name_of = |member: &RosterMember| {
        raid.pick(&member.user_id)
            .and_then(|key| context.character(&member.user_id, key))
            .map_or_else(|| member.name.clone(), |c| c.name.clone())
    };
    let twice: Vec<String> = members
        .iter()
        .filter(|member| {
            raid.pick(&member.user_id).is_some_and(|key| {
                others
                    .iter()
                    .any(|other| other.day == raid.day && other.pick(&member.user_id) == Some(key))
            })
        })
        .map(|member| name_of(member))
        .collect();
    checks.push(if twice.is_empty() {
        Check {
            ok: true,
            text: "No character in two raids that night".into(),
        }
    } else {
        Check {
            ok: false,
            text: format!("Also in another raid that night: {}", listed(&twice)),
        }
    });
    let picked = |member: &&&RosterMember| raid.pick(&member.user_id).is_some();
    let out: Vec<String> = members
        .iter()
        .filter(picked)
        .filter(|member| context.absence(&member.user_id, raid) == Some(Kind::Out))
        .map(|member| name_of(member))
        .collect();
    if !out.is_empty() {
        checks.push(Check {
            ok: false,
            text: format!("Out: {}", listed(&out)),
        });
    }
    let saved: Vec<String> = members
        .iter()
        .filter(picked)
        .filter_map(|member| {
            let character = context.character(&member.user_id, raid.pick(&member.user_id)?)?;
            context
                .saved(character, raid)
                .then(|| character.name.clone())
        })
        .collect();
    if !saved.is_empty() {
        checks.push(Check {
            ok: false,
            text: format!("Saved: {}", listed(&saved)),
        });
    }
    let unnamed: Vec<String> = members
        .iter()
        .filter(picked)
        .filter(|member| context.absence(&member.user_id, raid) != Some(Kind::Out))
        .filter(|member| {
            raid.pick(&member.user_id)
                .and_then(|key| context.character(&member.user_id, key))
                .and_then(art_name)
                .is_none()
        })
        .map(|member| name_of(member))
        .collect();
    checks.push(if unnamed.is_empty() {
        Check {
            ok: true,
            text: "Every name has a realm".into(),
        }
    } else {
        Check {
            ok: false,
            text: format!("Left out, no realm: {}", listed(&unnamed)),
        }
    });
    checks
}

fn segment(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(12.0).color(primary_text()))
            .corner_radius(egui::CornerRadius::same(5))
            .fill(if on {
                Color32::from_rgb(45, 50, 60)
            } else {
                FIELD_FILL
            })
            .stroke(Stroke::new(
                1.0_f32,
                if on {
                    SELECTED
                } else {
                    Color32::from_rgb(47, 52, 61)
                },
            ))
            .min_size(egui::vec2(0.0, 30.0)),
    )
}

fn section(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    panel_frame()
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
            add(ui);
        });
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(15.0)
            .strong()
            .color(primary_text()),
    );
}

/// Draws the screen. `others` are the week's other raids.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    ui: &mut egui::Ui,
    state: &mut ArtUi,
    raid: &mut PlannedRaid,
    number: Option<usize>,
    others: &[&PlannedRaid],
    members: &[&RosterMember],
    context: &Context,
    characters: Option<&CharactersView>,
) -> Outcome {
    let mut outcome = Outcome::default();
    let bosses = bosses(raid, characters);
    state.boss = state.boss.min(bosses.len().saturating_sub(1));
    let capacity = capacity(raid);
    let going = going(raid, members, context);
    let ids: Vec<&str> = going.iter().map(|(id, _)| *id).collect();
    let character_of: HashMap<&str, Option<&CharacterRow>> = going.iter().copied().collect();

    control_row(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let back = ui.add(
            egui::Button::new("")
                .corner_radius(egui::CornerRadius::same(8))
                .fill(FIELD_FILL)
                .min_size(egui::vec2(CONTROL_HEIGHT, CONTROL_HEIGHT)),
        );
        arrow(ui, back.rect.center(), -1.0);
        if back.on_hover_text("Planner").clicked() {
            outcome.back = true;
        }
        let title = match number {
            Some(number) => format!("Send to ART · Raid {number}"),
            None => "Send to ART".to_owned(),
        };
        ui.label(
            RichText::new(title)
                .size(18.0)
                .strong()
                .color(primary_text()),
        );
        let mut about = format!("{} {}", raid.difficulty.label(), raid.mode.label());
        if let Some(week) = context.week {
            about.push_str(&format!(
                " · {}",
                crate::planner_ui::long_date(raid.night(week))
            ));
        }
        if let Some(start) = &raid.start {
            about.push_str(&format!(" {start}"));
        }
        about.push_str(&format!(" · {} players", going.len()));
        ui.label(RichText::new(about).size(13.0).color(secondary_text()));
    });
    ui.add_space(10.0);

    let boss = bosses.get(state.boss);
    let (mut slots, bench) = layout(
        boss.and_then(|boss| raid.groups.get(&boss.id.to_string())),
        &ids,
        capacity,
    );
    let filled = slots.iter().flatten().count();
    let checks = checks(raid, others, members, context, filled, capacity);

    let wide = ui.available_width() >= 900.0;
    let groups_width = if wide {
        ui.available_width() - SIDE_WIDTH - 18.0
    } else {
        ui.available_width()
    };
    // Layout changes land after both panels are drawn.
    let mut stored: Vec<(String, Vec<Option<String>>)> = Vec::new();
    let mut copied_at = state.copied;
    let mut groups_ui = |ui: &mut egui::Ui| {
        section(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                heading(ui, "Groups");
                ui.add_space(4.0);
                for (index, boss) in bosses.iter().enumerate() {
                    if segment(ui, &boss.name, index == state.boss).clicked() {
                        state.boss = index;
                        state.selected = None;
                    }
                }
            });
            let Some(boss) = boss else {
                ui.label(RichText::new("No bosses").color(muted_text()));
                return;
            };
            let groups = capacity / SLOTS_PER_GROUP;
            let column = ((ui.available_width() - GROUP_GAP * (groups - 1) as f32) / groups as f32)
                .max(60.0);
            let height = 8.0 + 18.0 + SLOT_HEIGHT * SLOTS_PER_GROUP as f32 + 6.0;
            let (area, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            let mut clicked = None;
            for group in 0..groups {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(
                        area.left() + group as f32 * (column + GROUP_GAP),
                        area.top(),
                    ),
                    egui::vec2(column, height),
                );
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(6), BOX_FILL);
                ui.painter().rect_stroke(
                    rect,
                    egui::CornerRadius::same(6),
                    Stroke::new(1.0_f32, line_color()),
                    egui::StrokeKind::Inside,
                );
                text_at(
                    ui,
                    egui::pos2(rect.left() + 8.0, rect.top() + 8.0),
                    egui::Align2::LEFT_TOP,
                    &format!("Group {}", group + 1),
                    11.0,
                    secondary_text(),
                );
                for slot in 0..SLOTS_PER_GROUP {
                    let index = group * SLOTS_PER_GROUP + slot;
                    let cell = egui::Rect::from_min_size(
                        egui::pos2(
                            rect.left() + 4.0,
                            rect.top() + 26.0 + slot as f32 * SLOT_HEIGHT,
                        ),
                        egui::vec2(column - 8.0, SLOT_HEIGHT - 2.0),
                    );
                    let response =
                        ui.interact(cell, ui.id().with(("slot", index)), egui::Sense::click());
                    let selected = state.selected == Some(Slot::Group(index));
                    if response.hovered() || selected {
                        ui.painter()
                            .rect_filled(cell, egui::CornerRadius::same(4), HOVER_FILL);
                    }
                    if selected {
                        ui.painter().rect_stroke(
                            cell,
                            egui::CornerRadius::same(4),
                            Stroke::new(1.0_f32, SELECTED),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if response.clicked() {
                        clicked = Some(Slot::Group(index));
                    }
                    let Some(member) = &slots[index] else {
                        continue;
                    };
                    let character = character_of.get(member.as_str()).copied().flatten();
                    let (name, color) = character.map_or_else(
                        || ("Unavailable".to_owned(), muted_text()),
                        |character| {
                            (
                                character.name.clone(),
                                character
                                    .class
                                    .as_deref()
                                    .map_or_else(secondary_text, class_color),
                            )
                        },
                    );
                    let galley = clipped(ui, single(&name, 12.0, color), cell.width() - 8.0);
                    ui.painter().galley(
                        egui::pos2(cell.left() + 4.0, cell.center().y - galley.size().y / 2.0),
                        galley,
                        color,
                    );
                }
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 6.0);
                ui.label(
                    RichText::new(format!("{filled} in groups"))
                        .size(12.0)
                        .color(secondary_text()),
                );
                ui.label(
                    RichText::new(format!("{} bench", bench.len()))
                        .size(12.0)
                        .color(secondary_text()),
                );
                for member in &bench {
                    let name = character_of
                        .get(member.as_str())
                        .copied()
                        .flatten()
                        .map_or_else(|| "Unavailable".to_owned(), |c| c.name.clone());
                    let selected = state.selected == Some(Slot::Bench(member.clone()));
                    let chip = ui.add(
                        egui::Button::new(RichText::new(name).size(12.0).color(warning_accent()))
                            .fill(if selected {
                                HOVER_FILL
                            } else {
                                Color32::TRANSPARENT
                            })
                            .stroke(if selected {
                                Stroke::new(1.0_f32, SELECTED)
                            } else {
                                Stroke::NONE
                            })
                            .corner_radius(egui::CornerRadius::same(4)),
                    );
                    if chip.clicked() {
                        clicked = Some(Slot::Bench(member.clone()));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if bosses.len() > 1 && bar_button(ui, "Use for every boss").clicked() {
                        for other in &bosses {
                            stored.push((other.id.to_string(), trimmed(&slots)));
                        }
                    }
                });
            });
            if let Some(clicked) = clicked {
                match state.selected.take() {
                    None => state.selected = Some(clicked),
                    Some(first) if first == clicked => {}
                    Some(first) => {
                        swap(&mut slots, &first, &clicked);
                        stored.push((boss.id.to_string(), trimmed(&slots)));
                    }
                }
            }
        });
    };
    let mut side_ui = |ui: &mut egui::Ui| {
        section(ui, |ui| {
            heading(ui, "Checks");
            for check in &checks {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 18.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        dot.center(),
                        5.0,
                        if check.ok {
                            success_accent()
                        } else {
                            warning_accent()
                        },
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(&check.text)
                                .size(13.0)
                                .color(Color32::from_rgb(183, 194, 208)),
                        )
                        .wrap(),
                    );
                });
            }
        });
        ui.add_space(12.0);
        let copied = copied_at.is_some_and(|at| at.elapsed() < COPIED_FOR);
        let (text, fill, stroke, color) = if copied {
            (
                "Copied for ART",
                Color32::from_rgb(26, 59, 42),
                success_accent(),
                success_accent(),
            )
        } else {
            ("Copy for ART", SEND, SEND, Color32::WHITE)
        };
        let width = ui.available_width();
        let button = ui
            .add_enabled_ui(!bosses.is_empty() && filled > 0, |ui| {
                ui.add_sized(
                    [width, 48.0],
                    egui::Button::new(RichText::new(text).size(15.0).strong().color(color))
                        .fill(fill)
                        .stroke(Stroke::new(1.0_f32, stroke))
                        .corner_radius(egui::CornerRadius::same(8)),
                )
            })
            .inner;
        if button.clicked() {
            let presets = bosses.iter().map(|boss| {
                let (slots, _) = layout(raid.groups.get(&boss.id.to_string()), &ids, capacity);
                (
                    preset_name(raid, number.filter(|_| shares_night(raid, others)), boss),
                    slots
                        .iter()
                        .map(|slot| {
                            slot.as_deref()
                                .and_then(|id| character_of.get(id).copied().flatten())
                        })
                        .collect(),
                )
            });
            ui.ctx().copy_text(art_import(presets));
            copied_at = Some(Instant::now());
        }
        if copied {
            ui.ctx().request_repaint_after(COPIED_FOR);
        }
    };
    if wide {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            ui.allocate_ui_with_layout(
                egui::vec2(groups_width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| groups_ui(ui),
            );
            ui.allocate_ui_with_layout(
                egui::vec2(SIDE_WIDTH, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| side_ui(ui),
            );
        });
    } else {
        groups_ui(ui);
        ui.add_space(12.0);
        side_ui(ui);
    }
    state.copied = copied_at;
    for (boss, slots) in stored {
        raid.groups.insert(boss, slots);
        outcome.changed = true;
    }
    outcome
}

fn shares_night(raid: &PlannedRaid, others: &[&PlannedRaid]) -> bool {
    others.iter().any(|other| other.day == raid.day)
}

/// A layout as stored: no trailing open slots.
fn trimmed(slots: &[Option<String>]) -> Vec<Option<String>> {
    let end = slots
        .iter()
        .rposition(Option::is_some)
        .map_or(0, |last| last + 1);
    slots[..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn character(name: &str, realm: Option<&str>) -> CharacterRow {
        CharacterRow {
            name: name.into(),
            realm: realm.map(Into::into),
            ..CharacterRow::default()
        }
    }

    #[test]
    fn the_import_is_what_art_raid_groups_reads() {
        let lucas = character("Lucas", Some("Draenor"));
        let smygis = character("Smygis", Some("Twisting Nether"));
        let code = art_import([(
            "Wed Nekzali, the Bound".to_owned(),
            vec![Some(&lucas), None, None, None, None, Some(&smygis)],
        )]);
        let payload = code.strip_prefix("ART_RG2:").unwrap();
        let rows = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            rows,
            "preset,group,slot,character\nWed Nekzali the Bound,1,1,Lucas-Draenor\nWed Nekzali the Bound,2,1,Smygis-TwistingNether"
        );
        assert_eq!(art_name(&character("Lucas", None)), None);
    }

    #[test]
    fn layouts_keep_the_officers_order_and_fill_the_rest() {
        let going = ["1", "2", "3", "4"];
        let stored = vec![
            Some("3".to_owned()),
            None,
            Some("9".to_owned()),
            Some("1".to_owned()),
        ];
        let (slots, bench) = layout(Some(&stored), &going, 5);
        assert_eq!(
            slots,
            [Some("3"), Some("2"), Some("4"), Some("1"), None].map(|s| s.map(String::from))
        );
        assert!(bench.is_empty());
        let (slots, bench) = layout(None, &going, 2);
        assert_eq!(slots, [Some("1".to_owned()), Some("2".to_owned())]);
        assert_eq!(bench, ["3", "4"]);

        let mut slots = slots;
        swap(&mut slots, &Slot::Bench("4".into()), &Slot::Group(0));
        assert_eq!(slots, [Some("4".to_owned()), Some("2".to_owned())]);
        swap(&mut slots, &Slot::Group(0), &Slot::Group(1));
        assert_eq!(
            trimmed(&slots),
            [Some("2".to_owned()), Some("4".to_owned())]
        );
        assert_eq!(
            trimmed(&[Some("1".to_owned()), None, None]),
            [Some("1".to_owned())]
        );
    }

    #[test]
    fn specs_sort_tanks_and_healers_first() {
        assert_eq!(spec_role("Brewmaster"), Some(RaidRole::Tank));
        assert_eq!(spec_role("Holy"), Some(RaidRole::Healer));
        assert_eq!(spec_role("Augmentation"), Some(RaidRole::Dps));
        assert_eq!(spec_role(""), None);
    }
}
