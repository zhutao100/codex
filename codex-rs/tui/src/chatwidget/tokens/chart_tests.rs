use super::*;
use pretty_assertions::assert_eq;

fn bucket(start_date: &str, tokens: i64) -> TokenUsageProfileDailyBucket {
    TokenUsageProfileDailyBucket {
        start_date: start_date.to_string(),
        tokens,
    }
}

#[test]
fn parses_usage_view_args() {
    assert_eq!(TokenActivityView::parse(""), Some(TokenActivityView::Daily));
    assert_eq!(
        TokenActivityView::parse("day"),
        Some(TokenActivityView::Daily)
    );
    assert_eq!(
        TokenActivityView::parse("weekly"),
        Some(TokenActivityView::Weekly)
    );
    assert_eq!(
        TokenActivityView::parse("cumulative"),
        Some(TokenActivityView::Cumulative)
    );
    assert_eq!(TokenActivityView::parse("monthly"), None);
}

#[test]
fn daily_values_ignores_invalid_future_and_negative_buckets() {
    let today = NaiveDate::from_ymd_opt(2026, 6, 25).expect("valid date");
    let values = daily_values(
        &[
            bucket("2026-06-24", 10),
            bucket("2026-06-24", 5),
            bucket("2026-06-25", -4),
            bucket("2026-06-26", 99),
            bucket("not-a-date", 99),
        ],
        today,
    );

    let yesterday_index = cell_date_index(today, "2026-06-24");
    let today_index = cell_date_index(today, "2026-06-25");
    assert_eq!(values[yesterday_index], 15);
    assert_eq!(values[today_index], 0);
    assert_eq!(values.iter().sum::<i64>(), 15);
}

#[test]
fn loaded_lines_include_summary_and_footer() {
    let today = NaiveDate::from_ymd_opt(2026, 6, 25).expect("valid date");
    let profile = TokenUsageProfile {
        stats: codex_backend_client::TokenUsageProfileStats {
            lifetime_tokens: Some(12_500),
            peak_daily_tokens: Some(3_000),
            longest_running_turn_sec: Some(7_260),
            current_streak_days: Some(2),
            longest_streak_days: Some(5),
            daily_usage_buckets: Some(vec![bucket("2026-06-24", 10)]),
        },
    };

    let text = loaded_lines(TokenActivityView::Weekly, &profile, today, 80)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("Lifetime 12.5K"));
    assert!(text.contains("Peak 3K"));
    assert!(text.contains("Streak 2d (best 5d)"));
    assert!(text.contains("Longest task 2h 1m"));
    assert!(text.contains("daily - weekly - cumulative"));
}

fn cell_date_index(today: NaiveDate, date: &str) -> usize {
    let target = NaiveDate::parse_from_str(date, "%Y-%m-%d").expect("valid fixture date");
    let start = chart_start(today);
    target.signed_duration_since(start).num_days() as usize
}
