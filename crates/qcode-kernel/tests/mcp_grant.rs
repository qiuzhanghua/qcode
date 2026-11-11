//! ADR-0066 end-to-end: an MCP server installed under `$HOME` has to run, and
//! its writes and reads have to stop at the grant.
//!
//! The stub is a real child process (`tests/support/mcp_probe_stub.rs`). It
//! reports `ok` / `denied` / `missing` per path, which is exactly the question
//! the grant answers. Everything here is gated on the platform's sandbox
//! actually working: a kernel without Landlock skips instead of pretending.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use qcode_core::tool::{Tool, ToolContext};
use qcode_mcp::{GrantInput, GrantSet, McpConfig, McpServerConfig, connect_all_with};
use qcode_tools::sandbox::{Sandbox, SandboxMode, SandboxNetwork, build};
use serde_json::json;

const STUB: &str = env!("CARGO_BIN_EXE_mcp-probe-stub");

/// A scratch directory that is **not** under `/tmp`.
///
/// The Landlock backend grants `/tmp` and `/var/tmp` writable unconditionally,
/// so a `tempfile` directory proves nothing about the grant. `CARGO_TARGET_TMPDIR`
/// lives under the repo's `target/`, which the shell boundary does not cover —
/// exactly the situation of a user who unpacks a release tarball next to their
/// home directory.
fn outside_tmp(tag: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(tag);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Copy the stub into a release-tarball layout under `home`:
/// `<home>/installs/probe/bin/probe` next to `share/`. That is the shape that
/// used to be unreachable, because `/home` is not in the shell's read
/// whitelist.
fn install_under(home: &Path) -> PathBuf {
    let dist = home.join("installs").join("probe");
    std::fs::create_dir_all(dist.join("bin")).unwrap();
    std::fs::create_dir_all(dist.join("share")).unwrap();
    let target = dist.join("bin").join("probe");
    std::fs::copy(STUB, &target).unwrap();
    target
}

fn server(name: &str, command: &Path) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: command.display().to_string(),
        timeout_secs: 20,
        ..Default::default()
    }
}

fn mcp(server: McpServerConfig) -> McpConfig {
    McpConfig {
        servers: vec![server],
        ..Default::default()
    }
}

/// `None` when this machine cannot run the sandbox at all.
fn landlock(workspace: &Path) -> Option<Arc<dyn Sandbox>> {
    match build(SandboxMode::Landlock, workspace, SandboxNetwork::Allow, &[]) {
        Ok(sandbox) => Some(sandbox),
        Err(error) => {
            eprintln!("skipping: no usable landlock sandbox here: {error}");
            None
        }
    }
}

async fn call(tools: &[Arc<dyn Tool>], name: &str, arguments: serde_json::Value) -> String {
    let tool = tools
        .iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("missing tool {name}"));
    let context = ToolContext::for_test(std::env::temp_dir());
    tool.execute(&context, arguments)
        .await
        .unwrap_or_else(|error| panic!("{name} failed: {error}"))
        .output
}

