//! Where a whole-tree snapshot takes its root from.
//!
//! `run_shell` can write anywhere, so the engine copies the tree before running
//! it. The copy has to be keyed against the session's own root: that is the
//! directory `restore` writes back into (`qc sessions rollback` uses
//! `session.started.cwd`, the GUI its workspace, and both are the primary root in
//! practice).
//!
//! Using the *process* working directory instead looks equivalent and is not. A
//! frontend started in `$HOME` and then pointed at a checkout kept `cwd = ~`, so
//! every `run_shell` snapshotted the home directory: measured on one machine that
//! was 90.9% of 22.3 GB, and in 250 turns the 64 MB budget ran out before a
//! single file of the checkout was copied — spaces spent, nothing recoverable.
//! These two tests are the two directions of that mistake.

use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent, ToolSpec};
use qcode_core::snapshot::SnapshotStore;
use qcode_core::store::SessionStore;
use qcode_core::task::{Root, RootAccess};
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use qcode_providers::MockProvider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Named `run_shell` so the engine takes its snapshot branch; it does not have to
/// be the real tool for that, and not shelling out keeps the test hermetic.
struct FakeShell;

#[async_trait]
impl Tool for FakeShell {
    fn name(&self) -> &str {
        "run_shell"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".to_string(),
            description: "does nothing".to_string(),
            parameters: json!({"type": "object"}),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("ran"))
    }
}

fn meta(cwd: &str) -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: cwd.to_string(),
        version: "0.1.0".to_string(),
        parent: None,
    }
}

fn shell_response() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::ToolCallDone {
            call_id: "call-1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "true"}),
        },
        StreamEvent::Ended {
            stop_reason: StopReason::ToolUse,
        },
    ]
}

fn text_response(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Started,
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

async fn run_one_shell(
    dir: &tempfile::TempDir,
    cwd: &std::path::Path,
    roots: Vec<Root>,
) -> (String, SnapshotStore) {
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta(&cwd.to_string_lossy())).unwrap();
    store.replace_roots(&id, roots).unwrap();

    let snapshots = SnapshotStore::at_data_dir(store.root()).with_shell(true);
    let provider = MockProvider::scripted(vec![shell_response(), text_response("done")]);
    let tools = ToolRegistry::new(vec![Arc::new(FakeShell)]);
    let policy = Policy::default();

    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &AutoApprove,
            snapshots: Some(&snapshots),
            scheduler: None,
            driver: qcode_core::scheduler::DriverStatus::Unknown,
            questioner: None,
        },
        RunOptions {
            session_id: &id,
            model: "mock-1",
            input: "run it".to_string(),
            cwd: cwd.to_path_buf(),
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
            continuation: None,
            scheduled: None,
        },
        |_| {},
    )
    .await
    .expect("turn");

    (id, snapshots)
}

fn root(path: &std::path::Path) -> Root {
    Root {
        id: "checkout".to_string(),
        label: "checkout".to_string(),
        path: path.to_path_buf(),
        access: RootAccess::ReadWrite,
        primary: true,
    }
}

#[tokio::test]
async fn the_tree_snapshot_follows_the_session_root_not_the_process_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(checkout.join("src/lib.rs"), "the code").unwrap();
    std::fs::write(checkout.join("README.md"), "the readme").unwrap();
    // Only the process directory has this file. If the snapshot walks `cwd`
    // instead of the root it will show up, which is the bug.
    std::fs::write(elsewhere.join("decoy.txt"), "not the workspace").unwrap();

    let (id, snapshots) = run_one_shell(&dir, &elsewhere, vec![root(&checkout)]).await;

    let recorded = snapshots.files(&id, 1).expect("files");
    assert!(
        recorded.contains(&"src/lib.rs".to_string()),
        "the checkout must be snapshotted: {recorded:?}"
    );
    assert!(
        recorded.contains(&"README.md".to_string()),
        "the checkout must be snapshotted: {recorded:?}"
    );
    assert!(
        !recorded.contains(&"decoy.txt".to_string()),
        "the process cwd must not be walked when the session has a root: {recorded:?}"
    );
}

#[tokio::test]
async fn a_session_without_roots_falls_back_to_the_process_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("only.txt"), "content").unwrap();

    let (id, snapshots) = run_one_shell(&dir, &cwd, Vec::new()).await;

    assert_eq!(
        snapshots.files(&id, 1).expect("files"),
        vec!["only.txt".to_string()],
        "with no roots to prefer, the startup directory is still the workspace"
    );
}
