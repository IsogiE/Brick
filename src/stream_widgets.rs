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

pub fn encounter_groups(
    pulls: Vec<crate::warcraftlogs::Pull>,
) -> Vec<((u64, u64), Vec<crate::warcraftlogs::Pull>)> {
    let mut groups: Vec<((u64, u64), Vec<crate::warcraftlogs::Pull>)> = Vec::new();
    for pull in crate::warcraftlogs::canonical_pulls(pulls) {
        let key = (pull.encounter, pull.difficulty);
        if let Some((_, group)) = groups.iter_mut().find(|(k, _)| *k == key) {
            group.push(pull);
        } else {
            groups.push((key, vec![pull]));
        }
    }
    groups
}

pub fn encounter_label(pull: &crate::warcraftlogs::Pull) -> String {
    let difficulty = match pull.difficulty {
        3 | 4 | 14 => "Normal",
        5 | 6 | 15 => "Heroic",
        16 => "Mythic",
        7 | 17 => "Raid Finder",
        _ => "",
    };
    if difficulty.is_empty() {
        pull.name.clone()
    } else {
        format!("{} · {}", pull.name, difficulty)
    }
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
    ui.scope(|ui| {
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
            [width, 34.0],
            egui::Button::new(RichText::new(label).size(13.0).color(if selected {
                TEXT
            } else {
                MUTED
            }))
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
    cleared: bool,
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
    let difficulty = match pull.difficulty {
        3 | 4 | 14 => "Normal",
        5 | 6 | 15 => "Heroic",
        16 => "Mythic",
        7 | 17 => "LFR",
        _ => "Raid",
    };
    painter.text(
        egui::pos2(left, rect.top() + 36.0),
        egui::Align2::LEFT_CENTER,
        format!(
            "{difficulty} · {count} {}",
            if count == 1 { "pull" } else { "pulls" }
        ),
        egui::FontId::proportional(10.0),
        MUTED,
    );
    let status = if cleared {
        "Cleared".into()
    } else {
        best.map(|hp| format!("Best {hp:.1}%")).unwrap_or_default()
    };
    painter.text(
        egui::pos2(right, rect.top() + 36.0),
        egui::Align2::RIGHT_CENTER,
        status,
        egui::FontId::proportional(10.0),
        outcome_color(cleared, best),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            open,
            format!("{}: {count} pulls", pull.name),
        )
    });
    response
        .on_hover_text(encounter_label(pull))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn pull(
    ui: &mut egui::Ui,
    pull: &crate::warcraftlogs::Pull,
    selected: bool,
    best: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::click());
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
    let outcome = outcome_color(pull.kill, pull.remaining);
    let seconds = (pull.end_ms - pull.start_ms).max(0) / 1000;
    let result = if pull.kill {
        "Kill".into()
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
        format!("#{}   {}:{:02}", pull.id, seconds / 60, seconds % 60),
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
    let phase = pull.last_phase.map(|p| format!("P{p}")).unwrap_or_default();
    painter.text(
        egui::pos2(right - 62.0, rect.top() + 15.0),
        egui::Align2::RIGHT_CENTER,
        phase,
        egui::FontId::proportional(10.0),
        MUTED,
    );
    // A best marker does not change the outcome color or compete with selection.
    if best && !pull.kill {
        painter.circle_filled(egui::pos2(left + 2.0, rect.bottom() - 12.0), 2.0, outcome);
    }
    painter.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0_f32, if selected { ACCENT } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    if let Some(remaining) = if pull.kill {
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
            format!("{} fight {} {}", pull.name, pull.id, result),
        )
    });
    response
        .on_hover_text(format!(
            "{}\nLog {} · Fight {}\nPercentage: boss health remaining. Bar: health depleted.\nSelect to watch this moment",
            pull.name, pull.report, pull.id
        ))
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
    #[test]
    fn same_boss_across_reports_has_one_group_and_preserves_navigation() {
        let mut kill = pull("first", 69, 16, 900_000);
        kill.kill = true;
        kill.remaining = Some(0.0);
        let other = pull("second", 7, 16, 100_000);
        let duplicate_log = pull("third", 22, 16, 101_000);
        let groups = encounter_groups(vec![kill, other.clone(), duplicate_log, other]);
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
        let groups = encounter_groups(vec![pull("first", 1, 15, 0), pull("first", 2, 16, 500_000)]);
        assert_eq!(groups.len(), 2);
        assert!(encounter_label(&groups[0].1[0]).ends_with("Heroic"));
        assert!(encounter_label(&groups[1].1[0]).ends_with("Mythic"));
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
}
