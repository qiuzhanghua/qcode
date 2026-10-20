//! ratatui frontend (M2).
//!
//! The projection lives in `qcode-ui-state`; this crate only draws it.
//! Terminal setup and the event loop live in the `qc` binary, which keeps both
//! halves testable without a tty.

pub mod ui;

pub use qcode_ui_state::{
    App, ApprovalPrompt, Block, ContextDisplay, SkillView, Status, ToolEntry, UiAction,
};
pub use ui::render;
