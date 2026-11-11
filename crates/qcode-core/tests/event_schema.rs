//! Golden JSON snapshots for every persisted event kind.
//!
//! The JSONL log is the single source of truth for replay and resume, so a
//! silent rename or a dropped field is a data-loss bug. Each snapshot pins the
//! wire shape; changing one is a deliberate schema change (bump
//! `EVENT_SCHEMA_VERSION` and add a migration).

use qcode_core::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, BudgetHint, Compaction, ContentBlock,
    Continuation, Decision, ErrorEvent, EventKind, PauseInfo, PauseLimit, PauseReason, PauseUsage,
    RootsChanged, Scope, SessionStarted, TaskConfigured, TaskRenamed, ToolCall, ToolMetadata,
    ToolResult, ToolStatus, TurnEnded, TurnStarted, TurnStatus, Usage, UserMessage,
};
use qcode_core::task::{Root, RootAccess};
use serde_json::{Value, json};

fn assert_snapshot(kind: EventKind, expected: Value) {
    let actual = serde_json::to_value(&kind).expect("event kind serializes");
    assert_eq!(actual, expected);

    let back: EventKind = serde_json::from_value(actual.clone()).expect("event kind round-trips");
    assert_eq!(back, kind);
}

#[test]
fn session_started_snapshot() {
    assert_snapshot(
        EventKind::SessionStarted(SessionStarted {
            model: "deepseek-flash".into(),
            provider: "deepseek".into(),
            cwd: "/work/repo".into(),
            version: "0.1.0".into(),
        }),
        json!({
            "type": "session_started",
            "data": {
                "model": "deepseek-flash",
                "provider": "deepseek",
                "cwd": "/work/repo",
                "version": "0.1.0"
            }
        }),
    );
}

fn root() -> Root {
    Root {
        id: "root-1".into(),
        label: "workspace".into(),
        path: "/work/repo".into(),
        access: RootAccess::ReadWrite,
        primary: true,
    }
}

#[test]
fn task_configured_snapshot() {
    assert_snapshot(
        EventKind::TaskConfigured(TaskConfigured {
            roots: vec![root()],
        }),
        json!({
            "type": "task_configured",
            "data": {
                "roots": [{
                    "id": "root-1",
                    "label": "workspace",
                    "path": "/work/repo",
                    "access": "read-write",
                    "primary": true
                }]
            }
        }),
    );
}

#[test]
fn task_renamed_snapshot() {
    assert_snapshot(
        EventKind::TaskRenamed(TaskRenamed {
            title: "重构 sandbox".into(),
        }),
        json!({
            "type": "task_renamed",
            "data": {"title": "重构 sandbox"}
        }),
    );
}

#[test]
fn roots_changed_snapshot() {
    let mut docs = root();
    docs.id = "root-2".into();
    docs.label = "docs".into();
    docs.path = "/work/docs".into();
    docs.access = RootAccess::ReadOnly;
    docs.primary = false;
    assert_snapshot(
        EventKind::RootsChanged(RootsChanged {
            roots: vec![root(), docs],
        }),
        json!({
            "type": "roots_changed",
            "data": {
                "roots": [
                    {
                        "id": "root-1",
                        "label": "workspace",
                        "path": "/work/repo",
                        "access": "read-write",
                        "primary": true
                    },
                    {
                        "id": "root-2",
                        "label": "docs",
                        "path": "/work/docs",
                        "access": "read-only",
                        "primary": false
                    }
                ]
            }
        }),
    );
}

#[test]
fn turn_started_snapshot() {
    assert_snapshot(
        EventKind::TurnStarted(TurnStarted {
            input: "fix the tests".into(),
        }),
        json!({
            "type": "turn_started",
            "data": {"input": "fix the tests"}
        }),
    );
}

#[test]
fn user_message_snapshot_omits_empty_attachments() {
    assert_snapshot(
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "hi".into(),
            attachments: Vec::new(),

            continuation: None,
            question_reply: None,
            scheduled: None,
        }),
        json!({
            "type": "user_message",
            "data": {"text": "hi"}
        }),
    );

    // v7: attachments are references into the store, never paths or bytes.
    let id = qcode_attachments::AttachmentId::from_bytes([0x2a; 32]);
    assert_snapshot(
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "see attached".into(),
            attachments: vec![qcode_attachments::AttachmentRef::File(
                qcode_attachments::FileRef {
                    id,
                    name: "src/lib.rs".into(),
                    bytes: 42,
                },
            )],

            continuation: None,
            question_reply: None,
            scheduled: None,
        }),
        json!({
            "type": "user_message",
            "data": {
                "text": "see attached",
                "attachments": [{
                    "kind": "file",
                    "id": "2a".repeat(32),
                    "name": "src/lib.rs",
                    "bytes": 42
                }]
            }
        }),
    );
}

