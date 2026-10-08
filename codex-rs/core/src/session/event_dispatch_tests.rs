use super::*;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::ReasoningItem;
use codex_protocol::protocol::ItemCompletedEvent;
use pretty_assertions::assert_eq;

fn completed_event(item: TurnItem) -> EventMsg {
    EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn".to_string(),
        item,
        started_at_ms: None,
        completed_at_ms: 0,
    })
}

#[test]
fn completed_reasoning_captures_latest_nonempty_status() {
    let event = completed_event(TurnItem::Reasoning(ReasoningItem {
        id: "reasoning".to_string(),
        summary_text: vec![
            "Earlier status".to_string(),
            " Current status ".to_string(),
            " ".to_string(),
        ],
        raw_content: vec!["Raw reasoning".to_string()],
    }));

    assert_eq!(
        RealtimeMirrorEvent::from_event(&event),
        Some(RealtimeMirrorEvent::Text(
            RealtimeEventText::QuietReasoning("Current status".to_string(),)
        )),
    );
}

#[test]
fn completed_message_preserves_stream_fallback_and_phase() {
    let event = completed_event(TurnItem::AgentMessage(AgentMessageItem {
        id: "message".to_string(),
        content: vec![
            AgentMessageContent::Text {
                text: "Final ".to_string(),
            },
            AgentMessageContent::Text {
                text: "answer".to_string(),
            },
        ],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    }));

    assert_eq!(
        RealtimeMirrorEvent::from_event(&event),
        Some(RealtimeMirrorEvent::StreamCompleted {
            item_id: "message".to_string(),
            fallback: Some(RealtimeEventText::Handoff(
                "Final answer".to_string(),
                Some(MessagePhase::FinalAnswer),
            )),
        }),
    );
}
