//! qcode-core: domain model, event log, turn state machine and provider port.
//!
//! This crate deliberately knows nothing about HTTP, the filesystem layout of a
//! particular OS, or how a tool is executed. Implementations live in sibling
//! crates and are injected through the traits defined here.

pub mod approval;
pub mod cost;
pub mod engine;
pub mod event;
pub mod history;
pub mod policy;
pub mod provider;
pub mod state;
pub mod store;
pub mod tool;

pub use approval::{Answer, ApprovalRequest, Approver, AutoApprove, AutoDeny, InteractiveApprover};
pub use cost::Pricing;
pub use engine::{
    Delta, DeltaSink, EngineError, RunOptions, RunOutcome, SYSTEM_PROMPT, SessionWriter,
    TurnContext, run_turn,
};
pub use event::*;
pub use policy::{Action, Decision, Policy, Rule};
pub use provider::*;
pub use state::{TransitionError, Trigger, TurnState};
pub use store::{SessionStore, SessionSummary, StoreError};
pub use tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
