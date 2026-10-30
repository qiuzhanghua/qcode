//! Context compaction.
//!
//! The event log is append-only, so compaction is recorded as an event that
//! names the seq range it replaces. `history::messages_from_events` applies
//! those ranges when rebuilding the provider context, which is what makes
//! replay, resume and forks see the same compacted history.
//!
//! Tokens are estimated from characters (roughly four characters per token).
//! That is deliberately simple and reproducible: no tokenizer dependency, and
//! tests get the same numbers on every machine.

use crate::event::{ContentBlock, Event, EventKind};
use crate::provider::{Message, Role};

/// Stable trigger name written into `compaction` events.
pub const TRIGGER: &str = "context_78_percent";
/// Fraction of the context window that triggers compaction.
pub const THRESHOLD_PERCENT: u64 = 78;
/// Recent turns kept verbatim (the spec asks for two).
pub const KEEP_TURNS: usize = 2;
/// Cap on the transcript handed to the summarizer.
const SUMMARY_INPUT_CHARS: usize = 60_000;
/// Tool results larger than this are elided before summarization (SPEC §4.6).
const EVICT_TOOL_OUTPUT_CHARS: usize = 4_000;

pub const SUMMARY_SYSTEM: &str = "You compact the earlier part of a coding session for \\
     another model that will continue it. Preserve the user's goal, decisions made, files \\
     changed, open questions and the next step. Be concrete and terse. Reply with the \\
     summary only, no preamble.";

pub fn threshold(window_tokens: u64) -> u64 {
    ((window_tokens as u128 * THRESHOLD_PERCENT as u128) / 100) as u64
}

/// Rough token estimate for a provider message list.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(message_chars).sum();
    (chars / 4) as u64
}

fn message_chars(message: &Message) -> usize {
    message
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text, .. } => {
                text.chars().count()
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => name.chars().count() + arguments.to_string().chars().count(),
            // The payload is not sent back to the model, but it is part of the
            // context the provider has to carry.
            ContentBlock::Image { data_base64, .. } => data_base64.len() / 4,
        })
        .sum()
}

/// Seq of the first event that must stay verbatim: the start of the last
/// `keep` turns. `None` when there is nothing older to replace.
pub fn keep_from(events: &[Event], keep: usize) -> Option<u64> {
    let turn_starts: Vec<u64> = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::TurnStarted(_)))
        .map(|event| event.seq)
        .collect();
    if turn_starts.len() <= keep {
        return None;
    }
    let boundary = turn_starts[turn_starts.len() - keep];
    let has_old_messages = events
        .iter()
        .any(|event| event.seq < boundary && is_message_event(&event.kind));
    has_old_messages.then_some(boundary)
}

/// First seq before `boundary` that actually contributes a message.
pub fn first_message_seq_before(events: &[Event], boundary: u64) -> Option<u64> {
    events
        .iter()
        .find(|event| event.seq < boundary && is_message_event(&event.kind))
        .map(|event| event.seq)
}

pub fn is_message_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::UserMessage(_) | EventKind::AssistantMessage(_) | EventKind::ToolResult(_)
    )
}

/// Render the events before `boundary` as a plain transcript for the
/// summarizer. Old compactions are already folded into their summaries by
/// `messages_from_events`.
pub fn summary_input(events: &[Event], boundary: u64) -> String {
    let mut old: Vec<Event> = events
        .iter()
        .filter(|event| event.seq < boundary)
        .cloned()
        .collect();
    evict_large_tool_outputs(&mut old);
    let messages = crate::history::messages_from_events(&old);
    let mut text = String::new();
    for message in messages {
        let role = match message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        for block in message.blocks {
            match block {
                ContentBlock::Text { text: chunk }
                | ContentBlock::Reasoning { text: chunk, .. } => {
                    text.push_str(role);
                    text.push_str(": ");
                    text.push_str(&chunk);
                    text.push('\n');
                }
                // Images are never replayed into a summary: the summarizer
                // cannot read them, and their base64 would dwarf the text.
                ContentBlock::Image { .. } => {}
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => {
                    text.push_str(role);
                    text.push_str(" called ");
                    text.push_str(&name);
                    text.push(' ');
                    text.push_str(&arguments.to_string());
                    text.push('\n');
                }
            }
        }
    }
    truncate_middle(&text, SUMMARY_INPUT_CHARS)
}

