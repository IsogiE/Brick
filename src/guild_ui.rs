use std::{
    collections::{HashMap, HashSet},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText, Stroke};
use serde::Deserialize;

use crate::{
    discord_auth, download,
    presence::{self, Roster, RosterMember},
    profile::{self, ProfileUi, RaidRole},
    ui::{
        busy_indicator, capsule, empty_panel_message, muted_text, panel_background, panel_frame,
        panel_stroke, primary_text, roster_member_status, roster_notice, roster_refresh_label,
        secondary_text, section_title, success_accent, warning_accent,
    },
};

const MAX_VIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DETAIL_BYTES: u64 = 512 * 1024;
pub(crate) const MAX_ERROR_BYTES: u64 = 16 * 1024;
// The server reads Battle.net every 15 minutes; the roster cadence is plenty.
const VIEW_REFRESH_SECS: u64 = 60;
const TIER_PIECES: u8 = 5;
const GEAR_SLOTS: u8 = 16;
const EMBELLISHMENTS: u8 = 2;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CharactersView {
    pub can_edit_mains: bool,
    pub available: bool,
    pub ready_ilvl: Option<u32>,
    pub raids: Vec<RaidInstance>,
    pub members: Vec<MemberCharacters>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RaidInstance {
    pub id: u64,
    pub name: String,
    pub encounters: Vec<serde_json::Value>,
    pub tier: bool,
}

impl CharactersView {
    /// The current tier's raids. Servers before the tier picker sent none.
    pub(crate) fn tier(&self) -> HashSet<u64> {
        let tier: HashSet<u64> = self
            .raids
            .iter()
            .filter(|raid| raid.tier)
            .map(|raid| raid.id)
            .collect();
        if tier.is_empty() {
            self.raids.iter().map(|raid| raid.id).collect()
        } else {
            tier
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberCharacters {
    pub user_id: String,
    pub main: Option<String>,
    pub characters: Vec<CharacterRow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CharacterRow {
    pub key: String,
    pub name: String,
    pub realm: Option<String>,
    pub status: String,
    pub synced_at: Option<String>,
    pub class: Option<String>,
    pub spec: Option<String>,
    pub race: Option<String>,
    pub ilvl: Option<u32>,
    pub tier: u8,
    pub lockout: Lockout,
    pub keys: Keys,
    pub vault: Vault,
    pub checks: Option<Checks>,
    pub saved: Vec<SavedRaid>,
    pub main: bool,
}

impl CharacterRow {
    fn synced(&self) -> bool {
        self.status == "ok"
    }

    fn ready(&self, ready_ilvl: Option<u32>) -> bool {
        self.synced()
            && self.tier >= 4
            && self
                .ilvl
                .is_some_and(|ilvl| ilvl >= ready_ilvl.unwrap_or(0))
    }

    fn class_name(&self) -> &str {
        self.class.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Lockout {
    pub normal: u32,
    pub heroic: u32,
    pub mythic: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub runs: u32,
    pub best: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Vault {
    pub raid: u8,
    pub dungeon: u8,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Count {
    pub done: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Upgrades {
    pub items: u32,
    pub left: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Checks {
    pub enchants: Count,
    pub gems: Count,
    pub embellishments: u8,
    pub upgrades: Option<Upgrades>,
    pub myth: Option<u8>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SavedRaid {
    pub id: u64,
    pub name: String,
    pub normal: Vec<u64>,
    pub heroic: Vec<u64>,
    pub mythic: Vec<u64>,
    pub total: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CharacterDetail {
    pub gear: Vec<Gear>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Gear {
    pub slot: String,
    pub name: String,
    pub ilvl: Option<u32>,
    pub quality: Option<String>,
    pub track: Option<Track>,
    pub tier: bool,
    pub enchant: Option<String>,
    pub sockets: Sockets,
    pub embellished: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Track {
    pub name: String,
    pub level: u8,
    pub max: u8,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Sockets {
    pub total: u8,
    pub filled: u8,
}

type Reply<T> = mpsc::Receiver<Result<T, String>>;

struct Page {
    user_id: String,
    key: String,
}

#[derive(Default)]
pub struct GuildUi {
    view: Option<CharactersView>,
    view_error: Option<String>,
    view_rx: Option<Reply<CharactersView>>,
    last_fetch: Option<Instant>,
    expanded: HashSet<String>,
    seeded: bool,
    page: Option<Page>,
    details: HashMap<String, CharacterDetail>,
    detail_rx: Option<(String, Reply<CharacterDetail>)>,
    detail_error: Option<(String, String)>,
    main_rx: Option<(String, Reply<Option<String>>)>,
    tier_rx: Option<Reply<CharactersView>>,
    main_error: Option<String>,
}

impl GuildUi {
    /// Read the guild's characters. Gear is read per character on its page.
    pub fn refresh(&mut self, ctx: &egui::Context, force: bool) {
        if self.view_rx.is_some() || !presence::configured() {
            return;
        }
        if !force
            && self
                .last_fetch
                .is_some_and(|at| at.elapsed() < Duration::from_secs(VIEW_REFRESH_SECS))
        {
            return;
        }
        if force {
            self.details.clear();
            self.detail_error = None;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| fetch_characters(&access_token));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.view_rx = Some(rx);
        self.last_fetch = Some(Instant::now());
    }

    pub fn poll(&mut self) {
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
                    self.view_error = Some("Characters could not be loaded.".into());
                    self.view_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some((key, rx)) = &self.detail_rx {
            match rx.try_recv() {
                Ok(Ok(detail)) => {
                    self.details.insert(key.clone(), detail);
                    self.detail_rx = None;
                }
                Ok(Err(error)) => {
                    self.detail_error = Some((key.clone(), friendly_problem(&error)));
                    self.detail_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.detail_error =
                        Some((key.clone(), "This character could not be loaded.".into()));
                    self.detail_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.tier_rx {
            match rx.try_recv() {
                Ok(Ok(mut view)) => {
                    for member in &mut view.members {
                        sort_characters(&mut member.characters);
                    }
                    self.view = Some(view);
                    self.view_error = None;
                    self.tier_rx = None;
                }
                Ok(Err(error)) => {
                    self.view_error = Some(friendly_problem(&error));
                    self.tier_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.view_error = Some("The tier could not be saved.".into());
                    self.tier_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some((user_id, rx)) = &self.main_rx {
            match rx.try_recv() {
                Ok(Ok(main)) => {
                    let user_id = user_id.clone();
                    self.apply_main(&user_id, main);
                    self.main_error = None;
                    self.main_rx = None;
                }
                Ok(Err(error)) => {
                    self.main_error = Some(friendly_problem(&error));
                    self.main_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.main_error = Some("The main could not be saved.".into());
                    self.main_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn apply_main(&mut self, user_id: &str, main: Option<String>) {
        let Some(member) = self
            .view
            .as_mut()
            .and_then(|view| view.members.iter_mut().find(|m| m.user_id == user_id))
        else {
            return;
        };
        for character in &mut member.characters {
            character.main = main.as_deref() == Some(character.key.as_str());
        }
        member.main = main;
        sort_characters(&mut member.characters);
    }

    pub(crate) fn characters_view(&self) -> Option<&CharactersView> {
        self.view.as_ref()
    }

    fn members(&self) -> &[MemberCharacters] {
        self.view
            .as_ref()
            .map_or(&[][..], |view| view.members.as_slice())
    }

    fn member(&self, user_id: &str) -> Option<&MemberCharacters> {
        self.members()
            .iter()
            .find(|member| member.user_id == user_id)
    }

    fn open_page(&mut self, user_id: &str, key: &str) {
        self.page = Some(Page {
            user_id: user_id.to_owned(),
            key: key.to_owned(),
        });
        self.main_error = None;
    }

    fn request_detail(&mut self, ctx: &egui::Context, key: &str) {
        if self.details.contains_key(key)
            || self
                .detail_rx
                .as_ref()
                .is_some_and(|(pending, _)| pending == key)
            || self
                .detail_error
                .as_ref()
                .is_some_and(|(failed, _)| failed == key)
        {
            return;
        }
        let Some(path) = detail_path(key) else {
            self.detail_error = Some((key.to_owned(), "This character is unavailable.".into()));
            return;
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let result = discord_auth::current_access_token()
                .and_then(|access_token| fetch_detail(&access_token, &path));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.detail_rx = Some((key.to_owned(), rx));
    }

    fn set_main(&mut self, ctx: &egui::Context, own_id: &str, user_id: &str, key: &str) {
        if self.main_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let own_id = own_id.to_owned();
        let target = user_id.to_owned();
        let key = key.to_owned();
        crate::guild::spawn(move || {
            let _ = tx.send(save_main(&own_id, &target, &key));
            ctx.request_repaint();
        });
        self.main_rx = Some((user_id.to_owned(), rx));
        self.main_error = None;
    }

    fn set_tier(&mut self, ctx: &egui::Context, raids: Vec<u64>) {
        if self.tier_rx.is_some() || raids.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        crate::guild::spawn(move || {
            let _ = tx.send(save_tier(&raids));
            ctx.request_repaint();
        });
        self.tier_rx = Some(rx);
    }

    /// The Guild tab once the roster has loaded. Returns true when the user
    /// asked for a refresh.
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        roster: &Roster,
        own_id: &str,
        notice: Option<&str>,
        profile: &ProfileUi,
        role_change: &mut Option<(String, Option<RaidRole>)>,
    ) -> bool {
        if self.page.is_some() {
            return self.draw_page(ui, own_id);
        }
        let members = roster_order(roster);
        if !self.seeded && self.view.is_some() {
            self.expanded.insert(own_id.to_owned());
            self.seeded = true;
        }

        let mut refresh = false;
        let online = members.iter().filter(|member| member.online).count();
        let all_open = !members.is_empty()
            && members
                .iter()
                .all(|member| self.expanded.contains(&member.user_id));
        let mut tier_pick = None;
        control_row(ui, |ui| {
            ui.label(
                RichText::new("Roster")
                    .size(18.0)
                    .strong()
                    .color(primary_text()),
            );
            pill(ui, &format!("{online}/{} online", members.len()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if bar_button(ui, "Refresh").clicked() {
                    refresh = true;
                }
                if self.view.is_some()
                    && bar_button(
                        ui,
                        if all_open {
                            "Collapse all"
                        } else {
                            "Expand all"
                        },
                    )
                    .clicked()
                {
                    if all_open {
                        self.expanded.clear();
                    } else {
                        self.expanded = members.iter().map(|m| m.user_id.clone()).collect();
                    }
                }
                if let Some(view) = self
                    .view
                    .as_ref()
                    .filter(|view| view.can_edit_mains && !view.raids.is_empty())
                {
                    tier_pick = tier_picker(ui, view);
                }
                if self.view_rx.is_some() || self.tier_rx.is_some() {
                    busy_indicator(ui, 16.0, crate::ui::info_accent());
                }
            });
        });
        if let Some(raids) = tier_pick {
            self.set_tier(ui.ctx(), raids);
        }
        ui.add_space(10.0);

        let max_height = ui.available_height().max(0.0);
        let mut open = None;
        let mut toggle = None;
        egui::ScrollArea::vertical()
            .id_salt("guild-roster")
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
                panel_frame()
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        let columns = Columns::new(ui.available_rect_before_wrap());
                        draw_column_titles(ui, &columns);
                        let mut group = None;
                        for member in &members {
                            let role = profile::role_order(member.raid_role);
                            if group != Some(role) {
                                group = Some(role);
                                draw_group(ui, member.raid_role);
                            }
                            ui.push_id(&member.user_id, |ui| {
                                let characters = self.member(&member.user_id);
                                let expanded = self.expanded.contains(&member.user_id);
                                if draw_member_row(
                                    ui,
                                    member,
                                    characters,
                                    expanded,
                                    roster.can_edit_roles,
                                    profile,
                                    role_change,
                                ) {
                                    toggle = Some(member.user_id.clone());
                                }
                                if expanded {
                                    let ready = self.view.as_ref().and_then(|view| view.ready_ilvl);
                                    for character in characters.map_or(&[][..], |c| &c.characters) {
                                        if draw_character_row(ui, character, ready) {
                                            open = Some((
                                                member.user_id.clone(),
                                                character.key.clone(),
                                            ));
                                        }
                                    }
                                }
                            });
                        }
                    });
                ui.add_space(10.0);
                ui.label(
                    RichText::new(roster_refresh_label(
                        &roster.generated_at,
                        roster.online_window_seconds,
                    ))
                    .small()
                    .color(muted_text()),
                );
            });
        if let Some(user_id) = toggle {
            if !self.expanded.remove(&user_id) {
                self.expanded.insert(user_id);
            }
        }
        if let Some((user_id, key)) = open {
            self.open_page(&user_id, &key);
        }
        refresh
    }

    fn draw_page(&mut self, ui: &mut egui::Ui, own_id: &str) -> bool {
        let Some(page) = &self.page else {
            return false;
        };
        let Some(member) = self.member(&page.user_id).cloned() else {
            self.page = None;
            return false;
        };
        // An unlinked character falls back to the member's first one.
        let Some(character) = member
            .characters
            .iter()
            .find(|character| character.key == page.key)
            .or_else(|| member.characters.first())
            .cloned()
        else {
            self.page = None;
            return false;
        };
        let can_edit =
            own_id == member.user_id || self.view.as_ref().is_some_and(|view| view.can_edit_mains);
        let ready_ilvl = self.view.as_ref().and_then(|view| view.ready_ilvl);
        let mut refresh = false;
        let mut back = false;
        let mut chosen = None;
        let mut make_main = false;

        control_row(ui, |ui| {
            let button = bar_button(ui, "     Guild");
            arrow(ui, button.rect.left_center() + egui::vec2(18.0, 0.0), -1.0);
            if button.clicked() {
                back = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if bar_button(ui, "Refresh").clicked() {
                    refresh = true;
                }
                if can_edit && !character.main {
                    let busy = self.main_rx.is_some();
                    if ui
                        .add_enabled_ui(!busy, |ui| bar_button(ui, "Set as main"))
                        .inner
                        .clicked()
                    {
                        make_main = true;
                    }
                    if busy {
                        busy_indicator(ui, 16.0, crate::ui::info_accent());
                    }
                }
                bar_combo(
                    ui,
                    "guild-character",
                    300.0,
                    character_label(&character),
                    |ui| {
                        for option in &member.characters {
                            if ui
                                .selectable_label(
                                    option.key == character.key,
                                    character_label(option),
                                )
                                .clicked()
                            {
                                chosen = Some(option.key.clone());
                            }
                        }
                    },
                );
            });
        });
        if let Some(error) = &self.main_error {
            ui.add_space(8.0);
            roster_notice(ui, error);
        }
        ui.add_space(12.0);

        let view_raids = self
            .view
            .as_ref()
            .map(|view| view.raids.clone())
            .unwrap_or_default();
        self.request_detail(ui.ctx(), &character.key);
        let detail = self.details.get(&character.key);
        let detail_error = self
            .detail_error
            .as_ref()
            .filter(|(key, _)| key == &character.key)
            .map(|(_, error)| error.as_str());
        let loading = self
            .detail_rx
            .as_ref()
            .is_some_and(|(key, _)| key == &character.key);

        let max_height = ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("guild-character-page")
            .auto_shrink([false, true])
            .max_height(max_height)
            .min_scrolled_height(0.0)
            .show(ui, |ui| {
                let width = (ui.available_width() - 18.0).max(300.0);
                ui.set_width(width);
                draw_header(ui, &character, ready_ilvl);
                if !character.synced() {
                    return;
                }
                ui.add_space(14.0);
                draw_vault_and_lockouts(ui, &character, &view_raids);
                ui.add_space(14.0);
                if let Some(checks) = character.checks {
                    draw_checks(ui, &checks);
                    ui.add_space(18.0);
                }
                section_title(ui, "EQUIPPED");
                ui.add_space(8.0);
                match (detail, detail_error) {
                    (Some(detail), _) => draw_gear(ui, &detail.gear),
                    (None, Some(error)) => {
                        panel_frame().show(ui, |ui| {
                            empty_panel_message(ui, "Gear unavailable", error);
                        });
                    }
                    (None, None) if loading => {
                        busy_indicator(ui, 20.0, crate::ui::info_accent());
                    }
                    _ => {}
                }
            });

        if back {
            self.page = None;
            self.main_error = None;
        } else if let Some(key) = chosen {
            self.open_page(&member.user_id, &key);
        } else if make_main {
            self.set_main(ui.ctx(), own_id, &member.user_id, &character.key);
        }
        if refresh {
            self.detail_error = None;
            self.details.remove(&character.key);
        }
        refresh
    }
}

pub(crate) const CONTROL_HEIGHT: f32 = 34.0;
// The scroll areas below keep 18 px for their bar; controls above them stop at
// the same right edge as the panels.
pub(crate) const SCROLL_GUTTER: f32 = 18.0;

pub(crate) fn control_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = (ui.available_width() - SCROLL_GUTTER).max(300.0);
    ui.allocate_ui_with_layout(
        egui::vec2(width, CONTROL_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, CONTROL_HEIGHT));
            add(ui);
        },
    );
}

pub(crate) fn bar_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).strong().color(primary_text()))
            .corner_radius(egui::CornerRadius::same(8))
            .fill(Color32::from_rgb(38, 42, 50))
            .min_size(egui::vec2(0.0, CONTROL_HEIGHT)),
    )
}

/// A dropdown the height of the buttons beside it.
pub(crate) fn bar_combo(
    ui: &mut egui::Ui,
    id: &str,
    width: f32,
    selected: RichText,
    menu: impl FnOnce(&mut egui::Ui),
) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = CONTROL_HEIGHT;
        egui::ComboBox::from_id_salt(id)
            .width(width)
            .height(420.0)
            .selected_text(selected)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, menu);
    });
}

/// A fixed-height count beside a title.
pub(crate) fn pill(ui: &mut egui::Ui, text: &str) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(11.0),
        secondary_text(),
    );
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(galley.size().x + 18.0, 22.0),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), line_color());
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        secondary_text(),
    );
}

/// Officers pick the current tier's raids. Returns the new list when it changed.
fn tier_picker(ui: &mut egui::Ui, view: &CharactersView) -> Option<Vec<u64>> {
    let tier = view.tier();
    let names: Vec<&str> = view
        .raids
        .iter()
        .filter(|raid| tier.contains(&raid.id))
        .map(|raid| short_raid(&raid.name))
        .collect();
    let mut picked = None;
    bar_combo(
        ui,
        "guild-tier",
        260.0,
        RichText::new(names.join(" · ")).color(primary_text()),
        |ui| {
            for raid in &view.raids {
                let mut on = tier.contains(&raid.id);
                if ui.checkbox(&mut on, &raid.name).changed() {
                    let next: Vec<u64> = view
                        .raids
                        .iter()
                        .map(|known| known.id)
                        .filter(|id| {
                            if *id == raid.id {
                                on
                            } else {
                                tier.contains(id)
                            }
                        })
                        .collect();
                    if !next.is_empty() {
                        picked = Some(next);
                    }
                }
            }
        },
    );
    picked
}

/// "The Unbinding of Kith'ix" reads "Unbinding of Kith'ix" where space is short.
pub(crate) fn short_raid(name: &str) -> &str {
    name.strip_prefix("The ").unwrap_or(name)
}

/// Tanks, healers, DPS, then no role; online before offline before not reporting; then by name.
fn roster_order(roster: &Roster) -> Vec<&RosterMember> {
    let mut members: Vec<&RosterMember> = roster.officers.iter().chain(&roster.raiders).collect();
    members.sort_by_cached_key(|member| {
        let presence = if member.online {
            0
        } else if member.last_seen_at.is_some() {
            1
        } else {
            2
        };
        (
            profile::role_order(member.raid_role),
            presence,
            member.name.to_lowercase(),
            member.user_id.clone(),
        )
    });
    members
}

fn sort_characters(characters: &mut [CharacterRow]) {
    characters.sort_by(|a, b| {
        b.main
            .cmp(&a.main)
            .then(b.ilvl.unwrap_or(0).cmp(&a.ilvl.unwrap_or(0)))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn friendly_problem(error: &str) -> String {
    let message = error.split_once(": ").map_or(error, |(_, rest)| rest);
    if message.contains("HTTP 404") || message == "Not found." {
        "Characters are not available on this server yet.".into()
    } else if message.contains("HTTP") || message.trim().is_empty() {
        "Characters could not be loaded. Brick will try again.".into()
    } else {
        message.to_owned()
    }
}

pub(crate) fn get(
    access: &crate::guild::Access,
    path: &str,
    max: u64,
    prefix: &str,
) -> Result<Vec<u8>, String> {
    let client = presence::http_client()?;
    let response = client
        .get(access.endpoint(path)?)
        .bearer_auth(access.secret())
        .send()
        .map_err(|_| format!("{prefix}: the server could not be reached."))?;
    let status = response.status();
    let limit = if status.is_success() {
        max
    } else {
        MAX_ERROR_BYTES
    };
    let body = download::read_response(response, limit, prefix)?;
    if !status.is_success() {
        return Err(presence::profile_error(prefix, status, &body));
    }
    Ok(body)
}

pub fn fetch_characters(access: &crate::guild::Access) -> Result<CharactersView, String> {
    let body = get(access, "/v1/characters", MAX_VIEW_BYTES, "Characters")?;
    let mut view = serde_json::from_slice::<CharactersView>(&body)
        .map_err(|_| "Characters: the server sent an invalid response.".to_string())?;
    for member in &mut view.members {
        sort_characters(&mut member.characters);
    }
    Ok(view)
}

fn fetch_detail(access: &crate::guild::Access, path: &str) -> Result<CharacterDetail, String> {
    let body = get(access, path, MAX_DETAIL_BYTES, "Character")?;
    serde_json::from_slice(&body)
        .map_err(|_| "Character: the server sent an invalid response.".to_string())
}

fn save_main(own_id: &str, user_id: &str, key: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct Saved {
        main: Option<String>,
    }
    let token = discord_auth::current_or_refreshed_access_token()?
        .ok_or_else(|| "Sign in again to set a main.".to_string())?;
    let (status, body) = presence::profile_call(
        reqwest::Method::PUT,
        &format!("/v1/profiles/{user_id}/main"),
        &token,
        own_id,
        Some(&serde_json::json!({ "main": key })),
    )?;
    if !status.is_success() {
        return Err(presence::profile_error("Main", status, &body));
    }
    serde_json::from_slice::<Saved>(&body)
        .map(|saved| saved.main)
        .map_err(|_| "Main: the server sent an invalid response.".to_string())
}

/// The answer is the whole Guild tab list, so it reads with the list's limit.
fn save_tier(raids: &[u64]) -> Result<CharactersView, String> {
    let token = discord_auth::current_or_refreshed_access_token()?
        .ok_or_else(|| "Sign in again to set the tier.".to_string())?;
    let response = presence::http_client()?
        .put(token.endpoint("/v1/characters/tier")?)
        .bearer_auth(token.secret())
        .json(&serde_json::json!({ "raids": raids }))
        .send()
        .map_err(|_| "Tier: the server could not be reached.".to_string())?;
    let status = response.status();
    let limit = if status.is_success() {
        MAX_VIEW_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let body = download::read_response(response, limit, "Tier")?;
    if !status.is_success() {
        return Err(presence::profile_error("Tier", status, &body));
    }
    serde_json::from_slice(&body)
        .map_err(|_| "Tier: the server sent an invalid response.".to_string())
}

/// "eu:draenor:lucas" reads /v1/characters/eu/draenor/lucas. The server encodes
/// names in lower case, which keeps the path free of separators.
fn detail_path(key: &str) -> Option<String> {
    let mut parts = key.splitn(3, ':');
    let (region, realm, name) = (parts.next()?, parts.next()?, parts.next()?);
    let valid = matches!(region, "eu" | "us" | "kr" | "tw")
        && !realm.is_empty()
        && realm
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.is_empty()
        && name.chars().all(char::is_lowercase);
    valid.then(|| format!("/v1/characters/{region}/{realm}/{name}"))
}

pub fn class_color(class: &str) -> Color32 {
    match class {
        "Death Knight" => Color32::from_rgb(196, 30, 58),
        "Demon Hunter" => Color32::from_rgb(163, 48, 201),
        "Druid" => Color32::from_rgb(255, 124, 10),
        "Evoker" => Color32::from_rgb(51, 147, 127),
        "Hunter" => Color32::from_rgb(170, 211, 114),
        "Mage" => Color32::from_rgb(63, 199, 235),
        "Monk" => Color32::from_rgb(0, 255, 152),
        "Paladin" => Color32::from_rgb(244, 140, 186),
        "Priest" => Color32::from_rgb(255, 255, 255),
        "Rogue" => Color32::from_rgb(255, 244, 104),
        "Shaman" => Color32::from_rgb(0, 112, 221),
        "Warlock" => Color32::from_rgb(135, 136, 238),
        "Warrior" => Color32::from_rgb(198, 155, 109),
        _ => secondary_text(),
    }
}

/// Tier tokens: Dreadful, Mystic, Venerated and Zenith.
pub(crate) fn token(class: &str) -> Option<usize> {
    match class {
        "Death Knight" | "Demon Hunter" | "Warlock" => Some(0),
        "Druid" | "Hunter" | "Mage" => Some(1),
        "Paladin" | "Priest" | "Shaman" => Some(2),
        "Evoker" | "Monk" | "Rogue" | "Warrior" => Some(3),
        _ => None,
    }
}

fn quality_color(quality: Option<&str>) -> Color32 {
    match quality {
        Some("POOR") => Color32::from_rgb(157, 157, 157),
        Some("UNCOMMON") => Color32::from_rgb(30, 255, 0),
        Some("RARE") => Color32::from_rgb(0, 112, 221),
        Some("EPIC") => Color32::from_rgb(163, 53, 238),
        Some("LEGENDARY") => Color32::from_rgb(255, 128, 0),
        Some("ARTIFACT" | "HEIRLOOM") => Color32::from_rgb(230, 204, 128),
        _ => primary_text(),
    }
}

fn slot_label(slot: &str) -> &'static str {
    match slot {
        "HEAD" => "Head",
        "NECK" => "Neck",
        "SHOULDER" => "Shoulders",
        "BACK" => "Back",
        "CHEST" => "Chest",
        "WRIST" => "Wrist",
        "HANDS" => "Hands",
        "WAIST" => "Waist",
        "LEGS" => "Legs",
        "FEET" => "Feet",
        "FINGER_1" | "FINGER_2" => "Ring",
        "TRINKET_1" | "TRINKET_2" => "Trinket",
        "MAIN_HAND" => "Main hand",
        "OFF_HAND" => "Off hand",
        _ => "",
    }
}

fn character_label(character: &CharacterRow) -> RichText {
    let mut parts = vec![character.name.clone()];
    if let Some(class) = &character.class {
        parts.push(class.clone());
    }
    if let Some(ilvl) = character.ilvl {
        parts.push(ilvl.to_string());
    }
    if character.main {
        parts.push("Main".into());
    }
    RichText::new(parts.join(" · ")).color(class_color(character.class_name()))
}

fn dim() -> Color32 {
    Color32::from_rgb(94, 102, 116)
}

pub(crate) fn line_color() -> Color32 {
    Color32::from_rgb(38, 42, 50)
}

pub(crate) fn text_at(
    ui: &egui::Ui,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    size: f32,
    color: Color32,
) -> egui::Rect {
    ui.painter()
        .text(pos, align, text, egui::FontId::proportional(size), color)
}

/// One line of text cut to `width`, ending in an ellipsis.
pub(crate) fn clipped(ui: &egui::Ui, job: egui::text::LayoutJob, width: f32) -> Arc<egui::Galley> {
    let mut job = job;
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

pub(crate) fn single(text: &str, size: f32, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    append(&mut job, text, size, color);
    job
}

pub(crate) fn append(job: &mut egui::text::LayoutJob, text: &str, size: f32, color: Color32) {
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::proportional(size),
            color,
            ..Default::default()
        },
    );
}

// Column positions shared by the title row and every character row.
struct Columns {
    name: f32,
    name_end: f32,
    race: Option<f32>,
    ilvl: f32,
    tier: f32,
    vault: Option<f32>,
    keys: f32,
    myth: f32,
}

impl Columns {
    const VAULT_WIDTH: f32 = 6.0 * 14.0;

    fn new(rect: egui::Rect) -> Self {
        let wide = rect.width() >= 820.0;
        let mut x = rect.right() - 8.0;
        let myth = x;
        x -= 56.0 + 14.0;
        let keys = x;
        x -= 40.0 + 22.0;
        let vault = wide.then(|| {
            let left = x - Self::VAULT_WIDTH;
            x = left - 22.0;
            left
        });
        let tier = x;
        x -= 40.0 + 16.0;
        let ilvl = x;
        x -= 48.0 + 22.0;
        let race = wide.then(|| {
            let left = x - 120.0;
            x = left - 12.0;
            left
        });
        Self {
            name: rect.left() + 46.0,
            name_end: x,
            race,
            ilvl,
            tier,
            vault,
            keys,
            myth,
        }
    }
}

fn draw_column_titles(ui: &mut egui::Ui, columns: &Columns) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let y = rect.center().y;
    let color = muted_text();
    let left = |x: f32, text: &str| {
        text_at(
            ui,
            egui::pos2(x, y),
            egui::Align2::LEFT_CENTER,
            text,
            12.0,
            color,
        );
    };
    let right = |x: f32, text: &str| {
        text_at(
            ui,
            egui::pos2(x, y),
            egui::Align2::RIGHT_CENTER,
            text,
            12.0,
            color,
        );
    };
    left(columns.name, "Character");
    if let Some(x) = columns.race {
        left(x, "Race");
    }
    right(columns.ilvl, "ilvl");
    right(columns.tier, "Tier");
    if let Some(x) = columns.vault {
        left(x, "Vault");
    }
    right(columns.keys, "Keys");
    right(columns.myth, "Myth");
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0_f32, line_color()),
    );
}

/// The role divider above each group, as on Attendance.
fn draw_group(ui: &mut egui::Ui, role: Option<RaidRole>) {
    let label = match role {
        Some(RaidRole::Tank) => "TANKS",
        Some(RaidRole::Healer) => "HEALERS",
        Some(RaidRole::Dps) => "DPS",
        None => "NO ROLE",
    };
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), crate::attendance_ui::GROUP_HEIGHT),
        egui::Sense::hover(),
    );
    text_at(
        ui,
        egui::pos2(rect.left() + 4.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        11.0,
        Color32::from_rgb(164, 176, 193),
    );
}

/// The member line: avatar, name, characters and presence. Returns true when
/// it was clicked to expand or collapse.
fn draw_member_row(
    ui: &mut egui::Ui,
    member: &RosterMember,
    characters: Option<&MemberCharacters>,
    expanded: bool,
    can_edit_roles: bool,
    profile: &ProfileUi,
    role_change: &mut Option<(String, Option<RaidRole>)>,
) -> bool {
    let count = characters.map_or(0, |c| c.characters.len());
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 50.0),
        if count > 0 {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        Stroke::new(1.0_f32, line_color()),
    );
    if !ui.is_rect_visible(rect) {
        return false;
    }
    if response.hovered() && count > 0 {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(6),
            Color32::from_rgb(33, 37, 44),
        );
    }
    let y = rect.center().y;
    let center = egui::pos2(rect.left() + 20.0, y);
    crate::avatars::draw(
        ui,
        &member.name,
        member.avatar.as_deref(),
        center,
        16.0,
        12.0,
    );

    let chevron_width = if count > 0 { 52.0 } else { 0.0 };
    let role_width = if can_edit_roles { 106.0 } else { 0.0 };
    let status_width = 190.0;
    let status_right = rect.right() - chevron_width - role_width - 8.0;
    let name_left = rect.left() + 46.0;
    let name_right = (status_right - status_width - 16.0).max(name_left + 60.0);

    let mut x = name_left;
    if member.raid_role.is_some() {
        let icon_rect = egui::Rect::from_min_size(egui::pos2(x, y - 12.0), egui::vec2(22.0, 24.0));
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(icon_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| profile::role_icon(ui, member.raid_role),
        );
        x += 24.0;
    }
    let mut job = single(&member.name, 15.0, primary_text());
    if let Some(summary) = characters.and_then(member_summary) {
        append(&mut job, &format!("   {summary}"), 12.0, muted_text());
    }
    let galley = clipped(ui, job, name_right - x);
    let name_rect =
        egui::Rect::from_min_size(egui::pos2(x, y - galley.size().y / 2.0), galley.size());
    ui.painter().galley(name_rect.min, galley, primary_text());
    ui.interact(name_rect, ui.id().with("name"), egui::Sense::hover())
        .on_hover_text(format!("{} - {}", member.role, member.user_id));

    let status = roster_member_status(member);
    let status_rect = text_at(
        ui,
        egui::pos2(status_right, y),
        egui::Align2::RIGHT_CENTER,
        &status,
        12.0,
        if member.online {
            secondary_text()
        } else {
            muted_text()
        },
    );
    let dot = if member.online {
        success_accent()
    } else {
        Color32::from_rgb(87, 95, 108)
    };
    ui.painter()
        .circle_filled(egui::pos2(status_rect.left() - 12.0, y), 5.0, dot);

    if can_edit_roles {
        let picker = egui::Rect::from_min_max(
            egui::pos2(rect.right() - chevron_width - role_width + 6.0, rect.top()),
            egui::pos2(rect.right() - chevron_width, rect.bottom()),
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(picker)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                let mut role = profile.role_for_member(&member.user_id, member.raid_role);
                if profile::role_picker(ui, ("roster-role", &member.user_id), &mut role) {
                    *role_change = Some((member.user_id.clone(), role));
                }
            },
        );
    }
    if count > 0 {
        let chip = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 26.0, y),
            egui::vec2(44.0, 26.0),
        );
        ui.painter()
            .rect_filled(chip, egui::CornerRadius::same(6), line_color());
        chevron(ui, chip.center() - egui::vec2(8.0, 0.0), expanded);
        text_at(
            ui,
            chip.center() + egui::vec2(9.0, 0.0),
            egui::Align2::CENTER_CENTER,
            &count.to_string(),
            12.0,
            secondary_text(),
        );
    }
    response.clicked() && count > 0
}

