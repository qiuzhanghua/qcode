//! Truncated tool output is persisted under sessions/<id>/outputs/.

use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolMetadata};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct BigTool;

#[async_trait]
impl Tool for BigTool {
    fn name(&self) -> &str {
        "big"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "big output".to_string(),
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
        Ok(ToolOutput::ok("truncated view")
            .with_metadata(ToolMetadata {
                truncated: true,
                bytes: 42,
                ..ToolMetadata::default()
            })
            .with_full_output("the complete output", true))
    }
}

#[tokio::test]
async fn truncated_output_is_written_to_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store
        .create(&SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        })
        .expect("create");

    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "call/bad id".to_string(),
                name: "big".to_string(),
                arguments: json!({}),
            },
            StreamEvent::Ended {
                stop_reason: StopReason::ToolUse,
            },
        ],
        vec![
            StreamEvent::Started,
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Ended {
                stop_reason: StopReason::EndTurn,
            },
        ],
    ]);
    let tools = ToolRegistry::new(vec![Arc::new(BigTool)]);
    let policy = Policy::default();

    run_turn(
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
            cwd: dir.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: None,
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

    let events = store.load(&id).expect("load");
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert!(result.metadata.truncated);
    let path = result
        .metadata
        .full_output_path
        .as_deref()
        .expect("full output path");
    assert!(path.contains("outputs"), "{path}");
    assert!(
        !path.contains("call/bad"),
        "call id should be sanitised: {path}"
    );
    assert_eq!(
        std::fs::read_to_string(path).expect("read output"),
        "the complete output"
    );
}
