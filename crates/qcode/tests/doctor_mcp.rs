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

// ---------------------------------------------------------------------------
// Remote servers (ADR-0067).
//
// Doctor does not connect: it reads the config and says whether a connection
// *could* succeed. These cases are the ones that fail silently otherwise.
// ---------------------------------------------------------------------------

#[test]
fn a_remote_server_reports_its_transport_and_transparency() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        allow = ["mcp.context7.com"]

        [[mcp.servers]]
        name = "context7"
        url = "https://mcp.context7.com/mcp"
        bearer_token_env_var = "QCODE_DOCTOR_TEST_TOKEN"

        [mcp.servers.headers]
        Accept = "application/json, text/event-stream"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp context7:");
    assert!(line.starts_with("[ok]"), "{line}");
    assert!(line.contains("streamable-http"), "{line}");
    assert!(line.contains("https://mcp.context7.com/mcp"), "{line}");
    // ADR-0067 §8's last item: the user must be able to confirm the traffic is
    // audited without reading the source.
    assert!(line.contains("audited"), "{line}");
    assert!(line.contains("headers [1]"), "{line}");
    // The variable *name* is fine to print; a value never is.
    assert!(line.contains("QCODE_DOCTOR_TEST_TOKEN"), "{line}");
}

#[test]
fn a_remote_server_without_proxy_says_nothing_is_audited() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = false

        [[mcp.servers]]
        name = "remote"
        url = "https://mcp.context7.com/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    let summary = line(&text, "mcp remote:");
    assert!(
        summary.contains("nothing is audited"),
        "with no proxy, `audited` would be a lie: {summary}"
    );
    let warned = line(&text, "mcp remote network:");
    assert!(warned.starts_with("[warn]"), "{warned}");
    assert!(warned.contains("no egress client"), "{warned}");
}

#[test]
fn a_remote_host_outside_the_allowlist_names_the_line_to_add() {
    // The first thing that goes wrong with a new remote server, and the error
    // the proxy produces is a bare 403 at a later moment.
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        unknown = "deny"

        [[mcp.servers]]
        name = "remote"
        url = "https://api.example.test/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp remote network:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("403"), "{line}");
    assert!(line.contains("api.example.test:443"), "{line}");
    assert!(line.contains("allow = "), "{line}");
}

#[test]
fn an_allowlisted_host_gets_no_network_warning() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        unknown = "deny"
        allow = ["api.example.test:443"]

        [[mcp.servers]]
        name = "remote"
        url = "https://api.example.test/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    assert!(
        !text.contains("mcp remote network:"),
        "an allowlisted host must not be warned about:\n{text}"
    );
}

#[test]
fn unknown_allow_is_reported_as_the_reason_it_gets_through() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        unknown = "allow"

        [[mcp.servers]]
        name = "remote"
        url = "https://api.example.test/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp remote network:");
    assert!(line.contains("unknown = \"allow\""), "{line}");
}

#[test]
fn an_unset_token_variable_is_a_warning() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        allow = ["api.example.test:443"]

        [[mcp.servers]]
        name = "remote"
        url = "https://api.example.test/mcp"
        bearer_token_env_var = "QCODE_DOCTOR_SURELY_UNSET_TOKEN"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp remote bearer_token_env_var:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("QCODE_DOCTOR_SURELY_UNSET_TOKEN"), "{line}");
}

#[test]
fn a_remote_server_gets_no_sandbox_grant_line() {
    // A remote server has no process, so there is no grant to report. Printing
    // one (or reporting "sandbox confine") would claim a boundary that does not
    // exist: the egress proxy is the only one.
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [network]
        enabled = true
        allow = ["api.example.test:443"]

        [[mcp.servers]]
        name = "remote"
        url = "https://api.example.test/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp remote:");
    assert!(!line.contains("runtime ["), "{line}");
    assert!(!line.contains("sandbox confine"), "{line}");
}

#[test]
fn an_ambiguous_transport_fails_the_config_check() {
    // Not a warning from the MCP section: `validate()` rejects the file at load
    // time, so this is a `[fail]` on the config line and doctor never gets as
    // far as describing a transport it cannot determine. That is the right
    // severity — a server with both `command` and `url` cannot run at all.
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "confused"
        command = "/usr/bin/env"
        url = "https://api.example.test/mcp"
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "config:");
    assert!(line.starts_with("[fail]"), "{line}");
    assert!(line.contains("both `command` and `url`"), "{line}");
    // ... and the exit code says so, rather than "all checks passed".
    assert!(text.contains("1 check(s) failed"), "{text}");
}

/// `~` is expanded at load time, so by the time doctor sees a path it is
/// absolute. Printing both spellings is what makes "did my `~/Documents` mean
/// what I thought" a one-line check: both statements are true of the same
/// directory, and only the pair makes that visible.
#[test]
fn a_configured_path_is_shown_with_the_tilde_form_the_config_uses() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.home.join("Documents")).expect("~/Documents");
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "wordcraft"
        command = "/usr/bin/env"
        writable_paths = ["~/Documents"]
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp wordcraft writable_paths:");
    assert!(line.starts_with("[ok]"), "{line}");
    assert!(line.contains("~/Documents"), "{line}");
    let absolute = fixture.home.join("Documents");
    assert!(line.contains(&absolute.display().to_string()), "{line}");
}

