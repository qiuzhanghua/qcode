//! Shell detection, moved to `qcode-shell` so tools can use it too.
//!
//! Re-exported rather than duplicated: the terminal picks the shell a tab runs,
//! and `run_shell` picks the shell a command runs in (ADR-0064). Two answers to
//! the same question is how "the terminal is zsh, the agent's commands run in sh"
//! happens.
pub use qcode_shell::*;
