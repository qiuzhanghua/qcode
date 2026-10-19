//! ratatui frontend (M2).
//!
//! [`App`] projects persisted events into a transcript; [`render`] draws it.
//! Terminal setup and the event loop live in the `qc` binary, which keeps this
//! crate testable without a tty.

pub mod app;
pub mod ui;

pub use app::{App, ApprovalPrompt, Block, ContextDisplay, SkillView, Status, ToolEntry, UiAction};
pub use ui::render;
