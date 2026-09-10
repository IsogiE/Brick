//! Raid eligibility and the shared, fixed ART digit reader.
#[cfg(test)]
pub use crate::replay_digits::Region;
#[cfg(test)]
use crate::replay_digits::{recognize, template};
pub use crate::replay_digits::{Marker, Reading};
#[cfg(test)]
use image::{
    imageops::{resize, FilterType},
    GrayImage, Luma,
};
#[cfg(test)]
use std::io::Cursor;

pub(crate) fn supports_pull(pull: &crate::warcraftlogs::Pull) -> bool {
    // Warcraft Logs uses 3/4/5 for normal/heroic/mythic raids, not WoW's 14/15/16.
    pull.encounter > 0 && matches!(pull.difficulty, 3..=5)
}

pub fn read(png: &[u8], pull: &crate::warcraftlogs::Pull, locked: Option<Marker>) -> Reading {
    if !supports_pull(pull) {
        return Reading::Uncertain;
    }
    crate::replay_digits::read(png, pull.start_ms, locked)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn pull() -> crate::warcraftlogs::Pull {
        crate::warcraftlogs::Pull {
            report: "abcdefghABCDEFGH".into(),
            id: 7,
            encounter: 3134,
            difficulty: 5,
            report_start_ms: 1_788_950_000_000,
            start_ms: 1_788_950_123_375,
            end_ms: 1_788_950_423_375,
            remaining: None,
            name: "Test boss".into(),
            kill: false,
            last_phase: None,
            last_phase_is_intermission: false,
            seconds: 123,
        }
    }
    #[test]
    fn only_warcraft_logs_normal_heroic_and_mythic_raid_difficulties_are_supported() {
        let mut pull = pull();
        for difficulty in [3, 4, 5] {
            pull.difficulty = difficulty;
            assert!(supports_pull(&pull));
        }
        for difficulty in [0, 1, 2, 10, 14, 15, 16, 17, 100] {
            pull.difficulty = difficulty;
            assert!(!supports_pull(&pull));
        }
        pull.difficulty = 4;
        pull.encounter = 0;
        assert!(!supports_pull(&pull));
    }

    #[test]
    fn bright_encoded_digits_remain_readable_with_nearby_gray_scenery() {
        let text = template(1_788_950_123, 12);
        let mut frame = GrayImage::new(960, 540);
        image::imageops::replace(&mut frame, &text, 9, 6);
        for x in 9 + text.width() + 4..9 + text.width() + 9 {
            for y in 6..6 + text.height() {
                frame.put_pixel(x, y, Luma([180]));
            }
        }
        let mut encoded = Cursor::new(Vec::new());
        frame
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            read(encoded.get_ref(), &pull(), None),
            Reading::Present(Marker {
                unix_seconds: 1_788_950_123,
                ..
            })
        ));
        let mut dungeon = pull();
        dungeon.difficulty = 10;
        assert_eq!(read(encoded.get_ref(), &dungeon, None), Reading::Uncertain);
    }

    #[test]
    fn dimmed_digits_with_color_bleed_survive_the_encoded_reader() {
        let text = template(1_788_950_123, 12);
        let mut frame = image::RgbImage::new(320, 100);
        for (x, y, pixel) in text.enumerate_pixels() {
            let gray = (f64::from(pixel[0]) * 0.25) as u8;
            frame.put_pixel(
                x + 4,
                y + 4,
                image::Rgb([gray.saturating_add(18), gray, gray.saturating_add(8)]),
            );
        }
        let mut encoded = Cursor::new(Vec::new());
        frame
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            read(encoded.get_ref(), &pull(), None),
            Reading::Present(Marker {
                unix_seconds: 1_788_950_123,
                ..
            })
        ));
    }

    #[test]
    fn changed_video_scale_is_reacquired_instead_of_becoming_a_false_disappearance() {
        let mut first = GrayImage::new(960, 540);
        image::imageops::replace(&mut first, &template(1_788_950_123, 7), 4, 4);
        let Reading::Present(marker) = recognize(&first, pull().start_ms, None) else {
            panic!("initial timestamp")
        };
        let mut second = GrayImage::new(960, 540);
        image::imageops::replace(&mut second, &template(1_788_950_123, 12), 6, 6);
        let Reading::Present(updated) = recognize(&second, pull().start_ms, Some(marker)) else {
            panic!("resized timestamp")
        };
        assert_eq!(marker.unix_seconds, updated.unix_seconds);
        assert_ne!(marker.region, updated.region);
    }

    #[test]
    fn known_timestamp_is_read_and_wrong_seconds_are_rejected() {
        let source = template(1_788_950_123, 7);
        for (width, height) in [(64, 10), (51, 8), (45, 7), (38, 6)] {
            let mut frame = GrayImage::new(960, 540);
            let mut text = resize(&source, width, height, FilterType::Triangle);
            for pixel in text.pixels_mut() {
                pixel[0] = (pixel[0] as f64 * 0.65) as u8;
            }
            image::imageops::replace(&mut frame, &text, 4, 4);
            let reading = recognize(&frame, pull().start_ms, None);
            assert!(
                matches!(
                    reading,
                    Reading::Present(Marker {
                        unix_seconds: 1_788_950_123,
                        ..
                    })
                ),
                "{width}x{height}: {reading:?}"
            );
            if let Reading::Present(marker) = reading {
                assert!(matches!(
                    recognize(&frame, pull().start_ms, Some(marker)),
                    Reading::Present(_)
                ));
                assert_eq!(
                    recognize(&GrayImage::new(960, 540), pull().start_ms, Some(marker)),
                    Reading::Absent
                );
                assert_eq!(
                    recognize(&GrayImage::new(961, 540), pull().start_ms, Some(marker)),
                    Reading::Uncertain
                );
            }
            assert_eq!(
                recognize(&frame, pull().start_ms + 60_000, None),
                Reading::Absent
            );
        }
    }
    #[test]
    fn invalid_or_nonraid_captures_cannot_be_negative_evidence() {
        assert_eq!(read(b"not PNG", &pull(), None), Reading::Uncertain);
        let mut encoded = Cursor::new(Vec::new());
        GrayImage::new(8193, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert_eq!(read(encoded.get_ref(), &pull(), None), Reading::Uncertain);
    }
    #[test]
    #[ignore = "requires an explicit independently rendered or in-game timestamp fixture"]
    fn actual_art_render() {
        let path = std::env::var_os("BRICK_ART_MARKER_FIXTURE").unwrap();
        let png = std::fs::read(path).unwrap();
        let mut pull = pull();
        if let Ok(start_ms) = std::env::var("BRICK_ART_MARKER_START_MS") {
            pull.start_ms = start_ms.parse().unwrap();
        }
        let expected = std::env::var("BRICK_ART_MARKER_UNIX")
            .map(|value| value.parse::<i64>().unwrap())
            .unwrap_or(1_788_950_123);
        let reading = read(&png, &pull, None);
        eprintln!("ART marker: {reading:?}");
        assert!(matches!(reading, Reading::Present(marker) if marker.unix_seconds == expected));
    }
}
