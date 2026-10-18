//! Rebuild provider messages from the event log.
//!
//! This is the resume path: the JSONL log is the source of truth, so a restart
//! (or a fork) reconstructs the conversation instead of relying on in-memory
//! provider state.

use crate::event::{ContentBlock, Event, EventKind};
use crate::provider::{Message, Role};

pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    let mut messages = Vec::new();

    for event in events {
        match &event.kind {
            EventKind::UserMessage(message) => messages.push(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: message.text.clone(),
                }],
                tool_call_id: None,
            }),
            EventKind::AssistantMessage(message) => messages.push(Message {
                role: Role::Assistant,
                blocks: message.blocks.clone(),
                tool_call_id: None,
            }),
            EventKind::ToolResult(result) => messages.push(Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::Text {
                    text: result.output.clone(),
                }],
                tool_call_id: Some(result.call_id.clone()),
            }),
            _ => {}
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AssistantMessage, ToolResult, ToolStatus, UserMessage};
    use serde_json::json;

    fn event(seq: u64, kind: EventKind) -> Event {
        Event::new(seq, "s", 1, kind)
    }

    #[test]
    fn rebuilds_user_assistant_and_tool_messages() {
        let events = vec![
            event(
                1,
                EventKind::UserMessage(UserMessage {
                    text: "hi".into(),
                    attachments: vec![],
                }),
            ),
            event(
                2,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![ContentBlock::ToolCall {
                        call_id: "c1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a"}),
                    }],
                    provider_raw: json!({}),
                    usage: Default::default(),
                }),
            ),
            event(
                3,
                EventKind::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    status: ToolStatus::Ok,
                    output: "contents".into(),
                    metadata: Default::default(),
                }),
            ),
        ];

        let messages = messages_from_events(&events);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].blocks.len(), 1);
    }
}
