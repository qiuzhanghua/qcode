//! The engine folds tool-reported usage/cost (sub-agents) into the parent turn.

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::cost::Pricing;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{SessionStarted, Usage};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct UsageTool;

#[async_trait]
impl Tool for UsageTool {
    fn name(&self) -> &str {
        "fake_child"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "reports child usage".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _args: Value,
    ) -> Result<ToolOutput, qcode_core::tool::ToolError> {
        let mut output = ToolOutput::ok("child result");
        output.usage = Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            ..Usage::default()
        };
        output.cost_usd = 0.5;
        Ok(output)
    }
}

#[tokio::test]
async fn tool_usage_and_cost_reach_the_parent_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store
        .create(&SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: dir.path().display().to_string(),
            version: "0.1.0".to_string(),
        })
        .expect("create session");

    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "c1".to_string(),
                name: "fake_child".to_string(),
                arguments: json!({}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Usage(Usage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                ..Usage::default()
            }),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);
    let tools = ToolRegistry::new(vec![std::sync::Arc::new(UsageTool)]);
    let policy = Policy::default();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "go".to_string(),
            workspace: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: Some(Pricing {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 1.0,
                ..Pricing::default()
            }),
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: None,
            network: None,
            attachments: Vec::new(),
        },
        |_| {},
    )
    .await
    .expect("run turn");

    // 7 child input + 12 parent input; 3 child output + 4 parent output.
    assert_eq!(outcome.usage.input_tokens, Some(19));
    assert_eq!(outcome.usage.output_tokens, Some(7));
    // 0.5 from the tool plus the parent's own provider cost.
    assert!(outcome.cost_usd >= 0.5, "{}", outcome.cost_usd);

    // The persisted turn_ended event must agree with the outcome.
    let events = store.load(&id).expect("load");
    let ended = events.iter().find_map(|event| match &event.kind {
        qcode_core::event::EventKind::TurnEnded(ended) => Some(ended),
        _ => None,
    });
    let ended = ended.expect("turn_ended");
    assert_eq!(ended.usage.input_tokens, Some(19));
    assert!(ended.cost_usd >= 0.5);
    assert_eq!(ended.status, qcode_core::event::TurnStatus::Completed);
}
