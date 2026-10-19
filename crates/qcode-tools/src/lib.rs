//! Built-in tools. Every path is resolved against the workspace and every
//! write is atomic (temp file + rename).

pub mod fetch;
pub mod fs;
pub mod patch;
pub mod path;
pub mod sandbox;
pub mod search;
pub mod shell;
pub mod skills;
pub mod todo;

use std::sync::Arc;

use qcode_core::tool::ToolRegistry;

use crate::sandbox::{NoSandbox, Sandbox};

/// Built-ins with no OS sandbox (the v1 default).
pub fn builtin_registry() -> ToolRegistry {
    builtin_registry_with(Arc::new(NoSandbox))
}

/// Built-ins with the configured sandbox applied to `run_shell`.
pub fn builtin_registry_with(sandbox: Arc<dyn Sandbox>) -> ToolRegistry {
    ToolRegistry::new(vec![
        Arc::new(fs::ReadFile),
        Arc::new(fs::WriteFile),
        Arc::new(fs::EditFile),
        Arc::new(patch::ApplyPatch),
        Arc::new(fs::ListDir),
        Arc::new(search::GlobTool),
        Arc::new(search::GrepTool),
        Arc::new(fetch::WebFetch),
        Arc::new(shell::RunShell::new(sandbox)),
        Arc::new(todo::TodoWrite),
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
    }
}
