//! Session todo list (`todo_write`).
//!
//! Todos are session state, not files: the only copy outside memory is the
//! `tool_call` / `tool_result` pair in the event log. [`current`] replays those
//! events, which is what keeps the list alive across resume, replay and
//! compaction. The engine additionally renders the list into the system prompt
//! ([`prompt_section`]) rather than relying on the model to scroll back.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{Event, EventKind, ToolStatus};

/// Upper bound on one list. Keeps the prompt section and the TUI block small.
pub const MAX_ITEMS: usize = 50;

/// Upper bound on one item's text, in characters.
pub const MAX_CONTENT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(default)]
    pub status: TodoStatus,
}

/// Parse the `todo_write` arguments (`{"items": [...]}`).
///
/// The error strings are written for the model: they name the field and the
/// limit so a retry can fix the call instead of guessing.
pub fn from_arguments(args: &Value) -> Result<Vec<TodoItem>, String> {
    let array = args
        .get("items")
        .ok_or_else(|| "todo_write requires an `items` array".to_string())?
        .as_array()
        .ok_or_else(|| "`items` must be an array".to_string())?;

    if array.len() > MAX_ITEMS {
        return Err(format!(
            "`items` has {} entries; the limit is {MAX_ITEMS}",
            array.len()
        ));
    }

    let mut items = Vec::with_capacity(array.len());
    let mut in_progress = 0;
    for (index, raw) in array.iter().enumerate() {
        let number = index + 1;
        let object = raw
            .as_object()
            .ok_or_else(|| format!("item {number} must be an object"))?;

        let content = object
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("item {number} needs a string `content`"))?
            .trim();
        if content.is_empty() {
            return Err(format!("item {number} has empty `content`"));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(format!(
                "item {number} is longer than {MAX_CONTENT_CHARS} characters"
            ));
        }
        if content.chars().any(char::is_control) {
            return Err(format!(
                "item {number} contains a control character; keep each todo on one line"
            ));
        }

        let status = match object.get("status") {
            None | Some(Value::Null) => TodoStatus::Pending,
            Some(value) => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("item {number} `status` must be a string"))?;
                let status = TodoStatus::from_str(raw).ok_or_else(|| {
                    format!(
                        "item {number} has unknown status `{raw}`; use pending, in_progress or completed"
                    )
                })?;
                if status == TodoStatus::InProgress {
                    in_progress += 1;
                }
                status
            }
        };

        items.push(TodoItem {
            content: content.to_string(),
            status,
        });
    }

    if in_progress > 1 {
        return Err(
            "at most one item may be in_progress; finish or pause the current one first"
                .to_string(),
        );
    }

    Ok(items)
}

/// `(completed, total)` for the summary lines.
pub fn progress(items: &[TodoItem]) -> (usize, usize) {
    (
        items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count(),
        items.len(),
    )
}

/// Plain-text checklist returned to the model as the tool result.
pub fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list cleared (no items)".to_string();
    }

    let (done, total) = progress(items);
    let mut text = format!("Todo list ({done}/{total} done):");
    for (index, item) in items.iter().enumerate() {
        let glyph = match item.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        };
        text.push_str(&format!("\n{}. {glyph} {}", index + 1, item.content));
    }
    text
}

/// Section 5 of the context assembly order (SPEC §9). Kept out of the static
/// system prompt so a `todo_write` mid-turn is visible to the next model call.
pub fn prompt_section(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return String::new();
    }

    let (done, total) = progress(items);
    let mut text = format!(
        "# Todo\n\nCurrent todo list ({done}/{total} done). Keep it current with `todo_write`:\n"
    );
    for (index, item) in items.iter().enumerate() {
        let glyph = match item.status {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
        };
        text.push_str(&format!("{}. {glyph} {}\n", index + 1, item.content));
    }
    text.pop();
    text
}

