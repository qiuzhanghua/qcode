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
use qcode_tools::sandbox::{OutsideRoots, Sandbox, SandboxMode, SandboxNetwork, build};
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
        command: Some(command.display().to_string()),
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
    match build(
        SandboxMode::Landlock,
        workspace,
        SandboxNetwork::Allow,
        qcode_tools::sandbox::SandboxPaths::default(),
    ) {
        Ok(sandbox) => Some(sandbox),
        Err(error) => {
            eprintln!("skipping: no usable landlock sandbox here: {error}");
            None
        }
    }
}

/// Why a server did not come up, kept instead of dropped.
///
/// `connect_detailed` reports every failure as `tracing::warn!` and returns an
/// empty tool list. A test process installs no subscriber, so the reason went
/// nowhere: the symptom was `missing tool mcp__probe__probe` with no cause, and
/// that is indistinguishable from "the grant is wrong", "the binary is missing"
/// and "the sandbox refused to spawn".
///
/// This captures those warnings and puts them in the panic message. It is a
/// test-only sink on purpose: making the production path return errors would
/// change an API that four front-ends already depend on.
mod server_log {
    use std::io::Write;
    use std::sync::{Mutex, Once, OnceLock};

    static ONCE: Once = Once::new();
    /// Kept small: this exists to explain one panic, not to be a log file.
    const KEEP: usize = 8000;

    fn sink() -> &'static Mutex<String> {
        static LOG: OnceLock<Mutex<String>> = OnceLock::new();
        LOG.get_or_init(|| Mutex::new(String::new()))
    }

    struct Writer;

    impl Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut log) = sink().lock() {
                log.push_str(&String::from_utf8_lossy(buf));
                // Trim from the front so a long run cannot grow without bound.
                // `chars()` first: slicing a byte index inside a multi-byte
                // character would panic, and these lines carry paths.
                let excess = log.chars().count().saturating_sub(KEEP);
                if excess > 0 {
                    let kept: String = log.chars().skip(excess).collect();
                    *log = kept;
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Install once per process. Every test calls it; only the first wins.
    pub(super) fn install() {
        ONCE.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .with_writer(|| Writer)
                .try_init();
        });
    }

    /// What the server layer said, for a panic message.
    pub(super) fn tail() -> String {
        let Ok(log) = sink().lock() else {
            return String::from("(the server log mutex is poisoned)");
        };
        if log.trim().is_empty() {
            return String::from(
                "(the server layer logged nothing; it never got as far as a warning)",
            );
        }
        format!("server log:\n{log}")
    }
}

/// The sink has to actually receive what the server layer says, or the panic
/// message above is decoration. This is the only test of the mechanism itself.
#[test]
fn the_server_log_sink_captures_warnings() {
    server_log::install();
    tracing::warn!(
        marker = "sink-self-test",
        "if you can read this, the sink works"
    );
    let tail = server_log::tail();
    assert!(
        tail.contains("sink-self-test"),
        "the sink dropped the warning, so a failed spawn would still be unexplained:\n{tail}"
    );
}

