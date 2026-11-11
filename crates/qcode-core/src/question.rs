//! Human-in-the-loop questions: the `ask_user` tool's counterpart of approval.
//!
//! Approval is the engine asking the user for *permission* (policy-driven, the
//! user is passive); a question is the model asking the user for *information*
//! (model-driven, the model chose to pause). Both run on the same bridge, but
//! with different payloads and answer channels — a "allow the tool" decision is
//! not an "pick an option" answer.
//!
//! Question state is not stored anywhere on its own: everything the model sees
//! is projected from the session event log (`ToolCall` arguments carry the
//! questions, `ToolResult` carries the answers or the pending marker, and a
//! `question_reply` user message carries a late answer).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// One question the model asks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// One selectable option. `label` is what the user sees *and* the value the
/// model receives back — never translated by a frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The user's answer to one question, keyed by the question's `id`.
///
/// An empty `selected` with no `custom` is a deliberate skip, not a denial and
/// not a mistake: the model must continue working and ask again only if it
/// really needs the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerItem {
    pub id: String,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

impl AnswerItem {
    pub fn is_skipped(&self) -> bool {
        self.selected.is_empty() && self.custom.is_none()
    }
}

/// Presentation intent. Only changes how a frontend renders the question; the
/// answer encoding is identical either way, so a frontend that does not
/// understand an intent falls back to a plain option list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionIntent {
    #[default]
    Confirm,
    /// A fixed approval-style card (deny / allow / always). Requires exactly
    /// one single-select question whose option labels name the three actions.
    Approval,
}

/// Everything one `ask_user` tool call needs to ask.
#[derive(Debug, Clone)]
pub struct QuestionRequest {
    /// The provider's `tool_call_id`; keys the whole lifecycle of the question.
    pub call_id: String,
    pub questions: Vec<QuestionItem>,
    pub intent: QuestionIntent,
    /// `Some(seconds)` = timed mode (`-1` = wait forever, semantics of
    /// blocking); `None` = blocking.
    pub timeout: Option<i64>,
}

/// How a question round ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionAnswer {
    /// The user answered (or skipped) every question within the window.
    Answered(Vec<AnswerItem>),
    /// The timed window elapsed without an answer. The question is not settled:
    /// the answer must arrive later as a `question_reply` user message.
    TimedOut,
    /// The run was cancelled while the question was on screen.
    Cancelled,
}

/// A late answer to a timed-out `ask_user` call, riding on a user message so
/// the running turn picks it up at the next step boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionReply {
    pub call_id: String,
    #[serde(default)]
    pub answers: Vec<AnswerItem>,
}

/// Why an `ask_user` call failed, mapped to a model-visible tool error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionError {
    /// The call had no questions.
    EmptyQuestions,
    /// `intent` violated its constraints (e.g. approval with multiple
    /// questions, a timeout, or options that do not name deny/allow/always).
    BadIntent,
    /// A submitted answer batch does not cover every question id exactly once.
    BadAnswer,
    /// `timeout` is not a valid window (`0` or negative other than `-1`).
    BadTimeout,
    /// No questioner is wired into this run; nobody can answer.
    NoQuestioner,
    /// The wait was aborted (run cancelled, engine torn down).
    AskAborted,
    /// The timed window elapsed. `pending`, not an error — kept separate from
    /// the timeout payload the model sees.
    AskTimedOut,
    /// An answer for this call is already queued; a second one is a duplicate.
    ReplyQueued,
}

/// The reserved option labels of the `approval` intent, named rather than
/// positional so a frontend maps them without trusting option order.
pub const APPROVAL_LABELS: [&str; 3] = ["deny", "allow", "always"];

