use std::path::Path;

use qcode_core::event::SessionStarted;
use qcode_core::store::SessionSummary;
use qcode_gui::{short_id, workspace_sessions};

fn summary(id: &str, cwd: &str, started_at: &str) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        started: SessionStarted {
            model: "test".to_string(),
            provider: "mock".to_string(),
            cwd: cwd.to_string(),
            version: "0.1.0".to_string(),
        },
        started_at: started_at.to_string(),
        turns: 1,
        last_seq: 2,
        cost_usd: 0.0,
    }
}

#[test]
fn sidebar_keeps_only_the_current_workspace() {
    let sessions = vec![
        summary("a", "/work/a", "2026-10-21T10:00:00Z"),
        summary("b", "/work/b", "2026-10-21T11:00:00Z"),
        summary("c", "/work/a", "2026-10-21T12:00:00Z"),
    ];
    let filtered = workspace_sessions(sessions, Path::new("/work/a"));
    let ids: Vec<&str> = filtered.iter().map(|session| session.id.as_str()).collect();
    assert_eq!(ids, ["c", "a"]);
}

#[test]
fn short_id_is_at_most_eight_chars() {
    assert_eq!(short_id("01JABCDEFGHIJKLMNOPQRSTUV"), "01JABCDE");
    assert_eq!(short_id("short"), "short");
}
