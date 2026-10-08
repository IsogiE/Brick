use crate::stream_time::pull_start_time;
use eframe::egui::{self, Color32, RichText, Stroke};
pub const ACCENT: Color32 = Color32::from_rgb(244, 100, 56);
pub const SOFT: Color32 = Color32::from_rgb(63, 38, 32);
pub const BORDER: Color32 = Color32::from_rgb(55, 62, 76);
pub const MUTED: Color32 = Color32::from_rgb(145, 155, 173);
pub const TEXT: Color32 = Color32::from_rgb(239, 242, 247);
pub const GREEN: Color32 = Color32::from_rgb(83, 201, 142);
// Color describes progress, independently of selection or the best-attempt badge.
pub fn outcome_color(kill: bool, remaining: Option<f64>) -> Color32 {
    if kill {
        return GREEN;
    }
    let Some(hp) = remaining.filter(|hp| hp.is_finite()) else {
        return MUTED;
    };
    let hp = hp.clamp(0.0, 100.0);
    let red = Color32::from_rgb(229, 105, 112);
    let amber = Color32::from_rgb(226, 177, 91);
    let (from, to, t) = if hp >= 50.0 {
        (amber, red, ((hp - 50.0) / 50.0) as f32)
    } else {
        (GREEN, amber, (hp / 50.0) as f32)
    };
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
    )
}

/// Group normalized review pulls without changing their recording-wide order.
pub fn encounter_groups(
    pulls: Vec<crate::warcraftlogs::Pull>,
) -> Vec<((u64, u64, String), Vec<crate::warcraftlogs::Pull>)> {
    let mut groups: Vec<((u64, u64, String), Vec<crate::warcraftlogs::Pull>)> = Vec::new();
    for pull in pulls {
        let key = (
            pull.encounter,
            pull.difficulty,
            if pull.difficulty == 10 {
                pull.name.clone()
            } else {
                String::new()
            },
        );
        if let Some((_, group)) = groups.iter_mut().find(|(k, _)| *k == key) {
            group.push(pull);
        } else {
            groups.push((key, vec![pull]));
        }
    }
    groups
}

// Pulls use Warcraft Logs difficulty IDs, which differ from the game's IDs.
fn difficulty_label(difficulty: u64) -> Option<&'static str> {
    match difficulty {
        1 => Some("Raid Finder"),
        2 => Some("Flex"),
        3 => Some("Normal"),
        4 => Some("Heroic"),
        5 => Some("Mythic"),
        10 => Some("M+"),
        _ => None,
    }
}

pub fn encounter_label(pull: &crate::warcraftlogs::Pull) -> String {
    let difficulty = difficulty_label(pull.difficulty).unwrap_or_default();
    if difficulty.is_empty() {
        pull.name.clone()
    } else {
        format!("{} · {}", pull.name, difficulty)
    }
}

fn pull_start_label(pull: &crate::warcraftlogs::Pull) -> String {
    pull_start_time(pull.start_ms)
        .map(|(at, zone)| {
            format!(
                "{} {} · {:02}:{:02}:{:02} {zone}",
                at.day(),
                &at.month().to_string()[..3],
                at.hour(),
                at.minute(),
                at.second()
            )
        })
        .unwrap_or_else(|| "Start time unavailable".into())
}

pub fn pull_matches_search(pull: &crate::warcraftlogs::Pull, number: usize, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let date = pull_start_time(pull.start_ms)
        .map(|(at, _)| at.date().to_string())
        .unwrap_or_default();
    let text = format!(
        "{} {} {} #{} {} {}",
        pull.name,
        pull.report,
        number,
        number,
        pull_start_label(pull),
        date
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| text.contains(word))
}

