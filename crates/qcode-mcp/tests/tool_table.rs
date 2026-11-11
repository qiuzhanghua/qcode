//! ADR-0067 §5: the tool table has a lifetime.
//!
//! These are the three assertions the ADR names, plus the two consequences that
//! make the feature safe rather than merely present:
//!
//! * `ttlMs = 0` re-reads before every turn;
//! * a positive `ttlMs` does not re-read inside its window;
//! * `notifications/tools/list_changed` marks the table stale, and the change
//!   lands at the next turn boundary — never mid-turn;
//! * a tool that disappeared answers with a specific error instead of "unknown
//!   tool", which is what the model needs in order to stop retrying it.
//!
//! The stub (`tests/support/mcp_stub.rs`) drives all of it: with
//! `MCP_STUB_TTL_MS`/`MCP_STUB_MUTATE` it reports a generation-stamped tool
//! table, so a refresh is visible as `mcp__stub__echo_v1` becoming
//! `mcp__stub__echo_v2`.

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ToolRegistry, ToolSource};
use qcode_mcp::{ConnectOptions, McpConfig, McpServerConfig, connect_detailed};
use qcode_providers::MockProvider;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn config(env: &[(&str, &str)]) -> McpConfig {
    McpConfig {
        servers: vec![McpServerConfig {
            name: "stub".to_string(),
            command: Some(env!("CARGO_BIN_EXE_mcp-stub").to_string()),
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            timeout_secs: 10,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// One turn that answers with text and nothing else.
fn plain_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

/// A turn that calls `name` once, then finishes: one provider call per round.
fn call_turn(call_id: &str, name: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: json!({"message": "hi"}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        plain_turn("done"),
    ]
}

struct Harness {
    store: SessionStore,
    session: String,
    workspace: std::path::PathBuf,
    registry: ToolRegistry,
    _data: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

async fn harness(env: &[(&str, &str)]) -> Harness {
    let data = tempfile::tempdir().expect("data");
    let workspace = tempfile::tempdir().expect("workspace");
    let connected = connect_detailed(
        &config(env),
        workspace.path(),
        data.path(),
        &[],
        ConnectOptions::default(),
    )
    .await;

    let mut registry = ToolRegistry::new(Vec::new());
    registry.extend(connected.tools);
    if let Some(source) = connected.source {
        registry.add_source(source as std::sync::Arc<dyn ToolSource>);
    }

    let store = SessionStore::new(data.path().join("data"));
    let (session, _) = store
        .create(&SessionStarted {
            model: "mock".to_string(),
            provider: "mock".to_string(),
            cwd: workspace.path().display().to_string(),
            version: "0.1.0".to_string(),
        })
        .expect("session");

    Harness {
        store,
        session,
        workspace: workspace.path().to_path_buf(),
        registry,
        _data: data,
        _workspace: workspace,
    }
}

impl Harness {
    async fn turn(&self, script: Vec<Vec<StreamEvent>>) -> Vec<qcode_core::event::Event> {
        let provider = MockProvider::scripted(script);
        run_turn(
            TurnContext {
                store: &self.store,
                provider: &provider,
                tools: &self.registry,
                policy: &Policy::default(),
                approver: &AutoApprove,
                snapshots: None,
                // These tests exercise the MCP tool table / instructions, not
                // scheduling; the field exists for the runtime driver (ADR schedule).
                scheduler: None,
            },
            RunOptions {
                session_id: &self.session,
                model: "mock",
                input: "go".to_string(),
                cwd: self.workspace.clone(),
                max_turns: 8,
                cancel: CancellationToken::new(),
                pricing: None,
                max_cost_usd: None,
                on_delta: None,
                context_window: None,
                agents_md: None,
                skills: None,
                network: None,
                attachments: Vec::new(),
                inbox: None,
                continuation: None,
            },
            |_| {},
        )
        .await
        .expect("turn");
        self.store.load(&self.session).expect("events")
    }
}

/// The names the model was shown in the last request. Read back from the log's
/// tool calls rather than from the registry, because the interesting question
/// is what the *model* saw.
fn refresh_events(
    events: &[qcode_core::event::Event],
) -> Vec<&qcode_core::event::McpToolsRefreshed> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::McpToolsRefreshed(refreshed) => Some(refreshed),
            _ => None,
        })
        .collect()
}

fn tool_result(
    events: &[qcode_core::event::Event],
    call_id: &str,
) -> qcode_core::event::ToolResult {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == call_id => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

/// `ttlMs = 0` is a server saying "never treat this as fresh", so every turn
/// re-reads. The generation counter in the stub's tool names is what makes the
/// re-read visible.
#[tokio::test]
async fn ttl_zero_refetches_before_every_turn() {
    let harness = harness(&[("MCP_STUB_TTL_MS", "0")]).await;

    // The connect-time fetch is generation 1.
    let mut names = harness.registry.specs();
    names.retain(|spec| spec.name.starts_with("mcp__"));
    let names: Vec<String> = names.into_iter().map(|spec| spec.name).collect();
    assert!(
        names.iter().any(|name| name == "mcp__stub__echo_v1"),
        "the connect-time table should be generation 1: {names:?}"
    );

    // Turn 1 refreshes to generation 2.
    let events = harness.turn(vec![plain_turn("one")]).await;
    let refreshed = refresh_events(&events);
    assert_eq!(refreshed.len(), 1, "one refresh event for one turn");
    assert_eq!(refreshed[0].server, "stub");
    assert!(
        refreshed[0]
            .added
            .contains(&"mcp__stub__echo_v2".to_string()),
        "{:?}",
        refreshed[0]
    );
    assert!(
        refreshed[0]
            .removed
            .contains(&"mcp__stub__echo_v1".to_string()),
        "{:?}",
        refreshed[0]
    );

    // Turn 2 refreshes again: the TTL says the table it just read is already
    // stale by the time the next turn starts.
    let events = harness.turn(vec![plain_turn("two")]).await;
    let refreshed = refresh_events(&events);
    assert_eq!(refreshed.len(), 2, "each turn refreshes once");
    assert!(
        refreshed[1]
            .added
            .contains(&"mcp__stub__echo_v3".to_string()),
        "the second turn should have reached generation 3: {:?}",
        refreshed[1]
    );
}

/// A positive TTL means "trust this for n milliseconds". Inside that window no
/// round trip happens and no event is written — which is the entire reason
/// `ttlMs` exists.
#[tokio::test]
async fn a_positive_ttl_does_not_refetch_inside_its_window() {
    // 60s: far longer than the test takes.
    let harness = harness(&[("MCP_STUB_TTL_MS", "60000")]).await;

    let events = harness.turn(vec![plain_turn("one")]).await;
    assert!(
        refresh_events(&events).is_empty(),
        "a fresh table must not be re-read: {:?}",
        refresh_events(&events)
    );

    let events = harness.turn(vec![plain_turn("two")]).await;
    assert!(
        refresh_events(&events).is_empty(),
        "still inside the window: {:?}",
        refresh_events(&events)
    );

    // Still generation 1, i.e. nothing was fetched after connect.
    let events = harness
        .turn(call_turn("call-1", "mcp__stub__echo_v1"))
        .await;
    let result = tool_result(&events, "call-1");
    assert_eq!(
        result.status,
        ToolStatus::Ok,
        "generation 1 should still be live: {}",
        result.output
    );
}

/// The notification is the server's way of saying the TTL cannot be trusted.
/// It has to win over a long TTL, and the resulting reload has to happen at the
/// next turn boundary rather than under a running turn.
#[tokio::test]
async fn list_changed_wins_over_the_ttl_and_lands_at_the_turn_boundary() {
    // A TTL that would otherwise never expire, so the reload can only come from
    // the notification. `MCP_STUB_MUTATE` moves the table and sends
    // `notifications/tools/list_changed` right after the connect-time listing.
    let harness = harness(&[("MCP_STUB_TTL_MS", "600000"), ("MCP_STUB_MUTATE", "1")]).await;

    // The turn has two rounds (a tool call, then the final text). If the refresh
    // were per round instead of per turn there would be two events here.
    let events = harness
        .turn(call_turn("call-1", "mcp__stub__echo_v1"))
        .await;

    let refreshed = refresh_events(&events);
    assert_eq!(
        refreshed.len(),
        1,
        "a two-round turn must refresh once, at the boundary: {refreshed:?}"
    );
    assert_eq!(refreshed[0].server, "stub");
    assert_eq!(
        refreshed[0].added,
        vec!["mcp__stub__echo_v2".to_string()],
        "the notification's table should have been picked up"
    );
    assert_eq!(
        refreshed[0].removed,
        vec!["mcp__stub__echo_v1".to_string()],
        "the old generation must be reported as gone"
    );

    // The model called the tool it was shown at the start of the turn. By the
    // time the call ran, that tool had been replaced — and saying so is the
    // difference between a usable error and "unknown tool".
    let result = tool_result(&events, "call-1");
    assert_eq!(result.status, ToolStatus::Error);
    assert!(
        result.output.contains("no longer available"),
        "expected the retired-tool message: {}",
        result.output
    );
    assert!(
        result.output.contains("mcp__stub__echo_v1"),
        "the message must name the tool: {}",
        result.output
    );
}

/// The other side of the same coin: a name that was never a tool still says
/// "unknown tool", so the two cases do not blur into one message.
#[tokio::test]
async fn an_invented_tool_is_still_unknown() {
    let harness = harness(&[]).await;
    let events = harness.turn(call_turn("call-1", "mcp__stub__nope")).await;
    let result = tool_result(&events, "call-1");
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result.output.contains("unknown tool"), "{}", result.output);
    assert!(
        !result.output.contains("no longer available"),
        "{}",
        result.output
    );
}

/// A server that says nothing about freshness must behave exactly as it did
/// before ADR-0067: one listing, no events, no extra round trips.
#[tokio::test]
async fn a_server_without_ttl_ms_is_listed_once() {
    let harness = harness(&[]).await;
    for text in ["one", "two"] {
        let events = harness.turn(vec![plain_turn(text)]).await;
        assert!(
            refresh_events(&events).is_empty(),
            "a legacy server must not be re-read: {:?}",
            refresh_events(&events)
        );
    }
}

/// A registry with no sources must not pay for the refresh machinery: this is
/// the path every non-MCP session takes.
#[tokio::test]
async fn a_registry_without_sources_refreshes_to_itself() {
    let registry = ToolRegistry::new(Vec::new());
    assert!(!registry.has_sources());
    let (snapshot, changed) = registry.refreshed().await;
    assert!(changed.is_empty());
    assert_eq!(snapshot.len(), registry.len());
}
