//! Golden JSON snapshots for every persisted event kind.
//!
//! The JSONL log is the single source of truth for replay and resume, so a
//! silent rename or a dropped field is a data-loss bug. Each snapshot pins the
//! wire shape; changing one is a deliberate schema change (bump
//! `EVENT_SCHEMA_VERSION` and add a migration).

use qcode_core::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, Compaction, ContentBlock, Decision,
    ErrorEvent, EventKind, RootsChanged, Scope, SessionStarted, TaskConfigured, TaskRenamed,
    ToolCall, ToolMetadata, ToolResult, ToolStatus, TurnEnded, TurnStarted, TurnStatus, Usage,
    UserMessage,
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
            text: "hi".into(),
            attachments: Vec::new(),
        }),
        json!({
            "type": "user_message",
            "data": {"text": "hi"}
        }),
    );

    assert_snapshot(
        EventKind::UserMessage(UserMessage {
            text: "see attached".into(),
            attachments: vec!["src/lib.rs".into()],
        }),
        json!({
            "type": "user_message",
            "data": {"text": "see attached", "attachments": ["src/lib.rs"]}
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
    for status in [
        TurnStatus::Completed,
        TurnStatus::Cancelled,
        TurnStatus::Failed,
    ] {
        let expected = match status {
            TurnStatus::Completed => "completed",
            TurnStatus::Cancelled => "cancelled",
            TurnStatus::Failed => "failed",
        };
        assert_snapshot(
            EventKind::TurnEnded(TurnEnded {
                status,
                usage: Usage {
                    input_tokens: Some(7),
                    ..Usage::default()
                },
                cost_usd: 0.0,
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