#[test]
fn assistant_message_snapshot_covers_all_blocks() {
    assert_snapshot(
        EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "think".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "done".into(),
                },
                ContentBlock::ToolCall {
                    call_id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "src/lib.rs"}),
                },
            ],
            provider_raw: json!({"provider": "mock"}),
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                ..Usage::default()
            },
        }),
        json!({
            "type": "assistant_message",
            "data": {
                "blocks": [
                    {"type": "reasoning", "text": "think"},
                    {"type": "text", "text": "done"},
                    {
                        "type": "tool_call",
                        "call_id": "call-1",
                        "name": "read_file",
                        "arguments": {"path": "src/lib.rs"}
                    }
                ],
                "provider_raw": {"provider": "mock"},
                "usage": {"input_tokens": 10, "output_tokens": 4}
            }
        }),
    );
}

#[test]
fn approval_events_snapshot() {
    assert_snapshot(
        EventKind::ApprovalRequested(ApprovalRequested {
            call_id: "call-1".into(),
            rule_id: None,
            reason: "shell commands need approval".into(),
        }),
        json!({
            "type": "approval_requested",
            "data": {
                "call_id": "call-1",
                "reason": "shell commands need approval"
            }
        }),
    );

    assert_snapshot(
        EventKind::ApprovalDecided(ApprovalDecided {
            call_id: "call-1".into(),
            decision: Decision::Allow,
            rule_id: Some("allow-cargo-test".into()),
            scope: Scope::User,
        }),
        json!({
            "type": "approval_decided",
            "data": {
                "call_id": "call-1",
                "decision": "allow",
                "rule_id": "allow-cargo-test",
                "scope": "user"
            }
        }),
    );
}

#[test]
fn tool_call_snapshot() {
    assert_snapshot(
        EventKind::ToolCall(ToolCall {
            call_id: "call-1".into(),
            name: "run_shell".into(),
            arguments: json!({"command": "cargo test"}),
        }),
        json!({
            "type": "tool_call",
            "data": {
                "call_id": "call-1",
                "name": "run_shell",
                "arguments": {"command": "cargo test"}
            }
        }),
    );
}

#[test]
fn tool_result_snapshot_keeps_default_metadata() {
    assert_snapshot(
        EventKind::ToolResult(ToolResult {
            call_id: "call-1".into(),
            status: ToolStatus::Cancelled,
            output: "cancelled by user".into(),
            metadata: ToolMetadata {
                bytes: 17,
                exit_code: Some(130),
                ..ToolMetadata::default()
            },
            diff: None,
        }),
        json!({
            "type": "tool_result",
            "data": {
                "call_id": "call-1",
                "status": "cancelled",
                "output": "cancelled by user",
                "metadata": {
                    "truncated": false,
                    "bytes": 17,
                    "exit_code": 130
                }
            }
        }),
    );
}

#[test]
fn tool_result_snapshot_carries_the_diff() {
    assert_snapshot(
        EventKind::ToolResult(ToolResult {
            call_id: "call-2".into(),
            status: ToolStatus::Ok,
            output: "replaced 1 occurrence(s) in @workspace/notes.md".into(),
            metadata: ToolMetadata::default(),
            diff: Some(qcode_core::event::ToolDiff {
                patch: "--- a/notes.md\n+++ b/notes.md\n@@ -1 +1 @@\n-one\n+1\n".into(),
                files: vec![qcode_core::event::DiffFileStat {
                    path: "notes.md".into(),
                    kind: qcode_core::event::DiffKind::Modified,
                    additions: 1,
                    deletions: 1,
                    bytes: 2,
                }],
                truncated: false,
            }),
        }),
        json!({
            "type": "tool_result",
            "data": {
                "call_id": "call-2",
                "status": "ok",
                "output": "replaced 1 occurrence(s) in @workspace/notes.md",
                "metadata": {
                    "truncated": false,
                    "bytes": 0
                },
                "diff": {
                    "patch": "--- a/notes.md\n+++ b/notes.md\n@@ -1 +1 @@\n-one\n+1\n",
                    "files": [
                        {
                            "path": "notes.md",
                            "kind": "modified",
                            "additions": 1,
                            "deletions": 1,
                            "bytes": 2
                        }
                    ],
                    "truncated": false
                }
            }
        }),
    );
}

