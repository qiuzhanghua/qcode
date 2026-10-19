use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("fixture-json-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn cli(root: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cli"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("run cli")
}

#[test]
fn json_get_matches_the_documented_shape() {
    let root = temp_root("get");
    assert!(cli(&root, &["set", "alpha", "one"]).status.success());

    let output = cli(&root, &["--format", "json", "get", "alpha"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"key\":\"alpha\",\"value\":\"one\"}\n"
    );
}

#[test]
fn json_list_is_a_sorted_array() {
    let root = temp_root("list");
    assert!(cli(&root, &["set", "beta", "two"]).status.success());
    assert!(cli(&root, &["set", "alpha", "one"]).status.success());

    let output = cli(&root, &["--format", "json", "list"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[\"alpha\",\"beta\"]\n"
    );
}

#[test]
fn text_remains_the_default() {
    let root = temp_root("text");
    assert!(cli(&root, &["set", "alpha", "one"]).status.success());

    let output = cli(&root, &["get", "alpha"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "one\n");
}

#[test]
fn unknown_format_is_an_error() {
    let root = temp_root("bad-format");
    let output = cli(&root, &["--format", "yaml", "list"]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).is_empty());
}