/// A small open or closed triangle, drawn because the UI font has no arrows.
fn chevron(ui: &egui::Ui, center: egui::Pos2, open: bool) {
    let (dy, tip) = if open { (2.5, -3.5) } else { (-2.5, 3.5) };
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            center + egui::vec2(-4.5, dy),
            center + egui::vec2(4.5, dy),
            center + egui::vec2(0.0, tip),
        ],
        secondary_text(),
        Stroke::NONE,
    ));
}

/// A left (-1) or right (1) arrow for buttons.
pub(crate) fn arrow(ui: &egui::Ui, center: egui::Pos2, direction: f32) {
    let stroke = Stroke::new(1.6_f32, primary_text());
    let tip = center + egui::vec2(5.0 * direction, 0.0);
    let painter = ui.painter();
    painter.line_segment([center - egui::vec2(5.0 * direction, 0.0), tip], stroke);
    painter.line_segment([tip, tip + egui::vec2(-4.0 * direction, -4.0)], stroke);
    painter.line_segment([tip, tip + egui::vec2(-4.0 * direction, 4.0)], stroke);
}

fn member_summary(characters: &MemberCharacters) -> Option<String> {
    let first = characters.characters.first()?;
    let count = characters.characters.len();
    let noun = if count == 1 {
        "character"
    } else {
        "characters"
    };
    let role = [first.spec.as_deref(), first.class.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    Some(if role.is_empty() {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun} · {role}")
    })
}

