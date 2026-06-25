//! Renders account token usage summaries and activity charts for `/usage`.

use std::collections::BTreeMap;

use chrono::Datelike;
use chrono::Duration;
use chrono::NaiveDate;
use codex_backend_client::TokenUsageProfile;
use codex_backend_client::TokenUsageProfileDailyBucket;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::status::format_tokens_compact;

const WEEK_COUNT: usize = 52;
const DAY_COUNT: usize = 7;
const CELL_COUNT: usize = WEEK_COUNT * DAY_COUNT;
const CHART_LEFT_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenActivityView {
    Daily,
    Weekly,
    Cumulative,
}

impl TokenActivityView {
    pub(in crate::chatwidget) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "day" | "daily" => Some(Self::Daily),
            "week" | "weekly" => Some(Self::Weekly),
            "cumulative" => Some(Self::Cumulative),
            _ => None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Daily => "Daily",
            Self::Weekly => "Weekly",
            Self::Cumulative => "Cumulative",
        }
    }
}

pub(super) fn loaded_lines(
    view: TokenActivityView,
    profile: &TokenUsageProfile,
    today: NaiveDate,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![vec![" Token activity".bold(), "   last 12 months".dim()].into()];
    lines.extend(summary_lines(profile));
    lines.push(Line::default());

    let Some(buckets) = profile.stats.daily_usage_buckets.as_ref() else {
        lines.push("   Token activity history unavailable".dim().into());
        return lines;
    };

    lines.extend(chart_lines(view, buckets, today, width));
    lines
}

fn summary_lines(profile: &TokenUsageProfile) -> Vec<Line<'static>> {
    let stats = &profile.stats;
    vec![
        summary_line(vec![
            ("Lifetime", format_optional_tokens(stats.lifetime_tokens)),
            ("Peak", format_optional_tokens(stats.peak_daily_tokens)),
        ]),
        summary_line(vec![
            (
                "Streak",
                format_streak(stats.current_streak_days, stats.longest_streak_days),
            ),
            (
                "Longest task",
                format_optional_duration(stats.longest_running_turn_sec),
            ),
        ]),
    ]
}

fn summary_line(fields: Vec<(&'static str, String)>) -> Line<'static> {
    let mut spans = vec![" ".into()];
    for (index, (label, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            spans.push("   ".dim());
        }
        spans.push(format!("{label} ").dim());
        spans.push(Span::styled(value, numeric_style()));
    }
    spans.into()
}

fn chart_lines(
    view: TokenActivityView,
    buckets: &[TokenUsageProfileDailyBucket],
    today: NaiveDate,
    width: u16,
) -> Vec<Line<'static>> {
    let shown_columns = shown_columns(width);
    if shown_columns == 0 {
        return vec!["   Widen terminal to show activity graph".dim().into()];
    }

    let values = daily_values(buckets, today);
    let levels = levels_for_view(&values, view);
    let first_column = WEEK_COUNT - shown_columns;

    let mut lines = vec![month_labels(today, first_column, shown_columns)];
    for row in 0..DAY_COUNT {
        let mut spans = vec![weekday_label(view, row)];
        for column in first_column..WEEK_COUNT {
            if column > first_column {
                spans.push(" ".into());
            }
            let index = column * DAY_COUNT + row;
            if view == TokenActivityView::Daily
                && cell_date(today, index).is_some_and(|date| date > today)
            {
                spans.push(" ".into());
            } else {
                spans.push(Span::styled(
                    glyph(levels[index]),
                    style_for_level(levels[index]),
                ));
            }
        }
        lines.push(spans.into());
    }

    lines.push(Line::default());
    lines.push(caption_line(view, &values));
    lines.push(view_footer(view));
    lines
}

fn shown_columns(width: u16) -> usize {
    (usize::from(width)
        .saturating_sub(CHART_LEFT_WIDTH)
        .saturating_add(1)
        / 2)
    .min(WEEK_COUNT)
}

fn format_optional_tokens(value: Option<i64>) -> String {
    value
        .map(format_tokens_compact)
        .unwrap_or_else(|| "-".to_string())
}

fn format_streak(current: Option<i64>, longest: Option<i64>) -> String {
    match (current, longest) {
        (Some(current), Some(longest)) if current == longest => format!("{current}d"),
        (Some(current), Some(longest)) => format!("{current}d (best {longest}d)"),
        (Some(current), None) => format!("{current}d"),
        (None, Some(longest)) => format!("- (best {longest}d)"),
        (None, None) => "-".to_string(),
    }
}

fn format_optional_duration(value: Option<i64>) -> String {
    value.map_or_else(
        || "-".to_string(),
        |seconds| {
            let seconds = seconds.max(0);
            let hours = seconds / 3600;
            let minutes = (seconds % 3600) / 60;
            match (hours, minutes) {
                (0, 0) => format!("{seconds}s"),
                (0, minutes) => format!("{minutes}m"),
                (hours, 0) => format!("{hours}h"),
                (hours, minutes) => format!("{hours}h {minutes}m"),
            }
        },
    )
}

fn numeric_style() -> Style {
    Style::default().green()
}

fn style_for_level(level: usize) -> Style {
    match level {
        0 => Style::default().dim(),
        1 | 2 => Style::default().green().dim(),
        3 | 4 => Style::default().green(),
        _ => Style::default(),
    }
}

fn glyph(level: usize) -> &'static str {
    match level {
        0 => ".",
        1 => ":",
        2 => "*",
        3 | 4 => "#",
        _ => "?",
    }
}

