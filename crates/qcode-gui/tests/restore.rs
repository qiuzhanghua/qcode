use qcode_core::event::SessionStarted;
use qcode_core::store::SessionSummary;
use qcode_gui::TaskRow;
use qcode_gui::restore::session_to_restore;

fn row(id: &str) -> TaskRow {
    let summary = SessionSummary {
        id: id.to_string(),
        started: SessionStarted {
            model: "mock".to_string(),
            provider: "mock".to_string(),
            cwd: "/w".to_string(),
            version: "0".to_string(),
        },
        started_at: "2026-01-01T00:00:00Z".to_string(),
        turns: 1,
        last_seq: 1,
        cost_usd: 0.0,
    };
    let task = summary.legacy_task().expect("task");
    TaskRow {
        task,
        summary,
        has_scheduled: false,
    }
}

#[test]
fn the_remembered_task_is_reopened() {
    let tasks = vec![row("02"), row("01")];
    assert_eq!(
        session_to_restore(Some("01"), &tasks).as_deref(),
        Some("01")
    );
}

#[test]
fn a_missing_or_stale_id_falls_back_to_the_newest_task() {
    // No `last_session` (older gui.json) or it points at a deleted task: the
    // launch must still land on a real task, otherwise the first message
    // creates yet another one.
    let tasks = vec![row("02"), row("01")];
    assert_eq!(session_to_restore(None, &tasks).as_deref(), Some("02"));
    assert_eq!(
        session_to_restore(Some("gone"), &tasks).as_deref(),
        Some("02")
    );
}

#[test]
fn a_fresh_install_stays_on_an_empty_draft() {
    assert_eq!(session_to_restore(None, &[]), None);
    assert_eq!(session_to_restore(Some("gone"), &[]), None);
}