/// Rebuild the current list from the event log.
///
/// Only a successful `todo_write` counts; a denied or failed call leaves the
/// previous list untouched. An empty list clears the state (`None`).
pub fn current(events: &[Event]) -> Option<Vec<TodoItem>> {
    let mut pending: Vec<(String, Value)> = Vec::new();
    let mut current = None;

    for event in events {
        match &event.kind {
            EventKind::ToolCall(call) if call.name == "todo_write" => {
                pending.push((call.call_id.clone(), call.arguments.clone()));
            }
            EventKind::ToolResult(result) if result.status == ToolStatus::Ok => {
                let Some(index) = pending
                    .iter()
                    .position(|(call_id, _)| *call_id == result.call_id)
                else {
                    continue;
                };
                let (_, arguments) = pending.remove(index);
                if let Ok(items) = from_arguments(&arguments) {
                    current = (!items.is_empty()).then_some(items);
                }
            }
            _ => {}
        }
    }

    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AssistantMessage, ContentBlock, Event, EventKind, ToolCall, ToolResult, Usage,
    };
    use serde_json::json;

    fn call(seq: u64, call_id: &str, arguments: Value) -> Event {
        Event::new(
            seq,
            "s",
            1,
            EventKind::ToolCall(ToolCall {
                call_id: call_id.to_string(),
                name: "todo_write".to_string(),
                arguments,
            }),
        )
    }

    fn result(seq: u64, call_id: &str, status: ToolStatus) -> Event {
        Event::new(
            seq,
            "s",
            1,
            EventKind::ToolResult(ToolResult {
                call_id: call_id.to_string(),
                status,
                output: "done".to_string(),
                metadata: Default::default(),
            }),
        )
    }

    #[test]
    fn parses_items_and_defaults_status_to_pending() {
        let items = from_arguments(&json!({
            "items": [
                {"content": "read spec", "status": "completed"},
                {"content": "write tool"},
                {"content": " run tests ", "status": "in_progress"},
            ]
        }))
        .expect("parse");

        assert_eq!(items.len(), 3);
        assert_eq!(items[0].status, TodoStatus::Completed);
        assert_eq!(items[1].status, TodoStatus::Pending);
        assert_eq!(items[2].content, "run tests");
        assert_eq!(items[2].status, TodoStatus::InProgress);
    }

    #[test]
    fn rejects_malformed_lists_with_actionable_errors() {
        assert!(from_arguments(&json!({})).unwrap_err().contains("requires"));
        assert!(
            from_arguments(&json!({"items": "x"}))
                .unwrap_err()
                .contains("array")
        );
        assert!(
            from_arguments(&json!({"items": [{"content": "  "}]}))
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            from_arguments(&json!({"items": [{"content": "a\nb"}]}))
                .unwrap_err()
                .contains("control")
        );
        assert!(
            from_arguments(&json!({"items": [{"content": "x", "status": "done"}]}))
                .unwrap_err()
                .contains("unknown status")
        );
        assert!(
            from_arguments(&json!({"items": [
                {"content": "a", "status": "in_progress"},
                {"content": "b", "status": "in_progress"},
            ]}))
            .unwrap_err()
            .contains("at most one")
        );

        let too_many: Vec<Value> = (0..=MAX_ITEMS)
            .map(|index| json!({"content": format!("item {index}")}))
            .collect();
        assert!(
            from_arguments(&json!({"items": too_many}))
                .unwrap_err()
                .contains("limit")
        );

        let long = "x".repeat(MAX_CONTENT_CHARS + 1);
        assert!(
            from_arguments(&json!({"items": [{"content": long}]}))
                .unwrap_err()
                .contains("longer than")
        );
    }

    #[test]
    fn renders_progress_and_checklist() {
        let items = vec![
            TodoItem {
                content: "one".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "two".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "three".into(),
                status: TodoStatus::Pending,
            },
        ];
        assert_eq!(progress(&items), (1, 3));
        assert_eq!(
            render(&items),
            "Todo list (1/3 done):\n1. [x] one\n2. [~] two\n3. [ ] three"
        );
        assert!(render(&[]).contains("cleared"));
    }

    #[test]
    fn prompt_section_is_empty_without_items() {
        assert!(prompt_section(&[]).is_empty());

        let section = prompt_section(&[TodoItem {
            content: "ship it".into(),
            status: TodoStatus::InProgress,
        }]);
        assert!(section.starts_with("# Todo"));
        assert!(section.contains("0/1 done"));
        assert!(section.contains("1. [~] ship it"));
    }

    #[test]
    fn current_uses_the_latest_successful_write() {
        let events = vec![
            call(1, "c1", json!({"items": [{"content": "old"}]})),
            result(2, "c1", ToolStatus::Ok),
            call(
                3,
                "c2",
                json!({"items": [{"content": "new", "status": "completed"}]}),
            ),
            result(4, "c2", ToolStatus::Ok),
            // A denied call must not replace the list.
            call(5, "c3", json!({"items": [{"content": "denied"}]})),
            result(6, "c3", ToolStatus::Error),
        ];

        let items = current(&events).expect("state");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "new");
        assert_eq!(items[0].status, TodoStatus::Completed);
    }

    #[test]
    fn current_clears_on_an_empty_successful_write() {
        let events = vec![
            call(1, "c1", json!({"items": [{"content": "old"}]})),
            result(2, "c1", ToolStatus::Ok),
            call(3, "c2", json!({"items": []})),
            result(4, "c2", ToolStatus::Ok),
        ];
        assert!(current(&events).is_none());
    }

    #[test]
    fn ignores_tool_calls_from_other_tools() {
        let events = vec![Event::new(
            1,
            "s",
            1,
            EventKind::ToolCall(ToolCall {
                call_id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: json!({"path": "x"}),
            }),
        )];
        assert!(current(&events).is_none());
    }

    #[test]
    fn todo_state_is_derived_from_events_not_messages() {
        // A successful call followed by an assistant message: replay still
        // finds the list (no dedicated event type exists).
        let events = vec![
            call(1, "c1", json!({"items": [{"content": "keep"}]})),
            result(2, "c1", ToolStatus::Ok),
            Event::new(
                3,
                "s",
                1,
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: vec![ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    provider_raw: json!({}),
                    usage: Usage::default(),
                }),
            ),
        ];
        assert_eq!(current(&events).expect("state")[0].content, "keep");
    }
}
