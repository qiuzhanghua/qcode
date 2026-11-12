//! Built-in tools. Every path is resolved against the workspace and every
//! write is atomic (temp file + rename).

pub mod ask;
pub mod child_groups;
pub mod diff;
pub mod fetch;
pub mod fs;
pub mod patch;
pub mod path;
pub mod sandbox;
pub mod schedule;
pub mod search;
pub mod shell;
pub mod shell_env;
pub mod skills;
pub mod subagent;
pub mod todo;
pub mod web_search;
#[cfg(windows)]
pub mod windows_acl;

use std::sync::Arc;

use qcode_core::tool::ToolRegistry;

use crate::sandbox::{NoSandbox, Sandbox};

/// Built-ins with no OS sandbox (the v1 default).
pub fn builtin_registry() -> ToolRegistry {
    builtin_registry_with(Arc::new(NoSandbox))
}

/// Built-ins with the configured sandbox applied to `run_shell`.
pub fn builtin_registry_with(sandbox: Arc<dyn Sandbox>) -> ToolRegistry {
    builtin_registry_with_env(sandbox, Arc::new(shell_env::ShellEnvCache::disabled()))
}

/// Built-ins with both the sandbox and the sampled shell environment (ADR-0064).
pub fn builtin_registry_with_env(
    sandbox: Arc<dyn Sandbox>,
    env: Arc<shell_env::ShellEnvCache>,
) -> ToolRegistry {
    ToolRegistry::new(vec![
        Arc::new(fs::ReadFile),
        Arc::new(fs::WriteFile),
        Arc::new(fs::EditFile),
        Arc::new(patch::ApplyPatch),
        Arc::new(fs::ListDir),
        Arc::new(search::GlobTool),
        Arc::new(search::GrepTool),
        Arc::new(fetch::WebFetch),
        Arc::new(shell::RunShell::with_env(sandbox, env)),
        Arc::new(todo::TodoWrite),
        Arc::new(ask::AskUser),
        Arc::new(schedule::ScheduleCreate),
        Arc::new(schedule::ScheduleList),
        Arc::new(schedule::ScheduleView),
        Arc::new(schedule::ScheduleEdit),
    ])
}

/// Head+tail truncation on UTF-8 boundaries. The middle is dropped and the
/// omitted byte count is reported so the model knows something is missing.
pub(crate) fn truncate(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }

    let head = limit * 3 / 4;
    let tail = limit - head;
    let head_end = floor_char_boundary(text, head);
    let tail_start = ceil_char_boundary(text, text.len() - tail);
    let omitted = text.len() - head_end - (text.len() - tail_start);

    (
        format!(
            "{}\n… [truncated {omitted} bytes] …\n{}",
            &text[..head_end],
            &text[tail_start..]
        ),
        true,
    )
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_the_m3_tools() {
        let registry = builtin_registry();
        assert!(registry.get("apply_patch").is_some());
        assert!(registry.get("web_fetch").is_some());
        assert!(registry.get("todo_write").is_some());
        assert!(registry.get("schedule_create").is_some());
        assert!(registry.get("schedule_list").is_some());
        assert!(registry.get("schedule_view").is_some());
        assert!(registry.get("schedule_edit").is_some());
        assert!(
            registry
                .without(&crate::schedule::SCHEDULE_TOOL_NAMES)
                .get("schedule_create")
                .is_none()
        );
        assert!(registry.get("todo_write").is_some());
    }
}