/// The silent no-op this whole change is about: `~/Documents` on a machine
/// where the desktop directory is `~/文档` grants nothing, and the server just
/// fails to write. Say which directory the tilde meant, not just "missing".
#[test]
fn a_tilde_path_that_does_not_exist_names_the_directory_it_meant() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "wordcraft"
        command = "/usr/bin/env"
        writable_paths = ["~/Documents"]
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp wordcraft writable_paths:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("does not exist"), "{line}");
    let absolute = fixture.home.join("Documents");
    assert!(line.contains(&absolute.display().to_string()), "{line}");
}

/// The write side of the danger check, and the reason it exists: `["~/"]` is
/// two characters from `["~/Documents"]`, and a server that may write anywhere
/// in `$HOME` can take effect through a `.gitconfig` hook path or a shell rc
/// line without a second write.
#[test]
fn a_writable_path_of_the_whole_home_directory_is_refused() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "greedy"
        command = "/usr/bin/env"
        writable_paths = ["~/"]
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "mcp greedy:");
    assert!(line.starts_with("[warn]"), "{line}");
    assert!(line.contains("your home directory"), "{line}");
    assert!(line.contains("writable_paths"), "{line}");
}

/// `~user` is not `~`. It stays as written, so the "must be absolute" check
/// rejects it instead of the grant silently pointing somewhere else.
#[test]
fn another_users_tilde_is_a_config_error_not_a_grant() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [[mcp.servers]]
        name = "wordcraft"
        command = "/usr/bin/env"
        writable_paths = ["~user/Documents"]
        "#,
    );
    let text = fixture.doctor(&config);
    let line = line(&text, "config:");
    assert!(line.starts_with("[fail]"), "{line}");
    assert!(line.contains("~user/Documents"), "{line}");
    assert!(line.contains("must be an absolute path"), "{line}");
}

/// ADR-0070: a bare `command` is resolved in the shell environment's `PATH`,
/// so `command = "wordcraft-cli"` survives a version upgrade that renames the
/// directory.
///
/// `env_command` is faked to a `printf` so the test controls the `PATH`
/// exactly; the real sampler is exercised by `qc shell-env`'s own tests. The
/// point here is that doctor resolves and *reports* it, not that `zsh -lic`
/// works.
#[test]
fn a_bare_command_is_resolved_in_the_shell_environments_path() {
    let fixture = Fixture::new();
    let bin = fixture.dir.path().join("cot/bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::write(bin.join("wordcraft-cli"), b"not really an elf").expect("binary");

    let config = fixture.config(&format!(
        r#"
        [shell]
        env_command = "printf 'PATH={}\\0'"

        [[mcp.servers]]
        name = "wordcraft"
        command = "wordcraft-cli"
        "#,
        bin.display()
    ));
    let text = fixture.doctor(&config);

    // The `PATH` in play is stated, not left to be inferred from a failure.
    let path_line = line(&text, "mcp path:");
    assert!(path_line.starts_with("[ok]"), "{path_line}");
    assert!(path_line.contains("cot/bin"), "{path_line}");

    // ... and the grant describes the file that would actually run.
    let grant_line = line(&text, "mcp wordcraft:");
    assert!(grant_line.starts_with("[ok]"), "{grant_line}");
    let runtime = grant_line
        .split("runtime [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("no runtime section in {grant_line:?}"));
    assert!(runtime.contains("cot/bin"), "runtime: {runtime}");
}

/// The fallback, and the warning that makes it visible. Without a snapshot the
/// lookup uses qcode's own `PATH`, which under `qc gui` from the Dock is
/// launchd's minimal one — the case that brought this up.
#[test]
fn without_a_snapshot_a_bare_command_falls_back_and_says_so() {
    let fixture = Fixture::new();
    // A name no `PATH` can plausibly hold: the test must not depend on whether
    // the toolchain that runs it happens to expose the real binary.
    let config = fixture.config(
        r#"
        [shell]
        env_sampling = "off"

        [[mcp.servers]]
        name = "wordcraft"
        command = "qcode-doctor-definitely-missing"
        "#,
    );
    let text = fixture.doctor(&config);

    let path_line = line(&text, "mcp path:");
    assert!(path_line.starts_with("[warn]"), "{path_line}");
    assert!(path_line.contains("qcode's own PATH"), "{path_line}");

    // The server cannot start, and the message says which PATH was searched —
    // "not found" alone reads as a typo.
    let grant_line = line(&text, "mcp wordcraft:");
    assert!(grant_line.starts_with("[warn]"), "{grant_line}");
    assert!(
        grant_line.contains("qcode-doctor-definitely-missing"),
        "{grant_line}"
    );
    assert!(grant_line.contains("was not found"), "{grant_line}");
    assert!(grant_line.contains("PATH"), "{grant_line}");
}

/// No `mcp path` line when nothing needs a lookup: an absolute `command` is
/// resolved from the config alone, and sampling a shell to learn nothing would
/// be a startup cost for every launch.
#[test]
fn an_absolute_command_does_not_sample_the_shell() {
    let fixture = Fixture::new();
    let config = fixture.config(
        r#"
        [shell]
        env_command = "exit 1"

        [[mcp.servers]]
        name = "probe"
        command = "/usr/bin/env"
        "#,
    );
    let text = fixture.doctor(&config);
    assert!(
        !text.contains("mcp path:"),
        "an absolute command must not need a PATH lookup:\n{text}"
    );
    let grant_line = line(&text, "mcp probe:");
    assert!(grant_line.starts_with("[ok]"), "{grant_line}");
}
