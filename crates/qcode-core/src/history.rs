//! Rebuild provider messages from the event log.
//!
//! This is the resume path: the JSONL log is the source of truth, so a restart
//! (or a fork) reconstructs the conversation instead of relying on in-memory
//! provider state.

use crate::event::{ContentBlock, Event, EventKind};
use crate::provider::{Message, Role};

pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    // A compaction event replaces the seq range it names. Older compactions
    // nested inside a newer range are dropped with the rest of that range, so
    // only the newest summary survives.
    let replaced: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(compaction) => Some((
                compaction.replaced_from as u64,
                compaction.replaced_to as u64,
            )),
            _ => None,
        })
        .collect();
    let is_replaced = |seq: u64| replaced.iter().any(|(from, to)| seq >= *from && seq <= *to);

    let mut messages = Vec::new();

    for event in events {
        if is_replaced(event.seq) {
            continue;
        }
        match &event.kind {
            EventKind::Compaction(compaction) => messages.push(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: summary_text(&compaction.summary),
                }],
                tool_call_id: None,
            }),
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

/// Prefix makes the summary unmistakable to the model and to anyone reading
/// the rebuilt context.
pub fn summary_text(summary: &str) -> String {
    format!("Conversation summary (compacted):\n{summary}")
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
    fn compaction_replaces_its_range_with_a_summary() {
        use crate::event::Compaction;

        let events = vec![
            event(
                1,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: "old question".into(),
                    attachments: vec![],
                }),
            ),
            event(
                2,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![ContentBlock::Text {
                        text: "old answer".into(),
                    }],
                    provider_raw: json!({}),
                    usage: Default::default(),
                }),
            ),
            event(
                3,
                EventKind::Compaction(Compaction {
                    trigger: "context_78_percent".into(),
                    summary: "old work".into(),
                    replaced_from: 1,
                    replaced_to: 2,
                    tokens_before: 100,
                    tokens_after: 10,
                }),
            ),
            event(
                4,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: "new question".into(),
                    attachments: vec![],
                }),
            ),
        ];

        let messages = messages_from_events(&events);
        assert_eq!(messages.len(), 2, "summary + newest message");
        assert_eq!(messages[0].role, Role::User);
        let glued = serde_json::to_string(&messages[0].blocks).unwrap();
        assert!(glued.contains("Conversation summary (compacted)"));
        assert!(glued.contains("old work"));
        assert!(!glued.contains("old question"));
        let glued = serde_json::to_string(&messages[1].blocks).unwrap();
        assert!(glued.contains("new question"));
    }

    #[test]
    fn rebuilds_user_assistant_and_tool_messages() {
        let events = vec![
            event(
                1,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
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