// Reserve real space for list scrollbars; floating bars cover card outlines.
pub fn list_scroll_style(ui: &mut egui::Ui) {
    ui.visuals_mut().widgets.inactive.fg_stroke.color = MUTED;
    ui.visuals_mut().widgets.hovered.fg_stroke.color = Color32::from_rgb(191, 200, 214);
    ui.visuals_mut().widgets.active.fg_stroke.color = ACCENT;
    ui.spacing_mut().scroll = egui::style::ScrollStyle {
        bar_width: 9.0,
        bar_inner_margin: 8.0,
        bar_outer_margin: 2.0,
        handle_min_length: 28.0,
        content_margin: egui::Margin::same(2),
        foreground_color: true,
        ..egui::style::ScrollStyle::solid()
    };
}

pub fn style(ui: &mut egui::Ui) {
    let visuals = ui.visuals_mut();
    visuals.extreme_bg_color = Color32::from_rgb(24, 28, 35);
    visuals.selection.bg_fill = SOFT;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    for state in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        state.expansion = 0.0;
        state.corner_radius = egui::CornerRadius::same(6);
        state.bg_stroke.width = 1.0;
    }
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(31, 35, 44);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(31, 35, 44);
    visuals.widgets.inactive.bg_stroke.color = BORDER;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(44, 49, 60);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(44, 49, 60);
    visuals.widgets.hovered.bg_stroke.color = Color32::from_rgb(80, 87, 103);
    visuals.widgets.active.bg_fill = SOFT;
    visuals.widgets.active.weak_bg_fill = SOFT;
}
pub fn tab(ui: &mut egui::Ui, label: &str, selected: bool, width: f32) -> egui::Response {
    button(
        ui,
        RichText::new(label)
            .size(13.0)
            .color(if selected { TEXT } else { MUTED }),
        selected,
        [width, 34.0],
    )
}

