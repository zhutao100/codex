use super::*;
use crate::compact::SUMMARY_PREFIX;
use crate::compact::preserved_work_notes_message;
use codex_protocol::models::image_close_tag_text;
use codex_protocol::models::image_open_tag_text;
use pretty_assertions::assert_eq;

fn message(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".into(),
        content,
        end_turn: None,
        phase: None,
    }
}

fn text(value: &str) -> ResponseItem {
    message(vec![ContentItem::InputText { text: value.into() }])
}

fn image(url: &str) -> ContentItem {
    ContentItem::InputImage {
        image_url: url.into(),
    }
}

#[test]
fn filters_context_and_old_compaction_before_budgeting() {
    let user = text("real user");
    let summary = text(&format!("{SUMMARY_PREFIX}\nold local summary"));
    let aborted = text("<turn_aborted>\ninterrupted\n</turn_aborted>");
    let mut assistant = text("generated work");
    if let ResponseItem::Message { role, .. } = &mut assistant {
        *role = "assistant".into();
    }
    let input = vec![
        text(&format!(
            "<environment_context>{}</environment_context>",
            "x".repeat(1000)
        )),
        preserved_work_notes_message("exact notes"),
        user.clone(),
        ResponseItem::Compaction {
            encrypted_content: "old".into(),
        },
        summary.clone(),
        assistant,
        aborted.clone(),
        ResponseItem::CompactionTrigger {},
    ];
    assert_eq!(
        retain_messages(&input, 64_000),
        vec![user, summary, aborted]
    );
}

#[test]
fn newest_messages_win_and_full_messages_keep_their_shape() {
    let items = vec![text("first"), text("second"), text("last")];
    let budget = retained_tokens(&items[1]) + retained_tokens(&items[2]);
    assert_eq!(retain_messages(&items, budget), items[1..]);
    assert_eq!(retain_messages(&items, 0), vec![]);
}

#[test]
fn text_boundary_accounts_for_escaping_and_truncation_notice() {
    let input = vec![text("old"), text(&"✅\\\"\n".repeat(1000))];
    let retained = retain_messages(&input, 100);
    assert_eq!(retained.len(), 1);
    assert!(retained_tokens(&retained[0]) <= 100);
    let ResponseItem::Message { content, .. } = &retained[0] else {
        panic!("message")
    };
    assert!(
        content_items_to_text(content)
            .unwrap()
            .contains("truncated")
    );
}

#[test]
fn image_only_messages_have_nonzero_bounded_cost() {
    let item = message(vec![image("https://example.test/i.png")]);
    let input = vec![item.clone(); 50];
    let retained = retain_messages(&input, RETAINED_MESSAGE_TOKEN_BUDGET);
    assert!(!retained.is_empty());
    assert!(retained.len() < input.len());
    assert!(retained.iter().map(retained_tokens).sum::<usize>() <= RETAINED_MESSAGE_TOKEN_BUDGET);
    assert!(retained.into_iter().all(|retained| retained == item));
}

#[test]
fn image_and_labels_are_atomic_at_the_boundary() {
    let content = vec![
        ContentItem::InputText {
            text: "old ".repeat(5000),
        },
        ContentItem::InputText {
            text: image_open_tag_text(),
        },
        image("data:image/png;base64,AAAA"),
        ContentItem::InputText {
            text: image_close_tag_text(),
        },
        ContentItem::InputText {
            text: "recent".into(),
        },
    ];
    let item = message(content.clone());
    let budget = retained_tokens(&message(content[1..].to_vec())) + 10;
    let retained = retain_messages(&[item], budget);
    let ResponseItem::Message { content: kept, .. } = &retained[0] else {
        panic!("message")
    };
    assert_eq!(&kept[kept.len() - 4..], &content[1..]);
    assert!(retained_tokens(&retained[0]) <= budget);
}

#[test]
fn oversized_image_does_not_backfill_older_input_or_leave_labels() {
    let input = vec![
        text("old but cheap"),
        message(vec![
            ContentItem::InputText {
                text: image_open_tag_text(),
            },
            image("https://example.test/image"),
            ContentItem::InputText {
                text: image_close_tag_text(),
            },
            ContentItem::InputText {
                text: "new tail".into(),
            },
        ]),
    ];
    assert_eq!(retain_messages(&input, 100), vec![text("new tail")]);
    assert_eq!(
        retain_messages(&[input[0].clone(), message(vec![image("remote")])], 100),
        vec![]
    );
}

#[test]
fn inline_image_transport_bytes_are_never_truncated_or_charged_as_text() {
    let item = message(vec![image(&format!(
        "data:image/png;base64,{}",
        "A".repeat(100_000)
    ))]);
    let cost = retained_tokens(&item);
    assert!(cost < 3_000);
    assert_eq!(
        retain_messages(std::slice::from_ref(&item), cost),
        vec![item]
    );
}
