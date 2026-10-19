use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("fixture-delete-cli-{name}-{}", std::process::id()));
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
fn delete_removes_the_key_and_reports_missing_ones() {
    let root = temp_root("delete");
    assert!(cli(&root, &["set", "alpha", "one"]).status.success());

    let output = cli(&root, &["delete", "alpha"]);
    assert!(output.status.success(), "{output:?}");

    let output = cli(&root, &["get", "alpha"]);
    assert!(!output.status.success());

    let output = cli(&root, &["delete", "alpha"]);
    assert!(!output.status.success());
    assert!(
        !String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "missing key should report an error on stderr: {output:?}"
    );
}
