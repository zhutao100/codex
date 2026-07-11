use super::*;
use pretty_assertions::assert_eq;

fn lines_to_plain_text(lines: Vec<Line<'static>>) -> String {
    lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn token_activity_handle_finishes_loaded_state() {
    let (cell, handle) = new_token_activity_output(TokenActivityView::Daily);
    assert!(cell.display_lines(80).iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.content.contains("Loading"))
    }));

    let today = NaiveDate::from_ymd_opt(2026, 6, 25).expect("valid date");
    handle.finish_with_today(
        Ok(TokenUsageProfile {
            stats: codex_backend_client::TokenUsageProfileStats {
                lifetime_tokens: Some(100),
                peak_daily_tokens: Some(40),
                longest_running_turn_sec: None,
                current_streak_days: None,
                longest_streak_days: None,
                daily_usage_buckets: None,
            },
        }),
        today,
    );

    let text = lines_to_plain_text(cell.display_lines(80));
    assert!(text.contains("Lifetime 100"));
    assert!(text.contains("Token activity history unavailable"));
}

#[test]
fn token_activity_handle_finishes_error_state() {
    let (cell, handle) = new_token_activity_output(TokenActivityView::Daily);
    handle.finish(Err("boom".to_string()));

    let text = lines_to_plain_text(cell.display_lines(80));
    assert_eq!(
        text,
        "/usage daily\n\n Token activity\n   Token activity unavailable"
    );
}
