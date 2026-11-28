//! ADR-0108 end to end: a server's skills reach the index, and loading one goes
//! back over the wire to fetch and verify it.
//!
//! Everything here runs against a real child process (`mcp_probe_stub`), so the
//! path under test is the one a user gets: connect, `skills/list`, index,
//! `resources/read`, verify. The three questions are the ones the ADR has to
//! answer: does the config gate it, does the index show it, and can the body be
//! trusted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use qcode_core::tool::{Tool, ToolContext};
use qcode_mcp::{McpConfig, McpServerConfig};
use qcode_tools::sandbox::{SandboxMode, SandboxNetwork, SandboxPaths, build};
use qcode_tools::shell_env::ShellEnvCache;
use qcode_tools::skills::{SkillIndex, SkillSet, SkillSource, SkillTool};
use serde_json::json;

const STUB: &str = env!("CARGO_BIN_EXE_mcp-probe-stub");
const SKILL_URI: &str = "skill://probe-skill/SKILL.md";

fn workspace(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(tag);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    root
}

/// A config whose one server declares the skills extension.
fn declaring_server(skills: Option<bool>) -> (McpConfig, McpServerConfig) {
    let server = McpServerConfig {
        name: "probe".to_string(),
        command: Some(STUB.to_string()),
        timeout_secs: 20,
        env: [("MCP_STUB_CAPS".to_string(), "tools,skills".to_string())]
            .into_iter()
            .collect(),
        skills,
        ..Default::default()
    };
    (McpConfig::default(), server)
}

async fn connect(workspace: &Path, server: McpServerConfig) -> qcode_mcp::McpConnection {
    let config = McpConfig {
        servers: vec![server],
        ..Default::default()
    };
    let sandbox = build(
        SandboxMode::None,
        workspace,
        SandboxNetwork::Deny,
        SandboxPaths::default(),
    )
    .expect("sandbox");
    qcode_kernel::connect_mcp(
        &config,
        workspace,
        &workspace.join("data"),
        sandbox,
        &qcode_kernel::ToolWiring {
            network_env: None,
            network: None,
            shell_env: Arc::new(ShellEnvCache::disabled()),
        },
    )
    .await
}

/// The gate that matters most: a server is installed to offer tools, not to
/// write the system prompt.
#[tokio::test]
async fn skills_are_listed_only_when_the_config_allows_them() {
    let workspace = workspace("served_off");

    // `[skills] mcp = false` (the default) and no per-server override.
    let (_default, server) = declaring_server(None);
    let connection = connect(&workspace, server).await;
    let mut config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let served = qcode_kernel::collect_served_skills(&connection, &config, false).await;
    assert!(
        served.is_empty(),
        "not opted in, so the server must not even be asked"
    );

    // Same connection, same server, opt-in flipped on.
    config.servers[0].skills = Some(true);
    let served = qcode_kernel::collect_served_skills(&connection, &config, false).await;
    assert_eq!(served.len(), 1, "the opt-in is what makes the difference");
    assert_eq!(
        qcode_tools::skills::frontmatter_str(&served[0].frontmatter, "name"),
        Some("probe-skill")
    );
    // A field beyond `name`/`description` survives the wire: it is in the map
    // the body is later checked against, not dropped on the way.
    assert_eq!(
        qcode_tools::skills::frontmatter_str(&served[0].frontmatter, "license"),
        Some("MIT")
    );
    assert_eq!(served[0].uri, SKILL_URI);
}

/// `[skills] mcp = true` is the crate-wide default a server inherits.
#[tokio::test]
async fn the_crate_wide_default_is_inherited() {
    let workspace = workspace("served_default");
    let (_default, server) = declaring_server(None);
    let connection = connect(&workspace, server).await;
    let config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let served = qcode_kernel::collect_served_skills(&connection, &config, true).await;
    assert_eq!(served.len(), 1);

    // And an explicit `false` still wins over the crate default.
    let mut overriding = config.clone();
    overriding.servers[0].skills = Some(false);
    let served = qcode_kernel::collect_served_skills(&connection, &overriding, true).await;
    assert!(served.is_empty());
}

/// The index entry, then the body, then the checks — in one session.
#[tokio::test]
async fn the_index_entry_leads_to_a_verified_body() {
    let workspace = workspace("served_body");
    let (_default, server) = declaring_server(Some(true));
    let connection = connect(&workspace, server).await;
    let config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: Some(true),
            ..Default::default()
        }],
        ..Default::default()
    };
    let served = qcode_kernel::collect_served_skills(&connection, &config, false).await;
    assert_eq!(served.len(), 1);

    let skills = Arc::new(SkillIndex::new(
        SkillSet::default().with_served(qcode_kernel::served_skills(served)),
    ));
    // The tool needs a reader, and in a real session `build_tools` builds it
    // from the same connection. Reusing the connection here is the same thing.
    let source = connection.source.clone().expect("a connected server");
    let reader: Arc<dyn qcode_tools::skills::RemoteSkillReader> =
        qcode_kernel::mcp_skill_reader(source);
    let tool = SkillTool::with_reader(skills.clone(), reader);
    let context = ToolContext::for_test(&workspace);

    let output = tool
        .execute(&context, json!({"name": "probe-skill"}))
        .await
        .expect("the body should load");
    assert!(output.output.contains("Probe body."), "{}", output.output);
    assert!(output.output.contains("mcp:probe"), "{}", output.output);

    // A bundled file resolves against the skill root and is verified the same
    // way; the stub serves it under `references/`.
    let output = tool
        .execute(
            &context,
            json!({"name": "probe-skill", "file": "references/GUIDE.md"}),
        )
        .await
        .expect("a listed file should load");
    assert!(output.output.contains("guide body"), "{}", output.output);

    // And a file the manifest does not list never reaches the server.
    let error = tool
        .execute(
            &context,
            json!({"name": "probe-skill", "file": "references/OTHER.md"}),
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().is_empty());
}

