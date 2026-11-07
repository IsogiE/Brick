//! Discord profile pictures for member rows. Each link is fetched once on a
//! single worker and kept as a small texture; until then, or when a member has
//! no picture, the row shows coloured initials.

use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
};

use eframe::egui::{self, Color32};

use crate::{download, guild_ui::text_at, presence};

const HOST: &str = "cdn.discordapp.com";
const MAX_BYTES: u64 = 512 * 1024;
const MAX_SIDE: u32 = 256;
const MAX_LINKS: usize = 256;

enum Slot {
    Loading,
    Decoded(egui::ColorImage),
    Ready(egui::TextureHandle),
    Failed,
}

#[derive(Default)]
struct Store {
    slots: HashMap<String, Slot>,
    queue: Option<mpsc::Sender<String>>,
}

type Shared = Arc<Mutex<Store>>;

/// Draws a member's picture in a circle, or their initials while it loads or
/// when they have none.
pub(crate) fn draw(
    ui: &egui::Ui,
    name: &str,
    link: Option<&str>,
    center: egui::Pos2,
    radius: f32,
    text_size: f32,
) {
    if let Some(texture) = link
        .filter(|link| valid(link))
        .and_then(|link| texture(ui.ctx(), link))
    {
        let rect = egui::Rect::from_center_size(center, egui::Vec2::splat(radius * 2.0));
        egui::Image::new((texture.id(), rect.size()))
            .corner_radius(radius)
            .paint_at(ui, rect);
        return;
    }
    let hue = name
        .chars()
        .fold(7u32, |hash, ch| (hash * 31 + ch as u32) % 360);
    let fill = Color32::from(egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.55, 0.62, 1.0));
    ui.painter().circle_filled(center, radius, fill);
    let initials: String = name.chars().take(2).collect::<String>().to_uppercase();
    text_at(
        ui,
        center,
        egui::Align2::CENTER_CENTER,
        &initials,
        text_size,
        Color32::WHITE,
    );
}

fn texture(ctx: &egui::Context, link: &str) -> Option<egui::TextureHandle> {
    let store = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Shared>(egui::Id::new("discord-avatars"))
            .clone()
    });
    let mut guard = store.lock().ok()?;
    match guard.slots.get(link) {
        Some(Slot::Ready(texture)) => return Some(texture.clone()),
        Some(Slot::Loading | Slot::Failed) => return None,
        Some(Slot::Decoded(_)) => {
            let Some(Slot::Decoded(image)) = guard.slots.remove(link) else {
                return None;
            };
            let texture = ctx.load_texture(link, image, egui::TextureOptions::LINEAR);
            guard
                .slots
                .insert(link.to_owned(), Slot::Ready(texture.clone()));
            return Some(texture);
        }
        None => {}
    }
    if guard.slots.len() >= MAX_LINKS {
        guard.slots.clear();
    }
    let queue = match &guard.queue {
        Some(queue) => queue.clone(),
        None => {
            let queue = start_worker(ctx.clone(), Arc::clone(&store));
            guard.queue = Some(queue.clone());
            queue
        }
    };
    if queue.send(link.to_owned()).is_ok() {
        guard.slots.insert(link.to_owned(), Slot::Loading);
    }
    None
}

fn start_worker(ctx: egui::Context, store: Shared) -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for link in rx {
            let slot = match fetch(&link) {
                Ok(image) => Slot::Decoded(image),
                Err(_) => Slot::Failed,
            };
            if let Ok(mut guard) = store.lock() {
                guard.slots.insert(link, slot);
            }
            ctx.request_repaint();
        }
    });
    tx
}

fn fetch(link: &str) -> Result<egui::ColorImage, String> {
    let response = presence::http_client()?
        .get(link)
        .send()
        .map_err(|_| "unreachable".to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let bytes = download::read_response(response, MAX_BYTES, "Picture")?;
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(4 * 1024 * 1024);
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "not a small PNG".to_string())?
        .to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

/// Only HTTPS links on Discord's CDN are fetched.
fn valid(link: &str) -> bool {
    reqwest::Url::parse(link).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some(HOST)
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_discord_cdn_links_are_fetched() {
        assert!(valid(
            "https://cdn.discordapp.com/avatars/222/0123456789abcdef0123456789abcdef.png?size=64"
        ));
        assert!(!valid("http://cdn.discordapp.com/avatars/222/a.png"));
        assert!(!valid("https://cdn.discordapp.com.evil.test/a.png"));
        assert!(!valid("https://user@cdn.discordapp.com/a.png"));
        assert!(!valid("https://cdn.discordapp.com:8443/a.png"));
        assert!(!valid("https://example.com/a.png"));
        assert!(!valid("not a link"));
    }

    #[test]
    fn decodes_small_pngs_and_rejects_anything_else() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(64, 64, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(decode(&png).unwrap().size, [64, 64]);
        let mut big = Vec::new();
        image::RgbaImage::new(512, 8)
            .write_to(&mut std::io::Cursor::new(&mut big), image::ImageFormat::Png)
            .unwrap();
        assert!(decode(&big).is_err());
        assert!(decode(b"GIF89a").is_err());
    }
}
