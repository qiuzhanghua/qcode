//! ADR-0108 P1 C4: a skill index that changes between turns reaches the model.
//!
//! The unit tests cover the index itself and the TTL refresher; this one covers
//! the seam the frontends sit on: they read `prompt_section()` when a turn is
//! built, so a turn after a refresh must send a different system prompt. Two
//! turns, one provider that records what it was asked.

use std::path::Path;
use std::sync::Arc;

use qcode_core::approval::AutoApprove;
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::policy::Policy;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use qcode_tools::skills::{Skill, SkillDir, SkillIndex, SkillSet, SkillSource};
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
        parent: None,
    }
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

/// A served skill, in the shape the index stores one: no body, no entry, so the
/// prompt section is all this test looks at.
fn served(name: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: format!("The {name} skill."),
        source: SkillSource::Mcp("github".to_string()),
        dir: SkillDir::Remote {
            uri: format!("skill://{name}/SKILL.md"),
        },
        body: String::new(),
        files: Vec::new(),
        files_truncated: false,
        entry: None,
    }
}

fn options<'a>(
    session: &'a str,
    input: &str,
    skills: Option<String>,
    cwd: &Path,
) -> RunOptions<'a> {
    RunOptions {
        session_id: session,
        model: "mock-1",
        input: input.to_string(),
        cwd: cwd.to_path_buf(),
        max_turns: 2,
        cancel: CancellationToken::new(),
        pricing: None,
        max_cost_usd: None,
        on_delta: None,
        context_window: None,
        agents_md: None,
        skills,
        network: None,
        attachments: Vec::new(),
        inbox: None,
        continuation: None,
        scheduled: None,
    }
}

#[tokio::test]
async fn a_refreshed_index_reaches_the_next_turns_system_prompt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).expect("session");

    let index = Arc::new(SkillIndex::new(
        SkillSet::default().with_served(vec![served("alpha")]),
    ));
    let provider = MockProvider::scripted(vec![text_response("first"), text_response("second")]);
    let tools = ToolRegistry::new(Vec::new());
    let policy = Policy::default();
    let cwd = dir.path().to_path_buf();

    let turn = |input: &'static str, skills: Option<String>| {
        let options = options(&id, input, skills, &cwd);
        let provider = &provider;
        let store = &store;
        let tools = &tools;
        let policy = &policy;
        async move {
            run_turn(
                TurnContext {
                    store,
                    provider,
                    tools,
                    policy,
                    approver: &AutoApprove,
                    snapshots: None,
                    scheduler: None,
                    driver: qcode_core::scheduler::DriverStatus::Unknown,
                    questioner: None,
                },
                options,
                |_| {},
            )
            .await
        }
    };

    // What a frontend does per turn: render the index right then.
    turn("go", index.prompt_section())
        .await
        .expect("first turn");

    // A refresh (the TTL refresher calls exactly this) adds one skill.
    index.replace_served("github", vec![served("alpha"), served("beta")]);

    turn("again", index.prompt_section())
        .await
        .expect("second turn");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one request per turn");
    assert!(
        requests[0].system.contains("`alpha`"),
        "the first turn sees the first index"
    );
    assert!(
        !requests[0].system.contains("`beta`"),
        "the second skill did not exist yet: {}",
        requests[0].system
    );
    assert!(
        requests[1].system.contains("`beta`"),
        "the turn after the refresh must see the new skill: {}",
        requests[1].system
    );
}