/// Hybrid compaction, step one: keep huge tool results out of the summarizer
/// input. Kept turns still see the original event; only the summary prompt is
/// reduced.
fn evict_large_tool_outputs(events: &mut [Event]) {
    for event in events {
        let EventKind::ToolResult(result) = &mut event.kind else {
            continue;
        };
        let total = result.output.chars().count();
        if total > EVICT_TOOL_OUTPUT_CHARS {
            let head: String = result.output.chars().take(1_000).collect();
            result.output =
                format!("{head}\n… (tool output elided before summarization: {total} chars) …");
        }
    }
}

fn truncate_middle(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let head: String = chars[..limit / 3].iter().collect();
    let tail: String = chars[chars.len() - (limit * 2 / 3)..].iter().collect();
    format!("{head}\n… (older middle omitted) …\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AssistantMessage, ToolMetadata, ToolResult, ToolStatus, TurnStarted, UserMessage,
    };
    use serde_json::json;

    fn event(seq: u64, kind: EventKind) -> Event {
        Event::new(seq, "s", 1, kind)
    }

    fn user(seq: u64, text: &str) -> Event {
        event(
            seq,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: text.into(),
                attachments: vec![],
            }),
        )
    }

    #[test]
    fn threshold_is_78_percent() {
        assert_eq!(threshold(1_000), 780);
        assert_eq!(threshold(128_000), 99_840);
    }

    #[test]
    fn estimates_four_characters_per_token() {
        let message = Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "x".repeat(400),
            }],
            tool_call_id: None,
        };
        assert_eq!(estimate_tokens(&[message]), 100);
    }

    #[test]
    fn keeps_the_last_two_turns() {
        let events = vec![
            event(
                1,
                EventKind::SessionStarted(crate::event::SessionStarted {
                    model: "m".into(),
                    provider: "p".into(),
                    cwd: "/".into(),
                    version: "0".into(),
                }),
            ),
            event(
                2,
                EventKind::TurnStarted(TurnStarted {
                    input: "one".into(),
                }),
            ),
            user(3, "one"),
            event(
                4,
                EventKind::TurnStarted(TurnStarted {
                    input: "two".into(),
                }),
            ),
            user(5, "two"),
            event(
                6,
                EventKind::TurnStarted(TurnStarted {
                    input: "three".into(),
                }),
            ),
            user(7, "three"),
        ];
        // Keep turns "two" and "three"; turn "one" is compactable.
        assert_eq!(keep_from(&events, 2), Some(4));
        assert_eq!(first_message_seq_before(&events, 4), Some(3));
    }

    #[test]
    fn nothing_to_compact_when_only_two_turns_exist() {
        let events = vec![
            event(
                1,
                EventKind::TurnStarted(TurnStarted {
                    input: "one".into(),
                }),
            ),
            user(2, "one"),
            event(
                3,
                EventKind::TurnStarted(TurnStarted {
                    input: "two".into(),
                }),
            ),
            user(4, "two"),
        ];
        assert_eq!(keep_from(&events, 2), None);
    }

    #[test]
    fn summary_input_uses_roles_and_truncates_the_middle() {
        let events = vec![
            event(
                1,
                EventKind::TurnStarted(TurnStarted { input: "go".into() }),
            ),
            user(2, &"a".repeat(1_000)),
            event(
                3,
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
        ];
        let text = summary_input(&events, 10);
        assert!(text.contains("User: "));
        assert!(text.contains("Assistant called read_file"));
        assert!(text.len() <= SUMMARY_INPUT_CHARS * 4 + 100);
    }

    #[test]
    fn summary_input_elides_large_tool_results() {
        let huge = "x".repeat(10_000);
        let events = vec![
            event(
                1,
                EventKind::TurnStarted(TurnStarted {
                    input: "go".to_string(),
                }),
            ),
            user(2, "read it"),
            event(
                3,
                EventKind::ToolResult(ToolResult {
                    call_id: "c1".to_string(),
                    status: ToolStatus::Ok,
                    output: huge.clone(),
                    metadata: ToolMetadata::default(),
                }),
            ),
            event(
                4,
                EventKind::TurnStarted(TurnStarted {
                    input: "next".to_string(),
                }),
            ),
        ];

        let text = summary_input(&events, 4);
        assert!(text.contains("tool output elided"), "{text}");
        assert!(
            text.len() < 5_000,
            "summary input was not reduced: {}",
            text.len()
        );
    }
}
