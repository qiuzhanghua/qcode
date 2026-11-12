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
/// the missing diff only costs the inline diff view. v6 makes running out of
/// budget a pause instead of a failure (`TurnEnded::pause`,
/// `TurnStatus::Paused`, `budget_hint`, `UserMessage::continuation`); all of it
/// is additive too, so a v5 log replays unchanged.
///
/// v7 turns `UserMessage::attachments` into references into the attachment
/// store (ADR-0061). No migration: the field was never written non-empty, so
/// older logs simply omit it.
///
/// v8 adds `ShellEnvSampled` (ADR-0064), an additive diagnostic: a v7 reader
/// that ignores unknown kinds replays the log unchanged, and a log without the
/// event simply means "sampling was off or never ran".
///
/// v9 adds `UserMessage::question_reply`, an additive optional field for a late
/// answer to a timed-out `ask_user` call: an old reader replays the log as a
/// plain user message (the payload is also present in `text`).
///
/// v10 adds `UserMessage::scheduled` (ADR-0059), an additive optional field for
/// a scheduled-task delivery: an old reader replays it as a plain user message.
///
/// v11 adds `ToolResult::images` (ADR-0081): a tool can hand pictures back to
/// the model, and they are references into the same attachment store user
/// attachments use. Additive — a v10 reader replays the log with the images
/// missing, which costs the model the picture but not the text summary the tool
/// wrote about it.
pub const EVENT_SCHEMA_VERSION: u32 = 11;

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
    /// A one-off notice that the turn is approaching a budget. Persisted so a
    /// replay shows the same warning the live run did.
    BudgetHint(BudgetHint),
    /// One MCP server's tool table changed between turns (ADR-0067 §5).
    ///
    /// Persisted, because the model's next call may name a tool this event
    /// retired: a replay (and `qc` itself) has to be able to say why that call
    /// is an error rather than leaving it unexplained.
    McpToolsRefreshed(McpToolsRefreshed),
    /// What the shell environment sampling produced for this turn (ADR-0064).
    /// Names only: a value could be an API key, and the log outlives the turn.
    ShellEnvSampled(ShellEnvSampled),
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
    /// Attachments by reference; the bytes live in the attachment store
    /// (ADR-0061). Empty for a text-only message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<qcode_attachments::AttachmentRef>,
    /// The inbox item this message came from, when the engine injected a
    /// steering message. Lets a frontend reconcile its "waiting for the next
    /// step" row with the authoritative message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_id: Option<String>,
    /// This message opens a new transcript segment (turn card): the engine set
    /// it for injected steering, so projections do not have to guess.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_segment: bool,
    /// Set when the message is the engine's "keep going" instruction after a
    /// pause, so the transcript never pretends the user typed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
    /// Set when this message is a late answer to a timed-out `ask_user` call
    /// (steered in as a user message). The payload is also serialized into
    /// `text`, so an old reader still shows the answer; the structured field is
    /// what lets a projection pair it with the pending question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_reply: Option<crate::question::QuestionReply>,
    /// Set when this message is a scheduled-task delivery (ADR-0059), so the
    /// UI can render it as a scheduled-execution card and the title derivation
    /// can tell it apart from a typed message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<crate::schedule::ScheduledMessage>,
}

/// A continuation the user asked for after a paused turn (ADR-0060 §2.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Continuation {
    /// The paused turn this one picks up from.
    pub turn: u32,
    /// Budget window of *this* turn: 1 for the first run, 2 after one continue.
    #[serde(default = "first_window")]
    pub window: u32,
    /// Budgets the user raised for this window. `None` = same limits as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raise: Option<RaiseLimit>,
}

/// A budget the user raised when continuing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RaiseLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    /// True when the new value was also written to `config.toml`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist: bool,
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
    /// Pictures the call produced (ADR-0081): references into the attachment
    /// store, like a user message's attachments — the bytes never enter the
    /// log. Empty for every tool that only returns text, and for a log written
    /// before schema v11.
    ///
    /// Kept on the result rather than folded into `output` because the wire
    /// formats disagree about where an image may go: a tool *result* message
    /// carries text only, so the engine replays these as a user message right
    /// after it. See `history::messages_from_events_with`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<qcode_attachments::ImageRef>,
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
    /// Present exactly when `status` is `Paused`: which budget ran out, at what
    /// value, and the hand-off summary the model wrote before stopping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<PauseInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Cancelled,
    Failed,
    /// A budget ran out. Not an error: the work is intact and the user decides
    /// whether to continue (ADR-0060).
    Paused,
}

/// Why a turn paused and what it cost to get there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PauseInfo {
    pub reason: PauseReason,
    pub limit: PauseLimit,
    pub used: PauseUsage,
    /// 1 for the first budget window of a task, 2 after one continuation, …
    #[serde(default = "first_window")]
    pub window: u32,
    /// The model's "done / remaining / next" hand-off. `None` when the summary
    /// call was skipped (no budget left) or failed; the locally derived
    /// progress report still describes the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

fn first_window() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    MaxTurns,
    MaxCost,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PauseLimit {
    Turns(u32),
    Usd(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PauseUsage {
    pub turns: u32,
    pub cost_usd: f64,
}

/// Emitted once, when the turn first crosses 75% of a budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetHint {
    pub turns: u32,
    pub max_turns: u32,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    #[serde(default = "first_window")]
    pub window: u32,
}

/// One MCP server's tool table changed (ADR-0067 §5).
///
/// The names here are the *bare* MCP names the server used, not the
/// `mcp__<server>__<tool>` names the model sees: the event already carries the
/// server, and a reader of the log can compose the public name from the two.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolsRefreshed {
    pub server: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellEnvSampled {
    /// The shell the sample came from, e.g. `/bin/zsh`.
    pub program: String,
    /// How many variables the snapshot holds.
    pub count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    pub duration_ms: u64,
    /// True when this turn reused a snapshot instead of running the shell.
    #[serde(default)]
    pub cached: bool,
    /// Why the last attempt failed, when it did (the snapshot is then stale).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ShellEnvSampled {
    /// Nothing worth a log line: sampling was off, or it produced no snapshot
    /// and no failure (the first attempt in a brand-new session).
    pub fn is_quiet(&self) -> bool {
        self.count == 0 && self.error.is_none()
    }
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

                continuation: None,
                question_reply: None,
                scheduled: None,
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

                continuation: None,
                question_reply: None,
                scheduled: None,
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

                continuation: None,
                question_reply: None,
                scheduled: None,
            }),
        );
        let value: Value = serde_json::from_str(&serde_json::to_string(&input).unwrap()).unwrap();
        assert!(value["data"].get("queued_id").is_none());
        assert!(value["data"].get("starts_segment").is_none());
    }
}
