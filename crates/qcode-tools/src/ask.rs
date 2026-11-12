//! `ask_user`: let the model pause and ask the user a question.
//!
//! The tool has no filesystem or network side effects: it only waits on the
//! `Questioner` injected into `ToolContext` (the frontend's answer panel or the
//! headless stdin reader). The whole lifecycle is keyed by the provider's
//! `call_id`; nothing is stored here.
//!
//! - no `timeout` → blocking: wait until the user answers or the run is
//!   cancelled.
//! - `timeout > 0` → timed: return `{pending:true,..}` when the window elapses.
//!   The question stays answerable; a late answer arrives as a `question_reply`
//!   user message.
//! - `timeout == -1` → wait forever, but still keyed as a timed call so a UI
//!   can reopen the card per call.

use async_trait::async_trait;
use qcode_core::provider::ToolSpec;
use qcode_core::question::{
    QuestionAnswer, QuestionIntent, QuestionItem, QuestionRequest, pending_result, validate_request,
};
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct AskUser;

/// The `ask_user` tool's name, for sub-agent registry exclusion: a nested
/// agent must not ask the user (interaction belongs to the runtime root); it
/// reports its unanswered question to the parent model instead.
pub const ASK_USER_TOOL_NAMES: [&str; 1] = ["ask_user"];

#[derive(Debug, Deserialize)]
struct Params {
    questions: Vec<QuestionItem>,
    #[serde(default)]
    intent: QuestionIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout: Option<i64>,
}

/// The error prefix model-visible text carries; kept in sync with
/// `QuestionError` so a caller can tell validation failures apart.
fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::Invalid(format!("ask_user: {}", message.into()))
}

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Pause and ask the user a question. Use it only when the answer is missing ",
                "and you genuinely cannot proceed without it; do not ask for permission to ",
                "use tools — that is handled by the approval system. Each question has a ",
                "stable `id`, an optional `header`, the `question` text, optional markdown ",
                "`detail`, and `options` (each: `label`, which is both the visible text and ",
                "the value returned, plus optional `description`). Put your recommended ",
                "option first and append `(Recommended)` to its label. An answer with empty ",
                "`selected` and no `custom` means the user skipped the question: continue ",
                "working, do not ask again, and do not treat a skip as permission. ",
                "`timeout` (seconds) limits the wait; when it elapses the result is ",
                "`{pending:true,..}` and the user may still answer later."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "One or more questions. Ids must be unique.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "header": {
                                    "type": "string",
                                    "description": "Optional short eyebrow above the question."
                                },
                                "question": { "type": "string" },
                                "detail": {
                                    "type": "string",
                                    "description": "Optional markdown shown below the question."
                                },
                                "options": {
                                    "type": "array",
                                    "description": "Empty means the user answers freely.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "Visible text, returned as the selected value."
                                            },
                                            "description": { "type": "string" }
                                        },
                                        "required": ["label"],
                                        "additionalProperties": false
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "Allow several options. Defaults to false."
                                }
                            },
                            "required": ["id", "question"],
                            "additionalProperties": false
                        }
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Seconds to wait. Omit for an open-ended question; -1 waits forever."
                    },
                    "intent": {
                        "type": "string",
                        "enum": ["confirm", "approval"],
                        "description": "Presentation hint only. `approval` needs one single-select question whose option labels are exactly `deny`, `allow`, `always`."
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // The question *is* the interaction; the policy always allows it.
        ApprovalClass::Question
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let params: Params = serde_json::from_value(args)
            .map_err(|error| invalid(format!("bad arguments: {error}")))?;
        if let Err(reason) = validate_request(&params.questions, params.intent, params.timeout) {
            return Err(invalid(format!("{reason:?}")));
        }
        let request = QuestionRequest {
            call_id: ctx.call_id.clone(),
            questions: params.questions,
            intent: params.intent,
            timeout: params.timeout,
        };
        let Some(questioner) = &ctx.questioner else {
            // Nobody wired into this run. A blocking question cannot wait; a
            // timed one can at least expire with a pending result.
            return match params.timeout {
                Some(seconds) if seconds > 0 => {
                    tokio::time::sleep(std::time::Duration::from_secs(seconds as u64)).await;
                    Ok(ToolOutput::ok(pending_result(&request.call_id)))
                }
                _ => Err(invalid(
                    "no interactive frontend is wired in; nobody can answer",
                )),
            };
        };
        let answer = questioner.ask(&request, ctx.cancel.clone()).await;
        match answer {
            QuestionAnswer::Answered(answers) => {
                Ok(ToolOutput::ok(json!({ "answers": answers }).to_string()))
            }
            QuestionAnswer::TimedOut => Ok(ToolOutput::ok(pending_result(&request.call_id))),
            QuestionAnswer::Cancelled => Ok(ToolOutput::cancelled("cancelled by user")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use qcode_core::event::ToolStatus;
    use qcode_core::question::{AnswerItem, Questioner};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// A questioner that returns a fixed batch, for engine-less tool tests.
    #[derive(Debug)]
    struct Fixed(QuestionAnswer);

    #[async_trait]
    impl Questioner for Fixed {
        async fn ask(
            &self,
            _request: &QuestionRequest,
            _cancel: CancellationToken,
        ) -> QuestionAnswer {
            self.0.clone()
        }
    }

    fn context(questioner: Option<Arc<dyn Questioner>>) -> ToolContext {
        ToolContext::for_test(".")
            .with_cancel(CancellationToken::new())
            .with_questioner(questioner)
    }

    fn params() -> Value {
        json!({
            "questions": [{
                "id": "q1",
                "question": "pick one",
                "options": [{ "label": "A" }, { "label": "B" }]
            }]
        })
    }

    #[tokio::test]
    async fn answers_render_as_a_tool_result() {
        let ctx = context(Some(Arc::new(Fixed(QuestionAnswer::Answered(vec![
            AnswerItem {
                id: "q1".to_string(),
                selected: vec!["A".to_string()],
                custom: None,
            },
        ])))));
        let out = AskUser.execute(&ctx, params()).await.expect("tool result");
        assert_eq!(out.status, ToolStatus::Ok);
        assert!(out.output.contains("\"A\""));
        assert!(!out.output.contains("pending"));
    }

    #[tokio::test]
    async fn no_questioner_fails_blocking_calls() {
        let ctx = context(None);
        let err = AskUser
            .execute(&ctx, params())
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("no interactive frontend"));
    }

    #[tokio::test]
    async fn a_timed_call_without_a_questioner_expires_to_pending() {
        let ctx = context(None);
        let args = json!({
            "questions": [{ "id": "q1", "question": "pick one" }],
            "timeout": 1
        });
        let out = AskUser.execute(&ctx, args).await.expect("pending result");
        assert!(out.output.contains("\"pending\":true"));
    }

    #[tokio::test]
    async fn a_timed_out_answer_is_pending_not_an_answer() {
        let ctx = context(Some(Arc::new(Fixed(QuestionAnswer::TimedOut))));
        let args = json!({
            "questions": [{ "id": "q1", "question": "pick one" }],
            "timeout": 5
        });
        let out = AskUser.execute(&ctx, args).await.expect("pending result");
        assert!(out.output.contains("\"pending\":true"));
    }

    #[tokio::test]
    async fn bad_arguments_fail_with_a_model_visible_message() {
        let ctx = context(None);
        let args = json!({ "questions": [] });
        let err = AskUser.execute(&ctx, args).await.expect_err("must fail");
        assert!(err.to_string().contains("EmptyQuestions"));
    }
}
