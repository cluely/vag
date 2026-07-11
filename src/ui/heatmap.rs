//! GitHub-style activity overview for the top of the full dashboard: a 2x4
//! grid of stat cards (sessions, streaks, per-agent time) plus a calendar
//! heatmap (7 weekday rows x however many week-columns fit), fed entirely by
//! the persisted `ActivityStats` (src/stats.rs) — every number here is real,
//! nothing is fabricated to match the reference design.

use std::time::Duration;

use chrono::{Datelike, Days, NaiveDate};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::stats::ActivityStats;
use crate::types::AgentKind;
use crate::ui::dashboard::fmt_work_dur;
use crate::ui::theme::Theme;

/// Intensity levels above "no activity", brightest last — fixed GitHub
/// greens (semantic-fixed like the badge colors, not themed: intensity
/// needs to read the same regardless of the active color theme).
const HEAT_LEVELS: [Color; 4] = [
    Color::Rgb(0x0e, 0x44, 0x29),
    Color::Rgb(0x00, 0x6d, 0x32),
    Color::Rgb(0x26, 0xa6, 0x41),
    Color::Rgb(0x39, 0xd3, 0x53),
];

const CLAUDE_COLOR: Color = Color::LightYellow;
const CODEX_COLOR: Color = Color::LightBlue;

fn level_color(total_ms: u64, max_ms: u64, dim: Color) -> Color {
    if total_ms == 0 || max_ms == 0 {
        return dim;
    }
    let level = ((total_ms as f64 / max_ms as f64) * HEAT_LEVELS.len() as f64).ceil() as usize;
    HEAT_LEVELS[level.clamp(1, HEAT_LEVELS.len()) - 1]
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        let mut out: String = s.chars().take(w.saturating_sub(1).max(1)).collect();
        if w > 1 {
            out.push('…');
        }
        out
    } else {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(w - n));
        out
    }
}

/// One 2-line row of up to 4 stat cards: a dim label line over a bold value
/// line, each cell padded to an equal share of `width`. `color` overrides
/// the value's tint per-cell (agent-tinted cards); `None` uses `theme.accent`.
fn card_row(
    cells: &[(&str, String, Option<Color>)],
    theme: &Theme,
    width: u16,
) -> [Line<'static>; 2] {
    let col_w = ((width as usize).saturating_sub(2) / cells.len().max(1)).max(8);
    let mut labels = vec![Span::raw("  ")];
    let mut values = vec![Span::raw("  ")];
    for (label, value, color) in cells {
        labels.push(Span::styled(pad(label, col_w), Style::new().fg(theme.dim)));
        values.push(Span::styled(
            pad(value, col_w),
            Style::new()
                .fg(color.unwrap_or(theme.accent))
                .add_modifier(Modifier::BOLD),
        ));
    }
    [Line::from(labels), Line::from(values)]
}

/// The 2x4 stat-card grid: sessions/active days/current streak/longest
/// streak on top, per-agent active-output time/avg-per-day/favorite below.
pub fn stat_cards(
    stats: &ActivityStats,
    theme: &Theme,
    sessions_count: usize,
    today: NaiveDate,
    width: u16,
) -> Vec<Line<'static>> {
    let (claude_total, _) = stats.totals(AgentKind::Claude);
    let (codex_total, _) = stats.totals(AgentKind::Codex);
    let combined = claude_total + codex_total;
    let active_days = stats.active_days_count();
    let avg = if active_days > 0 {
        combined / active_days as u32
    } else {
        Duration::ZERO
    };
    let favorite = match claude_total.cmp(&codex_total) {
        _ if combined.is_zero() => ("—", None),
        std::cmp::Ordering::Greater => ("Claude", Some(CLAUDE_COLOR)),
        std::cmp::Ordering::Less => ("Codex", Some(CODEX_COLOR)),
        std::cmp::Ordering::Equal => ("Tied", None),
    };

    let row1: [(&str, String, Option<Color>); 4] = [
        ("Sessions", sessions_count.to_string(), None),
        ("Active days", active_days.to_string(), None),
        (
            "Current streak",
            format!("{}d", stats.current_streak(today)),
            None,
        ),
        (
            "Longest streak",
            format!("{}d", stats.longest_streak()),
            None,
        ),
    ];
    let row2: [(&str, String, Option<Color>); 4] = [
        (
            "Claude active",
            if claude_total.is_zero() {
                "—".into()
            } else {
                fmt_work_dur(claude_total)
            },
            Some(CLAUDE_COLOR),
        ),
        (
            "Codex active",
            if codex_total.is_zero() {
                "—".into()
            } else {
                fmt_work_dur(codex_total)
            },
            Some(CODEX_COLOR),
        ),
        (
            "Avg/day",
            if combined.is_zero() {
                "—".into()
            } else {
                fmt_work_dur(avg)
            },
            None,
        ),
        ("Favorite", favorite.0.to_string(), favorite.1),
    ];

    let mut lines = Vec::with_capacity(4);
    lines.extend(card_row(&row1, theme, width));
    lines.extend(card_row(&row2, theme, width));
    lines
}

