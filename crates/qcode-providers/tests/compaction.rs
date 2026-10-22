//! Compaction end-to-end: a session with three archived turns gets its older
//! two summarized before the next provider call, and replay rebuilds the same
//! compacted context.

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{
    AssistantMessage, ContentBlock, Event, EventKind, SessionStarted, TurnEnded, TurnStarted,
    TurnStatus, Usage, UserMessage,
};
use qcode_core::history;
use qcode_core::policy::Policy;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn append(store: &SessionStore, session: &str, seq: &mut u64, turn: u32, kind: EventKind) {
    *seq += 1;
    store
        .append(&Event::new(*seq, session, turn, kind))
        .expect("append event");
}

/// Seed three completed turns, each with a large assistant message so the
/// estimate is well above a tiny test window.
fn seed_three_turns(store: &SessionStore) -> String {
    let (session, _) = store.create(&meta()).expect("create session");
    let mut seq = 1;
    for turn in 1..=3u32 {
        append(
            store,
            &session,
            &mut seq,
            turn,
            EventKind::TurnStarted(TurnStarted {
                input: format!("turn {turn}"),
            }),
        );
        append(
            store,
            &session,
            &mut seq,
            turn,
            EventKind::UserMessage(UserMessage {
                text: format!("turn {turn}"),
                attachments: Vec::new(),
            }),
        );
        append(
            store,
            &session,
            &mut seq,
            turn,
            EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![ContentBlock::Text {
                    text: format!("OLD-{turn} {}", "x".repeat(2_000)),
                }],
                provider_raw: json!({}),
                usage: Usage::default(),
            }),
        );
        append(
            store,
            &session,
            &mut seq,
            turn,
            EventKind::TurnEnded(TurnEnded {
                status: TurnStatus::Completed,
                usage: Usage::default(),
                cost_usd: 0.0,
            }),
        );
    }
    session
}

#[tokio::test]
async fn old_turns_are_summarized_and_replay_uses_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let session = seed_three_turns(&store);

    let provider = MockProvider::scripted(vec![
        vec![
            qcode_core::provider::StreamEvent::Started,
            qcode_core::provider::StreamEvent::TextDelta("SUMMARY".to_string()),
            qcode_core::provider::StreamEvent::Ended {
                stop_reason: qcode_core::provider::StopReason::EndTurn,
            },
        ],
        vec![
            qcode_core::provider::StreamEvent::Started,
            qcode_core::provider::StreamEvent::TextDelta("done".to_string()),
            qcode_core::provider::StreamEvent::Ended {
                stop_reason: qcode_core::provider::StopReason::EndTurn,
            },
        ],
    ]);
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &session,
            model: "mock-1",
            input: "fourth".to_string(),
            cwd: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: Some(1_000),
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.text, "done");

    let events = store.load(&session).unwrap();
    let compaction = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Compaction(compaction) => Some(compaction),
            _ => None,
        })
        .expect("compaction event");
    assert_eq!(compaction.trigger, "context_78_percent");
    assert_eq!(compaction.summary, "SUMMARY");
    assert_eq!(compaction.replaced_from, 3); // first user message of turn 1
    assert_eq!(compaction.replaced_to, 9); // before turn 3 starts at seq 10
    assert!(
        compaction.tokens_after < compaction.tokens_before,
        "compaction did not shrink the context estimate"
    );

    // Replay sees the summary and the two recent turns, never the old text.
    let messages = history::messages_from_events(&events);
    let flattened: String = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text, .. } => text.clone(),
            ContentBlock::ToolCall { .. } => String::new(),
            ContentBlock::Image { .. } => String::new(),
        })
        .collect();
    assert!(flattened.contains("Conversation summary (compacted):"));
    assert!(flattened.contains("SUMMARY"));
    assert!(!flattened.contains("OLD-1"), "old turn survived compaction");
    assert!(flattened.contains("OLD-3"), "recent turn was dropped");

    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(EventKind::TurnEnded(ended)) if ended.status == TurnStatus::Completed
    ));
}

#[tokio::test]
async fn a_roomy_window_skips_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("data"));
    let session = seed_three_turns(&store);

    let provider = MockProvider::text("plain");

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &ToolRegistry::default(),
            policy: &Policy::default(),
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &session,
            model: "mock-1",
            input: "fourth".to_string(),
            cwd: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: Some(10_000_000),
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.text, "plain");
    assert!(
        !store
            .load(&session)
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, EventKind::Compaction(_))),
        "compaction ran despite a roomy context window"
    );
}
