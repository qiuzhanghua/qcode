//! Engine-backed sub-agent runner.
//!
//! A `subagent` tool call creates a fresh session and runs a nested turn loop
//! with the same provider, policy and approver. The child tool registry is the
//! parent's registry minus `subagent`, so nesting stops at depth one. Child
//! events go to their own JSONL file; the parent gets only the final answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::Approver;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::policy::Policy;
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_tools::subagent::{SubagentOutcome, SubagentRequest, SubagentRunner};
use tokio_util::sync::CancellationToken;

pub struct EngineSubagent {
    store: SessionStore,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    policy: Policy,
    approver: Arc<dyn Approver>,
    workspace: PathBuf,
    model: String,
    context_window: Option<u64>,
}

/// Build the runner used by the `subagent` tool. The caller extends the parent
/// registry with `SubagentTool::new(runner)`; `tools` here is the *child*
/// registry (no subagent tool).
#[allow(clippy::too_many_arguments)]
pub fn build(
    store: &SessionStore,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    policy: &Policy,
    approver: Arc<dyn Approver>,
    workspace: &Path,
    model: &str,
    context_window: Option<u64>,
) -> Arc<dyn SubagentRunner> {
    Arc::new(EngineSubagent {
        store: store.clone(),
        provider,
        tools,
        policy: policy.clone(),
        approver,
        workspace: workspace.to_path_buf(),
        model: model.to_string(),
        context_window,
    })
}

#[async_trait]
impl SubagentRunner for EngineSubagent {
    async fn run(
        &self,
        request: SubagentRequest,
        cancel: CancellationToken,
    ) -> Result<SubagentOutcome, String> {
        let (session_id, _) = self
            .store
            .create(&SessionStarted {
                model: self.model.clone(),
                provider: self.provider.name().to_string(),
                cwd: self.workspace.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
            .map_err(|err| format!("cannot create subagent session: {err}"))?;

        let options = RunOptions {
            session_id: &session_id,
            model: &self.model,
            input: request.task,
            workspace: self.workspace.clone(),
            max_turns: request.max_turns,
            cancel,
            // Child cost is not folded into the parent turn; the child session
            // records its own usage.
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: self.context_window,
            // Isolation: the child gets neither AGENTS.md nor the skills index.
            agents_md: None,
            skills: None,
        };

        let outcome = run_turn(
            TurnContext {
                store: &self.store,
                provider: self.provider.as_ref(),
                tools: &self.tools,
                policy: &self.policy,
                approver: self.approver.as_ref(),
            },
            options,
            |_| {},
        )
        .await
        .map_err(|err| err.to_string())?;

        Ok(SubagentOutcome {
            text: outcome.text,
            session_id,
            turns: outcome.turns,
            cancelled: outcome.cancelled,
        })
    }
}

#[cfg(test)]
mod tests {
    use qcode_core::approval::AutoApprove;
    use qcode_core::event::EventKind;
    use qcode_providers::MockProvider;

    use super::*;

    #[tokio::test]
    async fn runs_a_child_turn_in_its_own_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let workspace = tempfile::tempdir().expect("workspace");
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::text("child answer"));
        let tools = ToolRegistry::default();
        let policy = Policy::default();

        let runner = build(
            &store,
            provider,
            tools,
            &policy,
            Arc::new(AutoApprove),
            workspace.path(),
            "mock-1",
            Some(128_000),
        );

        let outcome = runner
            .run(
                SubagentRequest {
                    task: "investigate the bug".to_string(),
                    max_turns: 4,
                },
                CancellationToken::new(),
            )
            .await
            .expect("subagent run");

        assert_eq!(outcome.text, "child answer");
        assert_eq!(outcome.turns, 1);
        assert!(!outcome.cancelled);

        let events = store.load(&outcome.session_id).expect("child session");
        assert!(matches!(events[0].kind, EventKind::SessionStarted(_)));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::UserMessage(message) if message.text == "investigate the bug"
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantMessage(message)
                if message.blocks.iter().any(|block| matches!(
                    block,
                    qcode_core::event::ContentBlock::Text { text } if text == "child answer"
                ))
        )));
    }
}