/// Validate a request before it reaches a questioner. The same rules run in
/// the tool (so the model sees the failure) and in the engine.
pub fn validate_request(
    questions: &[QuestionItem],
    intent: QuestionIntent,
    timeout: Option<i64>,
) -> Result<(), QuestionError> {
    if questions.is_empty() {
        return Err(QuestionError::EmptyQuestions);
    }
    if let Some(seconds) = timeout
        && seconds != -1
        && seconds <= 0
    {
        return Err(QuestionError::BadTimeout);
    }
    let mut ids = std::collections::HashSet::new();
    if questions
        .iter()
        .any(|question| !ids.insert(question.id.clone()))
    {
        return Err(QuestionError::BadAnswer);
    }
    if intent == QuestionIntent::Approval {
        let mut labels = APPROVAL_LABELS.to_vec();
        labels.sort_unstable();
        let ok = questions.len() == 1
            && !questions[0].multi_select
            && timeout.is_none()
            && labels_of(&questions[0]) == labels;
        if !ok {
            return Err(QuestionError::BadIntent);
        }
    }
    Ok(())
}

fn labels_of(question: &QuestionItem) -> Vec<&str> {
    let mut labels: Vec<&str> = question
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    labels.sort_unstable();
    labels
}

/// An answer batch for one call, as the frontends validate it.
pub fn answer_covers(answers: &[AnswerItem], questions: &[QuestionItem]) -> bool {
    let ids: std::collections::HashSet<&str> = questions.iter().map(|q| q.id.as_str()).collect();
    answers.len() == ids.len()
        && answers
            .iter()
            .all(|answer| ids.contains(answer.id.as_str()))
}

/// The answerer service: who answers when the model pauses. Injected like the
/// `Approver`; `None` in `ToolContext` means nobody can answer here.
///
/// `Debug` is required so [`crate::tool::ToolContext`] (which derives it) can
/// hold an `Arc<dyn Questioner>`, exactly like `SchedulerHandle`.
#[async_trait]
pub trait Questioner: Send + Sync + std::fmt::Debug {
    async fn ask(&self, request: &QuestionRequest, cancel: CancellationToken) -> QuestionAnswer;
}

/// A blocking stdin answerer for headless runs, mirroring `InteractiveApprover`
/// (spawn_blocking + `tokio::select!`, so Ctrl-C still wins).
#[derive(Debug)]
pub struct InteractiveQuestioner;

#[async_trait]
impl Questioner for InteractiveQuestioner {
    async fn ask(&self, request: &QuestionRequest, cancel: CancellationToken) -> QuestionAnswer {
        let questions = request.questions.clone();
        let task = tokio::task::spawn_blocking(move || ask_on_stdin(&questions));
        let give_up: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match request.timeout {
                Some(seconds) if seconds > 0 => Box::pin(tokio::time::sleep(
                    std::time::Duration::from_secs(seconds as u64),
                )),
                _ => Box::pin(std::future::pending::<()>()),
            };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => QuestionAnswer::Cancelled,
            _ = give_up => QuestionAnswer::TimedOut,
            answers = task => {
                let answers = answers.unwrap_or_default();
                QuestionAnswer::Answered(answers)
            }
        }
    }
}

fn ask_on_stdin(questions: &[QuestionItem]) -> Vec<AnswerItem> {
    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        eprintln!();
        eprintln!("[ask] {question}", question = question.question);
        for (index, option) in question.options.iter().enumerate() {
            eprintln!("  {}. {}", index + 1, option.label);
        }
        eprint!("  > 选择编号（逗号分隔；留空=跳过；其他=自由回答） ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            answers.push(AnswerItem {
                id: question.id.clone(),
                selected: Vec::new(),
                custom: None,
            });
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            answers.push(AnswerItem {
                id: question.id.clone(),
                selected: Vec::new(),
                custom: None,
            });
            continue;
        }
        let indices: Vec<usize> = line
            .split(',')
            .filter_map(|part| part.trim().parse::<usize>().ok())
            .filter(|index| *index >= 1 && *index <= question.options.len())
            .collect();
        if !indices.is_empty() {
            answers.push(AnswerItem {
                id: question.id.clone(),
                selected: indices
                    .into_iter()
                    .map(|index| question.options[index - 1].label.clone())
                    .collect(),
                custom: None,
            });
        } else {
            answers.push(AnswerItem {
                id: question.id.clone(),
                selected: Vec::new(),
                custom: Some(line.to_string()),
            });
        }
    }
    answers
}

