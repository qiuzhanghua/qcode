//! TUI state: a projection of the event log plus the composer.
//!
//! This module deliberately performs no terminal IO. `handle_key` returns a
//! `UiAction` for the caller to execute, which keeps the key map unit-testable
//! and lets the renderer be exercised with `TestBackend`.

use qcode_core::event::{ContentBlock, Event, EventKind, ToolStatus, TurnStatus};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// One visible item in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    User(String),
    Assistant(String),
    Tool(ToolEntry),
    Notice(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolEntry {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    /// `None` while the tool is still running.
    pub status: Option<ToolStatus>,
    pub output: String,
    pub expanded: bool,
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

#[derive(Debug, Clone)]
pub struct App {
    pub blocks: Vec<Block>,
    pub input: String,
    pub status: Status,
    pub approval: Option<ApprovalPrompt>,
    /// Ctrl-C has been pressed once; the next one quits.
    pub quit_armed: bool,
}

impl App {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            blocks: Vec::new(),
            input: String::new(),
            status: Status {
                provider: provider.into(),
                model: model.into(),
                turn: 0,
                cost_usd: 0.0,
                running: false,
                last: None,
            },
            approval: None,
            quit_armed: false,
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
            }
            EventKind::UserMessage(message) => {
                self.blocks.push(Block::User(message.text.clone()));
            }
            EventKind::AssistantMessage(message) => {
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
                    status: None,
                    output: String::new(),
                    expanded: false,
                }));
            }
            EventKind::ToolResult(result) => {
                if let Some(tool) = self.tool_mut(&result.call_id) {
                    tool.status = Some(result.status);
                    tool.output = result.output.clone();
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
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    return None;
                }
                // The engine echoes a `user_message` event right away; the
                // transcript is projected from events, so do not add a second
                // copy here.
                let text = std::mem::take(&mut self.input);
                self.status.running = true;
                Some(UiAction::Submit(text))
            }
            KeyCode::Esc => self.status.running.then_some(UiAction::Cancel),
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Char('o') if ctrl => {
                self.toggle_last_tool();
                None
            }
            KeyCode::Char(character) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.push(character);
                None
            }
            _ => None,
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

/// One-line argument summary for the collapsed tool row.
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
        AssistantMessage, Event, EventKind, ToolCall, ToolResult, TurnStarted, Usage, UserMessage,
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

    #[test]
    fn enter_submits_and_clears_the_composer() {
        let mut app = App::new("mock", "mock-1");
        for character in "fix tests".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(action, Some(UiAction::Submit("fix tests".to_string())));
        assert!(app.input.is_empty());
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
        app.input = "hi".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.apply(&event(EventKind::UserMessage(UserMessage {
            text: "hi".to_string(),
            attachments: Vec::new(),
        })));
        assert_eq!(app.blocks, vec![Block::User("hi".to_string())]);
    }
}