/// GitHub-style calendar grid: 7 weekday rows (Sun top .. Sat bottom),
/// columns are weeks, oldest on the left, the current week rightmost and
/// aligned so `today` sits in its actual weekday row. Cells past `today`
/// (the rest of the current week) render blank rather than as "no data".
pub fn calendar_grid(
    stats: &ActivityStats,
    theme: &Theme,
    today: NaiveDate,
    width: u16,
) -> Vec<Line<'static>> {
    const CELL_AND_GAP: usize = 2; // 1-char cell + 1 space column gap
    let weeks_n = ((width as usize).saturating_sub(2) / CELL_AND_GAP).max(1);
    let today_dow = today.weekday().num_days_from_sunday() as u64; // 0=Sun..6=Sat
    // Practically infallible (weeks_n is bounded by terminal width, nowhere
    // near NaiveDate's range) — `unwrap_or(today)` is a harmless fallback.
    let last_col_start = today
        .checked_sub_days(Days::new(today_dow))
        .unwrap_or(today);
    let first_col_start = last_col_start
        .checked_sub_days(Days::new(7 * (weeks_n as u64 - 1)))
        .unwrap_or(last_col_start);

    let cell_date = |week: usize, row: usize| -> Option<NaiveDate> {
        first_col_start.checked_add_days(Days::new(7 * week as u64 + row as u64))
    };

    let mut max_ms = 0u64;
    for week in 0..weeks_n {
        for row in 0..7 {
            let Some(date) = cell_date(week, row) else {
                continue;
            };
            if date > today {
                continue;
            }
            let d = stats.day(date);
            max_ms = max_ms.max(d.claude_ms + d.codex_ms);
        }
    }

    let mut lines = Vec::with_capacity(7);
    for row in 0..7 {
        let mut spans = vec![Span::raw("  ")];
        for week in 0..weeks_n {
            let cell = match cell_date(week, row) {
                Some(date) if date <= today => {
                    let d = stats.day(date);
                    let total = d.claude_ms + d.codex_ms;
                    Some(level_color(total, max_ms, theme.dim))
                }
                _ => None, // future cell in the current week, or out of range
            };
            match cell {
                Some(color) => spans.push(Span::styled("█", Style::new().fg(color))),
                None => spans.push(Span::raw(" ")),
            }
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn stat_cards_empty_state_shows_dashes_and_zero_counts() {
        let stats = ActivityStats::default();
        let lines = stat_cards(&stats, &Theme::TRANSPARENT, 0, d(2026, 7, 9), 80);
        assert_eq!(lines.len(), 4);
        let values_row1 = line_text(&lines[1]);
        assert!(values_row1.contains('0'), "{values_row1:?}");
        let values_row2 = line_text(&lines[3]);
        assert!(values_row2.contains('—'), "{values_row2:?}");
    }

    #[test]
    fn stat_cards_report_real_totals_and_favorite() {
        let mut stats = ActivityStats::default();
        stats.credit(d(2026, 7, 8), AgentKind::Claude, Duration::from_secs(3600));
        stats.credit(d(2026, 7, 9), AgentKind::Claude, Duration::from_secs(3600));
        stats.credit(d(2026, 7, 9), AgentKind::Codex, Duration::from_secs(600));
        let lines = stat_cards(&stats, &Theme::TRANSPARENT, 12, d(2026, 7, 9), 100);
        let labels = line_text(&lines[0]);
        let activity_labels = line_text(&lines[2]);
        let values1 = line_text(&lines[1]);
        let values2 = line_text(&lines[3]);
        assert!(labels.contains("Sessions"));
        assert!(activity_labels.contains("Claude active"));
        assert!(activity_labels.contains("Codex active"));
        assert!(values1.contains("12"), "{values1:?}");
        assert!(values1.contains('2'), "active days: {values1:?}"); // 2 active days
        assert!(values1.contains("2d"), "current streak: {values1:?}");
        assert!(values2.contains("Claude"), "favorite: {values2:?}");
        assert!(values2.contains("2h00m"), "claude total: {values2:?}");
    }

    #[test]
    fn calendar_grid_has_seven_weekday_rows() {
        let stats = ActivityStats::default();
        let lines = calendar_grid(&stats, &Theme::TRANSPARENT, d(2026, 7, 9), 60);
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn calendar_grid_places_today_in_the_rightmost_column() {
        let mut stats = ActivityStats::default();
        let today = d(2026, 7, 9); // Thursday
        stats.credit(today, AgentKind::Claude, Duration::from_secs(3600));
        let lines = calendar_grid(&stats, &Theme::TRANSPARENT, today, 30);
        // Thursday is weekday row 4 (Sun=0..Sat=6).
        let thursday_row = &lines[4];
        let last_cell = &thursday_row.spans[thursday_row.spans.len() - 2];
        assert_eq!(last_cell.content.as_ref(), "█");
        assert_eq!(last_cell.style.fg, Some(*HEAT_LEVELS.last().unwrap()));
    }

    #[test]
    fn calendar_grid_future_cells_in_current_week_are_blank() {
        let stats = ActivityStats::default();
        let today = d(2026, 7, 9); // Thursday: Fri/Sat of this week are future
        let lines = calendar_grid(&stats, &Theme::TRANSPARENT, today, 30);
        let saturday_row = &lines[6];
        let last_cell = &saturday_row.spans[saturday_row.spans.len() - 2];
        assert_eq!(last_cell.content.as_ref(), " ", "future cell must be blank");
    }

    #[test]
    fn calendar_grid_handles_narrow_and_zero_width_without_panicking() {
        let stats = ActivityStats::default();
        let today = d(2026, 7, 9);
        assert_eq!(
            calendar_grid(&stats, &Theme::TRANSPARENT, today, 0).len(),
            7
        );
        assert_eq!(
            calendar_grid(&stats, &Theme::TRANSPARENT, today, 1).len(),
            7
        );
    }
}