async fn call(tools: &[Arc<dyn Tool>], name: &str, arguments: serde_json::Value) -> String {
    server_log::install();
    let tool = tools
        .iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("missing tool {name}\n{}", server_log::tail()));
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
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()),
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
                .with_roots(
                    std::slice::from_ref(&workspace_root),
                    None,
                    OutsideRoots::Nothing,
                )
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
        qcode_mcp::ConnectOptions::with_confine(old_style.as_ref()),
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
    // Two "outside" paths, because until ADR-0066's write-set revision they
    // failed for different reasons and only one of them was a real refusal:
    //
    // * `$CARGO_TARGET_TMPDIR` (under the repo's `target/`) was always refused.
    // * `/tmp` was *not*: the Landlock backend granted `/tmp` and `/var/tmp`
    //   writable unconditionally, so this test used to route around it. Now an
    //   MCP server gets `OutsideRoots::Nothing` and both must be denied.
    let elsewhere = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(elsewhere).unwrap();
    let outside = elsewhere.join("mcp-grant-outside.txt");
    let _ = std::fs::remove_file(&outside);
    let scratch = std::env::temp_dir().join("qc-mcp-grant-scratch.txt");
    let _ = std::fs::remove_file(&scratch);
    let inside = workspace.path().join("inside.txt");

    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()),
    )
    .await;
    assert_eq!(tools.len(), 3);

    let report = call(
        &tools,
        "mcp__probe__probe",
        json!({
            "read": [home.display().to_string()],
            "write": [
                inside.display().to_string(),
                outside.display().to_string(),
                scratch.display().to_string(),
            ],
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
    // The host scratch directory is not in the grant. This is the assertion
    // that would have caught the bug: `tempfile`-style paths used to be
    // writable by every MCP server because the backend shared `run_shell`'s
    // build-cache allowance.
    assert!(
        report.contains(&format!("write {} = denied", scratch.display())),
        "writing to the host temp dir must be denied:\n{report}"
    );
    assert!(inside.exists());
    assert!(!outside.exists());
    assert!(!scratch.exists());
}

/// The build caches, end to end: a real child spawned through `mcp_confine`
/// must not be able to write into `CARGO_HOME`.
///
/// This is the one that matters for severity. `writable_paths()` is what
/// `run_shell` uses to let `cargo build` write `$CARGO_HOME/registry`, and it
/// was applied to every MCP server too. A third-party server that can write
/// `$CARGO_HOME/config.toml` can set `[build] rustc-wrapper` and get arbitrary
/// code execution the next time anyone builds anything in that HOME.
#[tokio::test]
async fn the_build_caches_are_not_in_a_servers_write_grant() {
    let home = outside_tmp("grant-home-cargo");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    let binary = install_under(&home);
    let cargo_home = outside_tmp("grant-cargo-home");
    let config_toml = cargo_home.join("config.toml");

    // `writable_paths()` reads this out of the process env, so pointing it at a
    // directory this test owns is the only way to observe the difference.
    // SAFETY: no other test in this binary reads CARGO_HOME, and the guard
    // restores whatever was there.
    let previous = std::env::var_os("CARGO_HOME");
    unsafe { std::env::set_var("CARGO_HOME", &cargo_home) };
    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(server("probe", &binary)),
        workspace.path(),
        data.path(),
        &[],
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()),
    )
    .await;
    let report = call(
        &tools,
        "mcp__probe__probe",
        json!({"write": [config_toml.display().to_string()]}),
    )
    .await;
    match previous {
        // SAFETY: as above.
        Some(value) => unsafe { std::env::set_var("CARGO_HOME", value) },
        // SAFETY: as above.
        None => unsafe { std::env::remove_var("CARGO_HOME") },
    }

    assert_eq!(tools.len(), 3, "the server did not start");
    assert!(
        report.contains("= denied"),
        "an MCP server wrote $CARGO_HOME/config.toml, which names a rustc-wrapper:\n{report}"
    );
    assert!(!config_toml.exists());
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
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()),
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
        path: qcode_mcp::SearchPath::Process,
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

/// ADR-0070, end to end: a **bare** `command` resolved from a `PATH` the caller
/// supplies.
///
/// This is the test that pins the two halves together. `Command::new` is handed
/// `"probe"`, not a path, so the child can only be found through its own `PATH`;
/// if `connect_stdio` resolved the grant with one `PATH` and started the child
/// with another, this would fail with `NotFound` (or run a different binary).
#[tokio::test]
async fn a_bare_command_is_resolved_and_started_from_the_given_path() {
    let home = outside_tmp("grant-home-bare");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    let binary = install_under(&home);
    let bin_dir = binary.parent().unwrap().to_path_buf();
    let search = std::ffi::OsString::from(bin_dir.display().to_string());

    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        // The bare name: no `/`, so nothing but a `PATH` lookup can find it.
        &mcp(McpServerConfig {
            name: "probe".to_string(),
            command: Some("probe".to_string()),
            timeout_secs: 20,
            ..Default::default()
        }),
        workspace.path(),
        data.path(),
        &[],
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()).with_path(search.as_os_str()),
    )
    .await;

    assert_eq!(
        tools.len(),
        3,
        "the bare command did not resolve or the child could not find it; tools: {:?}",
        tools.iter().map(|tool| tool.name()).collect::<Vec<_>>()
    );
}

/// And the negative half: the same config without the `PATH` fails, so the test
/// above would not pass by accident.
#[tokio::test]
async fn a_bare_command_without_the_path_is_skipped_not_guessed() {
    let home = outside_tmp("grant-home-bare-missing");
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let Some(sandbox) = landlock(workspace.path()) else {
        return;
    };
    // Installed, but not on any `PATH` this test supplies.
    install_under(&home);

    let confine = qcode_kernel::mcp_confine(sandbox, workspace.path());
    let tools = connect_all_with(
        &mcp(McpServerConfig {
            name: "probe".to_string(),
            command: Some("qcode-test-not-on-path".to_string()),
            timeout_secs: 20,
            ..Default::default()
        }),
        workspace.path(),
        data.path(),
        &[],
        qcode_mcp::ConnectOptions::with_confine(confine.as_ref()),
    )
    .await;

    // `connect_all_with` reports and skips: one bad server must not take the
    // session down (ADR-0066 §7).
    assert!(tools.is_empty(), "{:?}", tools.len());
}