/// One character under its member. Returns true when it was clicked.
fn draw_character_row(
    ui: &mut egui::Ui,
    character: &CharacterRow,
    ready_ilvl: Option<u32>,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(6),
            Color32::from_rgb(45, 50, 60),
        );
    }
    let columns = Columns::new(rect);
    let y = rect.center().y;
    let color = class_color(character.class_name());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(egui::pos2(columns.name + 3.0, y), egui::vec2(6.0, 22.0)),
        egui::CornerRadius::same(2),
        color,
    );
    let name_left = columns.name + 16.0;
    let tag_width = if character.main { 44.0 } else { 0.0 };
    let galley = clipped(
        ui,
        single(&character.name, 14.0, color),
        columns.name_end - name_left - tag_width,
    );
    let name_width = galley.size().x;
    ui.painter().galley(
        egui::pos2(name_left, y - galley.size().y / 2.0),
        galley,
        color,
    );
    if character.main {
        let tag = egui::Rect::from_min_size(
            egui::pos2(name_left + name_width + 8.0, y - 9.0),
            egui::vec2(34.0, 18.0),
        );
        ui.painter().rect_stroke(
            tag,
            egui::CornerRadius::same(4),
            Stroke::new(1.0_f32, Color32::from_rgb(74, 80, 92)),
            egui::StrokeKind::Inside,
        );
        text_at(
            ui,
            tag.center(),
            egui::Align2::CENTER_CENTER,
            "Main",
            10.0,
            secondary_text(),
        );
    }

    let dash = |x: f32| {
        text_at(
            ui,
            egui::pos2(x, y),
            egui::Align2::RIGHT_CENTER,
            "–",
            13.0,
            dim(),
        );
    };
    if let Some(x) = columns.race {
        if let Some(race) = &character.race {
            let galley = clipped(ui, single(race, 13.0, secondary_text()), 116.0);
            ui.painter().galley(
                egui::pos2(x, y - galley.size().y / 2.0),
                galley,
                secondary_text(),
            );
        }
    }
    if !character.synced() {
        for x in [columns.ilvl, columns.tier, columns.keys, columns.myth] {
            dash(x);
        }
        return response.clicked();
    }

    let ilvl_color = if character.ready(ready_ilvl) {
        success_accent()
    } else {
        primary_text()
    };
    match character.ilvl {
        Some(ilvl) => {
            text_at(
                ui,
                egui::pos2(columns.ilvl, y),
                egui::Align2::RIGHT_CENTER,
                &ilvl.to_string(),
                14.0,
                ilvl_color,
            );
        }
        None => dash(columns.ilvl),
    }
    text_at(
        ui,
        egui::pos2(columns.tier, y),
        egui::Align2::RIGHT_CENTER,
        &format!("{}/{TIER_PIECES}", character.tier),
        13.0,
        if character.tier >= 4 {
            primary_text()
        } else {
            secondary_text()
        },
    );
    if let Some(x) = columns.vault {
        let filled = character.vault.raid.min(3) + character.vault.dungeon.min(3);
        for slot in 0..6u8 {
            let square = egui::Rect::from_min_size(
                egui::pos2(x + f32::from(slot) * 14.0, y - 6.0),
                egui::vec2(12.0, 12.0),
            );
            if slot < filled {
                ui.painter()
                    .rect_filled(square, egui::CornerRadius::same(2), success_accent());
            } else {
                ui.painter().rect_stroke(
                    square,
                    egui::CornerRadius::same(2),
                    Stroke::new(1.0_f32, Color32::from_rgb(74, 80, 92)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
    text_at(
        ui,
        egui::pos2(columns.keys, y),
        egui::Align2::RIGHT_CENTER,
        &character.keys.runs.to_string(),
        13.0,
        if character.keys.runs == 0 {
            dim()
        } else {
            secondary_text()
        },
    );
    match character.checks.and_then(|checks| checks.myth) {
        Some(myth) => {
            let color = if myth >= 12 {
                Color32::from_rgb(255, 128, 0)
            } else if myth >= 6 {
                secondary_text()
            } else {
                dim()
            };
            text_at(
                ui,
                egui::pos2(columns.myth, y),
                egui::Align2::RIGHT_CENTER,
                &format!("{myth}/{GEAR_SLOTS}"),
                13.0,
                color,
            );
        }
        None => dash(columns.myth),
    }
    response.clicked()
}

fn updated_label(synced_at: Option<&str>) -> Option<String> {
    let at =
        time::OffsetDateTime::parse(synced_at?, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let minutes = (time::OffsetDateTime::now_utc() - at)
        .whole_minutes()
        .max(0);
    Some(match minutes {
        0 => "Updated just now".into(),
        1..=59 => format!("Updated {minutes} min ago"),
        60..=2879 => format!("Updated {} h ago", minutes / 60),
        _ => format!("Updated {} days ago", minutes / 1440),
    })
}

fn draw_header(ui: &mut egui::Ui, character: &CharacterRow, ready_ilvl: Option<u32>) {
    panel_frame()
        .inner_margin(egui::Margin::symmetric(22, 18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&character.name)
                        .size(26.0)
                        .strong()
                        .color(class_color(character.class_name())),
                );
                if character.main {
                    ui.label(
                        RichText::new("MAIN")
                            .small()
                            .strong()
                            .color(secondary_text()),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(updated) = updated_label(character.synced_at.as_deref()) {
                        ui.label(RichText::new(updated).small().color(muted_text()));
                    }
                });
            });
            let subtitle = [
                [character.spec.as_deref(), character.class.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
                character.race.clone().unwrap_or_default(),
                character.realm.clone().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).color(secondary_text()));
            }
            if !character.synced() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(if character.status == "missing" {
                        "Battle.net has no profile for this character."
                    } else {
                        "Waiting for the next sync."
                    })
                    .color(muted_text()),
                );
                return;
            }
            ui.add_space(16.0);
            let lockout = character.lockout;
            let tier_detail = match character.tier {
                4.. => "4-set active",
                2..=3 => "2-set active",
                _ => "No set bonus",
            };
            let keys_detail = match character.keys.best {
                Some(best) => format!("this week · best +{best}"),
                None => "this week".into(),
            };
            let stats = [
                (
                    "ITEM LEVEL",
                    character.ilvl.map_or("–".into(), |ilvl| ilvl.to_string()),
                    "equipped".to_string(),
                    if character.ready(ready_ilvl) {
                        success_accent()
                    } else {
                        primary_text()
                    },
                ),
                (
                    "RAID",
                    format!(
                        "{} · {} · {}",
                        lockout.normal, lockout.heroic, lockout.mythic
                    ),
                    "N · H · M this reset".to_string(),
                    primary_text(),
                ),
                (
                    "TIER",
                    format!("{}/{TIER_PIECES}", character.tier),
                    tier_detail.to_string(),
                    primary_text(),
                ),
                (
                    "KEYS",
                    character.keys.runs.to_string(),
                    keys_detail,
                    primary_text(),
                ),
                (
                    "MYTH",
                    character
                        .checks
                        .and_then(|checks| checks.myth)
                        .map_or("–".into(), |myth| format!("{myth}/{GEAR_SLOTS}")),
                    "items on Myth track".to_string(),
                    primary_text(),
                ),
            ];
            let width = ui.available_width() / stats.len() as f32;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (label, value, detail, color) in stats {
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 64.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            ui.label(
                                RichText::new(label)
                                    .size(11.0)
                                    .color(Color32::from_rgb(164, 176, 193)),
                            );
                            ui.label(RichText::new(value).size(24.0).strong().color(color));
                            ui.label(RichText::new(detail).small().color(muted_text()));
                        },
                    );
                }
            });
        });
}

