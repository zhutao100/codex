use super::Session;
use super::turn::RealtimeEventText;
use super::turn::agent_message_text;
use super::turn::realtime_text_for_event;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use tracing::debug;

#[derive(Debug, PartialEq)]
pub(super) enum RealtimeMirrorEvent {
    StreamStarted {
        item_id: String,
        phase: Option<MessagePhase>,
        text: String,
    },
    StreamDelta {
        item_id: String,
        delta: String,
    },
    StreamCompleted {
        item_id: String,
        fallback: Option<RealtimeEventText>,
    },
    Text(RealtimeEventText),
}

#[cfg(test)]
#[path = "event_dispatch_tests.rs"]
mod tests;

impl RealtimeMirrorEvent {
    pub(super) fn from_event(msg: &EventMsg) -> Option<Self> {
        match msg {
            EventMsg::ItemStarted(event) => match &event.item {
                TurnItem::AgentMessage(item) => Some(Self::StreamStarted {
                    item_id: item.id.clone(),
                    phase: item.phase.clone(),
                    text: agent_message_text(item),
                }),
                _ => None,
            },
            EventMsg::AgentMessageContentDelta(event) => Some(Self::StreamDelta {
                item_id: event.item_id.clone(),
                delta: event.delta.clone(),
            }),
            EventMsg::ItemCompleted(event) => {
                let fallback = realtime_text_for_event(msg);
                match &event.item {
                    TurnItem::AgentMessage(item) => Some(Self::StreamCompleted {
                        item_id: item.id.clone(),
                        fallback,
                    }),
                    _ => fallback.map(Self::Text),
                }
            }
            _ => realtime_text_for_event(msg).map(Self::Text),
        }
    }
}

impl Session {
    pub(super) async fn maybe_mirror_event_to_realtime(&self, event: Option<RealtimeMirrorEvent>) {
        if self.conversation.running_state().await.is_none() {
            return;
        }
        let Some(event) = event else {
            return;
        };
        let text = match event {
            RealtimeMirrorEvent::StreamStarted {
                item_id,
                phase,
                text,
            } => {
                self.conversation
                    .register_handoff_stream_item(item_id, phase, text)
                    .await;
                return;
            }
            RealtimeMirrorEvent::StreamDelta { item_id, delta } => {
                if let Err(err) = self
                    .conversation
                    .stream_handoff_delta(&item_id, delta)
                    .await
                {
                    debug!("failed to stream event text to realtime conversation: {err}");
                }
                return;
            }
            RealtimeMirrorEvent::StreamCompleted { item_id, fallback } => {
                if self.conversation.finish_handoff_stream_item(&item_id).await {
                    None
                } else {
                    fallback
                }
            }
            RealtimeMirrorEvent::Text(text) => Some(text),
        };
        let result = match text {
            Some(RealtimeEventText::Handoff(text, phase)) => {
                self.conversation.handoff_out(text, phase).await
            }
            Some(RealtimeEventText::QuietReasoning(text)) => {
                self.conversation.send_reasoning_status(&text).await
            }
            None => return,
        };
        if let Err(err) = result {
            debug!("failed to mirror event text to realtime conversation: {err}");
        }
    }
}
