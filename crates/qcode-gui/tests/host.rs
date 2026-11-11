//! `SessionHost` routing rules (ADR-0058 §1): live sessions are the authority
//! for cancel and queue state, and one session must never disturb another.
//!
//! The engine here is never run — only the slot bookkeeping is exercised.

use std::sync::Arc;

use qcode_gui::Engine;
use qcode_gui::host::{Phase, SessionHost};
use qcode_ui_bridge::SessionTarget;

fn engine(dir: &std::path::Path) -> Engine {
    Engine {
        store: qcode_core::store::SessionStore::new(dir),
        provider: Arc::new(qcode_providers::MockProvider::text("hello")),
        provider_name: "mock".to_string(),
        proxy_url: None,
        tools: qcode_core::tool::ToolRegistry::new(Vec::new()),
        policy: qcode_core::policy::Policy::new(Vec::new()),
        workspace: dir.to_path_buf(),
        config_path: dir.join("config.toml"),
        model: "mock-1".to_string(),
        pricing: None,
        pricing_table: std::collections::BTreeMap::new(),
        max_turns: 4,
        max_cost_usd: None,
        context_window: Some(128_000),
        network: None,
        snapshots: None,
        shell_env: std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::disabled()),
        schedules: Vec::new(),
        schedule_errors: Vec::new(),
        schedule_tick: None,
    }
}

fn host() -> (tempfile::TempDir, SessionHost) {
    let data = tempfile::tempdir().expect("data dir");
    let host = SessionHost::new(engine(data.path()));
    (data, host)
}

fn session(id: &str) -> SessionTarget {
    SessionTarget::Session(id.to_string())
}

#[test]
fn two_sessions_get_independent_cancel_tokens() {
    let (_data, host) = host();
    let (a, _) = host.open(&session("a"));
    let (b, _) = host.open(&session("b"));

    assert!(host.cancel(&session("a")));
    assert!(a.is_cancelled());
    assert!(!b.is_cancelled(), "cancelling a must not stop b");
    assert_eq!(host.phase(&session("b")), Phase::Idle);
}

#[test]
fn reopening_a_stopped_session_hands_out_a_fresh_token() {
    let (_data, host) = host();
    let (first, _) = host.open(&session("a"));
    host.cancel(&session("a"));
    assert!(first.is_cancelled());

    // The next turn must not inherit the stop that ended the previous one.
    let (second, _) = host.open(&session("a"));
    assert!(!second.is_cancelled());
}

#[test]
fn a_live_session_keeps_its_follow_up_queue_across_turns() {
    let (_data, host) = host();
    let (_, first) = host.open(&session("a"));
    first.submit("queued".to_string(), Vec::new(), true);

    let (_, second) = host.open(&session("a"));
    assert_eq!(
        second.queued().len(),
        1,
        "a follow-up message must survive the turn boundary"
    );
}

#[test]
fn queues_are_per_session() {
    let (_data, host) = host();
    let (_, a) = host.open(&session("a"));
    let (_, b) = host.open(&session("b"));
    a.submit("for a".to_string(), Vec::new(), true);

    assert_eq!(b.queued().len(), 0, "b must not see a's queue");
}

#[test]
fn a_turn_in_flight_reports_running_and_pending_approvals_win() {
    let (_data, host) = host();
    host.open(&session("a"));
    assert_eq!(host.phase(&session("a")), Phase::Idle);

    host.begin_turn(&session("a"));
    assert_eq!(host.phase(&session("a")), Phase::Running);

    host.set_waiting_approval(&session("a"), true);
    assert_eq!(host.phase(&session("a")), Phase::WaitingApproval);

    host.end_turn(&session("a"));
    assert_eq!(host.phase(&session("a")), Phase::Idle);
}

#[test]
fn an_unknown_session_is_idle_and_not_a_cancel_error() {
    let (_data, host) = host();
    assert!(!host.cancel(&session("nope")));
    assert_eq!(host.phase(&session("nope")), Phase::Idle);
}

#[test]
fn cancel_all_stops_every_session() {
    let (_data, host) = host();
    let (a, _) = host.open(&session("a"));
    let (b, _) = host.open(&session("b"));

    assert_eq!(host.cancel_all(), 2);
    assert!(a.is_cancelled());
    assert!(b.is_cancelled());
}

#[test]
fn a_running_session_is_not_closed() {
    let (_data, host) = host();
    host.open(&session("a"));
    host.begin_turn(&session("a"));

    assert!(!host.close_if_idle(&session("a")));
    assert_eq!(host.sessions(), vec![session("a")]);
}

#[test]
fn a_session_with_a_queued_message_is_not_closed() {
    let (_data, host) = host();
    let (_, queue) = host.open(&session("a"));
    queue.submit("next".to_string(), Vec::new(), true);

    assert!(!host.close_if_idle(&session("a")));
}

#[test]
fn a_session_waiting_for_an_approval_is_not_closed() {
    let (_data, host) = host();
    host.open(&session("a"));
    host.begin_turn(&session("a"));
    host.set_waiting_approval(&session("a"), true);

    assert!(!host.close_if_idle(&session("a")));
}

#[test]
fn an_idle_session_can_be_closed() {
    let (_data, host) = host();
    host.open(&session("a"));
    assert!(host.close_if_idle(&session("a")));
    assert!(host.sessions().is_empty());
}

#[test]
fn a_draft_slot_is_distinct_from_a_session_slot() {
    let (_data, host) = host();
    host.open(&SessionTarget::Draft);
    host.open(&session("a"));

    assert_eq!(host.sessions(), vec![SessionTarget::Draft, session("a")]);
}
