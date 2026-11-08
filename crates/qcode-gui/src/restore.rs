//! Which session a launch reopens.
//!
//! Restoring the wrong thing is invisible until the first message: the window
//! looks normal, then the prompt lands in a brand new task. So the choice
//! lives here, away from the GPUI shell, and is covered by tests.

use crate::TaskRow;

/// The task to reopen on launch: the one `gui.json` remembered, or — when that
/// id is gone (deleted task, an older `gui.json` without the field, a stale
/// copy written by another window) — the most recent task.
///
/// `None` means there is genuinely nothing to reopen (fresh install); only then
/// does the window start on an empty draft.
pub fn session_to_restore(last: Option<&str>, tasks: &[TaskRow]) -> Option<String> {
    if let Some(id) = last
        && tasks.iter().any(|row| row.task.id == id)
    {
        return Some(id.to_string());
    }
    // `SessionStore::list` returns ULIDs in descending order, so the first row
    // is the newest session.
    tasks.first().map(|row| row.task.id.clone())
}