#[test]
fn compaction_snapshot() {
    assert_snapshot(
        EventKind::Compaction(Compaction {
            trigger: "context_78_percent".into(),
            summary: "earlier work, condensed".into(),
            replaced_from: 3,
            replaced_to: 17,
            tokens_before: 90_000,
            tokens_after: 12_000,
        }),
        json!({
            "type": "compaction",
            "data": {
                "trigger": "context_78_percent",
                "summary": "earlier work, condensed",
                "replaced_from": 3,
                "replaced_to": 17,
                "tokens_before": 90000,
                "tokens_after": 12000
            }
        }),
    );
}

#[test]
fn turn_ended_snapshot_covers_every_status() {
    // `paused` needs a `pause` payload, so it is asserted on its own below.
    for status in [
        TurnStatus::Completed,
        TurnStatus::Cancelled,
        TurnStatus::Failed,
    ] {
        let expected = match status {
            TurnStatus::Completed => "completed",
            TurnStatus::Cancelled => "cancelled",
            TurnStatus::Failed => "failed",
            TurnStatus::Paused => unreachable!("covered separately"),
        };
        assert_snapshot(
            EventKind::TurnEnded(TurnEnded {
                status,
                usage: Usage {
                    input_tokens: Some(7),
                    ..Usage::default()
                },
                cost_usd: 0.0,
                pause: None,
            }),
            json!({
                "type": "turn_ended",
                "data": {
                    "status": expected,
                    "usage": {"input_tokens": 7},
                    "cost_usd": 0.0
                }
            }),
        );
    }
}

/// v6: running out of budget is a pause with a hand-off, not a failure.
#[test]
fn a_paused_turn_carries_its_budget_and_hand_off() {
    assert_snapshot(
        EventKind::TurnEnded(TurnEnded {
            status: TurnStatus::Paused,
            usage: Usage {
                input_tokens: Some(7),
                ..Usage::default()
            },
            cost_usd: 0.12,
            pause: Some(PauseInfo {
                reason: PauseReason::MaxTurns,
                limit: PauseLimit::Turns(64),
                used: PauseUsage {
                    turns: 64,
                    cost_usd: 0.12,
                },
                window: 2,
                summary: Some("done / left".into()),
            }),
        }),
        json!({
            "type": "turn_ended",
            "data": {
                "status": "paused",
                "usage": {"input_tokens": 7},
                "cost_usd": 0.12,
                "pause": {
                    "reason": "max_turns",
                    "limit": {"kind": "turns", "value": 64},
                    "used": {"turns": 64, "cost_usd": 0.12},
                    "window": 2,
                    "summary": "done / left"
                }
            }
        }),
    );
}

/// A v5 log has no `pause` field and no `budget_hint`; it must still load.
#[test]
fn a_v5_turn_ended_still_loads() {
    let json = json!({
        "type": "turn_ended",
        "data": {
            "status": "failed",
            "usage": {"input_tokens": 7},
            "cost_usd": 0.0
        }
    });
    let kind: EventKind = serde_json::from_value(json).expect("v5 turn_ended loads");
    match kind {
        EventKind::TurnEnded(ended) => {
            assert_eq!(ended.status, TurnStatus::Failed);
            assert!(ended.pause.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// The 75% notice is its own event so a replay shows it too.
#[test]
fn budget_hint_snapshot() {
    assert_snapshot(
        EventKind::BudgetHint(BudgetHint {
            turns: 48,
            max_turns: 64,
            cost_usd: 0.0,
            max_cost_usd: None,
            window: 1,
        }),
        json!({
            "type": "budget_hint",
            "data": {
                "turns": 48,
                "max_turns": 64,
                "cost_usd": 0.0,
                "window": 1
            }
        }),
    );
}

/// A continuation is recorded on the user message so replay can label it.
#[test]
fn a_continuation_is_marked_on_the_user_message() {
    assert_snapshot(
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "[continue]".into(),
            attachments: Vec::new(),
            continuation: Some(Continuation {
                turn: 3,
                window: 2,
                raise: None,
            }),
            question_reply: None,
            scheduled: None,
        }),
        json!({
            "type": "user_message",
            "data": {
                "text": "[continue]",
                "continuation": {"turn": 3, "window": 2}
            }
        }),
    );
}

#[test]
fn error_snapshot() {
    assert_snapshot(
        EventKind::Error(ErrorEvent {
            kind: "max_turns".into(),
            message: "exceeded max turns (8)".into(),
            retryable: false,
        }),
        json!({
            "type": "error",
            "data": {
                "kind": "max_turns",
                "message": "exceeded max turns (8)",
                "retryable": false
            }
        }),
    );
}
