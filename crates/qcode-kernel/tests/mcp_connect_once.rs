//! ADR-0108 §2: the skill index is built from a connection that already
//! happened, not from a second one.
//!
//! `assemble` now runs `connect_mcp` → `load_skills` → `build_tools`. The
//! property that makes that ordering safe is that exactly **one** server
//! process starts: connecting twice would double every server for every
//! session. `build_tools` takes a `McpConnection` by value, so it *cannot*
//! connect — this test proves the wiring actually reaches both sides, and that
//! the stdio path (env, `PATH` lookup, confine) still works after the split.
//!
//! The counter is the stub itself: it appends one line to `$MCP_START_LOG` on
//! every start, so "how many processes did we launch" is a file we can count.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use qcode_mcp::{McpConfig, McpServerConfig};
use qcode_tools::sandbox::{SandboxMode, SandboxNetwork, SandboxPaths, build};
use qcode_tools::shell_env::ShellEnvCache;

const STUB: &str = env!("CARGO_BIN_EXE_mcp-probe-stub");

fn server(name: &str, log: &Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: Some(STUB.to_string()),
        timeout_secs: 20,
        env: [("MCP_START_LOG".to_string(), log.display().to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

fn starts(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

/// A connected server's tools reach the registry without a second launch.
#[tokio::test]
async fn connecting_then_building_starts_each_server_once() {
    let workspace = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("assemble_once");
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).unwrap();
    let data_dir = workspace.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let log = workspace.join("starts.log");

    let sandbox = build(
        SandboxMode::None,
        &workspace,
        SandboxNetwork::Deny,
        SandboxPaths::default(),
    )
    .expect("sandbox");
    let shell_env = Arc::new(ShellEnvCache::disabled());
    let wiring = qcode_kernel::ToolWiring {
        network_env: None,
        network: None,
        shell_env: shell_env.clone(),
    };
    let config = McpConfig {
        servers: vec![server("probe", &log)],
        ..Default::default()
    };

    let connection =
        qcode_kernel::connect_mcp(&config, &workspace, &data_dir, sandbox.clone(), &wiring).await;
    assert!(
        !connection.tools.is_empty(),
        "the stub should have contributed tools"
    );

    let skills = Arc::new(qcode_tools::skills::SkillIndex::new(
        qcode_tools::skills::SkillSet::default(),
    ));
    let tools =
        qcode_kernel::build_tools(connection, &Default::default(), &skills, sandbox, shell_env)
            .await;

    assert_eq!(
        starts(&log),
        1,
        "build_tools must not start the server again; the registry already had its tools"
    );
    assert!(
        tools
            .specs()
            .iter()
            .any(|spec| spec.name.starts_with("mcp__probe__")),
        "the connected server's tools must survive into the registry"
    );
}

/// The connection carries the server's skills declaration through the
/// kernel-level entry point, not just through `qcode_mcp`'s own.
///
/// This is the seam M5 consumes: if `connect_mcp` dropped `skill_servers`, the
/// index would silently contain nothing and no test below the kernel would
/// notice.
#[tokio::test]
async fn connect_mcp_reports_a_server_that_declares_skills() {
    let workspace = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("assemble_skills");
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).unwrap();
    let log: PathBuf = workspace.join("starts.log");

    let sandbox = build(
        SandboxMode::None,
        &workspace,
        SandboxNetwork::Deny,
        SandboxPaths::default(),
    )
    .expect("sandbox");
    let mut declaring = server("probe", &log);
    declaring.env.insert(
        "MCP_STUB_CAPS".to_string(),
        "tools,skills,directory-read".to_string(),
    );
    let config = McpConfig {
        servers: vec![declaring],
        ..Default::default()
    };

    let connection = qcode_kernel::connect_mcp(
        &config,
        &workspace,
        &workspace.join("data"),
        sandbox,
        &qcode_kernel::ToolWiring {
            network_env: None,
            network: None,
            shell_env: Arc::new(ShellEnvCache::disabled()),
        },
    )
    .await;
    // The label is the config's name (`probe`), not the stub's self-reported
    // `mcp-probe-stub` — the same rule the `qcode-mcp` test pins down.
    assert_eq!(connection.skill_servers, vec!["probe".to_string()]);
}