fn draw_vault_and_lockouts(ui: &mut egui::Ui, character: &CharacterRow, raids: &[RaidInstance]) {
    let flagged: Vec<&RaidInstance> = raids.iter().filter(|raid| raid.tier).collect();
    let tier: Vec<&RaidInstance> = if flagged.is_empty() {
        raids.iter().collect()
    } else {
        flagged
    };
    let saved = |id: u64| character.saved.iter().find(|raid| raid.id == id);
    let gap = 14.0;
    let wide = ui.available_width() >= 760.0;
    let width = if wide {
        (ui.available_width() - gap) / 2.0
    } else {
        ui.available_width()
    };
    // Side by side, both cards take the taller one's height.
    let vault_height: f32 = 194.0;
    let lockouts_height = 26.0 + 30.0 * tier.len().max(1) as f32;
    let height = if wide {
        vault_height.max(lockouts_height)
    } else {
        0.0
    };
    let vault = |ui: &mut egui::Ui| {
        card(ui, width, height.max(vault_height), "GREAT VAULT", |ui| {
            let bosses = tier
                .iter()
                .filter_map(|raid| saved(raid.id))
                .flat_map(|raid| raid.normal.iter().chain(&raid.heroic).chain(&raid.mythic))
                .collect::<HashSet<_>>()
                .len() as u32;
            vault_track(ui, "Raid", bosses, [2, 4, 6], ("boss", "bosses"));
            ui.add_space(10.0);
            vault_track(
                ui,
                "Mythic+",
                character.keys.runs,
                [1, 4, 8],
                ("run", "runs"),
            );
        });
    };
    let lockouts = |ui: &mut egui::Ui| {
        card(ui, width, height.max(lockouts_height), "LOCKOUTS", |ui| {
            for raid in &tier {
                let kills = saved(raid.id);
                let total = if raid.encounters.is_empty() {
                    kills.map_or(0, |kills| kills.total)
                } else {
                    raid.encounters.len() as u32
                };
                let count =
                    |pick: fn(&SavedRaid) -> &Vec<u64>| kills.map_or(0, |k| pick(k).len() as u32);
                lockout_line(
                    ui,
                    &raid.name,
                    [
                        count(|k| &k.normal),
                        count(|k| &k.heroic),
                        count(|k| &k.mythic),
                    ],
                    total,
                );
            }
        });
    };
    if wide {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            vault(ui);
            lockouts(ui);
        });
    } else {
        vault(ui);
        ui.add_space(gap);
        lockouts(ui);
    }
}

