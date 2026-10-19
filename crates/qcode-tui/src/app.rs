//! TUI state: a projection of the event log plus the composer.
//!
//! This module deliberately performs no terminal IO. `handle_key` returns a
//! `UiAction` for the caller to execute, which keeps the key map unit-testable
//! and lets the renderer be exercised with `TestBackend`.

use std::collections::HashSet;

use qcode_core::engine::Delta;
use qcode_core::event::{ContentBlock, Event, EventKind, ToolStatus, TurnStatus};
use qcode_core::todo::{self, TodoItem};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::widgets::Block as UiBlock;
use time::{Duration, OffsetDateTime};
use tui_textarea::TextArea;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
}

/// What the event loop should do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Submit(String),
    Approve { call_id: String, allow: bool },
    Cancel,
    Quit,
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
    /// Text streamed for the message currently being generated. Cleared when
    /// the authoritative `assistant_message` event arrives.
    pub streaming: String,
    /// Multi-line composer. tui-textarea owns editing keys, CJK width and
    /// cursor movement; the app only decides when Enter submits (Ctrl-N and
    /// Shift/Alt+Enter insert a newline).
    pub textarea: TextArea<'static>,
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
            streaming: String::new(),
            textarea: composer(),
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
            }
            EventKind::UserMessage(message) => {
                if let Some(name) = activated_skill(&message.text) {
                    self.blocks
                        .push(Block::Skill(format!("activated skill {name}")));
                } else {
                    self.blocks.push(Block::User(message.text.clone()));
                }
            }
            EventKind::AssistantMessage(message) => {
                // The completed message supersedes the streamed preview, which
                // may have been cut short by cancellation.
                self.streaming.clear();
                for block in &message.blocks {
                    if let ContentBlock::Text { text } = block {
                        self.blocks.push(Block::Assistant(text.clone()));
                    }
                }
            }
            EventKind::ToolCall(call) => {
                self.blocks.push(Block::Tool(ToolEntry {
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
                    if tool.name == "todo_write" && result.status == ToolStatus::Ok {
                        todo::from_arguments(&tool.arguments).ok()
                    } else {
                        None
                    }
                });
                if let Some(items) = todo_update {
                    if items.is_empty() {
                        self.blocks
                            .push(Block::Notice("todo list cleared".to_string()));
                    } else {
                        self.blocks.push(Block::Todo(items));
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
                });
            }
            EventKind::ApprovalDecided(_) => {
                self.approval = None;
            }
            EventKind::TurnEnded(ended) => {
                self.streaming.clear();
                self.status.running = false;
                self.status.last = Some(ended.status);
                self.status.cost_usd = ended.cost_usd;
            }
            EventKind::Error(error) => {
                self.blocks
                    .push(Block::Notice(format!("{}: {}", error.kind, error.message)));
            }
            EventKind::Compaction(_) => {
                self.blocks
                    .push(Block::Notice("context compacted".to_string()));
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
                self.blocks.push(Block::Subagent {
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
                        self.blocks.push(Block::Subagent {
                            text: text.clone(),
                            group: group.clone(),
                        });
                    }
                }
            }
            EventKind::ToolCall(call) => {
                self.blocks.push(Block::Tool(ToolEntry {
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
                self.blocks.push(Block::Subagent {
                    text: format!("error: {}: {}", error.kind, error.message),
                    group,
                });
            }
            EventKind::Compaction(_) => {
                self.blocks.push(Block::Subagent {
                    text: "context compacted".to_string(),
                    group,
                });
            }
            EventKind::TurnEnded(ended) => {
                self.blocks.push(Block::Subagent {
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
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl-C: first press cancels a running turn, second press quits.
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            if self.quit_armed {
                return Some(UiAction::Quit);
            }
            self.quit_armed = true;
            return self.status.running.then_some(UiAction::Cancel);
        }
        self.quit_armed = false;

        // An approval prompt swallows everything except its own answers; the
        // event loop must not mutate the turn while a decision is pending.
        if let Some(prompt) = self.approval.clone() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.approval = None;
                    return Some(UiAction::Approve {
                        call_id: prompt.call_id,
                        allow: true,
                    });
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.approval = None;
                    return Some(UiAction::Approve {
                        call_id: prompt.call_id,
                        allow: false,
                    });
                }
                _ => return None,
            }
        }

        match key.code {
            // Terminals without the Kitty keyboard protocol cannot tell
            // Shift+Enter from Enter, so Ctrl-N is the reliable newline key.
            KeyCode::Char('n') if ctrl => {
                self.textarea.insert_newline();
                return None;
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.textarea.insert_newline();
                return None;
            }
            KeyCode::Enter => {
                let text = self.textarea.lines().join("\n");
                if text.trim().is_empty() {
                    return None;
                }
                // Slash commands are handled by the UI and never reach the
                // model. They stay available while a turn is running.
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
                return Some(UiAction::Submit(text));
            }
            KeyCode::Esc => return self.status.running.then_some(UiAction::Cancel),
            // Laptops and compact keyboards often lack PageUp/PageDown, and
            // some terminals swallow them; Ctrl-U / Ctrl-D are the portable
            // equivalents (the same keys less/vim use).
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                return None;
            }
            KeyCode::Char('u') if ctrl => {
                self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                return None;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                return None;
            }
            KeyCode::Char('d') if ctrl => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                return None;
            }
            // Plain End belongs to the textarea (end of line); Ctrl+End jumps
            // the transcript back to the tail.
            KeyCode::End if ctrl => {
                self.scroll = 0;
                return None;
            }
            KeyCode::Char('o') if ctrl => {
                self.toggle_last_tool();
                return None;
            }
            KeyCode::Char('t') if ctrl => {
                self.context_display = self.context_display.next();
                return None;
            }
            KeyCode::Char('b') if ctrl => {
                if let Some(group) = self.child_groups.last().cloned() {
                    self.toggle_child_group(&group);
                }
                return None;
            }
            _ => {}
        }

        // Any other key edits the composer; typing while scrolled up returns
        // to the live tail.
        self.scroll = 0;
        self.textarea.input(key);
        None
    }

    fn clear_composer(&mut self) {
        self.textarea.select_all();
        self.textarea.cut();
    }

    /// Slash commands are local UI actions; the model never sees them.
    /// `/skills` inspects, `/<skill-name>` activates a skill by submitting a
    /// user message that contains its instructions.
    fn run_command(&mut self, raw: &str) -> Option<UiAction> {
        let (command, argument) = match raw.split_once(char::is_whitespace) {
            Some((command, argument)) => (command, argument.trim()),
            None => (raw, ""),
        };

        if command == "/subagents" {
            self.handle_subagents(argument);
            return None;
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
                "unknown command '{command}'; try /skills, /subagents, or /<skill-name>"
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
        self.blocks.push(Block::Skill(text));
    }

    /// Append a transient stream delta. Reasoning is acknowledged but not
    /// rendered yet (M2 keeps the transcript focused on visible output).
    pub fn apply_delta(&mut self, delta: &Delta) {
        match delta {
            Delta::Text(text) => self.streaming.push_str(text),
            Delta::Context { tokens, window } => {
                self.context_tokens = *tokens;
                self.context_window = Some(*window);
            }
            Delta::Reasoning(_) => {}
        }
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Notice(text.into()));
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolEntry> {
        self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(tool) if tool.call_id == call_id => Some(tool),
            _ => None,
        })
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

