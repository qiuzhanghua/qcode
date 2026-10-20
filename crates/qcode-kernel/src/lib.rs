//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;

/// Kept here so configuration stops reaching into the binary crate.
pub const DEFAULT_REPO: &str = "qiuzhanghua/qcode";
