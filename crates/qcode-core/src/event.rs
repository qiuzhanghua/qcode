//! Append-only event model.
//!
//! The JSONL session file is the single source of truth. Every event carries a
//! monotonic `seq`, the owning session id and the turn it belongs to. Provider
//! payloads are stored verbatim in `provider_raw` so that thinking blocks and
//! reasoning items can be replayed to the model later.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::task::Root;

/// Bumped whenever the on-disk shape changes in a breaking way.
///
/// v4 added `UserMessage::starts_segment`/`queued_id`; v5 added
/// `ToolResult::diff`, which is additive — a v5 reader still reads v4 logs, and
/// the missing diff only costs the inline diff view.
pub const EVENT_SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub session: String,
    pub turn: u32,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(seq: u64, session: impl Into<String>, turn: u32, kind: EventKind) -> Self {
        Self {
            v: EVENT_SCHEMA_VERSION,
            seq,
            ts: OffsetDateTime::now_utc(),
            session: session.into(),
            turn,
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted(SessionStarted),
    TaskConfigured(TaskConfigured),
    TaskRenamed(TaskRenamed),
    RootsChanged(RootsChanged),
    TurnStarted(TurnStarted),
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ApprovalRequested(ApprovalRequested),
    ApprovalDecided(ApprovalDecided),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Compaction(Compaction),
    TurnEnded(TurnEnded),
    Error(ErrorEvent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub model: String,
    pub provider: String,
    pub cwd: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskConfigured {
    pub roots: Vec<Root>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRenamed {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootsChanged {
    pub roots: Vec<Root>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStarted {
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// The inbox item this message came from, when the engine injected a
    /// steering message. Lets a frontend reconcile its "waiting for the next
    /// step" row with the authoritative message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_id: Option<String>,
    /// This message opens a new transcript segment (turn card): the engine set
    /// it for injected steering, so projections do not have to guess.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_segment: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub blocks: Vec<ContentBlock>,
    pub provider_raw: Value,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// An image the user attached. Base64 payload, no data URI prefix: the
    /// provider layer adds that when it builds the wire format.
    Image {
        mime_type: String,
        data_base64: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
}

/// One provider call's token counters.
///
/// `input_tokens` is the **total** prompt size, cached tokens included — that is
/// what `prompt_tokens` means in the OpenAI/DeepSeek wire format, and the only
/// provider this build talks to. `cache_read_tokens` is the cached subset of
/// that total, not an extra count; `Pricing::cost_usd` therefore bills
/// `input_tokens - cache_read_tokens` at the input rate. A provider that
/// reports cache hits outside `prompt_tokens` must add them back before
/// filling this struct.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl Usage {
    /// Accumulate another provider call's usage into this one.
    pub fn merge(&mut self, other: &Usage) {
        fn add(slot: &mut Option<u64>, value: Option<u64>) {
            if let Some(value) = value {
                *slot = Some(slot.unwrap_or(0) + value);
            }
        }
        add(&mut self.input_tokens, other.input_tokens);
        add(&mut self.output_tokens, other.output_tokens);
        add(&mut self.cache_read_tokens, other.cache_read_tokens);
        add(&mut self.cache_write_tokens, other.cache_write_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequested {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecided {
    pub call_id: String,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub status: ToolStatus,
    pub output: String,
    #[serde(default)]
    pub metadata: ToolMetadata,
    /// What this call actually changed on disk. `None` for a read-only tool, a
    /// failed call, or a log written before schema v5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ToolDiff>,
}

/// The change one write-ish tool call made, as a unified diff plus per-file
/// counts. Stored on the result rather than in [`ToolMetadata`] because it is
/// the outcome of the call, not bookkeeping about its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDiff {
    /// Unified diff with `--- a/<path>` / `+++ b/<path>` headers, so the GUI can
    /// hand it straight to a diff viewer. Empty when [`Self::truncated`].
    pub patch: String,
    pub files: Vec<DiffFileStat>,
    /// The patch outgrew the size cap and was dropped; the counts in `files`
    /// are still exact.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffFileStat {
    pub path: String,
    pub kind: DiffKind,
    pub additions: u32,
    pub deletions: u32,
    /// Size of the resulting file, for the "1KB" chip next to the row.
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolMetadata {
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    pub trigger: String,
    pub summary: String,
    pub replaced_from: u32,
    pub replaced_to: u32,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEnded {
    pub status: TurnStatus,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub kind: String,
    pub message: String,
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_shape_and_roundtrip() {
        let event = Event::new(
            1,
            "s1",
            0,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: "hi".into(),
                attachments: vec![],
            }),
        );
        let json = serde_json::to_string(&event).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["v"], EVENT_SCHEMA_VERSION);
        assert_eq!(value["seq"], 1);
        assert_eq!(value["session"], "s1");
        assert_eq!(value["type"], "user_message");
        assert_eq!(value["data"]["text"], "hi");

        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, event.seq);
        assert_eq!(back.kind, event.kind);
    }

    #[test]
    fn steering_fields_are_optional_on_the_wire() {
        // A v3 log entry has no steering fields: they must default.
        let older: Event = serde_json::from_str(
            r#"{"v":3,"seq":2,"ts":"2026-10-30T00:00:00Z","session":"s1","turn":1,
                "type":"user_message","data":{"text":"hi"}}"#,
        )
        .unwrap();
        let EventKind::UserMessage(message) = older.kind else {
            panic!("expected a user message");
        };
        assert_eq!(message.queued_id, None);
        assert!(!message.starts_segment);

        // A v4 steered message carries both fields and round-trips.
        let event = Event::new(
            3,
            "s1",
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: true,
                queued_id: Some("q1".into()),
                text: "steer".into(),
                attachments: vec![],
            }),
        );
        let json = serde_json::to_string(&event).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["data"]["queued_id"], "q1");
        assert_eq!(value["data"]["starts_segment"], true);
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, event.kind);

        // Defaults stay off the wire, so v4 input messages look like v3 ones.
        let input = Event::new(
            4,
            "s1",
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: "hi".into(),
                attachments: vec![],
            }),
        );
        let value: Value = serde_json::from_str(&serde_json::to_string(&input).unwrap()).unwrap();
        assert!(value["data"].get("queued_id").is_none());
        assert!(value["data"].get("starts_segment").is_none());
    }
}