fn card(ui: &mut egui::Ui, width: f32, height: f32, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            panel_frame()
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    // The frame adds its margin and stroke around this.
                    ui.set_width(width - 34.0);
                    ui.set_min_height(height - 34.0);
                    section_title(ui, title);
                    ui.add_space(8.0);
                    body(ui);
                });
        },
    );
}

fn vault_track(
    ui: &mut egui::Ui,
    label: &str,
    progress: u32,
    steps: [u32; 3],
    (one, many): (&str, &str),
) {
    let unlocked = steps.iter().filter(|step| progress >= **step).count();
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(secondary_text()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{unlocked}/3")).color(secondary_text()));
        });
    });
    let gap = 8.0;
    let width = (ui.available_width() - gap * 2.0) / 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for step in steps {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 32.0), egui::Sense::hover());
            let done = progress >= step;
            let (fill, color, text) = if done {
                (
                    Color32::from_rgb(26, 59, 42),
                    success_accent(),
                    format!("{step} {}", if step == 1 { one } else { many }),
                )
            } else {
                (line_color(), dim(), format!("{progress} of {step}"))
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(5), fill);
            text_at(
                ui,
                rect.center(),
                egui::Align2::CENTER_CENTER,
                &text,
                13.0,
                color,
            );
        }
    });
}

