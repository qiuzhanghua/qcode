//! `qc doctor`'s MCP section (ADR-0066 P2, ADR-0068 P3).
//!
//! Doctor's job is to answer "why doesn't it run?", so these tests assert on
//! the *reason* text, not just on the word "warn". Every case here was a real
//! silent no-op at some point:
//!
//! * a server that is configured but whose command does not exist;
//! * `credentials = ["github"]` on a machine without `~/.config/gh`;
//! * a token written as a literal into `env`;
//! * `sandbox = "off"` and `[sandbox] mode = "none"` reported as if confined.

use std::path::{Path, PathBuf};
use std::process::Command;

fn qc(args: &[&str], home: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(args)
        .env("HOME", home)
        // Doctor probes a provider and the toolchain; a stable, tiny PATH and
        // no proxy keep it offline and fast.
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .expect("run qc");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

/// One `[ok]`/`[warn]`/`[fail]` line, for assertions that care about the level
/// as well as the text.
fn line<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?} in:\n{text}"))
}

struct Fixture {
    dir: tempfile::TempDir,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        Self { dir, home }
    }

    fn config(&self, body: &str) -> PathBuf {
        let path = self.dir.path().join("config.toml");
        std::fs::write(&path, body).expect("write config");
        path
    }

    fn doctor(&self, config: &Path) -> String {
        let data = self.dir.path().join("data");
        qc(
            &[
                "--config",
                config.to_str().expect("utf-8"),
                "--data-dir",
                data.to_str().expect("utf-8"),
                "doctor",
            ],
            &self.home,
        )
    }
}

#[test]
fn a_working_server_gets_a_grant_line_with_its_runtime_write_and_home() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"
        "#,
    );
    let text = fixture.doctor(&config);

    let line = line(&text, "mcp probe:");
    assert!(line.starts_with("[ok]"), "{line}");
    // The three parts of the grant, each of which a user may need to change.
    assert!(line.contains("runtime ["), "{line}");
    assert!(line.contains("write ["), "{line}");
    assert!(line.contains("home "), "{line}");
    // The private HOME lives under the data directory, not under $HOME.
    assert!(line.contains("data/mcp/probe/home"), "{line}");
    assert!(
        !line.contains(&format!("home {}", fixture.home.display())),
        "{line}"
    );
}

#[test]
fn a_missing_command_names_the_path_instead_of_a_bare_permission_error() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "gone"
        command = "/nonexistent/definitely/missing"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp gone:");
    // A server that cannot start only loses its own tools, so this is a warn:
    // doctor must not fail the whole run over it.
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("/nonexistent/definitely/missing"), "{line}");
    assert!(line.contains("was not found"), "{line}");
}

#[test]
fn a_credential_preset_with_nothing_behind_it_is_reported() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"
        credentials = ["github", "aws"]
        "#,
    );
    let text = fixture.doctor(&config);

    // Neither `~/.config/gh` nor `~/.aws` exists in the fixture HOME, so both
    // presets grant nothing. Each gets its own line: a user fixing one of them
    // should not have to guess whether the other is fine.
    for (preset, wanted) in [("github", "~/.config/gh"), ("aws", "~/.aws")] {
        let warned = text
            .lines()
            .find(|line| line.contains("mcp probe credentials:") && line.contains(preset))
            .unwrap_or_else(|| panic!("no warning for {preset} in:\n{text}"));
        assert!(warned.starts_with("[warn]"), "{warned}");
        assert!(warned.contains("grants nothing"), "{warned}");
        assert!(warned.contains(wanted), "{warned}");
    }

    // ... and the grant summary must not claim it was granted.
    let summary = line(&text, "mcp probe:");
    assert!(summary.contains("aws(not found)"), "{summary}");
}

#[test]
fn a_credential_preset_that_exists_is_granted_read_only() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.home.join(".ssh")).expect("~/.ssh");
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"
        credentials = ["ssh"]
        "#,
    );
    let text = fixture.doctor(&config);
    let summary = line(&text, "mcp probe:");
    assert!(summary.contains("credentials [~/.ssh]"), "{summary}");
    assert!(!summary.contains("not found"), "{summary}");
    // A granted credential is a *read* root, so it never shows up in the write
    // half of the summary.
    let summary = line(&text, "mcp probe:");
    let write_part = summary
        .split("write [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("no write section in {summary:?}"));
    assert!(!write_part.contains(".ssh"), "write section: {write_part}");
}

#[test]
fn a_literal_secret_in_env_is_a_warning_and_a_reference_is_not() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "literal"
        command = "/usr/bin/env"
        env = { GITHUB_TOKEN = "ghp_in_the_file" }

        [[mcp.servers]]
        name = "reference"
        command = "/usr/bin/env"
        env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
        "#,
    );
    let text = fixture.doctor(&config);

    let warned = line(&text, "mcp literal env GITHUB_TOKEN:");
    assert!(warned.starts_with("[warn]"), "{warned}");
    assert!(warned.contains("literal"), "{warned}");
    // The value itself must never be echoed: doctor output gets pasted around.
    assert!(
        !text.contains("ghp_in_the_file"),
        "the secret leaked:\n{text}"
    );

    assert!(
        !text.contains("mcp reference env"),
        "a ${{VAR}} reference is the fix, not a finding:\n{text}"
    );
}

#[test]
fn sandbox_off_is_reported_as_unconfined() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "loose"
        command = "/usr/bin/env"
        sandbox = "off"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp loose:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("sandbox = \"off\""), "{line}");
    assert!(line.contains("not confined"), "{line}");
}

#[test]
fn a_globally_disabled_sandbox_does_not_claim_the_server_is_confined() {
    // The kernel builds MCP sandboxes out of `[sandbox] mode`; `[mcp] sandbox`
    // cannot turn confinement on by itself. Reporting "confine" here would be
    // the original bug all over again.
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"

        [sandbox]
        mode = "none"
        "#,
    );
    let text = fixture.doctor(&config);

    let summary = line(&text, "mcp probe:");
    assert!(summary.contains("sandbox off"), "{summary}");
    assert!(!summary.contains("sandbox confine"), "{summary}");
    let warned = line(&text, "mcp:");
    assert!(warned.starts_with("[warn]"), "{warned}");
    assert!(warned.contains("[sandbox] mode = \"none\""), "{warned}");
}

#[test]
fn disabled_servers_are_skipped_without_a_grant() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "gone"
        command = "/nonexistent/x"
        enabled = false
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp gone:");
    assert!(line.starts_with("[ok]"), "{line}");
    assert!(line.contains("disabled"), "{line}");
    // No grant section: a disabled server is never started, so listing one
    // would be noise at best and misleading at worst.
    assert!(!line.contains("runtime ["), "{line}");
}

#[test]
fn an_explicit_path_that_does_not_exist_is_reported() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"
        read_paths = ["/opt/not-installed"]
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp probe read_paths:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("/opt/not-installed"), "{line}");
    assert!(line.contains("skips it"), "{line}");
}

#[test]
fn no_mcp_section_when_no_servers_are_configured() {
    let fixture = Fixture::new();
    let config = fixture.config("[agent]\nmax_turns = 2\n");
    let text = fixture.doctor(&config);
    assert!(!text.contains("mcp"), "unexpected MCP output:\n{text}");
}
