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

#[derive(Debug, Clone)]
pub struct App {
    pub blocks: Vec<Block>,
    /// Turn view over `blocks`; block indices are stable for the session.
    pub turns: Vec<Turn>,
    /// The open turn, if any.
    pub current_turn: Option<usize>,
    /// Timestamp of the most recent applied event. A replayed log can end right
    /// after `turn_started` (the process died mid-turn); sealing that turn at
    /// this timestamp keeps it from rendering as forever running.
    pub last_ts: Option<OffsetDateTime>,
    /// Blocks that are not part of a turn (pre-session notices, skill dumps).
    pub loose_blocks: Vec<usize>,
    /// Live reasoning for the open turn, replaced by the authoritative message.
    pub reasoning_streaming: String,
    /// Block holding the live reasoning row so it keeps its place in the
    /// chronological process order (reasoning before the next tool call).
    pub streaming_reasoning_block: Option<usize>,
    /// Text streamed for the message currently being generated. Cleared when
    /// the authoritative `assistant_message` event arrives.
    pub streaming: String,
    /// Multi-line composer. Editing is toolkit-neutral; the renderer draws it.
    /// cursor movement; the app only decides when Enter submits (Ctrl-N and
    /// Shift/Alt+Enter insert a newline).
    pub composer: Composer,
    /// Rows scrolled up from the bottom of the transcript (0 = follow tail).
    pub scroll: usize,
    pub status: Status,
    /// Active sandbox backend (`none`, `sandbox-exec`, `bwrap`, `landlock`).
    /// Kept as a string so the TUI does not depend on `qcode-tools`.
    pub sandbox: String,
    /// Estimated prompt tokens reported by the engine before the last call.
    pub context_tokens: u64,
    /// Context window from `[context] window_tokens`; `None` until configured
    /// or reported by the engine.
    pub context_window: Option<u64>,
    /// Status-bar format for context usage (Ctrl-T cycles Auto/Percent/Tokens).
    pub context_display: ContextDisplay,
    /// Child groups in order, one per `subagent` call (child session ids).
    pub child_groups: Vec<String>,
    /// Child groups whose nested blocks are folded.
    pub collapsed_children: HashSet<String>,
    pub approval: Option<ApprovalPrompt>,
    /// Ctrl-C has been pressed once; the next one quits.
    pub quit_armed: bool,
    /// Skills available to this session, for the `/skills` debug commands.
    pub skills: Vec<SkillView>,
    /// An approval/probe answer was submitted and the engine has not confirmed
    /// it yet; the composer stays locked until `approval_decided` arrives.
    pub restoring: bool,
    /// `[ui] busy_delivery = "steer"`: sending while a turn runs steers the
    /// draft into it instead of queueing a follow-up.
    pub steer_on_send: bool,
}

