//! `todo_write`: the session todo list.
//!
//! The tool has no filesystem side effects. It validates the model's list and
//! echoes a checklist back; the engine keeps the authoritative copy in memory
//! and the event log is the replay source (see `qcode_core::todo`).

use async_trait::async_trait;
use qcode_core::provider::ToolSpec;
use qcode_core::todo;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Replace the session todo list. Send the complete list every time; it is not ",
                "appended to. Use one item per concrete step, mark exactly one item ",
                "in_progress while working on it, and completed as soon as it is done. ",
                "An empty items array clears the list. Todos are session state only and are ",
                "never written to the filesystem."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "The complete list, in order.",
                        "maxItems": todo::MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "One line, imperative, concrete."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Defaults to pending."
                                }
                            },
                            "required": ["content"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // Session memory only: no file, process or network side effects.
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        match todo::from_arguments(&args) {
            Ok(items) => Ok(ToolOutput::ok(todo::render(&items))),
            // The model can fix this by re-sending the list, so it is a tool
            // result rather than an engine error.
            Err(message) => Ok(ToolOutput::error(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use qcode_core::event::ToolStatus;
    use qcode_core::todo::{TodoItem, TodoStatus};
    use serde_json::json;

    use super::*;

    fn context() -> ToolContext {
        ToolContext::new(".")
    }

    #[tokio::test]
    async fn writes_and_renders_the_list() {
        let output = TodoWrite
            .execute(
                &context(),
                json!({"items": [
                    {"content": "read spec", "status": "completed"},
                    {"content": "build tool", "status": "in_progress"},
                ]}),
            )
            .await
            .expect("execute");

        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("Todo list (1/2 done)"));
        assert!(output.output.contains("[x] read spec"));
        assert!(output.output.contains("[~] build tool"));
    }

    #[tokio::test]
    async fn invalid_lists_are_tool_errors() {
        let output = TodoWrite
            .execute(&context(), json!({"items": [{"content": ""}]}))
            .await
            .expect("execute");

        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("empty"));
    }

    #[tokio::test]
    async fn an_empty_list_clears_the_state() {
        let output = TodoWrite
            .execute(&context(), json!({"items": []}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Ok);
        assert!(output.output.contains("cleared"));
    }

    #[test]
    fn spec_is_static_and_names_the_status_enum() {
        let spec = TodoWrite.spec();
        assert_eq!(spec.name, "todo_write");
        assert_eq!(
            spec.parameters["properties"]["items"]["items"]["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
    }

    #[test]
    fn todo_items_round_trip_through_json() {
        let items = vec![TodoItem {
            content: "x".to_string(),
            status: TodoStatus::Completed,
        }];
        let value = serde_json::to_value(&items).expect("serialize");
        assert_eq!(value[0]["status"], "completed");
    }
}
