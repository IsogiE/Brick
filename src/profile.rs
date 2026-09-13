use std::{
    collections::VecDeque,
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{
    discord_auth::{self, AuthorizedUser},
    presence,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RaidRole {
    Dps,
    Healer,
    Tank,
}

impl RaidRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dps => "DPS",
            Self::Healer => "Healer",
            Self::Tank => "Tank",
        }
    }
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Dps => include_bytes!("assets/roles/dps.png"),
            Self::Healer => include_bytes!("assets/roles/healer.png"),
            Self::Tank => include_bytes!("assets/roles/tank.png"),
        }
    }
}

pub fn role_order(role: Option<RaidRole>) -> u8 {
    match role {
        Some(RaidRole::Tank) => 0,
        Some(RaidRole::Healer) => 1,
        Some(RaidRole::Dps) => 2,
        None => 3,
    }
}

fn role_texture(ctx: &egui::Context, role: RaidRole) -> egui::TextureHandle {
    let id = egui::Id::new(("wow-role-texture", role.label()));
    if let Some(texture) = ctx.data(|data| data.get_temp::<egui::TextureHandle>(id)) {
        return texture;
    }
    let image = image::load_from_memory(role.bytes())
        .expect("bundled WoW role icon")
        .to_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    );
    let texture = ctx.load_texture(role.label(), color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
    texture
}

fn role_control_height(ui: &egui::Ui) -> f32 {
    ui.spacing().interact_size.y.max(
        ui.text_style_height(&egui::TextStyle::Button)
            .max(ui.spacing().icon_width)
            .max(18.0)
            + 2.0 * ui.spacing().button_padding.y,
    )
}

pub fn role_icon(ui: &mut egui::Ui, role: Option<RaidRole>) {
    if let Some(role) = role {
        let (slot, response) = ui.allocate_exact_size(
            egui::vec2(18.0, ui.spacing().interact_size.y.max(18.0)),
            egui::Sense::hover(),
        );
        paint_role_icon(
            ui,
            Some(role),
            egui::Rect::from_center_size(slot.center(), egui::vec2(18.0, 18.0)),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Image, ui.is_enabled(), role.label())
        });
        response.on_hover_text(role.label());
    }
}