pub fn button(
    ui: &mut egui::Ui,
    label: RichText,
    selected: bool,
    size: [f32; 2],
) -> egui::Response {
    ui.scope(|ui| {
        if size[1] <= 26.0 {
            ui.spacing_mut().button_padding = egui::vec2(4.0, 3.0);
        }
        let visuals = ui.visuals_mut();
        let idle = if selected {
            SOFT
        } else {
            Color32::from_rgb(27, 31, 39)
        };
        let hover = if selected {
            Color32::from_rgb(78, 49, 39)
        } else {
            Color32::from_rgb(43, 49, 61)
        };
        let pressed = if selected {
            Color32::from_rgb(94, 51, 37)
        } else {
            SOFT
        };
        for (state, fill) in [
            (&mut visuals.widgets.inactive, idle),
            (&mut visuals.widgets.hovered, hover),
            (&mut visuals.widgets.active, pressed),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
        }
        ui.add_sized(
            size,
            egui::Button::new(label)
                .stroke(Stroke::new(1.0_f32, if selected { ACCENT } else { BORDER })),
        )
    })
    .inner
}
pub fn member_background(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: &egui::Response,
    selected: bool,
) {
    let painter = ui.painter_at(rect);
    if selected || response.hovered() || response.has_focus() {
        painter.rect_filled(
            rect,
            5.0,
            if selected {
                SOFT
            } else {
                Color32::from_rgb(36, 42, 52)
            },
        );
    }
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_max(
                rect.left_top() + egui::vec2(0.0, 5.0),
                rect.left_bottom() + egui::vec2(3.0, -5.0),
            ),
            2.0,
            ACCENT,
        );
    }
}
pub fn encounter_header(
    ui: &mut egui::Ui,
    pull: &crate::warcraftlogs::Pull,
    count: usize,
    best: Option<f64>,
    killed: bool,
    open: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 54.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    if response.hovered() {
        painter.rect_filled(rect, 5.0, Color32::from_rgb(33, 38, 47));
    }
    let left = rect.left() + 7.0;
    let right = rect.right() - 8.0;
    let mut title = egui::text::LayoutJob::simple(
        pull.name.clone(),
        egui::FontId::proportional(13.0),
        TEXT,
        (rect.width() - 38.0).max(1.0),
    );
    title.wrap.max_rows = 1;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(title));
    painter.galley(egui::pos2(left, rect.top() + 5.0), galley, TEXT);
    let center = egui::pos2(right - 3.0, rect.top() + 13.0);
    let points = if open {
        vec![
            center + egui::vec2(-3.0, -2.0),
            center + egui::vec2(0.0, 1.0),
            center + egui::vec2(3.0, -2.0),
        ]
    } else {
        vec![
            center + egui::vec2(-2.0, -3.0),
            center + egui::vec2(1.0, 0.0),
            center + egui::vec2(-2.0, 3.0),
        ]
    };
    painter.add(egui::Shape::line(points, Stroke::new(1.5_f32, MUTED)));
    let difficulty = difficulty_label(pull.difficulty).unwrap_or("Raid");
    painter.text(
        egui::pos2(left, rect.top() + 36.0),
        egui::Align2::LEFT_CENTER,
        format!(
            "{difficulty} · {count} {}",
            if pull.difficulty == 10 {
                if count == 1 {
                    "segment"
                } else {
                    "segments"
                }
            } else if count == 1 {
                "pull"
            } else {
                "pulls"
            }
        ),
        egui::FontId::proportional(10.0),
        MUTED,
    );
    let status = if pull.difficulty == 10 {
        String::new()
    } else if killed {
        "Killed".into()
    } else {
        best.map(|hp| format!("Best {hp:.1}%")).unwrap_or_default()
    };
    painter.text(
        egui::pos2(right, rect.top() + 36.0),
        egui::Align2::RIGHT_CENTER,
        status,
        egui::FontId::proportional(10.0),
        outcome_color(killed, best),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            open,
            format!(
                "{}: {count} {}",
                pull.name,
                if pull.difficulty == 10 {
                    "segments"
                } else {
                    "pulls"
                }
            ),
        )
    });
    response
        .on_hover_text(RichText::new(encounter_label(pull)).size(12.0).color(TEXT))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn pull(
    ui: &mut egui::Ui,
    pull: &crate::warcraftlogs::Pull,
    number: usize,
    selected: bool,
    best: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 54.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        5.0,
        if selected {
            SOFT
        } else if response.hovered() {
            Color32::from_rgb(37, 43, 53)
        } else {
            Color32::from_rgb(26, 30, 38)
        },
    );
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_max(
                rect.left_top() + egui::vec2(0.0, 5.0),
                rect.left_bottom() + egui::vec2(3.0, -5.0),
            ),
            2.0,
            ACCENT,
        );
    }
    let dungeon = pull.difficulty == 10;
    let outcome = if dungeon {
        MUTED
    } else {
        outcome_color(pull.kill, pull.remaining)
    };
    let seconds = (pull.end_ms - pull.start_ms).max(0) / 1000;
    let result = if dungeon {
        "M+".into()
    } else if pull.kill {
        "Killed".into()
    } else {
        pull.remaining
            .map(|r| format!("{r:.1}%"))
            .unwrap_or_else(|| "—".into())
    };
    let left = rect.left() + 10.0;
    let right = rect.right() - 10.0;
    painter.text(
        egui::pos2(left, rect.top() + 15.0),
        egui::Align2::LEFT_CENTER,
        format!("#{number}   {}:{:02}", seconds / 60, seconds % 60),
        egui::FontId::proportional(12.0),
        TEXT,
    );
    painter.text(
        egui::pos2(right, rect.top() + 15.0),
        egui::Align2::RIGHT_CENTER,
        &result,
        egui::FontId::proportional(12.0),
        outcome,
    );
    let phase = if dungeon {
        String::new()
    } else {
        pull.last_phase.map(|p| format!("P{p}")).unwrap_or_default()
    };
    painter.text(
        egui::pos2(right - 62.0, rect.top() + 15.0),
        egui::Align2::RIGHT_CENTER,
        phase,
        egui::FontId::proportional(10.0),
        MUTED,
    );
    painter.text(
        egui::pos2(left, rect.top() + 34.0),
        egui::Align2::LEFT_CENTER,
        pull_start_label(pull),
        egui::FontId::proportional(10.0),
        MUTED,
    );
    // A best marker does not change the outcome color or compete with selection.
    if best && !pull.kill && !dungeon {
        painter.circle_filled(egui::pos2(right - 2.0, rect.top() + 34.0), 2.0, outcome);
    }
    painter.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0_f32, if selected { ACCENT } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    if let Some(remaining) = if dungeon {
        None
    } else if pull.kill {
        Some(0.0)
    } else {
        pull.remaining.filter(|hp| hp.is_finite())
    } {
        let track = egui::Rect::from_min_size(
            egui::pos2(left, rect.bottom() - 5.0),
            egui::vec2((right - left).max(0.0), 3.0),
        );
        painter.rect_filled(track, 1.5, outcome.gamma_multiply(0.20));
        painter.rect_filled(
            egui::Rect::from_min_size(
                track.min,
                egui::vec2(
                    track.width() * ((100.0 - remaining) / 100.0).clamp(0.0, 1.0) as f32,
                    3.0,
                ),
            ),
            1.5,
            outcome,
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            selected,
            format!(
                "{} {} {} {} {}",
                pull.name,
                if dungeon { "segment" } else { "pull" },
                number,
                result,
                pull_start_label(pull)
            ),
        )
    });
    response
        .on_hover_ui(|ui| {
            ui.set_max_width(280.0);
            ui.label(
                RichText::new(encounter_label(pull))
                    .size(13.0)
                    .strong()
                    .color(TEXT),
            );
            ui.label(
                RichText::new(format!(
                    "{} {} · {}:{:02}",
                    if dungeon { "Segment" } else { "Pull" },
                    number,
                    seconds / 60,
                    seconds % 60,
                ))
                .size(11.0)
                .color(MUTED),
            );
            if let Some((at, zone)) = pull_start_time(pull.start_ms) {
                ui.label(
                    RichText::new(format!(
                        "{} · {:02}:{:02}:{:02} {zone}",
                        at.date(),
                        at.hour(),
                        at.minute(),
                        at.second()
                    ))
                    .size(11.0)
                    .color(MUTED),
                );
            }
            if !dungeon {
                ui.label(
                    RichText::new(if pull.kill {
                        "Killed".to_string()
                    } else {
                        pull.remaining
                            .filter(|hp| hp.is_finite())
                            .map(|hp| format!("{hp:.1}% boss health remaining"))
                            .unwrap_or_else(|| "Wipe".into())
                    })
                    .size(12.0)
                    .color(outcome),
                );
            }
            ui.label(
                RichText::new("Select to watch this moment")
                    .size(11.0)
                    .color(TEXT),
            );
        })
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pull(report: &str, id: u64, difficulty: u64, start: i64) -> crate::warcraftlogs::Pull {
        crate::warcraftlogs::Pull {
            report: report.into(),
            id,
            encounter: 3178,
            difficulty,
            friendly_players: None,
            report_start_ms: 0,
            remaining: Some(70.3),
            name: "Ula'tek".into(),
            kill: false,
            last_phase: Some(2),
            last_phase_is_intermission: false,
            start_ms: start,
            end_ms: start + 247_000,
            seconds: 247,
        }
    }
    fn timestamp(text: &str) -> i64 {
        let at = time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .unwrap();
        (at.unix_timestamp_nanos() / 1_000_000) as i64
    }

    #[test]
    fn pull_times_use_central_european_dates_and_exact_dst_boundaries() {
        for (utc, expected) in [
            ("2026-09-10T16:01:05Z", "10 Sep · 18:01:05 CEST"),
            ("2026-09-10T22:15:00Z", "11 Sep · 00:15:00 CEST"),
            ("2026-01-10T16:01:05Z", "10 Jan · 17:01:05 CET"),
            ("2026-03-29T00:59:59Z", "29 Mar · 01:59:59 CET"),
            ("2026-03-29T01:00:00Z", "29 Mar · 03:00:00 CEST"),
            ("2026-10-25T00:59:59Z", "25 Oct · 02:59:59 CEST"),
            ("2026-10-25T01:00:00Z", "25 Oct · 02:00:00 CET"),
            ("2025-03-30T01:00:00Z", "30 Mar · 03:00:00 CEST"),
            ("2025-10-26T01:00:00Z", "26 Oct · 02:00:00 CET"),
            ("2026-09-10T18:01:05+02:00", "10 Sep · 18:01:05 CEST"),
        ] {
            let pull = pull("report", 7, 4, timestamp(utc));
            assert_eq!(pull_start_label(&pull), expected, "{utc}");
        }
        for invalid in [0, -1, i64::MIN, i64::MAX] {
            assert_eq!(pull_start_time(invalid), None);
        }
    }

    #[test]
    fn pull_time_search_uses_the_displayed_local_date_and_preserves_other_terms() {
        let pull = pull("exampleLog", 7, 4, timestamp("2026-09-10T22:15:32Z"));
        for query in [
            "",
            "ula examplelog 7",
            "11 SEP 00:15 CEST",
            "2026-09-11 00:15:32",
        ] {
            assert!(pull_matches_search(&pull, 7, query), "{query}");
        }
        for query in ["2026-09-10", "22:15", "00:15 CET", "12 Sep"] {
            assert!(!pull_matches_search(&pull, 7, query), "{query}");
        }
    }

    #[test]
    fn pull_search_uses_the_displayed_number_without_relabeling_source_identity() {
        let pull = pull("exampleLog", 91, 4, timestamp("2026-09-10T22:15:32Z"));
        assert!(pull_matches_search(&pull, 2, "#2"));
        assert!(pull_matches_search(&pull, 2, "ula examplelog #2"));
        assert!(!pull_matches_search(&pull, 2, "#91"));
        assert_eq!(pull.id, 91);
        assert_eq!(pull.report, "exampleLog");
    }

    #[test]
    fn pull_cards_show_start_time_on_its_own_line_without_overlapping_progress() {
        let pull = pull("report", 7, 4, timestamp("2026-09-10T16:01:05Z"));
        for width in [220.0, 300.0, 420.0] {
            let ctx = egui::Context::default();
            let mut bounds = egui::Rect::NOTHING;
            let mut output = None;
            for _ in 0..2 {
                output = Some(ctx.run_ui(Default::default(), |ui| {
                    ui.set_width(width);
                    bounds = super::pull(ui, &pull, 2, false, true).rect;
                }));
            }
            let output = output.unwrap();
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == "#2   4:07")));
            let text = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == pull_start_label(&pull) => {
                        Some(text)
                    }
                    _ => None,
                })
                .expect("Each card displays its Warcraft Logs start time");
            let text_bounds = egui::Rect::from_min_size(text.pos, text.galley.size());
            assert!(
                bounds.contains_rect(text_bounds),
                "{width}: {bounds:?}, {text_bounds:?}"
            );
            assert!(text_bounds.top() > bounds.top() + 23.0);
            assert!(text_bounds.bottom() < bounds.bottom() - 5.0);
        }
    }

    #[test]
    fn same_boss_across_reports_has_one_group_and_preserves_navigation() {
        let mut kill = pull("first", 69, 5, 900_000);
        kill.kill = true;
        kill.remaining = Some(0.0);
        let other = pull("second", 7, 5, 100_000);
        let duplicate_log = pull("third", 22, 5, 101_000);
        let groups = encounter_groups(crate::warcraftlogs::canonical_pulls(vec![
            kill,
            other.clone(),
            duplicate_log,
            other,
        ]));
        assert_eq!(groups.len(), 1);
        let group = &groups[0].1;
        assert_eq!(group.len(), 2);
        assert_eq!((&*group[0].report, group[0].id), ("second", 7));
        assert_eq!((&*group[1].report, group[1].id), ("first", 69));
        assert_eq!(
            group
                .iter()
                .filter_map(|p| if p.kill { Some(0.0) } else { p.remaining })
                .reduce(f64::min),
            Some(0.0)
        );
    }
    #[test]
    fn difficulties_stay_distinct_and_are_labelled() {
        let groups = encounter_groups(vec![pull("first", 1, 4, 0), pull("first", 2, 5, 500_000)]);
        assert_eq!(groups.len(), 2);
        assert!(encounter_label(&groups[0].1[0]).ends_with("Heroic"));
        assert!(encounter_label(&groups[1].1[0]).ends_with("Mythic"));
        for (difficulty, label) in [
            (1, "Raid Finder"),
            (2, "Flex"),
            (3, "Normal"),
            (4, "Heroic"),
            (5, "Mythic"),
            (10, "M+"),
        ] {
            assert!(encounter_label(&pull("first", 1, difficulty, 0)).ends_with(label));
        }
    }
    #[test]
    fn outcomes_use_health_not_best_rank() {
        let early = outcome_color(false, Some(90.0));
        let close = outcome_color(false, Some(9.0));
        assert!(early.r() > early.g());
        assert!(close.g() > close.r());
        assert_eq!(outcome_color(true, Some(70.0)), GREEN);
        assert_eq!(outcome_color(false, None), MUTED);
        assert_eq!(outcome_color(false, Some(f64::NAN)), MUTED);
    }
    #[test]
    fn mythic_plus_dungeons_with_zero_encounter_ids_stay_separate() {
        let mut first = pull("first", 1, 10, 0);
        first.encounter = 0;
        first.name = "Voidscar Arena".into();
        let mut second = pull("first", 2, 10, 500_000);
        second.encounter = 0;
        second.name = "Magisters' Terrace".into();
        let mut third = first.clone();
        third.id = 3;
        third.start_ms = 1_000_000;
        third.end_ms = 1_247_000;
        let groups = encounter_groups(vec![first, second, third]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 1);
        assert!(encounter_label(&groups[0].1[0]).ends_with("M+"));
    }

    #[test]
    fn dungeon_cards_hide_raid_outcomes_phases_and_health_bars() {
        fn collect(shape: &egui::Shape, labels: &mut Vec<String>, health_bars: &mut usize) {
            match shape {
                egui::Shape::Text(text) => labels.push(text.galley.job.text.clone()),
                egui::Shape::Rect(rect) if (rect.rect.height() - 3.0).abs() < 0.01 => {
                    *health_bars += 1
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, labels, health_bars);
                    }
                }
                _ => {}
            }
        }
        for kill in [false, true] {
            let mut dungeon = pull("first", 1, 10, 0);
            dungeon.kill = kill;
            let ctx = egui::Context::default();
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show_inside(ui, |ui| {
                    encounter_header(ui, &dungeon, 2, Some(70.3), kill, true);
                    super::pull(ui, &dungeon, 2, false, true);
                });
            });
            let mut labels = Vec::new();
            let mut bars = 0;
            for shape in &output.shapes {
                collect(&shape.shape, &mut labels, &mut bars);
            }
            assert!(labels.iter().any(|s| s == "M+"), "{labels:?}");
            assert!(
                labels.iter().any(|s| s.contains("2 segments")),
                "{labels:?}"
            );
            assert!(
                !labels
                    .iter()
                    .any(|s| s.contains('%') || s == "Killed" || s == "P2"),
                "{labels:?}"
            );
            assert_eq!(bars, 0);
        }
    }
}
