//! A workspace `.qcode/config.toml` can tighten policy but never allow.

use std::process::{Command, Stdio};

fn write_project(dir: &std::path::Path, rules: &str) {
    std::fs::create_dir_all(dir.join(".qcode")).unwrap();
    std::fs::write(dir.join(".qcode/config.toml"), format!("{rules}\n")).unwrap();
}

fn run(workspace: &std::path::Path, data: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-approval",
            "--json",
            "--cwd",
            workspace.to_str().expect("utf-8 workspace"),
            "--data-dir",
            data.to_str().expect("utf-8 data dir"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run qc");
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn project_deny_is_applied_with_project_scope() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    write_project(
        &workspace,
        r#"
        [[approval.rules]]
        id = "repo-deny-shell"
        tool = "run_shell"
        action = "deny"
        "#,
    );

    let stdout = run(&workspace, &dir.path().join("data"));
    assert!(
        stdout.contains(r#""rule_id":"repo-deny-shell""#),
        "project rule not matched:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""scope":"project""#),
        "scope was not project:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "project deny not applied:\n{stdout}"
    );
}

#[test]
fn project_allow_is_downgraded_to_ask() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    write_project(
        &workspace,
        r#"
        [[approval.rules]]
        id = "repo-allow-shell"
        tool = "run_shell"
        action = "allow"
        "#,
    );

    // Non-TTY: ask becomes deny, so a downgraded project allow must not run.
    let stdout = run(&workspace, &dir.path().join("data"));
    assert!(
        stdout.contains(r#""rule_id":"repo-allow-shell""#),
        "project rule not matched:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "project allow was not downgraded:\n{stdout}"
    );
    assert!(
        stdout.contains("denied"),
        "the tool result should report the denial:\n{stdout}"
    );
}