fn lockout_line(ui: &mut egui::Ui, name: &str, kills: [u32; 3], total: u32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let y = rect.center().y;
    let column = 64.0;
    let names_end = rect.right() - column * 3.0 - 8.0;
    let galley = clipped(
        ui,
        single(name, 13.0, primary_text()),
        names_end - rect.left(),
    );
    ui.painter().galley(
        egui::pos2(rect.left(), y - galley.size().y / 2.0),
        galley,
        primary_text(),
    );
    for (index, (label, count)) in ["N", "H", "M"].into_iter().zip(kills).enumerate() {
        let right = rect.right() - column * (2 - index) as f32;
        let color = if count == 0 {
            dim()
        } else if count >= total {
            secondary_text()
        } else {
            warning_accent()
        };
        text_at(
            ui,
            egui::pos2(right, y),
            egui::Align2::RIGHT_CENTER,
            &format!("{label} {count}/{total}"),
            13.0,
            color,
        );
    }
}

fn check_chip(ui: &mut egui::Ui, text: &str, ok: bool) {
    let (color, fill) = if ok {
        (success_accent(), Color32::from_rgb(26, 59, 42))
    } else {
        (warning_accent(), Color32::from_rgb(58, 46, 26))
    };
    capsule(ui, text, color, fill);
}

