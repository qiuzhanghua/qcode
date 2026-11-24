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
    // Pictures a tool read (ADR-0081) cannot ride on the tool message — every
    // wire format allows text only there — so they follow as a user message.
    // They may only appear *after the last* tool message of the batch: the
    // provider requires one assistant `tool_calls` message to be answered by
    // consecutive tool messages, and a user message between two results makes
    // the whole request invalid ("insufficient tool messages"). Holding them
    // here instead of pushing them next to their own result is what keeps a
    // batch of several calls (read_file + run_shell, say) legal.
    let mut tool_images: Vec<Vec<ContentBlock>> = Vec::new();

    for event in events {
        if is_replaced(event.seq) {
            continue;
        }
        match &event.kind {
            EventKind::Compaction(compaction) => {
                flush_tool_images(&mut messages, &mut tool_images);
                messages.push(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text {
                        text: summary_text(&compaction.summary),
                    }],
                    tool_call_id: None,
                });
            }
            EventKind::UserMessage(message) => {
                flush_tool_images(&mut messages, &mut tool_images);
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
            EventKind::AssistantMessage(message) => {
                flush_tool_images(&mut messages, &mut tool_images);
                messages.push(Message {
                    role: Role::Assistant,
                    blocks: message.blocks.clone(),
                    tool_call_id: None,
                });
            }
            EventKind::ToolResult(result) => {
                messages.push(Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::Text {
                        text: result.output.clone(),
                    }],
                    tool_call_id: Some(result.call_id.clone()),
                });
                if let Some(blocks) = tool_image_blocks(result, &mut load) {
                    tool_images.push(blocks);
                }
            }
            _ => {}
        }
    }
    // The batch ended with the event log: the pictures still have to land.
    flush_tool_images(&mut messages, &mut tool_images);
    // Logs written by older builds can hold a call that never got a result.
    drop_unanswered_calls(&mut messages);

    messages
}

/// Drop `tool_calls` that no tool message answers.
///
/// A turn that paused on its budget (or stopped on `no_progress`) used to
/// persist the call without a result, and a process that died mid-round leaves
/// the same shape. Replayed unchanged, such a message makes the provider reject
/// *every* later request ("insufficient tool messages following tool_calls
/// message"), so the session could never be resumed. The text stays; the
/// unanswered calls go — nothing ran, so nothing is lost.
fn drop_unanswered_calls(messages: &mut Vec<Message>) {
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role != Role::Assistant {
            index += 1;
            continue;
        }
        let calls: Vec<String> = messages[index]
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }

        // Only the messages directly behind it answer it; a call answered later
        // is no more valid than one answered never.
        let mut answered: Vec<String> = Vec::new();
        let mut next = index + 1;
        while next < messages.len() && messages[next].role == Role::Tool {
            if let Some(call_id) = &messages[next].tool_call_id {
                answered.push(call_id.clone());
            }
            next += 1;
        }
        if calls.iter().all(|call_id| answered.contains(call_id)) {
            index = next;
            continue;
        }

        messages[index].blocks.retain(|block| match block {
            ContentBlock::ToolCall { call_id, .. } => answered.contains(call_id),
            _ => true,
        });
        if messages[index].blocks.is_empty() {
            messages.remove(index);
        } else {
            index += 1;
        }
    }
}

/// Append the deferred picture messages, in order, after the tool messages
/// they belong to. Called at every message boundary and at the end of the log.
fn flush_tool_images(messages: &mut Vec<Message>, pending: &mut Vec<Vec<ContentBlock>>) {
    for blocks in pending.drain(..) {
        messages.push(Message {
            role: Role::User,
            blocks,
            tool_call_id: None,
        });
    }
}