/// Composer styling lives here so the renderer only has to draw `&TextArea`.
fn composer() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(
        UiBlock::bordered()
            .title(" message ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    textarea.set_cursor_line_style(Style::default());
    textarea
}

#[cfg(test)]
mod tests {
    use qcode_core::event::{
        AssistantMessage, Event, EventKind, SessionStarted, ToolCall, ToolResult, TurnStarted,
        Usage, UserMessage,
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::*;

    fn event(kind: EventKind) -> Event {
        Event::new(1, "s1", 1, kind)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
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
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(!app.status.running);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text))
                if text.contains("demo [config]") && text.contains("demo skill")
        ));

        type_text(&mut app, "/skills demo");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
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
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("no skills discovered")
        ));

        app.skills = vec![demo_skill()];
        type_text(&mut app, "/skills nope");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("unknown skill 'nope'")
        ));
    }

    #[test]
    fn unknown_slash_command_is_not_sent_to_the_model() {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, "/nope");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
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
        let action = app.handle_key(key(KeyCode::Enter));
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
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("already running")
        ));
    }

    #[test]
    fn enter_submits_and_clears_the_composer() {
        let mut app = App::new("mock", "mock-1");
        for character in "fix tests".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Some(UiAction::Submit("fix tests".to_string())));
        assert_eq!(app.textarea.lines(), [""]);
        assert!(app.blocks.is_empty());
        assert!(app.status.running);
    }

    #[test]
    fn esc_cancels_only_while_running() {
        let mut app = App::new("mock", "mock-1");
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);

        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Some(UiAction::Cancel));
    }

    #[test]
    fn ctrl_c_cancels_then_quits() {
        let mut app = App::new("mock", "mock-1");
        app.apply(&event(EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        })));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(app.handle_key(ctrl_c), Some(UiAction::Cancel));
        assert_eq!(app.handle_key(ctrl_c), Some(UiAction::Quit));
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

        let action = app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(
            action,
            Some(UiAction::Approve {
                call_id: "c1".to_string(),
                allow: true
            })
        );
        assert!(app.approval.is_none());
    }

    #[test]
    fn composer_supports_cjk_and_shift_enter_newlines() {
        let mut app = App::new("mock", "mock-1");
        for character in "中文".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(key(KeyCode::Char('!')));
        assert_eq!(app.textarea.lines(), ["中文", "!"]);

        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Some(UiAction::Submit("中文\n!".to_string())));
        assert_eq!(app.textarea.lines(), [""]);
    }

    #[test]
    fn ctrl_n_and_modified_enter_insert_newlines() {
        let mut app = App::new("mock", "mock-1");
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.textarea.lines(), ["a", "b", "c", "d"]);

        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Some(UiAction::Submit("a\nb\nc\nd".to_string())));
    }

    #[test]
    fn page_keys_move_the_scroll_offset() {
        let mut app = App::new("mock", "mock-1");
        app.handle_key(key(KeyCode::PageUp));
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll, 20);

        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll, 10);

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 20);
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 10);

        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.scroll, 0);
        assert_eq!(app.textarea.lines()[0], "x");
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

        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
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
        assert_eq!(app.blocks, vec![Block::Assistant("done".to_string())]);
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

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_b);
        assert!(app.child_group_folded("child-1"));
        app.handle_key(ctrl_b);
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
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notice(text)) if text.contains("sub-agent groups (1)") && text.contains("shown")
        ));

        type_text(&mut app, "/subagents 1");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(app.child_group_folded("child-1"));
    }

    #[test]
    fn ctrl_t_cycles_the_context_display() {
        let mut app = App::new("mock", "mock-1");
        assert_eq!(app.context_display, ContextDisplay::Auto);

        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_t);
        assert_eq!(app.context_display, ContextDisplay::Percent);
        app.handle_key(ctrl_t);
        assert_eq!(app.context_display, ContextDisplay::Tokens);
        app.handle_key(ctrl_t);
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
        app.textarea.insert_str("hi");
        app.handle_key(key(KeyCode::Enter));
        app.apply(&event(EventKind::UserMessage(UserMessage {
            text: "hi".to_string(),
            attachments: Vec::new(),
        })));
        assert_eq!(app.blocks, vec![Block::User("hi".to_string())]);
    }
}
