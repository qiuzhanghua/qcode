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

// ---------------------------------------------------------------------------
// Remote servers (ADR-0067 P3). The config file is the contract: what
// `qc mcp add` writes has to be what `qc doctor`/the client later accepts.
// ---------------------------------------------------------------------------

fn qc_err(args: &[&str]) -> String {
    let output = qc(args);
    assert!(
        !output.status.success(),
        "expected a failure from {args:?}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_remote_server_round_trips_through_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let added = qc(&[
        "--config",
        config,
        "mcp",
        "add",
        "context7",
        "--url",
        "https://mcp.context7.com/mcp",
        "--bearer-token-env",
        "CONTEXT7_TOKEN",
        "--header",
        "Accept: application/json, text/event-stream",
    ]);
    assert!(
        added.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&added.stderr)
    );

    // The written file must be readable as a remote server, not merely present.
    let text = std::fs::read_to_string(config).expect("read config");
    assert!(
        text.contains("url = \"https://mcp.context7.com/mcp\""),
        "{text}"
    );
    assert!(
        text.contains("bearer_token_env_var = \"CONTEXT7_TOKEN\""),
        "{text}"
    );
    assert!(
        text.contains("Accept = \"application/json, text/event-stream\""),
        "{text}"
    );
    assert!(
        !text.contains("command"),
        "a remote server must not get a command field: {text}"
    );

    let listed = qc(&["--config", config, "mcp", "list"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("transport=streamable-http"), "{stdout}");
    assert!(
        stdout.contains("url=\"https://mcp.context7.com/mcp\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("bearer_token_env_var=CONTEXT7_TOKEN"),
        "{stdout}"
    );
    // Not `command=None`: on a remote server that reads like a broken stdio one.
    assert!(!stdout.contains("command=None"), "{stdout}");
}

#[test]
fn a_stdio_server_still_writes_a_command_and_nothing_else() {
    // The pre-ADR-0067 shape, unchanged: this is the "老配置一行不改" promise.
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
        "--arg=srv",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let text = std::fs::read_to_string(config).expect("read config");
    assert!(text.contains("command = \"npx\""), "{text}");
    assert!(!text.contains("url"), "{text}");

    let listed =
        String::from_utf8_lossy(&qc(&["--config", config, "mcp", "list"]).stdout).into_owned();
    assert!(listed.contains("transport=stdio"), "{listed}");
    assert!(listed.contains("command=\"npx\""), "{listed}");
}

#[test]
fn command_and_url_are_mutually_exclusive() {
    // clap rejects both, and `validate()` rejects them again for configs
    // written by other tools (TOML is a wider boundary than the CLI).
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let neither = qc_err(&["--config", config, "mcp", "add", "x"]);
    assert!(neither.contains("--command"), "{neither}");
    assert!(neither.contains("--url"), "{neither}");

    let both = qc_err(&[
        "--config",
        config,
        "mcp",
        "add",
        "y",
        "--command",
        "npx",
        "--url",
        "https://a.test/mcp",
    ]);
    assert!(both.contains("cannot be used with"), "{both}");

    // Nothing was written by the rejected invocations.
    assert!(!std::path::Path::new(config).exists());
}

#[test]
fn a_sensitive_header_is_refused_with_the_alternative() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let error = qc_err(&[
        "--config",
        config,
        "mcp",
        "add",
        "remote",
        "--url",
        "https://a.test/mcp",
        "--header",
        "Authorization: Bearer sekrit",
    ]);
    assert!(error.contains("Authorization"), "{error}");
    assert!(error.contains("bearer_token_env_var"), "{error}");
    // The value must not be echoed back into a log or a terminal scrollback.
    assert!(!error.contains("sekrit"), "{error}");
}

#[test]
fn a_remote_only_flag_on_a_stdio_server_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let error = qc_err(&[
        "--config",
        config,
        "mcp",
        "add",
        "local",
        "--command",
        "npx",
        "--bearer-token-env",
        "TOKEN",
    ]);
    assert!(error.contains("streamable-http"), "{error}");
}

#[test]
fn a_malformed_header_says_what_the_shape_is() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let error = qc_err(&[
        "--config",
        config,
        "mcp",
        "add",
        "r",
        "--url",
        "https://a.test/mcp",
        "--header",
        "NoColon",
    ]);
    assert!(error.contains("Name: value"), "{error}");
}

#[test]
fn doctor_accepts_what_add_wrote() {
    // The two commands have to agree, or the user gets a config that `qc mcp
    // add` blessed and `qc doctor` rejects.
    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config = config.to_str().expect("utf-8 path");

    let added = qc(&[
        "--config",
        config,
        "mcp",
        "add",
        "remote",
        "--url",
        "http://127.0.0.1:9/mcp",
        "--disabled",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let doctor = qc(&["--config", config, "doctor"]);
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        !text.contains("[fail] config"),
        "doctor rejected the config `mcp add` wrote:\n{text}"
    );
    // `--disabled` means doctor skips it rather than warning about the port.
    assert!(text.contains("disabled"), "{text}");
}
