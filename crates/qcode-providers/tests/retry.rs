//! Provider retry policy (SPEC §10) and parallel-tool capability gating.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::policy::Policy;
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Provider, ProviderError, StopReason, StreamEvent,
    ToolSpec,
};
use qcode_core::store::SessionStore;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

enum Flaky {
    /// First call hits a rate limit, the second succeeds.
    RateLimitThenOk,
    /// Emits one token, then the stream dies.
    FailAfterToken,
}

struct FlakyProvider {
    attempts: Arc<AtomicUsize>,
    mode: Flaky,
}

#[async_trait]
impl Provider for FlakyProvider {
    fn name(&self) -> &str {
        "flaky"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: true,
            ..Capabilities::default()
        }
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        match self.mode {
            Flaky::RateLimitThenOk if attempt == 1 => Err(ProviderError::RateLimit {
                retry_after_secs: Some(0),
                detail: String::new(),
            }),
            Flaky::RateLimitThenOk => Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::Started),
                Ok(StreamEvent::TextDelta("recovered".to_string())),
                Ok(StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                }),
            ]))),
            Flaky::FailAfterToken => Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::Started),
                Ok(StreamEvent::TextDelta("partial".to_string())),
                Err(ProviderError::Network("connection reset".to_string())),
            ]))),
        }
    }
}

fn options<'a>(session: &'a str, workspace: &std::path::Path) -> RunOptions<'a> {
    RunOptions {
        session_id: session,
        model: "mock-1",
        input: "go".to_string(),
        cwd: workspace.to_path_buf(),
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
        inbox: None,
    }
}

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "flaky".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

#[tokio::test]
async fn retries_before_the_first_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create");
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FlakyProvider {
        attempts: attempts.clone(),
        mode: Flaky::RateLimitThenOk,
    };
    let tools = ToolRegistry::default();
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
        options(&id, dir.path()),
        |_| {},
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.text, "recovered");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn does_not_retry_after_the_first_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create");
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FlakyProvider {
        attempts: attempts.clone(),
        mode: Flaky::FailAfterToken,
    };
    let tools = ToolRegistry::default();
    let policy = Policy::default();

    let result = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        options(&id, dir.path()),
        |_| {},
    )
    .await;

    assert!(result.is_err(), "a post-token failure must end the turn");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// A read tool that takes long enough to reveal serialization.
struct SlowRead;

#[async_trait]
impl Tool for SlowRead {
    fn name(&self) -> &str {
        "slow_read"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "sleep".to_string(),
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
        tokio::time::sleep(Duration::from_millis(120)).await;
        Ok(ToolOutput::ok("slept"))
    }
}

#[tokio::test]
async fn parallel_reads_require_provider_capability() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path());
    let (id, _) = store.create(&meta()).expect("create");
    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "c1".to_string(),
                name: "slow_read".to_string(),
                arguments: json!({}),
            },
            StreamEvent::ToolCallDone {
                call_id: "c2".to_string(),
                name: "slow_read".to_string(),
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
    ])
    .with_capabilities(Capabilities {
        tools: true,
        parallel_tools: false,
        ..Capabilities::default()
    });
    let tools = ToolRegistry::new(vec![Arc::new(SlowRead)]);
    let policy = Policy::default();

    let started = Instant::now();
    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: None,
        },
        options(&id, dir.path()),
        |_| {},
    )
    .await
    .expect("run turn");
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(220),
        "reads ran in parallel despite parallel_tools=false: {elapsed:?}"
    );

    let events = store.load(&id).expect("load");
    let results = events
        .iter()
        .filter(|event| matches!(event.kind, qcode_core::event::EventKind::ToolResult(_)))
        .count();
    assert_eq!(results, 2);
}
