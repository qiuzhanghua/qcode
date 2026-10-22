//! `subagent`: delegate a self-contained task to a nested turn loop.
//!
//! The tool itself only validates arguments and formats the result. The engine
//! supplies a [`SubagentRunner`] (see `qcode::subagent`) that owns the child
//! session, provider and tool registry. The child registry intentionally does
//! not contain `subagent`, so nesting is one level deep.

use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::event::{ToolMetadata, ToolStatus, Usage};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Child turn budget when the model does not pass `max_turns`.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Hard cap on the child turn budget.
pub const MAX_TURNS: u32 = 40;

const OUTPUT_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    pub task: String,
    pub max_turns: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubagentOutcome {
    pub text: String,
    pub session_id: String,
    pub turns: u32,
    pub cancelled: bool,
    /// Tokens spent by the child turn; the engine folds this into the parent.
    pub usage: Usage,
    /// Child USD estimate under the parent's pricing.
    pub cost_usd: f64,
}

/// Runs a child agent. Implementations own the child session and inherit the
/// parent's provider, policy and approver.
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(
        &self,
        request: SubagentRequest,
        cancel: CancellationToken,
    ) -> Result<SubagentOutcome, String>;
}

pub struct SubagentTool {
    runner: Arc<dyn SubagentRunner>,
}

impl SubagentTool {
    pub fn new(runner: Arc<dyn SubagentRunner>) -> Self {
        Self { runner }
    }
}

#[derive(Deserialize)]
struct Args {
    task: String,
    max_turns: Option<u32>,
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Delegate a self-contained task to a fresh sub-agent and return its final answer. ",
                "The sub-agent works in the same workspace, uses the same provider and approval ",
                "policy, and has its own session log; it cannot spawn another sub-agent. ",
                "Use it for independent research or a focused change that would otherwise flood ",
                "the main conversation with tool output."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Complete instructions; the sub-agent does not see this conversation."
                    },
                    "max_turns": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TURNS,
                        "description": "Child turn budget (default 20)."
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // The child's own tool calls are gated individually; this class keeps
        // the sub-agent call itself serial so approvals cannot interleave.
        ApprovalClass::External
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: Args =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;
        let task = args.task.trim();
        if task.is_empty() {
            return Err(ToolError::Invalid("task is empty".to_string()));
        }
        let max_turns = args
            .max_turns
            .unwrap_or(DEFAULT_MAX_TURNS)
            .clamp(1, MAX_TURNS);

        let outcome = match self
            .runner
            .run(
                SubagentRequest {
                    task: task.to_string(),
                    max_turns,
                },
                ctx.cancel.clone(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(message) => return Ok(ToolOutput::error(format!("subagent failed: {message}"))),
        };

        let body = outcome.text.trim();
        let body = if body.is_empty() {
            "(subagent finished without a final message)"
        } else {
            body
        };
        let raw = format!(
            "subagent session {} ({} turn(s)):\n{body}",
            outcome.session_id, outcome.turns
        );
        let (output, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);
        let status = if outcome.cancelled {
            ToolStatus::Cancelled
        } else {
            ToolStatus::Ok
        };

        Ok(ToolOutput {
            status,
            output,
            metadata: ToolMetadata {
                truncated,
                bytes: raw.len() as u64,
                ..ToolMetadata::default()
            },
            usage: outcome.usage,
            cost_usd: outcome.cost_usd,
            full_output: truncated.then(|| raw.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeRunner {
        seen: Mutex<Option<(SubagentRequest, CancellationToken)>>,
        fail: bool,
    }

    #[async_trait]
    impl SubagentRunner for FakeRunner {
        async fn run(
            &self,
            request: SubagentRequest,
            cancel: CancellationToken,
        ) -> Result<SubagentOutcome, String> {
            *self.seen.lock().expect("seen lock") = Some((request, cancel));
            if self.fail {
                return Err("provider exploded".to_string());
            }
            Ok(SubagentOutcome {
                text: "child answer".to_string(),
                session_id: "child-1".to_string(),
                turns: 3,
                cancelled: false,
                usage: Usage {
                    input_tokens: Some(11),
                    output_tokens: Some(5),
                    ..Usage::default()
                },
                cost_usd: 0.25,
            })
        }
    }

    fn context() -> ToolContext {
        ToolContext::for_test(".")
    }

    #[tokio::test]
    async fn delegates_and_formats_the_result() {
        let runner = Arc::new(FakeRunner::default());
        let tool = SubagentTool::new(runner.clone());

        let output = tool
            .execute(&context(), json!({"task": "  find the bug  "}))
            .await
            .expect("execute");

        assert_eq!(output.status, ToolStatus::Ok);
        assert!(
            output
                .output
                .contains("subagent session child-1 (3 turn(s))")
        );
        assert!(output.output.contains("child answer"));
        assert_eq!(output.usage.input_tokens, Some(11));
        assert_eq!(output.usage.output_tokens, Some(5));
        assert!((output.cost_usd - 0.25).abs() < f64::EPSILON);

        let seen = runner.seen.lock().expect("seen lock");
        let (request, _) = seen.as_ref().expect("runner called");
        assert_eq!(request.task, "find the bug");
        assert_eq!(request.max_turns, DEFAULT_MAX_TURNS);
    }

    #[tokio::test]
    async fn rejects_empty_tasks() {
        let tool = SubagentTool::new(Arc::new(FakeRunner::default()));
        let error = tool
            .execute(&context(), json!({"task": "   "}))
            .await
            .expect_err("empty task");
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[tokio::test]
    async fn runner_failures_become_tool_errors() {
        let tool = SubagentTool::new(Arc::new(FakeRunner {
            fail: true,
            ..FakeRunner::default()
        }));
        let output = tool
            .execute(&context(), json!({"task": "x"}))
            .await
            .expect("execute");
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("provider exploded"));
    }

    #[test]
    fn max_turns_is_clamped() {
        let runner = Arc::new(FakeRunner::default());
        let tool = SubagentTool::new(runner.clone());
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let _ = tool
                .execute(&context(), json!({"task": "x", "max_turns": 999}))
                .await
                .expect("execute");
        });
        let seen = runner.seen.lock().expect("seen lock");
        assert_eq!(seen.as_ref().expect("called").0.max_turns, MAX_TURNS);
    }
}