#[tokio::test]
async fn a_server_installed_under_the_home_directory_runs_under_its_grant() {
    let home = outside_tmp("grant-home-run");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    let binary = install_under(&home);

    // The derived runtime root is `<home>/installs/probe`; nothing about the
    // user's home is needed and `$HOME` itself stays out of the grant.
    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        Some(confine.as_ref()),
    )
    .await;

    assert_eq!(
        tools.len(),
        3,
        "the server did not start; tools: {:?}",
        tools.iter().map(|tool| tool.name()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn without_the_grant_the_same_server_is_refused() {
    // The pre-ADR-0066 wiring: one sandbox, built for the workspace only. This
    // is the "Permission denied" from the bug report, kept as a regression
    // test so the cause stays obvious.
    let home = outside_tmp("grant-home-refused");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    let binary = install_under(&home);

    let workspace_root = qcode_core::task::Root {
        id: "workspace".to_string(),
        label: "workspace".to_string(),
        path: workspace.path().to_path_buf(),
        access: qcode_core::task::RootAccess::ReadWrite,
        primary: true,
    };
    let old_style: Box<qcode_mcp::Confine> = {
        let sandbox = sandbox.clone();
        let workspace_root = workspace_root.clone();
        Box::new(move |program, args, cwd, _grant| {
            let per_server = sandbox
                .with_roots(std::slice::from_ref(&workspace_root), None)
                .map_err(|error| error.to_string())?;
            per_server
                .command(program, args, cwd)
                .map_err(|error| error.to_string())
        })
    };
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        Some(old_style.as_ref()),
    )
    .await;

    assert!(
        tools.is_empty(),
        "the old boundary should still refuse a server outside the workspace"
    );
}

#[tokio::test]
async fn writes_and_reads_stop_at_the_grant() {
    let home = outside_tmp("grant-home-write");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    let binary = install_under(&home);
    // `tempfile` lives under `/tmp`, and the Landlock backend grants `/tmp` and
    // `/var/tmp` writable unconditionally — so an "outside" path has to be
    // somewhere else entirely. `CARGO_TARGET_TMPDIR` is under the repo's
    // `target/`, which is neither.
    let elsewhere = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(elsewhere).unwrap();
    let outside = elsewhere.join("mcp-grant-outside.txt");
    let _ = std::fs::remove_file(&outside);
    let inside = workspace.path().join("inside.txt");

    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        Some(confine.as_ref()),
    )
    .await;
    assert_eq!(tools.len(), 3);

    let report = call(
        &tools,
        "mcp__probe__probe",
        json!({
            "read": [home.display().to_string()],
            "write": [inside.display().to_string(), outside.display().to_string()],
        }),
    )
    .await;
    let line = |prefix: &str| {
        report
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting with {prefix:?} in:\n{report}"))
            .to_string()
    };

    // Writing into the workspace is the whole point of the write grant.
    assert!(line("write ").contains("= ok"), "{report}");
    // `$HOME` itself is never granted. The tempdir stand-in for an install
    // location is *inside* `/tmp`, which Landlock always allows, so this
    // assertion uses the real home when there is one.
    if let Some(real) = std::env::var_os("HOME").map(PathBuf::from)
        && real.is_dir()
        && !real.starts_with("/tmp")
    {
        let report = call(
            &tools,
            "mcp__probe__probe",
            json!({"read": [real.display().to_string()]}),
        )
        .await;
        assert!(
            report.contains("= denied"),
            "reading the user's home must stay denied:\n{report}"
        );
    }
    assert!(
        report.contains(&format!("write {} = denied", outside.display())),
        "writing outside the workspace must stay denied:\n{report}"
    );
    assert!(inside.exists());
    assert!(!outside.exists());
}

#[tokio::test]
async fn the_server_sees_a_private_home_and_no_inherited_secrets() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    // Installed in the workspace so this test needs no `$HOME` install.
    let dist = workspace.path().join("probe");
    std::fs::create_dir_all(dist.join("bin")).unwrap();
    std::fs::create_dir_all(dist.join("share")).unwrap();
    let binary = dist.join("bin").join("probe");
    std::fs::copy(STUB, &binary).unwrap();

    // A secret exists in qcode's own environment; it must not reach the child.
    // SAFETY: no other test in this binary reads this variable.
    unsafe { std::env::set_var("QCODE_GRANT_LEAK_CANARY", "leaked") };

    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        Some(confine.as_ref()),
    )
    .await;
    assert_eq!(tools.len(), 3);

    let names = call(&tools, "mcp__probe__env", json!({})).await;
    assert!(
        !names
            .split(' ')
            .any(|name| name == "QCODE_GRANT_LEAK_CANARY"),
        "the child inherited an unrelated variable: {names}"
    );
    assert!(names.split(' ').any(|name| name == "PATH"), "{names}");

    let home = call(&tools, "mcp__probe__home", json!({})).await;
    let expected = data.path().join("mcp").join("probe").join("home");
    assert!(
        home.contains(&format!("HOME={}", expected.display())),
        "expected the synthetic HOME {}, got: {home}",
        expected.display()
    );
    assert!(
        expected.is_dir(),
        "the private HOME should have been created"
    );

    unsafe { std::env::remove_var("QCODE_GRANT_LEAK_CANARY") };
}

#[tokio::test]
async fn a_directly_derived_grant_lists_read_only_runtime_and_writable_workspace() {
    // No sandbox here: this pins the shape `mcp_roots` consumes, on any host.
    let home = outside_tmp("grant-home-derive");
    let workspace = tempfile::tempdir().unwrap();
    let binary = install_under(&home);

    let grant = GrantSet::derive(GrantInput {
        server: &server("probe", &binary),
        workspace: workspace.path(),
        server_state: home.join("state"),
        real_home: Some(&home),
        confined: true,
    })
    .unwrap();

    let roots = qcode_kernel::mcp_roots(workspace.path(), &grant);
    let workspace_root = roots
        .iter()
        .find(|root| root.path == workspace.path())
        .expect("the workspace is always a root");
    assert_eq!(
        workspace_root.access,
        qcode_core::task::RootAccess::ReadWrite
    );
    assert!(workspace_root.primary);

    let runtime_root = roots
        .iter()
        .find(|root| root.path == binary.parent().unwrap().parent().unwrap())
        .expect("the derived dist root must be a root");
    assert_eq!(runtime_root.access, qcode_core::task::RootAccess::ReadOnly);

    // `$HOME` itself is never a root, only the derived install tree.
    assert!(
        !roots.iter().any(|root| root.path == home),
        "the user's home must not be granted: {roots:?}"
    );
}