/// The index is what the model sees in the prompt, so its shape is the
/// contract: name, description, and the origin label as the source.
#[tokio::test]
async fn the_prompt_index_shows_the_served_skill_under_its_server() {
    let workspace = workspace("served_prompt");
    let (_default, server) = declaring_server(Some(true));
    let connection = connect(&workspace, server).await;
    let config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: Some(true),
            ..Default::default()
        }],
        ..Default::default()
    };
    let served = qcode_kernel::collect_served_skills(&connection, &config, false).await;
    let skills = SkillSet::default().with_served(qcode_kernel::served_skills(served));
    let section = skills.prompt_section().expect("a non-empty index");

    assert!(section.contains("probe-skill"), "{section}");
    assert!(
        section.contains("A skill the probe stub serves."),
        "{section}"
    );
    // The label is the config's name, so two servers cannot collide silently.
    assert!(section.contains("mcp:probe"), "{section}");
}

/// A skill served under a local skill's name must not take its place: the model
/// would otherwise be loading something the user never reviewed.
#[tokio::test]
async fn a_served_skill_does_not_shadow_a_local_one() {
    let workspace = workspace("served_shadow");
    std::fs::create_dir_all(workspace.join("skills/probe-skill")).unwrap();
    std::fs::write(
        workspace.join("skills/probe-skill/SKILL.md"),
        "---\nname: probe-skill\ndescription: The local one.\n---\n\nlocal body\n",
    )
    .unwrap();

    let local =
        SkillSet::discover(&[(SkillSource::Project, workspace.join("skills").to_path_buf())]);
    let (_default, server) = declaring_server(Some(true));
    let connection = connect(&workspace, server).await;
    let config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: Some(true),
            ..Default::default()
        }],
        ..Default::default()
    };
    let served = qcode_kernel::collect_served_skills(&connection, &config, false).await;
    let set = local.with_served(qcode_kernel::served_skills(served));

    // Both are present under distinct sources, and a bare name is ambiguous
    // rather than resolved by luck.
    match set.lookup("probe-skill") {
        qcode_tools::skills::Lookup::Ambiguous(candidates) => {
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected an ambiguity, got {other:?}"),
    }
}

/// SEP-2640 has no `list_changed` notification: `ttlMs` is the only hint that a
/// server's catalogue moved. A stale listing must be re-fetched and the new
/// skill must reach the prompt on the next turn (ADR-0108 §2).
#[tokio::test]
async fn a_servers_ttl_refresh_reaches_the_index() {
    let workspace = workspace("ttl_refresh");
    let mut server = declaring_server(Some(true)).1;
    server
        .env
        .insert("MCP_STUB_SKILL_TTL_MS".to_string(), "120".to_string());
    server
        .env
        .insert("MCP_STUB_SKILL_ROTATE".to_string(), "1".to_string());
    let connection = connect(&workspace, server).await;

    let config = McpConfig {
        servers: vec![McpServerConfig {
            name: "probe".to_string(),
            skills: Some(true),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (served, ttls) =
        qcode_kernel::collect_served_skill_listings(&connection, &config, false).await;
    assert_eq!(served.len(), 1, "the first listing only has probe-skill");
    assert_eq!(
        ttls,
        vec![("probe".to_string(), Some(120))],
        "the stub advertised a ttl"
    );

    let index = Arc::new(SkillIndex::new(
        SkillSet::default().with_served(qcode_kernel::served_skills(served)),
    ));
    qcode_kernel::spawn_skill_refresh(
        connection.source.clone().expect("a connected server"),
        ttls,
        index.clone(),
    );

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && index.get("late-skill").is_none() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(
        index.get("late-skill").is_some(),
        "the ttl refresh never re-listed the server"
    );
    assert!(
        index.get("probe-skill").is_some(),
        "a refresh must not drop the skills that stayed"
    );
    let section = index.prompt_section().expect("a non-empty index");
    assert!(section.contains("late-skill"), "{section}");
    // A refresh replaces the server's entries; it must not leave two copies of
    // the skill that stayed behind.
    assert_eq!(
        section.matches("`probe-skill`").count(),
        1,
        "a refresh duplicated the skill that did not change: {section}"
    );
}
