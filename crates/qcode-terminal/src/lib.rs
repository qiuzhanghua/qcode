//! Terminal emulation core for the qcode desktop GUI (ADR-0062).
//!
//! This crate owns the VT state machine and nothing else: no window, no PTY
//! wiring, no policy. It exists so the ANSI handling can be tested headless —
//! feed it bytes, assert the grid — before any of it is drawn or spawned.
//!
//! The split is deliberate. ADR-0062 requires that terminal I/O never runs on
//! the runtime that drives tasks and scheduled ticks, so the parsing side has to
//! be usable without a `LocalBackend` anywhere in sight.

pub mod screen;
pub mod session;

pub use screen::{CellView, Events, Screen, View};
pub use session::{Session, SessionConfig};

// Re-exported so a caller can name `TermMode`, `Color`, `Flags` and friends
// without depending on `alacritty_terminal` directly at a second version.
pub use alacritty_terminal;
