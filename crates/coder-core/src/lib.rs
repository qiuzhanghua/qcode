//! coder-core: domain model, event log, turn state machine and provider port.
//!
//! This crate deliberately knows nothing about HTTP, the filesystem layout of a
//! particular OS, or how a tool is executed. Implementations live in sibling
//! crates and are injected through the traits defined here.

pub mod engine;
pub mod event;
pub mod provider;
pub mod state;
pub mod store;

pub use engine::{EngineError, RunOptions, RunOutcome, SYSTEM_PROMPT, SessionWriter, run_turn};
pub use event::*;
pub use provider::*;
pub use state::{TransitionError, Trigger, TurnState};
pub use store::{SessionStore, SessionSummary, StoreError};
