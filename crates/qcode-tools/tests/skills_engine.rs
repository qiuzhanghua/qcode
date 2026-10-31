//! One engine turn that activates a skill through the `skill` tool.

use std::sync::Arc;

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{EventKind, SessionStarted, ToolStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use qcode_tools::skills::{SKILL_FILE, SkillSet, SkillSource, SkillTool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn skill_is_listed_then_loaded_by_the_tool() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    let skill_dir = workspace.path().join(".qcode/skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join(SKILL_FILE),
        "---\nname: demo\ndescription: Demo skill for the test\n---\n\nFollow the demo steps.\n",
    )
    .unwrap();

    let skills = Arc::new(SkillSet::discover(&[(
        SkillSource::Project,
        workspace.path().join(".qcode/skills"),
    )]));
    let prompt = skills.prompt_section().expect("skill index");
    assert!(prompt.contains("`demo` (project)"));

    let mut registry = ToolRegistry::new(Vec::new());
    registry.extend([Arc::new(SkillTool::new(skills)) as Arc<dyn qcode_core::tool::Tool>]);

    let store = SessionStore::new(data.path().join("data"));
    let (session, _) = store
        .create(&SessionStarted {
            model: "mock".to_string(),
            provider: "mock".to_string(),
            cwd: workspace.path().display().to_string(),
            version: "0.1.0".to_string(),
        })
        .unwrap();

    let provider = MockProvider::scripted(vec![
        vec![
            StreamEvent::Started,
            StreamEvent::ToolCallDone {
                call_id: "skill-1".to_string(),
                name: "skill".to_string(),
                arguments: json!({"name": "demo"}),
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

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &registry,
            policy: &Policy::default(),
            approver: &AutoApprove,
            snapshots: None,
        },
        RunOptions {
            session_id: &session,
            model: "mock",
            input: "use the demo skill".to_string(),
            cwd: workspace.path().to_path_buf(),
            max_turns: 4,
            cancel: CancellationToken::new(),
            pricing: None,
            max_cost_usd: None,
            on_delta: None,
            context_window: None,
            agents_md: None,
            skills: Some(prompt),
            network: None,
            attachments: Vec::new(),
            inbox: None,
        },
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.text, "done");

    let events = store.load(&session).unwrap();
    let result = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult(result) if result.call_id == "skill-1" => Some(result),
            _ => None,
        })
        .expect("tool result");
    assert_eq!(result.status, ToolStatus::Ok);
    assert!(result.output.starts_with("# Skill: demo"));
    assert!(result.output.contains("Follow the demo steps."));
}