pub fn paint_role_icon(ui: &egui::Ui, role: Option<RaidRole>, rect: egui::Rect) {
    if let Some(role) = role.filter(|_| ui.is_rect_visible(rect)) {
        ui.painter().image(
            role_texture(ui.ctx(), role).id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
}

pub fn role_picker(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    value: &mut Option<RaidRole>,
) -> bool {
    let before = *value;
    let height = role_control_height(ui);
    // ComboBox inherits its parent's layout for its padded contents. Allocate the
    // whole control first so a centered horizontal row cannot offset that padding.
    ui.allocate_ui_with_layout(
        egui::vec2(96.0, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.spacing_mut().interact_size.y = height;
            egui::ComboBox::from_id_salt(id)
                .width(96.0)
                .selected_text(value.map(RaidRole::label).unwrap_or("No role"))
                .show_ui(ui, |ui| {
                    ui.spacing_mut().interact_size.y = height;
                    let size = egui::vec2(ui.available_width(), height);
                    if ui
                        .add_sized(size, egui::Button::selectable(value.is_none(), "No role"))
                        .clicked()
                    {
                        *value = None;
                        ui.close();
                    }
                    for role in [RaidRole::Dps, RaidRole::Healer, RaidRole::Tank] {
                        let icon = egui::Image::new((
                            role_texture(ui.ctx(), role).id(),
                            egui::vec2(18.0, 18.0),
                        ));
                        if ui
                            .add_sized(
                                size,
                                egui::Button::selectable(
                                    *value == Some(role),
                                    (icon, role.label()),
                                ),
                            )
                            .clicked()
                        {
                            *value = Some(role);
                            ui.close();
                        }
                    }
                });
        },
    );
    before != *value
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub custom_name: Option<String>,
    pub raid_role: Option<RaidRole>,
    #[serde(default)]
    pub available: bool,
}

enum Operation {
    Load,
    Save(String, Option<RaidRole>),
    Role(String, Option<RaidRole>),
}
struct Completed {
    target: String,
    saved: bool,
    result: Result<Profile, String>,
}

// Profiles are server-authoritative and stay only in memory on this device.
// Background requests wake egui exactly once when they finish.
#[derive(Default)]
pub struct ProfileUi {
    user_id: String,
    value: Option<Profile>,
    name: String,
    role: Option<RaidRole>,
    pending: Option<mpsc::Receiver<Completed>>,
    pending_role: Option<(String, Option<RaidRole>)>,
    queued_roles: VecDeque<(String, Option<RaidRole>)>,
    completed_role: Option<(String, Option<RaidRole>)>,
    last_fetch: Option<Instant>,
}

impl ProfileUi {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn tick(
        &mut self,
        ctx: &egui::Context,
        user: Option<&AuthorizedUser>,
        visible: bool,
    ) -> bool {
        let id = user.map(|user| user.user_id.as_str()).unwrap_or("");
        if id != self.user_id {
            *self = Self {
                user_id: id.to_owned(),
                ..Self::default()
            };
        }
        let mut changed = false;
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(completed) => {
                    self.pending = None;
                    let role_only = self.pending_role.take().is_some();
                    match completed.result {
                        Ok(profile) => {
                            changed = completed.saved && !role_only;
                            if role_only {
                                self.completed_role =
                                    Some((completed.target.clone(), profile.raid_role));
                            }
                            if completed.target == self.user_id {
                                if (completed.saved && !role_only) || !self.dirty() {
                                    self.name = profile.custom_name.clone().unwrap_or_default();
                                    self.role = profile.raid_role;
                                } else if role_only {
                                    self.role = profile.raid_role;
                                }
                                self.value = Some(profile);
                            }
                        }
                        // Keep the confirmed value and unsaved draft on failure.
                        // The form remains available for another attempt without banners.
                        Err(_) => {}
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.pending_role = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !id.is_empty() && self.pending.is_none() {
            if let Some((id, role)) = self.queued_roles.pop_front() {
                self.start(ctx, Operation::Role(id, role));
            }
        }
        if !id.is_empty()
            && visible
            && presence::configured()
            && self.pending.is_none()
            && self
                .last_fetch
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
        {
            self.start(ctx, Operation::Load);
        }
        changed
    }

    pub fn repaint_after(&self, visible: bool) -> Duration {
        if !visible || self.user_id.is_empty() || self.pending.is_some() || !presence::configured()
        {
            Duration::from_secs(60)
        } else {
            self.last_fetch
                .map(|last| Duration::from_secs(60).saturating_sub(last.elapsed()))
                .unwrap_or_default()
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, discord_name: &str) {
        ui.label(egui::RichText::new("Your profile").strong());
        let enabled = self.value.as_ref().is_some_and(|value| value.available) && !self.busy();
        let mut save = false;
        ui.add_enabled_ui(enabled, |ui| {
            // Size the row before placing labels and icons, so later padded
            // controls cannot move its center after those widgets are painted.
            ui.spacing_mut().interact_size.y = role_control_height(ui);
            ui.horizontal(|ui| {
                ui.label("Name");
                let name = ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .hint_text(discord_name)
                        .char_limit(64)
                        .desired_width(190.0),
                );
                save |= name.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            });
            ui.horizontal(|ui| {
                ui.label("Raid role");
                role_icon(ui, self.role);
                role_picker(ui, "own-raid-role", &mut self.role);
                save |= ui
                    .add_enabled(self.dirty(), egui::Button::new("Save profile"))
                    .clicked();
            });
        });
        if enabled && self.dirty() && save {
            self.start(ui.ctx(), Operation::Save(self.name.clone(), self.role));
        }
        ui.label(egui::RichText::new("Leave the name empty to use your Discord name.").small());
    }

    pub fn set_member_role(&mut self, ctx: &egui::Context, id: String, role: Option<RaidRole>) {
        if !self.busy() {
            self.start(ctx, Operation::Role(id, role));
        } else if let Some(queued) = self.queued_roles.iter_mut().find(|queued| queued.0 == id) {
            queued.1 = role;
        } else if self.queued_roles.len() < 32 {
            self.queued_roles.push_back((id, role));
        }
    }

    pub fn role_for_member(&self, id: &str, confirmed: Option<RaidRole>) -> Option<RaidRole> {
        self.queued_roles
            .iter()
            .find(|queued| queued.0 == id)
            .or_else(|| self.pending_role.as_ref().filter(|pending| pending.0 == id))
            .map_or(confirmed, |pending| pending.1)
    }

    pub fn take_role_change(&mut self) -> Option<(String, Option<RaidRole>)> {
        self.completed_role.take()
    }

    fn dirty(&self) -> bool {
        self.value.as_ref().is_some_and(|value| {
            self.name != value.custom_name.as_deref().unwrap_or("") || self.role != value.raid_role
        })
    }

    fn start(&mut self, ctx: &egui::Context, operation: Operation) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let own_id = self.user_id.clone();
        self.last_fetch = Some(Instant::now());
        self.pending_role = match &operation {
            Operation::Role(id, role) => Some((id.clone(), *role)),
            _ => None,
        };
        self.pending = Some(rx);
        crate::guild::spawn(move || {
            let expected_user = own_id.clone();
            let (target, method, path, body, saved) = match operation {
                Operation::Load => (
                    own_id,
                    Method::GET,
                    "/v1/profile/me".to_owned(),
                    None,
                    false,
                ),
                Operation::Save(name, role) => (
                    own_id,
                    Method::PUT,
                    "/v1/profile/me".to_owned(),
                    Some(serde_json::json!({"customName": name, "raidRole": role})),
                    true,
                ),
                Operation::Role(id, role) => (
                    id.clone(),
                    Method::PUT,
                    format!("/v1/profiles/{id}/role"),
                    Some(serde_json::json!({"raidRole": role})),
                    true,
                ),
            };
            let result = discord_auth::current_or_refreshed_access_token()
                .and_then(|token| {
                    token.ok_or_else(|| "Sign in again to update your profile.".to_string())
                })
                .and_then(|token| {
                    presence::profile_request(method, &path, &token, &expected_user, body.as_ref())
                });
            let _ = tx.send(Completed {
                target,
                saved,
                result,
            });
            ctx.request_repaint();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> AuthorizedUser {
        AuthorizedUser {
            user_id: "123".into(),
            display_name: "Discord Name".into(),
            username: "user".into(),
            guild_name: "Advance".into(),
            role_label: "Raider".into(),
            expires_at_unix: u64::MAX,
            created_at_unix: u64::MAX,
            guild_id: crate::guild::ADVANCE.into(),
            guilds: Vec::new(),
        }
    }

    #[test]
    fn initial_profile_populates_the_form_and_refresh_preserves_unsaved_edits() {
        let (tx, rx) = mpsc::channel();
        let loaded = Profile {
            custom_name: Some("Raid Name".into()),
            raid_role: Some(RaidRole::Healer),
            available: true,
        };
        tx.send(Completed {
            target: "123".into(),
            saved: false,
            result: Ok(loaded.clone()),
        })
        .unwrap();
        let mut profile = ProfileUi {
            user_id: "123".into(),
            pending: Some(rx),
            last_fetch: Some(Instant::now()),
            ..ProfileUi::default()
        };
        let ctx = egui::Context::default();
        assert!(!profile.tick(&ctx, Some(&user()), false));
        assert_eq!(profile.name, "Raid Name");
        assert_eq!(profile.role, Some(RaidRole::Healer));
        assert!(!profile.dirty());
        profile.name = "Unsaved Name".into();
        let (tx, rx) = mpsc::channel();
        profile.pending = Some(rx);
        tx.send(Completed {
            target: "123".into(),
            saved: false,
            result: Ok(Profile {
                raid_role: Some(RaidRole::Tank),
                ..loaded
            }),
        })
        .unwrap();
        profile.tick(&ctx, Some(&user()), false);
        assert_eq!(profile.name, "Unsaved Name");
        assert!(profile.dirty());
        profile.tick(&ctx, None, false);
        assert!(profile.name.is_empty());
        assert!(profile.value.is_none());
    }

    #[test]
    fn embedded_roles_decode_and_wire_values_are_stable() {
        for (role, expected) in [
            (RaidRole::Dps, "dps"),
            (RaidRole::Healer, "healer"),
            (RaidRole::Tank, "tank"),
        ] {
            assert_eq!(serde_json::to_value(role).unwrap(), expected);
            assert_eq!(image::load_from_memory(role.bytes()).unwrap().width(), 64);
        }
        assert!(serde_json::from_str::<RaidRole>("\"officer\"").is_err());
    }

    #[test]
    fn confirmed_role_save_updates_only_its_member_without_a_full_refresh() {
        let (tx, rx) = mpsc::channel();
        let mut profile = ProfileUi {
            user_id: "123".into(),
            pending: Some(rx),
            pending_role: Some(("456".into(), Some(RaidRole::Tank))),
            last_fetch: Some(Instant::now()),
            ..Default::default()
        };
        assert_eq!(
            profile.role_for_member("456", Some(RaidRole::Dps)),
            Some(RaidRole::Tank)
        );
        assert_eq!(
            profile.role_for_member("789", Some(RaidRole::Healer)),
            Some(RaidRole::Healer)
        );
        tx.send(Completed {
            target: "456".into(),
            saved: true,
            result: Ok(Profile {
                raid_role: Some(RaidRole::Tank),
                available: true,
                ..Default::default()
            }),
        })
        .unwrap();
        assert!(!profile.tick(&egui::Context::default(), Some(&user()), false));
        assert_eq!(
            profile.take_role_change(),
            Some(("456".into(), Some(RaidRole::Tank)))
        );
        assert!(profile.take_role_change().is_none());
        assert!(!profile.busy());
    }

    #[test]
    fn rapid_role_choices_are_coalesced_bounded_and_cleared_on_account_change() {
        let (_tx, rx) = mpsc::channel();
        let mut profile = ProfileUi {
            user_id: "123".into(),
            pending: Some(rx),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        for id in 1000..1100 {
            profile.set_member_role(&ctx, id.to_string(), Some(RaidRole::Tank));
        }
        assert_eq!(profile.queued_roles.len(), 32);
        profile.set_member_role(&ctx, "1000".into(), Some(RaidRole::Healer));
        assert_eq!(profile.queued_roles.len(), 32);
        assert_eq!(
            profile.role_for_member("1000", None),
            Some(RaidRole::Healer)
        );
        profile.tick(&ctx, None, false);
        assert!(profile.queued_roles.is_empty() && profile.pending.is_none());
        assert!(profile.role_for_member("1000", None).is_none());
    }

    #[test]
    fn a_failed_profile_fetch_keeps_the_retry_interval() {
        let (tx, rx) = mpsc::channel();
        tx.send(Completed {
            target: "123".into(),
            saved: false,
            result: Err("offline".into()),
        })
        .unwrap();
        let mut profile = ProfileUi {
            user_id: "123".into(),
            pending: Some(rx),
            last_fetch: Some(Instant::now()),
            ..ProfileUi::default()
        };
        assert!(!profile.tick(&egui::Context::default(), Some(&user()), true));
        assert!(!profile.busy());
        assert!(profile.repaint_after(true) > Duration::from_secs(59));
    }
}
