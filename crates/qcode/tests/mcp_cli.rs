//! `qc mcp add/list/remove` and `qc config show` round-trip through the file.

use std::process::Command;

fn qc(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(args)
        .output()
        .expect("run qc")
}

#[test]
fn mcp_servers_can_be_added_listed_and_removed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let added = qc(&[
        "--config",
        config,
        "mcp",
        "add",
        "filesystem",
        "--command",
        "npx",
        "--arg=-y",
        "--arg=@modelcontextprotocol/server-filesystem",
        "--disabled",
    ]);
    assert!(
        added.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&added.stderr)
    );

    let listed = qc(&["--config", config, "mcp", "list"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("filesystem"), "{stdout}");
    assert!(stdout.contains("enabled=false"), "{stdout}");

    let shown = qc(&["--config", config, "config", "show"]);
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(stdout.contains("mcp servers (1)"), "{stdout}");
    assert!(stdout.contains("filesystem"), "{stdout}");

    let removed = qc(&["--config", config, "mcp", "remove", "filesystem"]);
    assert!(
        removed.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let listed = qc(&["--config", config, "mcp", "list"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("mcp servers (0)"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}
