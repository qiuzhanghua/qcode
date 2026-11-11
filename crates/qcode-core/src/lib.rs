//! qcode-core: domain model, event log, turn state machine and provider port.
//!
//! This crate deliberately knows nothing about HTTP, the filesystem layout of a
//! particular OS, or how a tool is executed. Implementations live in sibling
//! crates and are injected through the traits defined here.

pub mod approval;
pub mod compact;
pub mod cost;
pub mod engine;
pub mod event;
pub mod history;
pub mod inbox;
pub mod network;
pub mod policy;
pub mod progress;
pub mod provider;
pub mod question;
pub mod schedule;
pub mod scheduler;
pub mod snapshot;
pub mod state;
pub mod store;
pub mod task;
pub mod todo;
pub mod tool;

pub use approval::{Answer, ApprovalRequest, Approver, AutoApprove, AutoDeny, InteractiveApprover};
pub use cost::Pricing;
pub use engine::{
    Delta, DeltaSink, EngineError, RunOptions, RunOutcome, SYSTEM_PROMPT, SessionWriter,
    TurnContext, run_turn,
};
pub use event::*;
pub use inbox::{Inbox, QueuedMessage};
pub use network::{Network, SharedNetwork};
pub use policy::{Action, Decision, Policy, Rule};
pub use progress::{CommandLine, FailureNote, ProgressReport};
pub use provider::*;
pub use question::{
    AnswerItem, InteractiveQuestioner, QuestionAnswer, QuestionError, QuestionIntent, QuestionItem,
    QuestionOption, QuestionReply, QuestionRequest, Questioner, answer_covers, pending_result,
    reply_text, validate_request,
};
pub use schedule::{
    Advance, CronSpec, DeliveryReceipt, Overlap, Schedule, ScheduleState, ScheduleStatus,
    ScheduledTask, Target, TaskStatus, TaskTick, Tick, advance_rule, append_delivery,
    import_config_schedule, now_local, parse_rule, task_tick,
};
pub use scheduler::{EditTask, NewTask, ScheduleError, Scheduler, SchedulerHandle};
pub use state::{TransitionError, Trigger, TurnState};
pub use store::{SessionStore, SessionSummary, StoreError};
pub use task::{MAX_ROOTS, Root, RootAccess, Task, TaskError};
pub use todo::{TodoItem, TodoStatus};
pub use tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
