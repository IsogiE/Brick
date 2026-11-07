//! Discord profile pictures for member rows. Links load on a few background
//! workers and become small textures. The PNGs are kept in Brick's encrypted
//! cache, so later starts show them without downloading; a link only downloads
//! again when the picture changes. Until a picture is ready, or when a member
//! has none, the row shows coloured initials.

use std::{
    collections::HashMap,
    sync::{mpsc, Arc, Mutex, OnceLock},
};

use eframe::egui::{self, Color32};

use crate::{download, guild_ui::text_at, presence};

const HOST: &str = "cdn.discordapp.com";
const MAX_BYTES: u64 = 512 * 1024;
const MAX_SIDE: u32 = 256;
const MAX_LINKS: usize = 256;
const WORKERS: usize = 6;
const CACHE_SCOPE: &str = "discord-avatars";
const MAX_CACHE_BYTES: usize = 1536 * 1024;

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
    disk: Arc<Disk>,
}

/// The saved PNGs, read once, and what this session will write back.
#[derive(Default)]
struct Disk {
    saved: OnceLock<HashMap<String, Vec<u8>>>,
    state: Mutex<DiskState>,
}

#[derive(Default)]
struct DiskState {
    seen: HashMap<String, Vec<u8>>,
    pending: usize,
    dirty: bool,
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
            let queue = start_workers(ctx.clone(), Arc::clone(&store), Arc::clone(&guard.disk));
            guard.queue = Some(queue.clone());
            queue
        }
    };
    if let Ok(mut disk) = guard.disk.state.lock() {
        disk.pending += 1;
    }
    if queue.send(link.to_owned()).is_ok() {
        guard.slots.insert(link.to_owned(), Slot::Loading);
    }
    None
}

fn start_workers(ctx: egui::Context, store: Shared, disk: Arc<Disk>) -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..WORKERS {
        let (ctx, store, disk, rx) = (
            ctx.clone(),
            Arc::clone(&store),
            Arc::clone(&disk),
            Arc::clone(&rx),
        );
        std::thread::spawn(move || loop {
            let Ok(link) = rx
                .lock()
                .map_err(|_| ())
                .and_then(|rx| rx.recv().map_err(|_| ()))
            else {
                return;
            };
            let slot = match load(&disk, &link) {
                Ok(image) => Slot::Decoded(image),
                Err(_) => Slot::Failed,
            };
            if let Ok(mut guard) = store.lock() {
                guard.slots.insert(link, slot);
            }
            finish(&disk);
            ctx.request_repaint();
        });
    }
    tx
}

/// A saved PNG when there is one, otherwise a download that the cache keeps.
fn load(disk: &Disk, link: &str) -> Result<egui::ColorImage, String> {
    let saved = disk.saved.get_or_init(|| {
        crate::protected_cache::load(CACHE_SCOPE)
            .ok()
            .flatten()
            .and_then(|bytes| unpack(&bytes))
            .unwrap_or_default()
    });
    let (bytes, downloaded) = match saved.get(link) {
        Some(bytes) => (bytes.clone(), false),
        None => (fetch(link)?, true),
    };
    let image = decode(&bytes)?;
    if let Ok(mut state) = disk.state.lock() {
        state.seen.insert(link.to_owned(), bytes);
        state.dirty |= downloaded;
    }
    Ok(image)
}

/// Writes the cache once the queue drains after a download. Pictures seen this
/// session come first; older ones stay while they fit.
fn finish(disk: &Disk) {
    let Ok(mut state) = disk.state.lock() else {
        return;
    };
    state.pending = state.pending.saturating_sub(1);
    if state.pending > 0 || !state.dirty {
        return;
    }
    state.dirty = false;
    let mut entries: Vec<(&str, &[u8])> = state
        .seen
        .iter()
        .map(|(link, bytes)| (link.as_str(), bytes.as_slice()))
        .collect();
    if let Some(saved) = disk.saved.get() {
        entries.extend(
            saved
                .iter()
                .filter(|(link, _)| !state.seen.contains_key(*link))
                .map(|(link, bytes)| (link.as_str(), bytes.as_slice())),
        );
    }
    let _ = crate::protected_cache::save(CACHE_SCOPE, &pack(&entries));
}

/// Length-prefixed link and PNG pairs, cut off before the size limit.
fn pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (link, bytes) in entries {
        let size = 2 + link.len() + 4 + bytes.len();
        if link.len() > u16::MAX as usize || out.len() + size > MAX_CACHE_BYTES {
            continue;
        }
        out.extend_from_slice(&(link.len() as u16).to_be_bytes());
        out.extend_from_slice(link.as_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn unpack(mut bytes: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    let mut entries = HashMap::new();
    let take = |bytes: &mut &[u8], count: usize| -> Option<Vec<u8>> {
        let (head, rest) = (bytes.get(..count)?, bytes.get(count..)?);
        *bytes = rest;
        Some(head.to_vec())
    };
    while !bytes.is_empty() {
        let len = u16::from_be_bytes(take(&mut bytes, 2)?.try_into().ok()?) as usize;
        let link = String::from_utf8(take(&mut bytes, len)?).ok()?;
        let len = u32::from_be_bytes(take(&mut bytes, 4)?.try_into().ok()?) as usize;
        if len as u64 > MAX_BYTES {
            return None;
        }
        let png = take(&mut bytes, len)?;
        if valid(&link) {
            entries.insert(link, png);
        }
    }
    Some(entries)
}

fn fetch(link: &str) -> Result<Vec<u8>, String> {
    let response = presence::http_client()?
        .get(link)
        .send()
        .map_err(|_| "unreachable".to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    download::read_response(response, MAX_BYTES, "Picture")
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
    fn the_cache_file_round_trips_and_rejects_damage() {
        let link =
            "https://cdn.discordapp.com/avatars/222/0123456789abcdef0123456789abcdef.png?size=64";
        let packed = pack(&[
            (link, b"png-bytes"),
            ("https://example.com/x.png", b"other"),
        ]);
        let entries = unpack(&packed).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[link], b"png-bytes");
        assert!(unpack(&packed[..packed.len() - 1]).is_none());
        assert!(unpack(&[0, 5, b'a']).is_none());
        assert_eq!(unpack(&[]).unwrap().len(), 0);
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
