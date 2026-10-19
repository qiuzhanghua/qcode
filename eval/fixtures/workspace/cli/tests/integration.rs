use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("fixture-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn run(root: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run cli")
}

#[test]
fn set_get_and_list_roundtrip() {
    let root = temp_root("roundtrip");

    let output = run(&root, &["set", "alpha", "one"]);
    assert!(output.status.success(), "{output:?}");

    let output = run(&root, &["set", "beta", "two"]);
    assert!(output.status.success(), "{output:?}");

    let output = run(&root, &["get", "alpha"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "one");

    let output = run(&root, &["list"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "alpha\nbeta\n",
        "{output:?}"
    );
}

#[test]
fn missing_key_fails() {
    let root = temp_root("missing");
    let output = run(&root, &["get", "nope"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
}

#[test]
fn unknown_command_prints_usage() {
    let root = temp_root("usage");
    let output = run(&root, &["frobnicate"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
}