/// The default pending payload a timed-out `ask_user` call returns to the
/// model: a fixed instruction instead of an answer, so the projection and the
/// UI can tell "timed out" apart from "answered".
pub fn pending_result(call_id: &str) -> String {
    format!(
        r#"{{"pending":true,"call_id":"{call_id}","message":"等待超时。这不是跳过：问题仍可回答，之后的回答会以带 question_reply 标记的用户消息送达。"}}"#
    )
}

/// The model-visible text of a late answer: original questions plus answers, so
/// the model can pair them without external state.
pub fn reply_text(call_id: &str, questions: &[QuestionItem], answers: &[AnswerItem]) -> String {
    let body = serde_json::json!({
        "question_reply": {
            "call_id": call_id,
            "questions": questions,
            "answers": answers,
        }
    });
    serde_json::to_string(&body).expect("question reply serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str, options: &[&str]) -> QuestionItem {
        QuestionItem {
            id: id.to_string(),
            header: None,
            question: "pick one".to_string(),
            detail: None,
            options: options
                .iter()
                .map(|label| QuestionOption {
                    label: label.to_string(),
                    description: None,
                })
                .collect(),
            multi_select: false,
        }
    }

    #[test]
    fn empty_questions_are_rejected() {
        assert_eq!(
            validate_request(&[], QuestionIntent::Confirm, None),
            Err(QuestionError::EmptyQuestions)
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let questions = vec![question("q", &["a"]), question("q", &["b"])];
        assert_eq!(
            validate_request(&questions, QuestionIntent::Confirm, None),
            Err(QuestionError::BadAnswer)
        );
    }

    #[test]
    fn zero_timeout_is_rejected_but_minus_one_is_fine() {
        let one = vec![question("q", &["a"])];
        assert_eq!(
            validate_request(&one, QuestionIntent::Confirm, Some(0)),
            Err(QuestionError::BadTimeout)
        );
        assert!(validate_request(&one, QuestionIntent::Confirm, Some(-1)).is_ok());
    }

    #[test]
    fn approval_needs_one_single_select_question_with_reserved_labels() {
        let approval = vec![question("q", &["allow", "deny", "always"])];
        assert!(validate_request(&approval, QuestionIntent::Approval, None).is_ok());
        let with_timeout = validate_request(&approval, QuestionIntent::Approval, Some(30));
        assert_eq!(with_timeout, Err(QuestionError::BadIntent));
        let wrong_labels = vec![question("q", &["yes", "no"])];
        assert_eq!(
            validate_request(&wrong_labels, QuestionIntent::Approval, None),
            Err(QuestionError::BadIntent)
        );
        let multi = QuestionItem {
            multi_select: true,
            ..question("q", &["allow", "deny", "always"])
        };
        assert_eq!(
            validate_request(&[multi], QuestionIntent::Approval, None),
            Err(QuestionError::BadIntent)
        );
    }

    #[test]
    fn a_batch_that_covers_every_id_once_is_well_formed() {
        let questions = vec![question("q1", &["a"]), question("q2", &["b"])];
        let answers = vec![
            AnswerItem {
                id: "q1".to_string(),
                selected: vec!["a".to_string()],
                custom: None,
            },
            AnswerItem {
                id: "q2".to_string(),
                selected: Vec::new(),
                custom: None,
            },
        ];
        assert!(answer_covers(&answers, &questions));
        assert!(!answer_covers(&answers[..1], &questions));
    }

    #[test]
    fn pending_result_is_recognisable_by_key() {
        let text = pending_result("call_1");
        assert!(text.contains("\"pending\":true"));
        assert!(text.contains("call_1"));
        assert!(text.contains("question_reply"));
    }
}