/// Assemble the follow-up user message for pictures a tool produced: one
/// caption naming them, then the image blocks (ADR-0081).
///
/// Shared by the two places that need it — replaying the log, and the turn that
/// just ran the tool — because the caption and the block order are the part
/// that must not drift between them. `images` is `(mime, base64, label)`.
pub(crate) fn image_followup(images: Vec<(String, String, String)>) -> Option<Vec<ContentBlock>> {
    if images.is_empty() {
        return None;
    }
    let mut names = Vec::with_capacity(images.len());
    let mut blocks = Vec::with_capacity(images.len() + 1);
    for (mime_type, data_base64, label) in images {
        names.push(label);
        blocks.push(ContentBlock::Image {
            mime_type,
            data_base64,
        });
    }
    let mut with_caption = vec![ContentBlock::Text {
        text: format!(
            "Image(s) read by the tool call above: {}.",
            names.join(", ")
        ),
    }];
    with_caption.extend(blocks);
    Some(with_caption)
}

/// The image blocks for one tool result, or `None` when it returned no
/// pictures (or none of them load any more).
fn tool_image_blocks<F>(
    result: &crate::event::ToolResult,
    load: &mut F,
) -> Option<Vec<ContentBlock>>
where
    F: FnMut(&ImageRef) -> Option<Vec<u8>>,
{
    let mut images = Vec::with_capacity(result.images.len());
    for image in &result.images {
        // A missing blob (cleaned data dir) costs the picture, not the turn:
        // the tool's text output already said a picture was read.
        let Some(bytes) = load(image) else {
            continue;
        };
        images.push((
            image.mime.as_str().to_string(),
            BASE64.encode(bytes),
            image.name.clone().unwrap_or_else(|| "image".to_string()),
        ));
    }
    image_followup(images)
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
                    images: Vec::new(),
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

    /// A picture a tool read (ADR-0081) cannot ride on the tool message — every
    /// wire format allows text only there — so it comes back as a user message
    /// right behind it, caption first.
    #[test]
    fn a_tools_picture_replays_as_a_user_message_after_the_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = qcode_attachments::AttachmentStore::open(dir.path()).expect("store");
        let image = store
            .put_image(qcode_attachments::NewImage {
                bytes: &png_bytes(),
                mime: qcode_attachments::ImageMime::Png,
                name: Some("shot.png"),
            })
            .expect("put image");

        // No `ToolCall` event here on purpose: a call rides inside the
        // assistant message's blocks, so a tool result replays straight after
        // the message that asked for it.
        let events = vec![event(
            2,
            EventKind::ToolResult(ToolResult {
                call_id: "c1".into(),
                status: ToolStatus::Ok,
                output: "shot.png: image/png, 70 bytes — returned as an image".into(),
                metadata: Default::default(),
                diff: None,
                images: vec![image],
            }),
        )];

        let messages = messages_from_events_with(
            &events,
            |image| store.read_image(image).ok(),
            |_| String::new(),
        );

        assert_eq!(messages.len(), 2, "the result, then the picture");
        assert_eq!(messages[0].role, Role::Tool);
        assert_eq!(messages[1].role, Role::User);
        assert!(messages[1].tool_call_id.is_none());
        let blocks = &messages[1].blocks;
        assert_eq!(blocks.len(), 2, "caption plus the picture");
        let ContentBlock::Text { text } = &blocks[0] else {
            panic!("the first block has to name the picture: {blocks:?}");
        };
        assert!(text.contains("shot.png"), "{text}");
        let ContentBlock::Image {
            mime_type,
            data_base64,
        } = &blocks[1]
        else {
            panic!("the picture has to reach the model: {blocks:?}");
        };
        assert_eq!(mime_type, "image/png");
        assert!(!data_base64.is_empty());
    }

    /// A cleaned data dir loses the blob but must not lose the turn: the tool's
    /// text already said what it read.
    #[test]
    fn a_missing_tool_image_blob_degrades_to_no_message() {
        let events = vec![event(
            1,
            EventKind::ToolResult(ToolResult {
                call_id: "c1".into(),
                status: ToolStatus::Ok,
                output: "shot.png: image/png, 70 bytes".into(),
                metadata: Default::default(),
                diff: None,
                images: vec![qcode_attachments::ImageRef {
                    id: qcode_attachments::AttachmentId::from_bytes([0; 32]),
                    mime: qcode_attachments::ImageMime::Png,
                    bytes: 70,
                    width: 1,
                    height: 1,
                    origin: None,
                    name: Some("shot.png".into()),
                }],
            }),
        )];

        let messages = messages_from_events_with(&events, |_| None, |_| String::new());
        assert_eq!(messages.len(), 1, "only the tool result, no empty caption");
        assert_eq!(messages[0].role, Role::Tool);
    }

    /// One assistant message may ask for several calls (`read_file` plus
    /// `run_shell` in the same round, which a real run did). The provider wants
    /// every call answered by tool messages *back to back*; a user message
    /// carrying the picture between the two results makes it reject the request
    /// with "insufficient tool messages following tool_calls message". So the
    /// picture has to wait for the end of the batch.
    #[test]
    fn a_tools_picture_waits_for_the_whole_batch_of_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = qcode_attachments::AttachmentStore::open(dir.path()).expect("store");
        let image = store
            .put_image(qcode_attachments::NewImage {
                bytes: &png_bytes(),
                mime: qcode_attachments::ImageMime::Png,
                name: Some("shot.png"),
            })
            .expect("put image");

        let events = vec![
            event(
                1,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![
                        ContentBlock::ToolCall {
                            call_id: "c1".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "shot.png"}),
                        },
                        ContentBlock::ToolCall {
                            call_id: "c2".into(),
                            name: "run_shell".into(),
                            arguments: json!({"command": "ls"}),
                        },
                    ],
                    provider_raw: json!({}),
                    usage: Default::default(),
                }),
            ),
            event(
                2,
                EventKind::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    status: ToolStatus::Ok,
                    output: "shot.png: image/png".into(),
                    metadata: Default::default(),
                    diff: None,
                    images: vec![image],
                }),
            ),
            event(
                3,
                EventKind::ToolResult(ToolResult {
                    call_id: "c2".into(),
                    status: ToolStatus::Ok,
                    output: "a.txt".into(),
                    metadata: Default::default(),
                    diff: None,
                    images: Vec::new(),
                }),
            ),
        ];

        let messages = messages_from_events_with(
            &events,
            |image| store.read_image(image).ok(),
            |_| String::new(),
        );

        let roles: Vec<Role> = messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            vec![Role::Assistant, Role::Tool, Role::Tool, Role::User],
            "the picture has to follow *both* tool messages"
        );
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c2"));
        assert!(messages[3].tool_call_id.is_none());
        assert!(matches!(
            &messages[3].blocks[0],
            ContentBlock::Text { text } if text.contains("shot.png")
        ));
    }

    /// A log written by an older build can hold a call that never ran (a pause
    /// on budget, a `no_progress` stop). Replayed unchanged, the provider
    /// rejects every later request, so the replay drops exactly the calls that
    /// have no answer right behind them.
    #[test]
    fn unanswered_calls_are_dropped_on_replay() {
        let events = vec![
            event(
                1,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![
                        ContentBlock::Text {
                            text: "let me look".into(),
                        },
                        ContentBlock::ToolCall {
                            call_id: "ran".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "a"}),
                        },
                        ContentBlock::ToolCall {
                            call_id: "never-ran".into(),
                            name: "run_shell".into(),
                            arguments: json!({"command": "ls"}),
                        },
                    ],
                    provider_raw: json!({}),
                    usage: Default::default(),
                }),
            ),
            event(
                2,
                EventKind::ToolResult(ToolResult {
                    call_id: "ran".into(),
                    status: ToolStatus::Ok,
                    output: "a".into(),
                    metadata: Default::default(),
                    diff: None,
                    images: Vec::new(),
                }),
            ),
        ];

        let messages = messages_from_events(&events);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        let calls: Vec<&str> = messages[0]
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec!["ran"], "only the answered call survives");
        assert_eq!(messages[1].role, Role::Tool);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("ran"));

        // A call-only message with nothing answered disappears entirely: an
        // assistant turn that said nothing and did nothing is no message.
        let only = vec![event(
            3,
            EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![ContentBlock::ToolCall {
                    call_id: "never-ran".into(),
                    name: "run_shell".into(),
                    arguments: json!({"command": "ls"}),
                }],
                provider_raw: json!({}),
                usage: Default::default(),
            }),
        )];
        assert!(
            messages_from_events(&only).is_empty(),
            "an unanswered call-only round leaves no message behind"
        );
    }
}