impl App {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            blocks: Vec::new(),
            turns: Vec::new(),
            current_turn: None,
            last_ts: None,
            loose_blocks: Vec::new(),
            reasoning_streaming: String::new(),
            streaming_reasoning_block: None,
            streaming: String::new(),
            composer: Composer::new(),
            scroll: 0,
            status: Status {
                provider: provider.into(),
                model: model.into(),
                turn: 0,
                cost_usd: 0.0,
                running: false,
                last: None,
                queued: 0,
                steering: 0,
            },
            sandbox: "none".to_string(),
            context_tokens: 0,
            context_window: None,
            context_display: ContextDisplay::default(),
            child_groups: Vec::new(),
            collapsed_children: HashSet::new(),
            approval: None,
            quit_armed: false,
            skills: Vec::new(),
            restoring: false,
            steer_on_send: false,
        }
    }

    /// Clear the transcript before replaying a resumed session.
    ///
    /// Provider/model/sandbox/skills are configuration, not transcript state;
    /// the replayed `session_started` event refreshes the provider and model.
    pub fn reset_transcript(&mut self) {
        self.blocks.clear();
        self.turns.clear();
        self.current_turn = None;
        self.last_ts = None;
        self.loose_blocks.clear();
        self.reasoning_streaming.clear();
        self.streaming_reasoning_block = None;
        self.streaming.clear();
        self.scroll = 0;
        self.status.turn = 0;
        self.status.cost_usd = 0.0;
        self.status.running = false;
        self.status.last = None;
        self.context_tokens = 0;
        self.child_groups.clear();
        self.collapsed_children.clear();
        self.approval = None;
        self.restoring = false;
    }

    fn start_turn(&mut self, event: &Event) {
        // A malformed/truncated log may start a turn without ending the
        // previous one, and an aborted run may leave several split turns open.
        // Close every still-open turn at this timestamp instead of leaving it
        // permanently "running".
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(event.ts.max(turn.started_at));
            }
        }
        let index = self.turns.len();
        self.turns.push(Turn {
            id: event.seq,
            number: event.turn,
            started_at: event.ts,
            ended_at: None,
            status: None,
            input_block: None,
            process: Vec::new(),
            answer_block: None,
            usage: Usage::default(),
            cost_usd: 0.0,
        });
        self.current_turn = Some(index);
    }

    /// A queued follow-up message steered into the running turn: the backend
    /// appends it inside the same turn number without a new `turn_started`, so
    /// the UI splits it into its own question here. Unlike [`start_turn`] this
    /// must not close the parent turn — it stays open until the run's
    /// `turn_ended` closes every split turn together.
    fn split_turn(&mut self, event: &Event) {
        let index = self.turns.len();
        self.turns.push(Turn {
            id: event.seq,
            number: event.turn,
            started_at: event.ts,
            ended_at: None,
            status: None,
            input_block: None,
            process: Vec::new(),
            answer_block: None,
            usage: Usage::default(),
            cost_usd: 0.0,
        });
        self.current_turn = Some(index);
    }

    fn ensure_turn(&mut self, event: &Event) -> usize {
        match self.current_turn {
            Some(index) => index,
            None => {
                self.start_turn(event);
                self.current_turn.expect("just started")
            }
        }
    }

    fn append_block(&mut self, block: Block) -> usize {
        let index = self.blocks.len();
        self.blocks.push(block);
        index
    }

    fn push_process(&mut self, block: Block) -> usize {
        let index = self.append_block(block);
        match self.current_turn {
            Some(turn) => self.turns[turn].process.push(index),
            None => self.loose_blocks.push(index),
        }
        index
    }

    fn push_input(&mut self, block: Block) -> usize {
        let index = self.append_block(block);
        match self.current_turn {
            Some(turn) => {
                if self.turns[turn].input_block.is_none() {
                    self.turns[turn].input_block = Some(index);
                } else {
                    self.turns[turn].process.push(index);
                }
            }
            None => self.loose_blocks.push(index),
        }
        index
    }

    fn close_turn(&mut self, event: &Event, status: TurnStatus, usage: Usage, cost_usd: f64) {
        // 排队追加的消息被拆成独立 turn 后，一次 run 可能同时开着多个 turn
        // （A 和插入其中的 B）；`turn_ended` 属于整个 run，把仍开着的全部收口。
        self.current_turn = None;
        let ts = event.ts;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ts.max(turn.started_at));
                turn.status = Some(status.clone());
                turn.usage = usage.clone();
                turn.cost_usd = cost_usd;
            }
        }
    }

    /// 历史事件重放结束后调用。
    ///
    /// 重放里的 `TurnStarted` 只说明“当时开始过”，不代表这个进程里现在
    /// 还有 turn 在跑。恢复被中断的会话时日志没有对应的 `TurnEnded`，
    /// 不复位就会一直显示“运行中”，还会挡住输入和切换会话。
    pub fn finish_replay(&mut self) {
        self.status.running = false;
        self.restoring = false;
        // 日志可能停在 `turn_started`（进程在 turn 中途退出，没有 TurnEnded）。
        // 用最后一个事件的时间戳把仍开着的 turn（含排队追加拆出的）全部封口，
        // 否则它们会一直显示“running”，耗时持续累加，执行过程也永远打不开。
        let ended = self.last_ts;
        self.current_turn = None;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ended.unwrap_or(turn.started_at).max(turn.started_at));
                turn.status = Some(TurnStatus::Cancelled);
            }
        }
    }

    /// 引擎在发出 `TurnEnded` 之前就结束了（运行失败/被中断）时收口仍开着的
    /// turn（含排队追加拆出的）。
    ///
    /// 没有这一步，`Turn::running()` 会一直是 true：卡片上的耗时不停增长，
    /// 执行过程被强制展开而无法折叠。
    pub fn abort_turn(&mut self, status: TurnStatus) {
        self.restoring = false;
        let ended = OffsetDateTime::now_utc();
        self.current_turn = None;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ended.max(turn.started_at));
                turn.status = Some(status.clone());
            }
        }
        self.status.running = false;
    }

    /// Project a persisted event into the transcript and status line.
    pub fn apply(&mut self, event: &Event) {
        self.last_ts = Some(event.ts);
        match &event.kind {
            EventKind::SessionStarted(started) => {
                self.status.provider = started.provider.clone();
                self.status.model = started.model.clone();
            }
            EventKind::TurnStarted(_) => {
                self.status.turn = event.turn;
                self.status.running = true;
                self.status.last = None;
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.streaming_reasoning_block = None;
                self.start_turn(event);
            }
            EventKind::UserMessage(message) => {
                // v4 marks injected steering explicitly (`starts_segment`), so
                // the projection no longer has to guess. v3 logs lack the flag;
                // there, a user message that arrives while the current turn
                // already has input is the steered follow-up, split into its own
                // turn (A and B each keep their process and rail dot).
                let v3_heuristic = event.v < 4
                    && self
                        .current_turn
                        .and_then(|index| self.turns.get(index))
                        .is_some_and(|turn| turn.input_block.is_some());
                if message.starts_segment || v3_heuristic {
                    self.split_turn(event);
                }
                self.ensure_turn(event);
                if let Some(name) = activated_skill(&message.text) {
                    self.push_input(Block::Skill(format!("activated skill {name}")));
                } else {
                    self.push_input(Block::User(message.text.clone()));
                }
            }
            EventKind::AssistantMessage(message) => {
                // The completed message supersedes the streamed previews, which
                // may have been cut short by cancellation.
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.ensure_turn(event);
                let turn = self.current_turn.expect("ensured turn");
                let mut live_reasoning = self.streaming_reasoning_block.take();
                let mut answer = String::new();
                for block in &message.blocks {
                    match block {
                        ContentBlock::Reasoning { text, .. } => {
                            let elapsed =
                                (event.ts - self.turns[turn].started_at).max(Duration::ZERO);
                            let replaced = live_reasoning.take().and_then(|index| {
                                match self.blocks.get_mut(index) {
                                    Some(Block::Reasoning(entry)) => {
                                        entry.text = text.clone();
                                        entry.started_at = self.turns[turn].started_at;
                                        entry.elapsed = Some(elapsed);
                                        Some(())
                                    }
                                    _ => None,
                                }
                            });
                            if replaced.is_none() {
                                self.push_process(Block::Reasoning(ReasoningEntry {
                                    text: text.clone(),
                                    started_at: self.turns[turn].started_at,
                                    elapsed: Some(elapsed),
                                }));
                            }
                        }
                        ContentBlock::Text { text } => {
                            if !answer.is_empty() {
                                answer.push_str("\n\n");
                            }
                            answer.push_str(text);
                        }
                        _ => {}
                    }
                }
                if !answer.is_empty() {
                    let index = self.append_block(Block::Assistant(answer));
                    self.turns[turn].answer_block = Some(index);
                }
            }
            EventKind::ToolCall(call) => {
                self.ensure_turn(event);
                self.push_process(Block::Tool(ToolEntry {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    summary: summarize_call(&call.name, &call.arguments),
                    arguments: call.arguments.clone(),
                    status: None,
                    output: String::new(),
                    expanded: false,
                    started: event.ts,
                    elapsed: None,
                    child: false,
                    group: None,
                }));
            }
            EventKind::ToolResult(result) => {
                let todo_update = self.tool_mut(&result.call_id).and_then(|tool| {
                    tool.status = Some(result.status);
                    tool.output = result.output.clone();
                    tool.elapsed = Some((event.ts - tool.started).max(Duration::ZERO));
                    if result.status != ToolStatus::Ok {
                        tool.expanded = true;
                    }
                    if tool.name == "todo_write" && result.status == ToolStatus::Ok {
                        todo::from_arguments(&tool.arguments).ok()
                    } else {
                        None
                    }
                });
                if let Some(items) = todo_update {
                    if items.is_empty() {
                        self.push_process(Block::Notice("todo list cleared".to_string()));
                    } else {
                        self.push_process(Block::Todo(items));
                    }
                }
            }
            EventKind::ApprovalRequested(request) => {
                let (tool, summary) = self
                    .tool_mut(&request.call_id)
                    .map(|tool| (tool.name.clone(), tool.summary.clone()))
                    .unwrap_or_else(|| (request.call_id.clone(), String::new()));
                self.approval = Some(ApprovalPrompt {
                    call_id: request.call_id.clone(),
                    tool,
                    summary,
                    reason: request.reason.clone(),
                    rule: request.rule_id.clone(),
                    argv: None,
                    cwd: None,
                });
            }
            EventKind::ApprovalDecided(_) => {
                self.approval = None;
                self.restoring = false;
            }
            EventKind::TurnEnded(ended) => {
                self.restoring = false;
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.streaming_reasoning_block = None;
                self.status.running = false;
                self.status.last = Some(ended.status);
                self.status.cost_usd = ended.cost_usd;
                self.close_turn(event, ended.status, ended.usage.clone(), ended.cost_usd);
            }
            EventKind::Error(error) => {
                self.push_process(Block::Notice(format!("{}: {}", error.kind, error.message)));
            }
            EventKind::Compaction(_) => {
                self.push_process(Block::Notice("context compacted".to_string()));
            }
            // Task/root configuration is not rendered in the transcript yet.
            EventKind::TaskConfigured(_)
            | EventKind::TaskRenamed(_)
            | EventKind::RootsChanged(_) => {}
        }
    }

    /// Project an event from a child agent.
    ///
    /// Child turns must not move the parent status machine, so this only
    /// appends nested transcript rows. Tool call ids are namespaced with the
    /// child session id to avoid clashing with parent calls.
    pub fn apply_child(&mut self, event: &Event) {
        let group = event.session.clone();
        match &event.kind {
            EventKind::SessionStarted(started) => {
                if !self.child_groups.iter().any(|known| known == &group) {
                    self.child_groups.push(group.clone());
                }
                // Bind the child session to the parent `subagent` row that
                // spawned it. Sub-agent calls are serial, so the newest
                // unbound row is the right one.
                if let Some(tool) = self.blocks.iter_mut().rev().find_map(|block| match block {
                    Block::Tool(tool)
                        if !tool.child && tool.name == "subagent" && tool.group.is_none() =>
                    {
                        Some(tool)
                    }
                    _ => None,
                }) {
                    tool.group = Some(group.clone());
                }
                self.push_process(Block::Subagent {
                    text: format!(
                        "subagent {} started ({}/{})",
                        short_id(&group),
                        started.provider,
                        started.model
                    ),
                    group,
                });
            }
            EventKind::AssistantMessage(message) => {
                for block in &message.blocks {
                    if let ContentBlock::Text { text } = block {
                        self.push_process(Block::Subagent {
                            text: text.clone(),
                            group: group.clone(),
                        });
                    }
                }
            }
            EventKind::ToolCall(call) => {
                self.push_process(Block::Tool(ToolEntry {
                    call_id: child_call_id(&group, &call.call_id),
                    name: call.name.clone(),
                    summary: summarize_call(&call.name, &call.arguments),
                    arguments: call.arguments.clone(),
                    status: None,
                    output: String::new(),
                    expanded: false,
                    started: event.ts,
                    elapsed: None,
                    child: true,
                    group: Some(group),
                }));
            }
            EventKind::ToolResult(result) => {
                let call_id = child_call_id(&group, &result.call_id);
                if let Some(tool) = self.tool_mut(&call_id) {
                    tool.status = Some(result.status);
                    tool.output = result.output.clone();
                    tool.elapsed = Some((event.ts - tool.started).max(Duration::ZERO));
                }
            }
            EventKind::Error(error) => {
                self.push_process(Block::Subagent {
                    text: format!("error: {}: {}", error.kind, error.message),
                    group,
                });
            }
            EventKind::Compaction(_) => {
                self.push_process(Block::Subagent {
                    text: "context compacted".to_string(),
                    group,
                });
            }
            EventKind::TurnEnded(ended) => {
                self.push_process(Block::Subagent {
                    text: format!("finished ({:?})", ended.status),
                    group,
                });
            }
            // Approval prompts come through the approver channel; the child
            // never owns the parent status line.
            EventKind::UserMessage(_)
            | EventKind::TurnStarted(_)
            | EventKind::ApprovalRequested(_)
            | EventKind::ApprovalDecided(_)
            | EventKind::TaskConfigured(_)
            | EventKind::TaskRenamed(_)
            | EventKind::RootsChanged(_) => {}
        }
    }

    /// Fold or unfold one sub-agent group.
    pub fn toggle_child_group(&mut self, group: &str) {
        if !self.collapsed_children.remove(group) {
            self.collapsed_children.insert(group.to_string());
        }
    }

    pub fn child_group_folded(&self, group: &str) -> bool {
        self.collapsed_children.contains(group)
    }

    /// Number of nested blocks belonging to a group (parent row label).
    pub fn child_block_count(&self, group: &str) -> usize {
        self.blocks
            .iter()
            .filter(|block| match block {
                Block::Subagent { group: id, .. } => id == group,
                Block::Tool(tool) => tool.child && tool.group.as_deref() == Some(group),
                _ => false,
            })
            .count()
    }

    /// Turn a key press into an action. Unhandled keys return `None`.
    /// Apply one editing event. While an approval prompt is up, `y`/`a`/`n`
    /// answer it and everything else is swallowed.
    pub fn handle_input(&mut self, event: InputEvent) -> Option<UiAction> {
        self.quit_armed = false;
        if self.approval.is_some() {
            let InputEvent::Insert(ch) = event else {
                return None;
            };
            return match ch {
                'y' | 'Y' => self.answer(true, false),
                'a' | 'A' => self.answer(true, true),
                'n' | 'N' => self.answer(false, false),
                _ => None,
            };
        }
        // Typing while scrolled up returns to the live tail.
        self.scroll = 0;
        self.composer.apply(event);
        None
    }

    /// A non-text action from the frontend: submit, scroll, approve, quit…
    /// The composer state, for renderers and frontends (ADR-0042 §1).
    pub fn composer_state(&self) -> ComposerState {
        if self.approval.is_some() {
            ComposerState::Pending
        } else if self.restoring {
            ComposerState::Restoring
        } else if self.status.running {
            ComposerState::Running
        } else {
            ComposerState::Idle
        }
    }

    pub fn handle_action(&mut self, action: AppAction) -> Option<UiAction> {
        // Ctrl-C is special: the first press cancels a running turn, the second
        // quits, and any other key disarms it.
        if let AppAction::Interrupt = action {
            if self.quit_armed {
                return Some(UiAction::Quit);
            }
            self.quit_armed = true;
            return self.status.running.then_some(UiAction::Cancel);
        }
        self.quit_armed = false;

        if self.restoring && matches!(action, AppAction::Submit | AppAction::Steer) {
            // The decision is in flight: lock the interaction's submissions
            // until the engine confirms it (ADR-0042 §1, Restoring).
            return None;
        }

        // An approval prompt swallows everything except its own answers; the
        // event loop must not mutate the turn while a decision is pending.
        if self.approval.is_some() {
            return match action {
                AppAction::Approval(allow, session) => self.answer(allow, session),
                AppAction::Escape => self.answer(false, false),
                _ => None,
            };
        }

        match action {
            AppAction::Interrupt => None,
            AppAction::Approval(..) => None,
            AppAction::Newline => {
                self.composer.apply(InputEvent::Newline);
                None
            }
            AppAction::Submit => self.submit(),
            AppAction::Steer => self.steer(),
            AppAction::Escape => self.status.running.then_some(UiAction::Cancel),
            AppAction::PageUp => {
                self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                None
            }
            AppAction::PageDown => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                None
            }
            // Arrows scroll only while the draft is a single line: a multi-line
            // draft still needs them to move the cursor.
            AppAction::Up => {
                if self.composer.line_count() <= 1 {
                    self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                } else {
                    self.composer.apply(InputEvent::MoveUp);
                }
                None
            }
            AppAction::Down => {
                if self.composer.line_count() <= 1 {
                    self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                } else {
                    self.composer.apply(InputEvent::MoveDown);
                }
                None
            }
            AppAction::ScrollToTail => {
                self.scroll = 0;
                None
            }
            AppAction::ExpandLastTool => {
                self.toggle_last_tool();
                None
            }
            AppAction::CycleContext => {
                self.context_display = self.context_display.next();
                None
            }
            AppAction::ToggleChildGroup => {
                if let Some(group) = self.child_groups.last().cloned() {
                    self.toggle_child_group(&group);
                }
                None
            }
            AppAction::PasteImage => Some(UiAction::PasteImage),
        }
    }

    fn answer(&mut self, allow: bool, session: bool) -> Option<UiAction> {
        let prompt = self.approval.take()?;
        self.restoring = true;
        Some(UiAction::Approve {
            call_id: prompt.call_id,
            allow,
            session,
        })
    }

    /// Enter: slash commands run locally, everything else goes to the model.
    fn submit(&mut self) -> Option<UiAction> {
        let text = self.composer.text();
        if text.trim().is_empty() {
            return None;
        }
        // Slash commands never reach the model, and stay usable mid-turn.
        if text.trim_start().starts_with('/') {
            self.clear_composer();
            self.scroll = 0;
            if let Some(action) = self.run_command(text.trim()) {
                return Some(action);
            }
            return None;
        }
        if self.status.running {
            // A follow-up while the turn runs: queue it (default) or steer it
            // into the next step (`[ui] busy_delivery = "steer"`).
            self.clear_composer();
            self.scroll = 0;
            return Some(if self.steer_on_send {
                UiAction::Steer(text)
            } else {
                UiAction::Queue(text)
            });
        }
        self.clear_composer();
        self.scroll = 0;
        self.status.running = true;
        Some(UiAction::Submit(text))
    }

    /// Ctrl-S while a turn runs: hand the draft to its next step boundary.
    fn steer(&mut self) -> Option<UiAction> {
        let text = self.composer.text();
        if text.trim().is_empty() {
            return None;
        }
        if !self.status.running {
            self.push_notice("steer: no turn is running".to_string());
            return None;
        }
        self.clear_composer();
        self.scroll = 0;
        Some(UiAction::Steer(text))
    }

    fn clear_composer(&mut self) {
        self.composer = Composer::new();
    }

    /// Slash commands are local UI actions; the model never sees them.
    /// `/skills` inspects, `/<skill-name>` activates a skill by submitting a
    /// user message that contains its instructions.
    fn run_command(&mut self, raw: &str) -> Option<UiAction> {
        let (command, argument) = match raw.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (raw, ""),
        };

        if command == "/checkpoint" {
            return Some(UiAction::Checkpoints);
        }

        if command == "/queue" {
            return Some(UiAction::QueueList);
        }

        if command == "/steering" {
            return Some(UiAction::SteeringList);
        }

        if command == "/unsteer" {
            let position = argument.parse::<usize>().ok().filter(|n| *n > 0);
            return match position {
                Some(position) => Some(UiAction::SteeringUndo(position)),
                None => {
                    self.push_notice("usage: /unsteer <n> (see /steering)");
                    None
                }
            };
        }

        if command == "/steer" || command == "/drop" {
            let position = argument.parse::<usize>().ok().filter(|n| *n > 0);
            return match position {
                Some(position) if command == "/steer" => Some(UiAction::QueueSteer(position)),
                Some(position) => Some(UiAction::QueueDrop(position)),
                None => {
                    self.push_notice(format!("usage: {command} <n> (see /queue)"));
                    None
                }
            };
        }

        if command == "/rollback" {
            let turn = argument.parse::<u32>().map_err(|_| ()).ok();
            return match turn {
                Some(turn) => Some(UiAction::Rollback(turn)),
                None => {
                    self.push_notice("usage: /rollback <turn> (see /checkpoint)".to_string());
                    None
                }
            };
        }

        if command == "/subagents" {
            self.handle_subagents(argument);
            return None;
        }

        // Three spellings: /exit is the documented one, /quit is what vim users
        // type, /q is the shortest that does not collide with a skill name.
        if matches!(command, "/exit" | "/quit" | "/q") {
            return Some(UiAction::Quit);
        }

        if command == "/skills" {
            if argument.is_empty() {
                self.show_skills();
            } else {
                self.show_skill(argument);
            }
            return None;
        }

        let name = command.strip_prefix('/').unwrap_or_default();
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            self.push_notice(format!(
                "unknown command '{command}'; try /skills, /subagents, /exit, or /<skill-name>"
            ));
            return None;
        };

        if self.status.running {
            self.push_notice("a turn is already running; Esc cancels it before activating a skill");
            return None;
        }

        let mut text = format!("[skill:{}]\n{}\n", skill.name, skill.body.trim());
        if argument.is_empty() {
            text.push_str("\nFollow the skill instructions above now.");
        } else {
            text.push('\n');
            text.push_str(argument);
        }
        self.status.running = true;
        Some(UiAction::Submit(text))
    }

    /// `/subagents` lists child groups; `/subagents N` folds/unfolds one.
    fn handle_subagents(&mut self, argument: &str) {
        if self.child_groups.is_empty() {
            self.push_notice("no sub-agent runs in this session");
            return;
        }

        if argument.is_empty() {
            let groups = self.child_groups.clone();
            let mut text = format!("sub-agent groups ({}):", groups.len());
            for (index, group) in groups.iter().enumerate() {
                let state = if self.child_group_folded(group) {
                    "folded"
                } else {
                    "shown"
                };
                text.push_str(&format!(
                    "\n  {}. {} · {} nested · {state}",
                    index + 1,
                    short_id(group),
                    self.child_block_count(group),
                ));
            }
            text.push_str("\n\nUse /subagents <n> to toggle one; Ctrl-B toggles the newest.");
            self.push_notice(text);
            return;
        }

        let parsed = argument.parse::<usize>().ok();
        match parsed {
            Some(index) if index >= 1 && index <= self.child_groups.len() => {
                let group = self.child_groups[index - 1].clone();
                self.toggle_child_group(&group);
                let state = if self.child_group_folded(&group) {
                    "folded"
                } else {
                    "shown"
                };
                self.push_notice(format!("sub-agent group {index}: {state}"));
            }
            _ => self.push_notice(format!(
                "usage: /subagents [1..{}]",
                self.child_groups.len()
            )),
        }
    }

    fn show_skills(&mut self) {
        if self.skills.is_empty() {
            self.push_notice(
                "no skills discovered; run `qc skills list` to see the roots that were checked",
            );
            return;
        }

        let mut text = format!("skills ({}):", self.skills.len());
        for skill in &self.skills {
            text.push_str(&format!(
                "\n  {} [{}] files={}\n    {}",
                skill.name,
                skill.source,
                skill.files.len(),
                skill.description.replace(['\n', '\r'], " ")
            ));
        }
        text.push_str("\n\nUse /skills <name> to show the full instructions.");
        self.push_notice(text);
    }

    fn show_skill(&mut self, name: &str) {
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            self.push_notice(format!("unknown skill '{name}'; try /skills"));
            return;
        };

        let mut text = format!(
            "# Skill: {}\nsource: {} ({})\n",
            skill.name, skill.source, skill.dir
        );
        if !skill.files.is_empty() {
            text.push_str(&format!("files: {}", skill.files.join(", ")));
            if skill.files_truncated {
                text.push_str(" …");
            }
            text.push('\n');
        }
        text.push('\n');
        text.push_str(skill.body.trim());
        self.push_process(Block::Skill(text));
    }

    /// Append a transient stream delta. Reasoning is kept in its own buffer
    /// so the GUI can show a Thinking row before the authoritative message.
    pub fn apply_delta(&mut self, delta: &Delta) {
        match delta {
            Delta::Text(text) => self.streaming.push_str(text),
            Delta::Context { tokens, window } => {
                self.context_tokens = *tokens;
                self.context_window = Some(*window);
            }
            Delta::Reasoning(text) => {
                self.reasoning_streaming.push_str(text);
                let current = self.reasoning_streaming.clone();
                let index = match self.streaming_reasoning_block {
                    Some(index) => index,
                    None => {
                        let index = self.push_process(Block::Reasoning(ReasoningEntry {
                            text: current.clone(),
                            started_at: OffsetDateTime::now_utc(),
                            elapsed: None,
                        }));
                        self.streaming_reasoning_block = Some(index);
                        index
                    }
                };
                if let Some(Block::Reasoning(entry)) = self.blocks.get_mut(index) {
                    entry.text = current;
                }
            }
        }
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push_process(Block::Notice(text.into()));
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolEntry> {
        self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(tool) if tool.call_id == call_id => Some(tool),
            _ => None,
        })
    }

    /// Toggle one tool card by call id (GUI click).
    pub fn toggle_tool(&mut self, call_id: &str) {
        if let Some(Block::Tool(tool)) = self
            .blocks
            .iter_mut()
            .find(|block| matches!(block, Block::Tool(tool) if tool.call_id == call_id))
        {
            tool.expanded = !tool.expanded;
        }
    }

    fn toggle_last_tool(&mut self) {
        if let Some(tool) = self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(tool) => Some(tool),
            _ => None,
        }) {
            tool.expanded = !tool.expanded;
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

#[cfg(test)]
mod tests;
