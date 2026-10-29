//! `--approval-policy` replaces the config rules and works without a TTY.

use std::process::{Command, Stdio};

fn write(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).expect("write fixture");
}

/// A config that pins the OS sandbox off.
///
/// These tests are about the approval layer, so they must not depend on
/// whether the host can apply a kernel sandbox: a nested container cannot, and
/// the default Landlock backend would then abort `qc` before any approval runs.
const NO_SANDBOX: &str = "[sandbox]\nmode = \"none\"\n";

#[test]
fn policy_file_allows_shell_without_a_tty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let policy = dir.path().join("policy.toml");
    let config = dir.path().join("config.toml");
    write(&config, NO_SANDBOX);
    write(
        &policy,
        r#"
        [[rules]]
        id = "allow-echo"
        tool = "run_shell"
        command_prefix = "echo"
        action = "allow"
        "#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-approval",
            "--json",
            "--config",
            config.to_str().expect("utf-8 path"),
            "--approval-policy",
            policy.to_str().expect("utf-8 path"),
            "--data-dir",
            dir.path().to_str().expect("utf-8 path"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run qc");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "policy did not allow the call:\n{stdout}"
    );
    assert!(
        stdout.contains("qcode-approval-demo"),
        "the shell tool did not run:\n{stdout}"
    );
    assert!(
        stdout.contains("审批流程演示结束。"),
        "the model did not continue:\n{stdout}"
    );
}

#[test]
fn policy_file_replaces_config_rules() {
    let dir = tempfile::tempdir().expect("temp dir");
    let policy = dir.path().join("policy.toml");
    let config = dir.path().join("config.toml");
    write(
        &policy,
        r#"
        [[rules]]
        id = "allow-shell"
        tool = "run_shell"
        action = "allow"
        "#,
    );
    write(
        &config,
        r#"
        [sandbox]
        mode = "none"

        [[approval.rules]]
        id = "deny-shell"
        tool = "run_shell"
        action = "deny"
        "#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "demo",
            "--provider",
            "mock-approval",
            "--json",
            "--config",
            config.to_str().expect("utf-8 path"),
            "--approval-policy",
            policy.to_str().expect("utf-8 path"),
            "--data-dir",
            dir.path().to_str().expect("utf-8 path"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run qc");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "config deny rule leaked into the explicit policy:\n{stdout}"
    );
}

#[test]
fn missing_or_malformed_policy_fails_before_running() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("missing.toml");
    let invalid = dir.path().join("invalid.toml");
    let config = dir.path().join("config.toml");
    write(&config, NO_SANDBOX);
    write(&invalid, "rules = 1");

    for policy in [missing, invalid] {
        let output = Command::new(env!("CARGO_BIN_EXE_qc"))
            .args([
                "run",
                "demo",
                "--provider",
                "mock",
                "--config",
                config.to_str().expect("utf-8 path"),
                "--approval-policy",
                policy.to_str().expect("utf-8 path"),
                "--data-dir",
                dir.path().to_str().expect("utf-8 path"),
            ])
            .stdin(Stdio::null())
            .output()
            .expect("run qc");

        assert!(!output.status.success(), "policy should be rejected");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("approval policy"),
            "error should name the policy: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
