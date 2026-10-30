//! TUI state: a projection of the event log plus the composer.
//!
//! This module deliberately performs no terminal IO. `handle_action` returns a
//! `UiAction` for the caller to execute, which keeps the key map unit-testable
//! and lets the renderer be exercised with `TestBackend`.

pub mod composer;

use std::collections::HashSet;

pub use composer::{Composer, InputEvent};

use qcode_core::engine::Delta;
use qcode_core::event::{ContentBlock, Event, EventKind, ToolStatus, TurnStatus, Usage};
use qcode_core::todo::{self, TodoItem};
use time::{Duration, OffsetDateTime};

/// Rows moved by PageUp / PageDown. The UI clamps this to the transcript.
const SCROLL_STEP: usize = 10;

/// One visible item in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    User(String),
    Assistant(String),
    Tool(ToolEntry),
    /// Progress from a child agent: text, notices and a nested tool row.
    /// `group` is the child session id, shared with its parent `subagent` row.
    Subagent {
        text: String,
        group: String,
    },
    /// The session todo list as returned by `todo_write`. Rendered inline so
    /// every update stays visible in the transcript.
    Todo(Vec<TodoItem>),
    Notice(String),
    /// A skill body loaded through the `/skills <name>` debug command.
    Skill(String),
    /// Model reasoning. Kept separate from the final answer.
    Reasoning(ReasoningEntry),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningEntry {
    pub text: String,
    pub started_at: OffsetDateTime,
    pub elapsed: Option<Duration>,
}

/// One user turn: its input, process rows and final answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Stable id derived from the `turn_started` event sequence.
    pub id: u64,
    pub number: u32,
    pub started_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    pub status: Option<TurnStatus>,
    /// Block index of the user message (or activated-skill row).
    pub input_block: Option<usize>,
    /// Block indices for reasoning/tool/notice/subagent/todo rows.
    pub process: Vec<usize>,
    /// Block index of the final assistant answer.
    pub answer_block: Option<usize>,
    pub usage: Usage,
    pub cost_usd: f64,
}

impl Turn {
    pub fn duration(&self) -> Option<Duration> {
        self.ended_at
            .map(|ended| (ended - self.started_at).max(Duration::ZERO))
    }

    pub fn running(&self) -> bool {
        self.status.is_none() && self.ended_at.is_none()
    }
}

/// Read-only skill metadata handed to the UI for `/skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillView {
    pub name: String,
    pub source: String,
    pub description: String,
    pub dir: String,
    pub body: String,
    pub files: Vec<String>,
    pub files_truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolEntry {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    /// Kept so a successful `todo_write` result can be turned back into a
    /// todo block (and, on replay, so the state does not depend on tool output
    /// being re-parsed).
    pub arguments: serde_json::Value,
    /// `None` while the tool is still running.
    pub status: Option<ToolStatus>,
    pub output: String,
    pub expanded: bool,
    /// When the call started, kept to show a duration once it finishes.
    pub started: OffsetDateTime,
    pub elapsed: Option<Duration>,
    /// True for a child agent's tool call (rendered nested).
    pub child: bool,
    /// Child session id for a nested tool row, or the linked child session for
    /// the parent `subagent` row.
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub provider: String,
    pub model: String,
    pub turn: u32,
    pub cost_usd: f64,
    pub running: bool,
    pub last: Option<TurnStatus>,
    /// Follow-up messages waiting for the running turn to end.
    pub queued: usize,
    /// Steering messages waiting for the next step boundary.
    pub steering: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub rule: Option<String>,
    pub argv: Option<String>,
    pub cwd: Option<String>,
}

/// What the composer is doing right now (ADR-0042 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerState {
    /// No turn running: typing and sending are free.
    Idle,
    /// A turn is running: sending queues a follow-up or steers.
    Running,
    /// A pending interaction owns the composer (approval, and later plan/question).
    Pending,
    /// The decision was submitted; the editor returns once the engine confirms.
    Restoring,
}

/// Frontend-neutral actions that are not text editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    Submit,
    Newline,
    /// Ctrl-C: cancel a running turn, quit on the second press.
    Interrupt,
    /// Esc: cancel a running turn, or reject a pending approval.
    Escape,
    Up,
    Down,
    PageUp,
    PageDown,
    /// Ctrl-S: steer the composer into the running turn.
    Steer,
    /// Ctrl-End: jump back to the live tail.
    ScrollToTail,
    /// Ctrl-O: expand or collapse the last tool block.
    ExpandLastTool,
    /// Ctrl-T: cycle the context meter between used/left/percent.
    CycleContext,
    /// Ctrl-B: fold or unfold the last child group.
    ToggleChildGroup,
    /// Ctrl-V: attach the clipboard image.
    PasteImage,
    /// Answer a pending approval: (allow, remember for the session).
    Approval(bool, bool),
}

/// What the event loop should do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Submit(String),
    /// A follow-up sent while a turn was running: the frontend queues it.
    Queue(String),
    /// Steer input for the running turn's next step boundary.
    Steer(String),
    Approve {
        call_id: String,
        allow: bool,
        /// `a`: allow this tool for the rest of the session, no prompt again.
        session: bool,
    },
    Cancel,
    Quit,
    /// /checkpoint: list the recorded checkpoints for this session.
    Checkpoints,
    /// /rollback N: restore the workspace to the state before turn N.
    Rollback(u32),
    /// /queue: list the pending follow-ups.
    QueueList,
    /// /steer N: push queued message N into the running turn.
    QueueSteer(usize),
    /// /drop N: remove queued message N.
    QueueDrop(usize),
    /// /steering: list steering waiting for the next step boundary.
    SteeringList,
    /// /unsteer N: take steering item N back out of the running turn.
    SteeringUndo(usize),
    /// Ctrl-V: attach the image on the clipboard to the next message.
    PasteImage,
}

/// How the status bar renders context usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextDisplay {
    /// Percent on narrow terminals, absolute tokens on wide ones.
    #[default]
    Auto,
    /// Always `ctx 42%`.
    Percent,
    /// Always `ctx 15.4k/128k`.
    Tokens,
}

impl ContextDisplay {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Percent,
            Self::Percent => Self::Tokens,
            Self::Tokens => Self::Auto,
        }
    }
}

/// Namespaced id so a child call cannot update a parent tool row.
fn child_call_id(session: &str, call_id: &str) -> String {
    format!("{session}:{call_id}")
}

/// Last few characters of a session id, enough to tell child runs apart.
fn short_id(id: &str) -> String {
    let count = id.chars().count();
    if count <= 6 {
        return id.to_string();
    }
    let suffix: String = id.chars().skip(count - 6).collect();
    format!("…{suffix}")
}

/// One-line argument summary for the collapsed tool row.
/// User messages produced by `/<skill-name>` start with this marker; the UI
/// renders them as an activation notice instead of echoing the whole body.
fn activated_skill(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("[skill:")?;
    let end = rest.find(']')?;
    Some(&rest[..end])
}

pub fn summarize_call(name: &str, arguments: &serde_json::Value) -> String {
    let raw = match name {
        "run_shell" => arguments
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        "read_file" | "write_file" | "edit_file" | "list_dir" => arguments
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => arguments.to_string(),
    };
    if raw.chars().count() > 100 {
        format!("{}…", raw.chars().take(100).collect::<String>())
    } else {
        raw
    }
}

mod actions;
mod app;
mod apply;

#[cfg(test)]
mod tests;

pub use app::App;