fn weekday_label(view: TokenActivityView, row: usize) -> Span<'static> {
    if view != TokenActivityView::Daily {
        return Span::styled(
            match row {
                0 => "max ",
                6 => "  0 ",
                _ => "    ",
            },
            Style::default().dim(),
        );
    }
    Span::styled(
        match row {
            0 => " Su ",
            1 => " Mo ",
            2 => " Tu ",
            3 => " We ",
            4 => " Th ",
            5 => " Fr ",
            6 => " Sa ",
            _ => "    ",
        },
        Style::default().dim(),
    )
}

fn caption_line(view: TokenActivityView, values: &[i64]) -> Line<'static> {
    let weeks = weekly_totals(values);
    let (lead, peak) = match view {
        TokenActivityView::Daily => (
            "Each cell = 1 day - peak ",
            values.iter().copied().max().unwrap_or(0),
        ),
        TokenActivityView::Weekly => (
            "Each column = 1 week - peak ",
            weeks.iter().copied().max().unwrap_or(0),
        ),
        TokenActivityView::Cumulative => ("Running total - top ", weeks.iter().sum::<i64>()),
    };
    if peak <= 0 {
        return "   No token activity in the last 12 months".dim().into();
    }
    vec![
        Span::from("   ").dim(),
        Span::from(lead).dim(),
        Span::styled(format_tokens_compact(peak), numeric_style()),
    ]
    .into()
}

fn view_footer(active: TokenActivityView) -> Line<'static> {
    let mut spans = vec![Span::from("   ").dim()];
    let views = [
        (TokenActivityView::Daily, "daily"),
        (TokenActivityView::Weekly, "weekly"),
        (TokenActivityView::Cumulative, "cumulative"),
    ];
    for (index, (view, name)) in views.into_iter().enumerate() {
        if index > 0 {
            spans.push(" - ".dim());
        }
        let style = if view == active {
            numeric_style().bold()
        } else {
            Style::default().dim()
        };
        spans.push(Span::styled(name, style));
    }
    spans.into()
}

fn month_labels(today: NaiveDate, first_column: usize, shown_columns: usize) -> Line<'static> {
    let mut cells = vec![' '; shown_columns * 2 - 1];
    let start = chart_start(today);
    let mut last_end = 0;
    for column in first_column..WEEK_COUNT {
        let date = start + Duration::days((column * DAY_COUNT) as i64);
        if date.day() > 7 {
            continue;
        }
        let label = date.format("%b").to_string();
        let offset = (column - first_column) * 2;
        if offset < last_end || offset + label.len() > cells.len() {
            continue;
        }
        for (index, ch) in label.chars().enumerate() {
            cells[offset + index] = ch;
        }
        last_end = offset + label.len() + 1;
    }
    vec![
        "    ".into(),
        Span::styled(
            cells.into_iter().collect::<String>(),
            Style::default().dim(),
        ),
    ]
    .into()
}

fn daily_values(buckets: &[TokenUsageProfileDailyBucket], today: NaiveDate) -> Vec<i64> {
    let start = chart_start(today);
    let end = start + Duration::days(CELL_COUNT as i64);
    let mut by_date = BTreeMap::new();
    for bucket in buckets {
        let Ok(date) = NaiveDate::parse_from_str(&bucket.start_date, "%Y-%m-%d") else {
            continue;
        };
        if date < start || date >= end || date > today {
            continue;
        }
        *by_date.entry(date).or_insert(0) += bucket.tokens.max(0);
    }
    (0..CELL_COUNT)
        .map(|offset| {
            by_date
                .get(&(start + Duration::days(offset as i64)))
                .copied()
                .unwrap_or(0)
        })
        .collect()
}

fn levels_for_view(values: &[i64], view: TokenActivityView) -> Vec<usize> {
    match view {
        TokenActivityView::Daily => graded_levels(values),
        TokenActivityView::Weekly => bar_levels(&weekly_totals(values)),
        TokenActivityView::Cumulative => {
            let cumulative = weekly_totals(values)
                .into_iter()
                .scan(0, |sum, value| {
                    *sum += value;
                    Some(*sum)
                })
                .collect::<Vec<_>>();
            bar_levels(&cumulative)
        }
    }
}

fn graded_levels(values: &[i64]) -> Vec<usize> {
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|value| match (*value, max) {
            (0, _) | (_, 0) => 0,
            (value, max) if value * 4 > max * 3 => 4,
            (value, max) if value * 2 > max => 3,
            (value, max) if value * 4 > max => 2,
            _ => 1,
        })
        .collect()
}

fn weekly_totals(values: &[i64]) -> Vec<i64> {
    values
        .chunks(DAY_COUNT)
        .map(|week| week.iter().sum())
        .collect()
}

fn bar_levels(totals: &[i64]) -> Vec<usize> {
    let max = totals.iter().copied().max().unwrap_or(0);
    totals
        .iter()
        .flat_map(|value| {
            let height = if *value <= 0 || max <= 0 {
                0
            } else {
                ((*value * DAY_COUNT as i64 + max - 1) / max) as usize
            };
            (0..DAY_COUNT).map(move |row| if DAY_COUNT - row <= height { 4 } else { 0 })
        })
        .collect()
}

fn chart_start(today: NaiveDate) -> NaiveDate {
    let week_start = today - Duration::days(i64::from(today.weekday().num_days_from_sunday()));
    week_start - Duration::weeks((WEEK_COUNT - 1) as i64)
}

fn cell_date(today: NaiveDate, index: usize) -> Option<NaiveDate> {
    chart_start(today).checked_add_signed(Duration::days(index as i64))
}

#[cfg(test)]
#[path = "chart_tests.rs"]
mod tests;
