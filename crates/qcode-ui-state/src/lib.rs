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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    Compact,
    #[default]
    Standard,
    Detailed,
}

impl DisplayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::Detailed => "detailed",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "compact" => Self::Compact,
            "detailed" => Self::Detailed,
            _ => Self::Standard,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub provider: String,
    pub model: String,
    pub turn: u32,
    pub cost_usd: f64,
    pub running: bool,
    pub last: Option<TurnStatus>,
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
    /// Blocks that are not part of a turn (pre-session notices, skill dumps).
    pub loose_blocks: Vec<usize>,
    /// Live reasoning for the open turn, replaced by the authoritative message.
    pub reasoning_streaming: String,
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
}

impl App {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            blocks: Vec::new(),
            turns: Vec::new(),
            current_turn: None,
            loose_blocks: Vec::new(),
            reasoning_streaming: String::new(),
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
        self.loose_blocks.clear();
        self.reasoning_streaming.clear();
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
    }

    fn start_turn(&mut self, event: &Event) {
        if let Some(index) = self.current_turn.take() {
            // A malformed/truncated log may start a turn without ending the
            // previous one. Close it at this timestamp instead of leaving it
            // permanently "running".
            if self.turns[index].ended_at.is_none() {
                self.turns[index].ended_at = Some(event.ts);
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
        if let Some(index) = self.current_turn.take() {
            let turn = &mut self.turns[index];
            turn.ended_at = Some(event.ts);
            turn.status = Some(status);
            turn.usage = usage;
            turn.cost_usd = cost_usd;
        }
    }

    /// Project a persisted event into the transcript and status line.
    pub fn apply(&mut self, event: &Event) {
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
                self.start_turn(event);
            }
            EventKind::UserMessage(message) => {
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
                let mut answer = String::new();
                for block in &message.blocks {
                    match block {
                        ContentBlock::Reasoning { text, .. } => {
                            let elapsed =
                                (event.ts - self.turns[turn].started_at).max(Duration::ZERO);
                            self.push_process(Block::Reasoning(ReasoningEntry {
                                text: text.clone(),
                                started_at: self.turns[turn].started_at,
                                elapsed: Some(elapsed),
                            }));
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
            }
            EventKind::TurnEnded(ended) => {
                self.streaming.clear();
                self.reasoning_streaming.clear();
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
            | EventKind::ApprovalDecided(_) => {}
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
            return None;
        }
        self.clear_composer();
        self.scroll = 0;
        self.status.running = true;
        Some(UiAction::Submit(text))
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
            Delta::Reasoning(text) => self.reasoning_streaming.push_str(text),
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
mod tests {
    use qcode_core::event::{
        AssistantMessage, Event, EventKind, SessionStarted, ToolCall, ToolResult, TurnStarted,
        Usage, UserMessage,
    };
    use serde_json::json;

    use super::*;

    fn event(kind: EventKind) -> Event {
        Event::new(1, "s1", 1, kind)
    }

    /// Send one named key, the way a frontend would: text goes in as input
    /// events, everything else as an action.
    fn press(app: &mut App, name: &str) -> Option<UiAction> {
        match name {
            "enter" => app.handle_action(AppAction::Submit),
            "esc" => app.handle_action(AppAction::Escape),
            "up" => app.handle_action(AppAction::Up),
            "down" => app.handle_action(AppAction::Down),
            "page-up" | "ctrl-u" => app.handle_action(AppAction::PageUp),
            "page-down" | "ctrl-d" => app.handle_action(AppAction::PageDown),
            "ctrl-c" => app.handle_action(AppAction::Interrupt),
            "ctrl-n" | "shift-enter" | "alt-enter" => app.handle_action(AppAction::Newline),
            "ctrl-end" => app.handle_action(AppAction::ScrollToTail),
            "ctrl-o" => app.handle_action(AppAction::ExpandLastTool),
            "ctrl-t" => app.handle_action(AppAction::CycleContext),
            "ctrl-b" => app.handle_action(AppAction::ToggleChildGroup),
            "ctrl-v" => app.handle_action(AppAction::PasteImage),
            other => {
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(ch), None) => app.handle_input(InputEvent::Insert(ch)),
                    _ => None,
                }
            }
        }
    }

    fn type_text(app: &mut App, text: &str) {
        app.handle_input(InputEvent::InsertStr(text.to_string()));
    }
    fn demo_skill() -> SkillView {
        SkillView {
            name: "demo".to_string(),
            source: "config".to_string(),
            description: "demo skill".to_string(),
            dir: "/tmp/demo".to_string(),
            body: "Follow the steps.".to_string(),
            files: vec!["references/x.md".to_string()],
            files_truncated: false,
        }
    }

    #[test]
    fn slash_skills_lists_and_shows_a_skill() {
        let mut app = App::new("mock", "mock-1");
        app.skills = vec![demo_skill()];

        type_text(&mut app, "/skills");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(!app.status.running);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text))
                if text.contains("demo [config]") && text.contains("demo skill")
        ));

        type_text(&mut app, "/skills demo");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Skill(text))
                if text.contains("# Skill: demo")
                    && text.contains("references/x.md")
                    && text.contains("Follow the steps.")
        ));
    }

    #[test]
    fn slash_skills_reports_unknown_and_empty_cases() {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, "/skills");
        press(&mut app, "enter");
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("no skills discovered")
        ));

        app.skills = vec![demo_skill()];
        type_text(&mut app, "/skills nope");
        press(&mut app, "enter");
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("unknown skill 'nope'")
        ));
    }

    #[test]
    fn unknown_slash_command_is_not_sent_to_the_model() {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, "/nope");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(!app.status.running);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("unknown command '/nope'")
        ));
    }

    #[test]
    fn slash_skill_name_activates_it_as_a_user_message() {
        let mut app = App::new("mock", "mock-1");
        app.skills = vec![demo_skill()];

        type_text(&mut app, "/demo 盘点本机");
        let action = press(&mut app, "enter");
        let Some(UiAction::Submit(text)) = action else {
            panic!("expected a submit action");
        };
        assert!(text.starts_with("[skill:demo]\n"));
        assert!(text.contains("Follow the steps."));
        assert!(text.ends_with("盘点本机"));
        assert!(app.status.running);

        app.apply(&event(EventKind::UserMessage(UserMessage {
            text,
            attachments: Vec::new(),
        })));
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Skill(text)) if text == "activated skill demo"
        ));
    }

    #[test]
    fn slash_skill_activation_is_rejected_while_running() {
        let mut app = App::new("mock", "mock-1");
        app.skills = vec![demo_skill()];
        app.status.running = true;

        type_text(&mut app, "/demo");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("already running")
        ));
    }

    #[test]
    fn enter_submits_and_clears_the_composer() {
        let mut app = App::new("mock", "mock-1");
        for character in "fix tests".chars() {
            press(&mut app, &character.to_string());
        }
        let action = press(&mut app, "enter");
        assert_eq!(action, Some(UiAction::Submit("fix tests".to_string())));
        assert!(app.composer.is_empty());
        assert!(app.blocks.is_empty());
        assert!(app.status.running);
    }

    #[test]
    fn esc_cancels_only_while_running() {
        let mut app = App::new("mock", "mock-1");
        assert_eq!(press(&mut app, "esc"), None);

        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        assert_eq!(press(&mut app, "esc"), Some(UiAction::Cancel));
    }

    #[test]
    fn ctrl_c_cancels_then_quits() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));

        assert_eq!(press(&mut app, "ctrl-c"), Some(UiAction::Cancel));
        assert_eq!(press(&mut app, "ctrl-c"), Some(UiAction::Quit));
    }

    #[test]
    fn approval_prompt_answers_with_y_or_n() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "cargo test"}),
        })));
        app.apply(&event(EventKind::ApprovalRequested(
            qcode_core::event::ApprovalRequested {
                call_id: "c1".to_string(),
                rule_id: None,
                reason: "process execution".to_string(),
            },
        )));

        let prompt = app.approval.clone().expect("prompt");
        assert_eq!(prompt.tool, "run_shell");
        assert_eq!(prompt.summary, "cargo test");

        let action = press(&mut app, "y");
        assert_eq!(
            action,
            Some(UiAction::Approve {
                call_id: "c1".to_string(),
                allow: true,
                session: false,
            })
        );
        assert!(app.approval.is_none());
    }

    #[test]
    fn a_answers_allow_for_the_session() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "cargo test"}),
        })));
        app.apply(&event(EventKind::ApprovalRequested(
            qcode_core::event::ApprovalRequested {
                call_id: "c1".to_string(),
                rule_id: None,
                reason: "process execution".to_string(),
            },
        )));

        assert_eq!(
            press(&mut app, "a"),
            Some(UiAction::Approve {
                call_id: "c1".to_string(),
                allow: true,
                session: true,
            })
        );
        assert!(app.approval.is_none());
    }

    #[test]
    fn composer_supports_cjk_and_shift_enter_newlines() {
        let mut app = App::new("mock", "mock-1");
        for character in "中文".chars() {
            press(&mut app, &character.to_string());
        }
        press(&mut app, "shift-enter");
        press(&mut app, "!");
        assert_eq!(app.composer.text(), "中文\n!");

        let action = press(&mut app, "enter");
        assert_eq!(action, Some(UiAction::Submit("中文\n!".to_string())));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn ctrl_n_and_modified_enter_insert_newlines() {
        let mut app = App::new("mock", "mock-1");
        press(&mut app, "a");
        press(&mut app, "ctrl-n");
        press(&mut app, "b");
        press(&mut app, "shift-enter");
        press(&mut app, "c");
        press(&mut app, "alt-enter");
        press(&mut app, "d");
        assert_eq!(app.composer.text(), "a\nb\nc\nd");

        let action = press(&mut app, "enter");
        assert_eq!(action, Some(UiAction::Submit("a\nb\nc\nd".to_string())));
    }

    #[test]
    fn page_keys_move_the_scroll_offset() {
        let mut app = App::new("mock", "mock-1");
        press(&mut app, "page-up");
        press(&mut app, "page-up");
        assert_eq!(app.scroll, 20);

        press(&mut app, "page-down");
        assert_eq!(app.scroll, 10);

        press(&mut app, "ctrl-u");
        assert_eq!(app.scroll, 20);
        press(&mut app, "ctrl-d");
        assert_eq!(app.scroll, 10);

        press(&mut app, "x");
        assert_eq!(app.scroll, 0);
        assert_eq!(app.composer.text(), "x");
    }

    #[test]
    fn arrows_scroll_when_the_draft_is_a_single_line() {
        let mut app = App::new("mock", "mock-1");
        press(&mut app, "up");
        press(&mut app, "up");
        assert_eq!(app.scroll, 20);
        press(&mut app, "down");
        assert_eq!(app.scroll, 10);
    }

    #[test]
    fn arrows_edit_the_composer_when_the_draft_has_multiple_lines() {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, "a");
        press(&mut app, "shift-enter");
        type_text(&mut app, "b");
        assert_eq!(app.composer.line_count(), 2);

        press(&mut app, "up");
        assert_eq!(app.scroll, 0, "a multi-line draft keeps the arrow");
    }

    #[test]
    fn slash_exit_quits_in_three_spellings() {
        for command in ["/exit", "/quit", "/q"] {
            let mut app = App::new("mock", "mock-1");
            type_text(&mut app, command);
            assert_eq!(
                press(&mut app, "enter"),
                Some(UiAction::Quit),
                "{command} should quit"
            );
        }
    }

    #[test]
    fn slash_checkpoint_and_rollback_are_local_actions() {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, "/checkpoint");
        assert_eq!(press(&mut app, "enter"), Some(UiAction::Checkpoints));

        type_text(&mut app, "/rollback 3");
        assert_eq!(press(&mut app, "enter"), Some(UiAction::Rollback(3)));

        // A turn number is required; the UI explains instead of guessing.
        type_text(&mut app, "/rollback");
        assert_eq!(press(&mut app, "enter"), None);
        let notices: Vec<&str> = app
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Notice(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("/rollback <turn>")),
            "{notices:?}"
        );
    }

    #[test]
    fn ctrl_v_asks_the_loop_to_paste_an_image() {
        let mut app = App::new("mock", "mock-1");

        assert_eq!(press(&mut app, "ctrl-v"), Some(UiAction::PasteImage));
        // The UI has no clipboard access, so the key must not touch the composer.
        assert!(app.composer.text().is_empty());
    }

    #[test]
    fn tool_result_updates_the_matching_row() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "cargo test"}),
        })));
        app.apply(&event(EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "ok".to_string(),
            metadata: Default::default(),
        })));

        match app.blocks.last() {
            Some(Block::Tool(tool)) => {
                assert_eq!(tool.status, Some(ToolStatus::Ok));
                assert_eq!(tool.output, "ok");
                assert!(!tool.expanded);
            }
            other => panic!("unexpected block: {other:?}"),
        }

        press(&mut app, "ctrl-o");
        assert!(matches!(app.blocks.last(), Some(Block::Tool(tool)) if tool.expanded));
    }

    #[test]
    fn todo_write_result_becomes_a_todo_block() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "todo_write".to_string(),
            arguments: json!({"items": [
                {"content": "read spec", "status": "completed"},
                {"content": "build tool", "status": "in_progress"},
                {"content": "run tests"},
            ]}),
        })));
        app.apply(&event(EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "Todo list (1/3 done):".to_string(),
            metadata: Default::default(),
        })));

        match app.blocks.last() {
            Some(Block::Todo(items)) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].status, qcode_core::todo::TodoStatus::Completed);
                assert_eq!(items[2].content, "run tests");
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn failed_todo_write_does_not_replace_the_list() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "todo_write".to_string(),
            arguments: json!({"items": [{"content": "ok"}]}),
        })));
        app.apply(&event(EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "Todo list (0/1 done):".to_string(),
            metadata: Default::default(),
        })));
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c2".to_string(),
            name: "todo_write".to_string(),
            arguments: json!({"items": [{"content": ""}]}),
        })));
        app.apply(&event(EventKind::ToolResult(ToolResult {
            call_id: "c2".to_string(),
            status: ToolStatus::Error,
            output: "item 1 has empty `content`".to_string(),
            metadata: Default::default(),
        })));

        assert_eq!(
            app.blocks
                .iter()
                .filter(|block| matches!(block, Block::Todo(_)))
                .count(),
            1,
            "{:?}",
            app.blocks
        );
    }

    #[test]
    fn empty_todo_write_clears_the_list_with_a_notice() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "todo_write".to_string(),
            arguments: json!({"items": []}),
        })));
        app.apply(&event(EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "todo list cleared".to_string(),
            metadata: Default::default(),
        })));

        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("cleared")
        ));
    }

    #[test]
    fn assistant_text_and_usage_reach_the_transcript() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "thinking".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                },
            ],
            provider_raw: json!({}),
            usage: Usage::default(),
        })));
        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(
            &app.blocks[0],
            Block::Reasoning(entry) if entry.text == "thinking"
        ));
        assert_eq!(app.blocks[1], Block::Assistant("done".to_string()));
        assert_eq!(app.turns.len(), 1);
        assert_eq!(app.turns[0].process, vec![0]);
        assert_eq!(app.turns[0].answer_block, Some(1));
    }

    #[test]
    fn turns_track_input_process_answer_and_usage() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&Event::new(
            1,
            "s1",
            1,
            EventKind::TurnStarted(TurnStarted {
                input: "go".to_string(),
            }),
        ));
        app.apply(&Event::new(
            2,
            "s1",
            1,
            EventKind::UserMessage(UserMessage {
                text: "go".to_string(),
                attachments: Vec::new(),
            }),
        ));
        app.apply(&Event::new(
            3,
            "s1",
            1,
            EventKind::ToolCall(ToolCall {
                call_id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "src/lib.rs"}),
            }),
        ));
        app.apply(&Event::new(
            4,
            "s1",
            1,
            EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![
                    ContentBlock::Reasoning {
                        text: "look first".to_string(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "answer".to_string(),
                    },
                ],
                provider_raw: json!({}),
                usage: Usage::default(),
            }),
        ));
        app.apply(&Event::new(
            5,
            "s1",
            1,
            EventKind::TurnEnded(qcode_core::event::TurnEnded {
                status: TurnStatus::Completed,
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(3),
                    ..Usage::default()
                },
                cost_usd: 0.01,
            }),
        ));

        assert_eq!(app.turns.len(), 1);
        let turn = &app.turns[0];
        assert_eq!(turn.number, 1);
        assert_eq!(turn.status, Some(TurnStatus::Completed));
        assert!(turn.input_block.is_some());
        assert!(turn.answer_block.is_some());
        assert_eq!(turn.process.len(), 2);
        assert_eq!(turn.usage.output_tokens, Some(3));
        assert_eq!(turn.cost_usd, 0.01);
    }

    #[test]
    fn turns_are_closed_and_indexed_independently() {
        let mut app = App::new("mock", "mock-1");
        for (turn, input, answer) in [(1u32, "first", "one"), (2u32, "second", "two")] {
            app.apply(&Event::new(
                u64::from(turn * 10),
                "s1",
                turn,
                EventKind::TurnStarted(TurnStarted {
                    input: input.to_string(),
                }),
            ));
            app.apply(&Event::new(
                u64::from(turn * 10 + 1),
                "s1",
                turn,
                EventKind::UserMessage(UserMessage {
                    text: input.to_string(),
                    attachments: Vec::new(),
                }),
            ));
            app.apply(&Event::new(
                u64::from(turn * 10 + 2),
                "s1",
                turn,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![ContentBlock::Text {
                        text: answer.to_string(),
                    }],
                    provider_raw: json!({}),
                    usage: Usage::default(),
                }),
            ));
            app.apply(&Event::new(
                u64::from(turn * 10 + 3),
                "s1",
                turn,
                EventKind::TurnEnded(qcode_core::event::TurnEnded {
                    status: TurnStatus::Completed,
                    usage: Usage::default(),
                    cost_usd: 0.0,
                }),
            ));
        }
        assert_eq!(app.turns.len(), 2);
        assert_eq!(app.turns[0].number, 1);
        assert_eq!(app.turns[1].number, 2);
        assert!(app.turns[0].duration().is_some());
        assert!(app.turns[1].duration().is_some());
        assert_eq!(app.turns[0].input_block, Some(0));
        assert_eq!(app.turns[1].input_block, Some(2));
        assert_eq!(app.turns[0].answer_block, Some(1));
        assert_eq!(app.turns[1].answer_block, Some(3));
    }

    #[test]
    fn reasoning_delta_is_kept_until_the_authoritative_message() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        app.apply_delta(&Delta::Reasoning("step ".to_string()));
        app.apply_delta(&Delta::Reasoning("two".to_string()));
        assert_eq!(app.reasoning_streaming, "step two");

        app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "step two".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                },
            ],
            provider_raw: json!({}),
            usage: Usage::default(),
        })));
        assert!(app.reasoning_streaming.is_empty());
        assert!(matches!(
            &app.blocks[0],
            Block::Reasoning(entry) if entry.text == "step two"
        ));
    }

    #[test]
    fn child_events_are_nested_and_do_not_move_parent_status() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({"task": "investigate"}),
        })));

        let child = |seq: u64, kind: EventKind| Event::new(seq, "child-1", 1, kind);
        app.apply_child(&child(
            1,
            EventKind::SessionStarted(SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0.1.0".to_string(),
            }),
        ));
        app.apply_child(&child(
            2,
            EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![ContentBlock::Text {
                    text: "child thinking".to_string(),
                }],
                provider_raw: json!({}),
                usage: Usage::default(),
            }),
        ));
        // Same call_id as the parent: the session prefix must keep them apart.
        app.apply_child(&child(
            3,
            EventKind::ToolCall(ToolCall {
                call_id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "a.txt"}),
            }),
        ));
        app.apply_child(&child(
            4,
            EventKind::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                status: ToolStatus::Ok,
                output: "contents".to_string(),
                metadata: Default::default(),
            }),
        ));
        app.apply_child(&child(
            5,
            EventKind::TurnEnded(qcode_core::event::TurnEnded {
                status: TurnStatus::Completed,
                usage: Usage::default(),
                cost_usd: 0.0,
            }),
        ));

        // Parent turn is still running and the parent tool row is untouched.
        assert!(app.status.running);
        let parent = app
            .blocks
            .iter()
            .find_map(|block| match block {
                Block::Tool(tool) if !tool.child && tool.call_id == "c1" => Some(tool),
                _ => None,
            })
            .expect("parent tool row");
        assert_eq!(parent.status, None);
        assert_eq!(parent.group.as_deref(), Some("child-1"));

        let child_tool = app
            .blocks
            .iter()
            .find_map(|block| match block {
                Block::Tool(tool) if tool.child && tool.name == "read_file" => Some(tool),
                _ => None,
            })
            .expect("child tool row");
        assert_eq!(child_tool.status, Some(ToolStatus::Ok));
        assert_eq!(child_tool.output, "contents");

        assert!(app.blocks.iter().any(
            |block| matches!(block, Block::Subagent { text, .. } if text == "child thinking")
        ));
        assert!(app.blocks.iter().any(
            |block| matches!(block, Block::Subagent { text, .. } if text.contains("finished"))
        ));
        assert_eq!(app.child_groups, vec!["child-1"]);
        assert_eq!(app.child_block_count("child-1"), 4);
    }

    #[test]
    fn context_deltas_update_the_status_fields() {
        let mut app = App::new("mock", "mock-1");
        assert_eq!(app.context_tokens, 0);
        assert_eq!(app.context_window, None);

        app.apply_delta(&Delta::Context {
            tokens: 15_360,
            window: 128_000,
        });
        assert_eq!(app.context_tokens, 15_360);
        assert_eq!(app.context_window, Some(128_000));
    }

    #[test]
    fn ctrl_b_folds_the_newest_child_group() {
        let mut app = App::new("mock", "mock-1");
        assert!(app.child_groups.is_empty());

        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "p1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({"task": "x"}),
        })));
        app.apply_child(&Event::new(
            1,
            "child-1",
            1,
            EventKind::SessionStarted(SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0.1.0".to_string(),
            }),
        ));
        assert_eq!(app.child_groups, vec!["child-1"]);
        assert!(!app.child_group_folded("child-1"));

        press(&mut app, "ctrl-b");
        assert!(app.child_group_folded("child-1"));
        press(&mut app, "ctrl-b");
        assert!(!app.child_group_folded("child-1"));
    }

    #[test]
    fn subagents_command_lists_and_toggles_groups() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(ToolCall {
            call_id: "p1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({"task": "x"}),
        })));
        app.apply_child(&Event::new(
            1,
            "child-1",
            1,
            EventKind::SessionStarted(SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0.1.0".to_string(),
            }),
        ));

        type_text(&mut app, "/subagents");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("sub-agent groups (1)") && text.contains("shown")
        ));

        type_text(&mut app, "/subagents 1");
        assert_eq!(press(&mut app, "enter"), None);
        assert!(app.child_group_folded("child-1"));
    }

    #[test]
    fn ctrl_t_cycles_the_context_display() {
        let mut app = App::new("mock", "mock-1");
        assert_eq!(app.context_display, ContextDisplay::Auto);

        press(&mut app, "ctrl-t");
        assert_eq!(app.context_display, ContextDisplay::Percent);
        press(&mut app, "ctrl-t");
        assert_eq!(app.context_display, ContextDisplay::Tokens);
        press(&mut app, "ctrl-t");
        assert_eq!(app.context_display, ContextDisplay::Auto);
    }

    #[test]
    fn streamed_text_is_replaced_by_the_final_message() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        app.apply_delta(&Delta::Text("hel".to_string()));
        app.apply_delta(&Delta::Text("lo".to_string()));
        assert_eq!(app.streaming, "hello");

        app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            provider_raw: json!({}),
            usage: Usage::default(),
        })));
        assert!(app.streaming.is_empty());
        assert_eq!(app.blocks, vec![Block::Assistant("hello".to_string())]);
    }

    #[test]
    fn turn_ended_updates_status_and_cost() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        app.apply(&event(EventKind::TurnEnded(qcode_core::event::TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage::default(),
            cost_usd: 0.000_015,
        })));
        assert!(!app.status.running);
        assert_eq!(app.status.last, Some(TurnStatus::Completed));
        assert!((app.status.cost_usd - 0.000_015).abs() < f64::EPSILON);
    }

    #[test]
    fn submitted_text_is_rendered_once_the_event_arrives() {
        let mut app = App::new("mock", "mock-1");
        app.composer.apply(InputEvent::InsertStr("hi".to_string()));
        press(&mut app, "enter");
        app.apply(&event(EventKind::UserMessage(UserMessage {
            text: "hi".to_string(),
            attachments: Vec::new(),
        })));
        assert_eq!(app.blocks, vec![Block::User("hi".to_string())]);
    }

    #[test]
    fn toggle_tool_flips_only_the_requested_card() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::ToolCall(qcode_core::event::ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "a"}),
        })));
        app.apply(&event(EventKind::ToolCall(qcode_core::event::ToolCall {
            call_id: "c2".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "b"}),
        })));
        app.toggle_tool("c1");
        let expanded: Vec<bool> = app
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Tool(tool) => Some(tool.expanded),
                _ => None,
            })
            .collect();
        assert_eq!(expanded, [true, false]);
        app.toggle_tool("c1");
        assert!(matches!(
            &app.blocks[0],
            Block::Tool(tool) if !tool.expanded
        ));
    }

    #[test]
    fn reset_transcript_keeps_configuration_and_clears_state() {
        let mut app = App::new("deepseek", "deepseek-flash");
        app.push_notice("old");
        app.status.running = true;
        app.status.turn = 3;
        app.status.cost_usd = 1.5;
        app.context_tokens = 42;
        app.child_groups.push("child".to_string());
        app.reset_transcript();
        assert!(app.blocks.is_empty());
        assert!(app.streaming.is_empty());
        assert!(!app.status.running);
        assert_eq!(app.status.turn, 0);
        assert_eq!(app.status.cost_usd, 0.0);
        assert_eq!(app.status.provider, "deepseek");
        assert_eq!(app.status.model, "deepseek-flash");
        assert_eq!(app.context_tokens, 0);
        assert!(app.child_groups.is_empty());
    }
}