fn draw_checks(ui: &mut egui::Ui, checks: &Checks) {
    panel_frame()
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                section_title(ui, "CHECKS");
                ui.add_space(6.0);
                check_chip(
                    ui,
                    &format!(
                        "Enchants {}/{}",
                        checks.enchants.done, checks.enchants.total
                    ),
                    checks.enchants.done >= checks.enchants.total,
                );
                check_chip(
                    ui,
                    &format!("Gems {}/{}", checks.gems.done, checks.gems.total),
                    checks.gems.done >= checks.gems.total,
                );
                check_chip(
                    ui,
                    &format!("Embellishments {}/{EMBELLISHMENTS}", checks.embellishments),
                    checks.embellishments >= EMBELLISHMENTS,
                );
                match checks.upgrades {
                    Some(Upgrades { items: 0, .. }) => check_chip(ui, "Fully upgraded", true),
                    Some(Upgrades { items, left }) => check_chip(
                        ui,
                        &format!(
                            "Upgrades · {items} {} · {left} left",
                            if items == 1 { "item" } else { "items" }
                        ),
                        false,
                    ),
                    None => {}
                }
            });
        });
}

fn draw_gear(ui: &mut egui::Ui, gear: &[Gear]) {
    let gap = 12.0;
    let width = ui.available_width();
    let columns = ((width + gap) / 290.0).floor().clamp(1.0, 4.0) as usize;
    let card_width = (width - gap * (columns as f32 - 1.0)) / columns as f32;
    for row in gear.chunks(columns) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for item in row {
                gear_card(ui, item, card_width);
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
}

fn gear_card(ui: &mut egui::Ui, item: &Gear, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 70.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(8), panel_background());
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(8),
        Stroke::new(1.0_f32, panel_stroke()),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(egui::vec2(14.0, 12.0));
    let ilvl = item.ilvl.map_or("–".into(), |ilvl| ilvl.to_string());
    text_at(
        ui,
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        &ilvl,
        18.0,
        primary_text(),
    );
    if let Some(track) = &item.track {
        let behind = track.level < track.max;
        text_at(
            ui,
            inner.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            &format!("{}/{}", track.level, track.max),
            12.0,
            if behind {
                warning_accent()
            } else {
                muted_text()
            },
        );
    }
    let left = inner.left() + 52.0;
    let text_width = inner.right() - left;
    let mut slot = single(slot_label(&item.slot), 11.0, muted_text());
    if let Some(track) = &item.track {
        append(&mut slot, &format!(" · {}", track.name), 11.0, muted_text());
    }
    if item.tier {
        append(&mut slot, " · Tier", 11.0, secondary_text());
    }
    if item.embellished {
        append(&mut slot, " · Embellished", 11.0, secondary_text());
    }
    let galley = clipped(ui, slot, text_width);
    painter.galley(egui::pos2(left, inner.top()), galley, muted_text());
    let color = quality_color(item.quality.as_deref());
    let galley = clipped(ui, single(&item.name, 14.0, color), text_width);
    painter.galley(
        egui::pos2(left, inner.center().y - galley.size().y / 2.0),
        galley,
        color,
    );

    let mut detail = egui::text::LayoutJob::default();
    match item.enchant.as_deref() {
        Some("") => append(&mut detail, "No enchant", 12.0, warning_accent()),
        Some(enchant) => append(&mut detail, enchant, 12.0, secondary_text()),
        None => {}
    }
    let missing = item.sockets.total.saturating_sub(item.sockets.filled);
    if missing > 0 {
        if !detail.text.is_empty() {
            append(&mut detail, " · ", 12.0, muted_text());
        }
        let text = if missing == 1 {
            "Empty socket".to_string()
        } else {
            format!("{missing} empty sockets")
        };
        append(&mut detail, &text, 12.0, warning_accent());
    }
    if !detail.text.is_empty() {
        let galley = clipped(ui, detail, text_width);
        painter.galley(
            egui::pos2(left, inner.bottom() - galley.size().y),
            galley,
            secondary_text(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> CharactersView {
        serde_json::from_str(
            r#"{
                "generatedAt": "2026-11-04T18:00:00.000Z",
                "canEditMains": false,
                "available": true,
                "syncedAt": "2026-11-04T17:55:00.000Z",
                "minLevel": 90,
                "readyIlvl": 270,
                "raids": [{ "id": 1307, "name": "The Voidspire", "encounters": [{ "id": 1, "name": "One" }] }],
                "members": [{
                    "userId": "1",
                    "name": "Lucas",
                    "role": "Officer",
                    "main": "eu:draenor:isogi",
                    "characters": [
                        { "key": "eu:draenor:wrexo", "name": "Wrexo", "realm": "Draenor", "region": "eu",
                          "class": "Warrior", "status": "pending", "level": 90, "main": false },
                        { "key": "eu:draenor:isogi", "name": "Isogi", "realm": "Draenor", "region": "eu",
                          "status": "ok", "syncedAt": "2026-11-04T17:55:00.000Z", "level": 90, "race": "Void Elf",
                          "class": "Mage", "spec": "Frost", "ilvl": 276, "tier": 4,
                          "lockout": { "normal": 0, "heroic": 6, "mythic": 2 },
                          "keys": { "runs": 3, "best": 12 }, "vault": { "raid": 3, "dungeon": 1 },
                          "checks": { "enchants": { "done": 7, "total": 8 }, "gems": { "done": 2, "total": 2 },
                                      "embellishments": 2, "upgrades": { "items": 3, "left": 5 }, "myth": 9 },
                          "saved": [{ "id": 1307, "name": "The Voidspire", "normal": [], "heroic": [1, 2, 3, 4, 5, 6],
                                      "mythic": [1, 2], "total": 8 }],
                          "main": true }
                    ]
                }]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn roster_sorts_by_role_then_presence_then_name() {
        let member =
            |id: &str, name: &str, role: Option<RaidRole>, online: bool, seen: bool| RosterMember {
                user_id: id.to_owned(),
                name: name.to_owned(),
                avatar: None,
                raid_role: role,
                role: "Raider".to_owned(),
                online,
                last_seen_at: seen.then(|| "2026-11-07T10:00:00.000Z".to_owned()),
                app_version: None,
                platform: None,
            };
        let roster = Roster {
            generated_at: String::new(),
            online_window_seconds: 120,
            officers: vec![
                member("1", "Sep", Some(RaidRole::Healer), false, true),
                member("2", "Luml", None, true, true),
                member("3", "wolsku", Some(RaidRole::Dps), false, true),
            ],
            raiders: vec![
                member("4", "muko", Some(RaidRole::Healer), false, false),
                member("5", "Isogi", Some(RaidRole::Healer), true, true),
                member("6", "Andy", Some(RaidRole::Tank), false, false),
                member("7", "Aery", Some(RaidRole::Dps), false, true),
                member("8", "ViklunD", Some(RaidRole::Dps), true, true),
            ],
            can_edit_roles: false,
        };
        let names: Vec<&str> = roster_order(&roster)
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["Andy", "Isogi", "Sep", "muko", "ViklunD", "Aery", "wolsku", "Luml"]
        );
    }

    #[test]
    fn reads_the_server_view_and_puts_the_main_first() {
        let mut view = view();
        for member in &mut view.members {
            sort_characters(&mut member.characters);
        }
        let characters = &view.members[0].characters;
        assert_eq!(characters[0].name, "Isogi");
        assert!(characters[0].ready(view.ready_ilvl));
        assert!(!characters[1].synced());
        assert!(!characters[1].ready(None));
        assert_eq!(characters[0].checks.unwrap().myth, Some(9));
        assert_eq!(
            member_summary(&view.members[0]).as_deref(),
            Some("2 characters · Frost Mage")
        );
    }

    #[test]
    fn the_tier_is_the_flagged_raids_or_every_raid_from_older_servers() {
        let mut view = view();
        assert_eq!(view.tier(), HashSet::from([1307]));
        view.raids.push(RaidInstance {
            id: 1324,
            name: "The Unbinding of Kith'ix".into(),
            encounters: Vec::new(),
            tier: true,
        });
        assert_eq!(view.tier(), HashSet::from([1324]));
        assert_eq!(
            short_raid("The Unbinding of Kith'ix"),
            "Unbinding of Kith'ix"
        );
    }

    #[test]
    fn a_new_main_moves_to_the_top() {
        let mut guild = GuildUi {
            view: Some(view()),
            ..GuildUi::default()
        };
        guild.apply_main("1", Some("eu:draenor:wrexo".into()));
        let member = &guild.view.as_ref().unwrap().members[0];
        assert_eq!(member.main.as_deref(), Some("eu:draenor:wrexo"));
        assert!(member.characters[0].main && member.characters[0].name == "Wrexo");
        assert!(!member.characters[1].main);
    }

    #[test]
    fn character_paths_only_carry_valid_keys() {
        assert_eq!(
            detail_path("eu:draenor:smygìs").as_deref(),
            Some("/v1/characters/eu/draenor/smygìs")
        );
        assert_eq!(
            detail_path("eu:twisting-nether:lucas").as_deref(),
            Some("/v1/characters/eu/twisting-nether/lucas")
        );
        for key in [
            "xx:draenor:lucas",
            "eu:Draenor:lucas",
            "eu:draenor:Lucas",
            "eu:draenor:lu/cas",
            "eu:draenor:lu?as",
            "eu::lucas",
            "eu:draenor",
        ] {
            assert_eq!(detail_path(key), None, "{key}");
        }
    }

    #[test]
    fn reads_gear_from_the_character_route() {
        let detail: CharacterDetail = serde_json::from_value(serde_json::json!({
            "userId": "1",
            "main": true,
            "status": "ok",
            "gear": [{
                "slot": "FINGER_1", "name": "Band", "ilvl": 276, "quality": "EPIC", "source": "Mythic+",
                "track": { "name": "Hero", "level": 6, "max": 6 }, "tier": false, "enchant": "",
                "sockets": { "total": 2, "filled": 1 }, "embellished": false
            }]
        }))
        .unwrap();
        let ring = &detail.gear[0];
        assert_eq!(slot_label(&ring.slot), "Ring");
        assert_eq!(ring.enchant.as_deref(), Some(""));
        assert_eq!(ring.track.as_ref().map(|t| (t.level, t.max)), Some((6, 6)));
    }

    #[test]
    fn missing_routes_read_as_unavailable() {
        assert_eq!(
            friendly_problem("Characters: Not found."),
            "Characters are not available on this server yet."
        );
        assert_eq!(
            friendly_problem("Main: Choose one of the synced Battle.net characters."),
            "Choose one of the synced Battle.net characters."
        );
    }

    #[test]
    fn every_class_has_a_token() {
        for class in [
            "Death Knight",
            "Demon Hunter",
            "Druid",
            "Evoker",
            "Hunter",
            "Mage",
            "Monk",
            "Paladin",
            "Priest",
            "Rogue",
            "Shaman",
            "Warlock",
            "Warrior",
        ] {
            assert!(token(class).is_some(), "{class}");
            assert_ne!(class_color(class), secondary_text(), "{class}");
        }
    }
}
