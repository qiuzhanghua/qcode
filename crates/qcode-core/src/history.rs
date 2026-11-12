//! Rebuild provider messages from the event log.
//!
//! This is the resume path: the JSONL log is the source of truth, so a restart
//! (or a fork) reconstructs the conversation instead of relying on in-memory
//! provider state.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use qcode_attachments::{AttachmentRef, FileRef, ImageRef};

use crate::event::{ContentBlock, Event, EventKind};
use crate::provider::{Message, Role};

pub fn messages_from_events(events: &[Event]) -> Vec<Message> {
    messages_from_events_with(events, |_| None, |_| String::new())
}

/// The blocks one user message turns into: the text, then a block per
/// attachment. `load` returns `None` when the object is gone (the reference
/// stays in the log, so a later pass can serve it); `describe_file` returns the
/// handle text for a stored file, or an empty string to skip it.
pub fn blocks_for_message<F, G>(
    text: &str,
    attachments: &[AttachmentRef],
    mut load: F,
    mut describe_file: G,
) -> Vec<ContentBlock>
where
    F: FnMut(&ImageRef) -> Option<Vec<u8>>,
    G: FnMut(&FileRef) -> String,
{
    let mut blocks = vec![ContentBlock::Text {
        text: text.to_string(),
    }];
    for attachment in attachments {
        match attachment {
            AttachmentRef::Image(image) => {
                if let Some(bytes) = load(image) {
                    blocks.push(ContentBlock::Image {
                        mime_type: image.mime.as_str().to_string(),
                        data_base64: BASE64.encode(bytes),
                    });
                }
            }
            AttachmentRef::File(file) => {
                let text = describe_file(file);
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text { text });
                }
            }
        }
    }
    blocks
}

/// Like [`messages_from_events`], but expands stored references: images come
/// back as bytes, files as the handle text the model reads (ADR-0061 §7).
pub fn messages_from_events_with<F, G>(
    events: &[Event],
    mut load: F,
    mut describe_file: G,
) -> Vec<Message>
where
    F: FnMut(&ImageRef) -> Option<Vec<u8>>,
    G: FnMut(&FileRef) -> String,
{
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
            EventKind::UserMessage(message) => {
                let blocks = blocks_for_message(
                    &message.text,
                    &message.attachments,
                    &mut load,
                    &mut describe_file,
                );
                messages.push(Message {
                    role: Role::User,
                    blocks,
                    tool_call_id: None,
                });
            }
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

    /// A PNG header is enough for the store: it never decodes pixels.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(&32u32.to_be_bytes());
        bytes
    }

    #[test]
    fn stored_images_come_back_in_the_user_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = qcode_attachments::AttachmentStore::open(dir.path()).expect("store");
        let png = png_bytes();
        let image = store
            .put_image(qcode_attachments::NewImage {
                bytes: &png,
                mime: qcode_attachments::ImageMime::Png,
                name: Some("shot.png"),
            })
            .expect("put image");

        let events = vec![event(
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: "look at this".into(),
                attachments: vec![AttachmentRef::Image(image)],
                continuation: None,
                question_reply: None,
                scheduled: None,
            }),
        )];

        let messages = messages_from_events_with(
            &events,
            |image| store.read_image(image).ok(),
            |_| String::new(),
        );
        let blocks = &messages[0].blocks;
        assert_eq!(blocks.len(), 2, "text plus the image");
        assert_eq!(
            blocks[0],
            ContentBlock::Text {
                text: "look at this".into()
            }
        );
        match &blocks[1] {
            ContentBlock::Image {
                mime_type,
                data_base64,
            } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(BASE64.decode(data_base64).unwrap(), png);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_object_drops_the_image_instead_of_failing() {
        let events = vec![event(
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: "gone".into(),
                attachments: vec![AttachmentRef::Image(qcode_attachments::ImageRef {
                    id: qcode_attachments::AttachmentId::from_bytes([7; 32]),
                    mime: qcode_attachments::ImageMime::Png,
                    bytes: 10,
                    width: 1,
                    height: 1,
                    origin: None,
                    name: None,
                })],
                continuation: None,
                question_reply: None,
                scheduled: None,
            }),
        )];

        let messages = messages_from_events_with(&events, |_| None, |_| String::new());
        assert_eq!(messages[0].blocks.len(), 1);
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

                    continuation: None,
                    question_reply: None,
                    scheduled: None,
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

                    continuation: None,
                    question_reply: None,
                    scheduled: None,
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

                    continuation: None,
                    question_reply: None,
                    scheduled: None,
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
                    diff: None,
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
