//! Shell detection (ADR-0062, verification item 4).
//!
//! The cases that matter cannot be produced with real processes: a wrapper `sh`
//! sitting between the user's shell and the program, a parent chain that loops, a
//! `$SHELL` pointing at a path that no longer exists. So most of these inject a
//! fake process tree and a fake filesystem, and the last few check the real system.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use qcode_terminal::shell::{
    Choice, Probe, ProcessRow, ProcessSource, Source, detect, detect_with, enclosing_shell,
    login_shell_from, parse_ps_table,
};
// Only the Unix case below looks a bare name up on `PATH`; Windows appends `PATHEXT`
// and resolves through `qcode_terminal::shell` itself.
#[cfg(unix)]
use qcode_terminal::shell::resolve_in_path;

/// A probe whose process tree, PATH and user database are all invented.
struct Fake {
    me: u32,
    /// pid → (ppid, comm, exe)
    processes: HashMap<u32, ProcessRow>,
    /// Paths that exist and are executable.
    executable: HashSet<PathBuf>,
    env: HashMap<String, String>,
    passwd_shell: Option<PathBuf>,
}

impl Fake {
    fn new() -> Self {
        Self {
            me: 1000,
            processes: HashMap::new(),
            executable: HashSet::new(),
            env: HashMap::new(),
            passwd_shell: None,
        }
    }

    /// Add `pid` to the chain, parented by the previously added process.
    fn chain(mut self, pid: u32, comm: &str, exe: &str, parent: u32) -> Self {
        self.processes.insert(
            pid,
            ProcessRow {
                pid,
                ppid: parent,
                comm: comm.to_string(),
                exe: Some(PathBuf::from(exe)),
            },
        );
        self.executable.insert(PathBuf::from(exe));
        self
    }

    fn executable(mut self, path: &str) -> Self {
        self.executable.insert(PathBuf::from(path));
        self
    }

    fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    fn passwd_shell(mut self, path: &str) -> Self {
        self.passwd_shell = Some(PathBuf::from(path));
        self
    }
}

impl Probe for Fake {
    fn self_pid(&self) -> u32 {
        self.me
    }

    fn env(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn resolve(&self, program: &str) -> Option<PathBuf> {
        // Same logic as the real one, but against the fake filesystem.
        let candidate = Path::new(program);
        if program.contains('/') {
            return self
                .is_executable(candidate)
                .then(|| candidate.to_path_buf());
        }
        self.env
            .get("PATH")
            .unwrap_or(&String::new())
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(program))
            .find(|candidate| self.is_executable(candidate))
    }

    fn login_shell(&self) -> Option<PathBuf> {
        self.passwd_shell.clone()
    }

    fn is_executable(&self, path: &Path) -> bool {
        self.executable.contains(path)
    }

    fn processes(&self) -> &dyn ProcessSource {
        self
    }
}

impl ProcessSource for Fake {
    fn row(&self, pid: u32) -> Option<ProcessRow> {
        self.processes.get(&pid).cloned()
    }
}

fn found(choice: Choice, program: &str, source: Source) {
    assert_eq!(
        choice,
        Choice {
            program: PathBuf::from(program),
            source
        }
    );
}

#[test]
fn the_shell_the_user_is_in_wins_over_the_login_shell() {
    // The whole reason this module exists: `$SHELL` says zsh, the user is in bash.
    let fake = Fake::new()
        .env("SHELL", "/bin/zsh")
        .executable("/bin/zsh")
        .chain(1000, "qc", "/usr/bin/qc", 999)
        .chain(999, "bash", "/bin/bash", 1);

    found(detect_with(&fake, None), "/bin/bash", Source::ParentProcess);
}

#[test]
fn a_wrapper_shell_is_skipped_in_favour_of_the_real_one() {
    // Measured on this machine: `python3 → sh → qc`. `sh` is a script interpreter,
    // so taking "the nearest shell" would hand the user a dash they never use.
    let fake = Fake::new()
        .env("SHELL", "/bin/zsh")
        .executable("/bin/zsh")
        .chain(1000, "qc", "/usr/bin/qc", 501)
        .chain(501, "sh", "/bin/dash", 500)
        .chain(500, "zsh", "/usr/bin/zsh", 1);

    found(
        detect_with(&fake, None),
        "/usr/bin/zsh",
        Source::ParentProcess,
    );
}

#[test]
fn a_chain_of_only_script_interpreters_falls_through_to_the_environment() {
    // The skip must not be a dead end: if all we can see is `sh`, the login shell
    // is the best remaining answer.
    let fake = Fake::new()
        .env("SHELL", "/usr/bin/fish")
        .executable("/usr/bin/fish")
        .chain(1000, "qc", "/usr/bin/qc", 501)
        .chain(501, "sh", "/bin/sh", 500)
        .chain(500, "dash", "/bin/dash", 1);

    found(
        detect_with(&fake, None),
        "/usr/bin/fish",
        Source::Environment,
    );
}

#[test]
fn a_shell_that_no_longer_exists_is_not_offered() {
    // A rebuilt binary leaves the running process pointing at a path that is gone
    // (`/usr/bin/zsh (deleted)` on Linux). Offering it would mean a terminal that
    // cannot start.
    let fake = Fake::new()
        .env("SHELL", "/bin/bash")
        .executable("/bin/bash")
        .chain(1000, "qc", "/usr/bin/qc", 999);

    let mut fake = fake;
    fake.processes.insert(
        999,
        ProcessRow {
            pid: 999,
            ppid: 1,
            comm: "zsh".to_string(),
            // Present in the tree but absent from `executable`.
            exe: Some(PathBuf::from("/gone/bin/zsh")),
        },
    );

    found(detect_with(&fake, None), "/bin/bash", Source::Environment);
}

#[test]
fn a_deleted_executable_still_counts_as_its_shell() {
    // When the binary is replaced while running, Linux reports the link target as
    // `/usr/bin/zsh (deleted)`. That path cannot be executed, but the shell was
    // reinstalled at the clean path — so the suffix comes off and that is used.
    let mut fake = Fake::new().chain(1000, "qc", "/usr/bin/qc", 999);
    fake.executable.insert(PathBuf::from("/usr/bin/zsh"));
    fake.processes.insert(
        999,
        ProcessRow {
            pid: 999,
            ppid: 1,
            comm: "zsh".to_string(),
            exe: Some(PathBuf::from("/usr/bin/zsh (deleted)")),
        },
    );

    found(
        detect_with(&fake, None),
        "/usr/bin/zsh",
        Source::ParentProcess,
    );
}

#[test]
fn the_configured_shell_wins_when_it_resolves() {
    let fake = Fake::new()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .executable("/usr/local/bin/nu")
        .chain(999, "zsh", "/usr/bin/zsh", 1)
        .chain(1000, "qc", "/usr/bin/qc", 999);

    found(
        detect_with(&fake, Some("nu")),
        "/usr/local/bin/nu",
        Source::Configured,
    );
}

#[test]
fn a_configured_shell_that_is_not_on_the_path_falls_back() {
    // "不能让用户一个手误就开不了终端."
    let fake = Fake::new()
        .env("PATH", "/usr/bin:/bin")
        .executable("/usr/bin/zsh")
        .chain(1000, "qc", "/usr/bin/qc", 999)
        .chain(999, "zsh", "/usr/bin/zsh", 1);

    found(
        detect_with(&fake, Some("nosuchshell")),
        "/usr/bin/zsh",
        Source::ParentProcess,
    );
}

#[test]
fn an_empty_configured_shell_is_ignored() {
    let fake =
        Fake::new()
            .chain(1000, "qc", "/usr/bin/qc", 999)
            .chain(999, "zsh", "/usr/bin/zsh", 1);

    found(
        detect_with(&fake, Some("   ")),
        "/usr/bin/zsh",
        Source::ParentProcess,
    );
}

#[test]
fn a_configured_absolute_path_is_used_as_written() {
    let fake = Fake::new()
        .executable("/opt/custom/fish")
        .chain(1000, "qc", "/usr/bin/qc", 1);

    found(
        detect_with(&fake, Some("/opt/custom/fish")),
        "/opt/custom/fish",
        Source::Configured,
    );
}

#[test]
fn the_login_shell_wins_over_a_stale_environment() {
    // The shape this machine actually has: launched by an app, so no shell in the
    // parent chain, `$SHELL` still saying bash (macOS sets it from launchd), and
    // the account changed to zsh long ago. Starting bash would greet the user with
    // "The default interactive shell is now zsh".
    let fake = Fake::new()
        .env("SHELL", "/bin/bash")
        .executable("/bin/bash")
        .executable("/bin/zsh")
        .passwd_shell("/bin/zsh")
        .chain(1000, "qc", "/usr/bin/qc", 1);

    found(detect_with(&fake, None), "/bin/zsh", Source::LoginShell);
}

#[test]
fn with_nothing_at_all_the_fallback_is_bin_sh() {
    let fake = Fake::new().env("SHELL", "/bin/does-not-exist");

    found(detect_with(&fake, None), "/bin/sh", Source::Fallback);
}

#[test]
fn a_looping_parent_chain_terminates() {
    // A recycled pid or a confused table must not hang the terminal's creation.
    let fake = Fake::new()
        .env("SHELL", "/bin/bash")
        .executable("/bin/bash")
        .chain(1000, "qc", "/usr/bin/qc", 1001)
        .chain(1001, "unknown", "/usr/bin/unknown", 1000);

    found(detect_with(&fake, None), "/bin/bash", Source::Environment);
}

#[test]
fn a_parent_that_the_os_will_not_describe_ends_the_walk() {
    // `row()` returns `None` for a process that has exited; the walk stops rather
    // than guessing what came next.
    let fake = Fake::new()
        .env("SHELL", "/bin/zsh")
        .executable("/bin/zsh")
        .chain(1000, "qc", "/usr/bin/qc", 4242);

    found(detect_with(&fake, None), "/bin/zsh", Source::Environment);
    assert_eq!(enclosing_shell(&fake, 1000), None);
}

#[test]
fn the_walk_gives_up_after_a_bounded_number_of_hops() {
    // A chain of 40 shells with none matching is not a chain we should keep
    // following: it means something is wrong with the table.
    let mut fake = Fake::new();
    for pid in 1000..1040 {
        fake.processes.insert(
            pid,
            ProcessRow {
                pid,
                ppid: pid + 1,
                comm: "process".to_string(),
                exe: Some(PathBuf::from("/usr/bin/process")),
            },
        );
    }
    fake.executable.insert(PathBuf::from("/usr/bin/process"));

    assert_eq!(enclosing_shell(&fake, 1000), None);
}

#[test]
fn pwsh_counts_as_a_shell() {
    // PowerShell is a shell people live in, on any platform.
    let fake =
        Fake::new()
            .chain(1000, "qc", "/usr/bin/qc", 999)
            .chain(999, "pwsh", "/usr/bin/pwsh", 1);

    found(
        detect_with(&fake, None),
        "/usr/bin/pwsh",
        Source::ParentProcess,
    );
}

// ------------------------------------------------------------------ pure helpers

#[test]
fn passwd_is_parsed_by_uid_not_by_name() {
    let passwd = "\
root:x:0:0:root:/root:/bin/bash
alice:x:1000:1000:Alice:/home/alice:/usr/bin/zsh
bob:x:1001:1001:Bob:/home/bob:/bin/fish
";
    assert_eq!(
        login_shell_from(passwd, 1000),
        Some(PathBuf::from("/usr/bin/zsh"))
    );
    assert_eq!(
        login_shell_from(passwd, 1001),
        Some(PathBuf::from("/bin/fish"))
    );
    assert_eq!(login_shell_from(passwd, 4242), None);
}

#[test]
fn an_account_without_a_shell_is_not_a_shell() {
    // A service account must not be offered as the terminal's shell.
    let passwd = "svc:x:900:900::/var/empty:/sbin/nologin\n";
    assert_eq!(login_shell_from(passwd, 900), None);
}

#[cfg(unix)]
#[test]
fn path_lookup_respects_order_and_executability() {
    // `/nope` does not exist, `/bin` does. A lookup that ignored existence would
    // return `/nope/sh` and the terminal would fail to start.
    assert_eq!(
        resolve_in_path("sh", Some("/nope:/bin"), None),
        Some(PathBuf::from("/bin/sh"))
    );
    assert_eq!(resolve_in_path("sh", Some("/nope"), None), None);
    // An empty PATH entry means the current directory in POSIX, and is skipped
    // rather than turned into a relative path we did not mean.
    assert!(resolve_in_path("sh", Some("/nope::"), None).is_none());
}

#[test]
fn a_ps_table_is_parsed_including_names_with_spaces() {
    // Real `ps -A -o pid=,ppid=,comm=` output on Linux; on macOS the shape is the
    // same but `comm` is a full path, which the parser keeps.
    let output = "\
    1     0 systemd
  501     1 /usr/libexec/some daemon
 1000   501 zsh
 1001  1000 qc
";
    let table = parse_ps_table(output);

    assert_eq!(table[&1].ppid, 0);
    assert_eq!(table[&501].comm, "/usr/libexec/some daemon");
    assert_eq!(table[&1001].ppid, 1000);
    // A `comm` that is already a path is usable as one.
    assert_eq!(
        table[&501].exe,
        Some(PathBuf::from("/usr/libexec/some daemon"))
    );
    assert_eq!(table[&1000].exe, None, "a bare name is not a path");
}

#[test]
fn a_malformed_ps_line_is_skipped_rather_than_guessed() {
    let output = "\
not a table
 12
 13 abc zsh
 14   x  broken
 16 17 zsh
";
    let table = parse_ps_table(output);
    assert_eq!(table.keys().copied().collect::<Vec<_>>(), vec![16]);
}

// ---------------------------------------------------------------- the real system

#[test]
fn the_real_system_yields_a_shell_that_can_be_run() {
    let choice = detect();
    assert!(
        choice.program.is_absolute() || cfg!(windows),
        "detected shell should be an absolute path, got {:?}",
        choice.program
    );
    assert!(
        choice.program.exists(),
        "detected shell {:?} does not exist (source: {})",
        choice.program,
        choice.source.label()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&choice.program)
            .expect("metadata")
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "detected shell {:?} is not executable",
            choice.program
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_proc_and_ps_sources_agree_about_this_process() {
    // The `ps` parser is the only source on macOS, where CI cannot run it. Checking
    // it against /proc here means the parsing is exercised against real system
    // output on every platform.
    let from_proc = qcode_terminal::shell::system_process_row(std::process::id())
        .expect("/proc should describe this process");
    let from_ps = parse_ps_table(&ps_output()).remove(&std::process::id());

    if let Some(from_ps) = from_ps {
        assert_eq!(
            from_ps.ppid, from_proc.ppid,
            "the two sources disagree about this process's parent"
        );
    }
}

#[cfg(target_os = "linux")]
fn ps_output() -> String {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(target_os = "linux")]
#[test]
fn the_real_parent_chain_is_readable() {
    // Whatever launched the test, its parent must be visible: the search is built
    // on that, and a silently empty chain would make every case fall through to
    // `$SHELL`.
    let me = std::process::id();
    let row = qcode_terminal::shell::system_process_row(me).expect("own row");
    assert_eq!(row.pid, me);
    assert_ne!(row.ppid, 0, "the test runner should have a parent");
    assert!(
        qcode_terminal::shell::system_process_row(row.ppid).is_some(),
        "the parent {} is not readable",
        row.ppid
    );
}

#[test]
fn a_live_process_reports_the_directory_it_is_in() {
    // `process_cwd` is the "inherit where I actually am" half of ADR-0062 §3.
    // Start a shell in a temp dir, make it `cd` somewhere else, and ask again:
    // the answer has to be the new directory, not the one the child started in.
    let first = std::env::temp_dir().join(format!("qcode-cwd-a-{}", std::process::id()));
    let second = std::env::temp_dir().join(format!("qcode-cwd-b-{}", std::process::id()));
    std::fs::create_dir_all(&first).expect("first dir");
    std::fs::create_dir_all(&second).expect("second dir");

    let mut child = std::process::Command::new("/bin/sh")
        .current_dir(&first)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("shell");

    let settled = wait_for_dir(child.id(), &first);
    assert!(settled, "the shell never reported its starting directory");
    // Compare resolved paths: macOS reports `/private/var/...` where `temp_dir()`
    // says `/var/...`. The ADR accepts that loss of symlink fidelity.
    assert_eq!(
        canonical(qcode_terminal::shell::process_cwd(child.id())),
        canonical(Some(first.clone()))
    );

    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(format!("cd {}\n", second.display()).as_bytes())
        .expect("write");

    let moved = wait_for_dir(child.id(), &second);
    assert!(moved, "the shell's cwd change was not visible");
    assert_eq!(
        canonical(qcode_terminal::shell::process_cwd(child.id())),
        canonical(Some(second.clone()))
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

fn canonical(path: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    path.and_then(|path| std::fs::canonicalize(path).ok())
}

fn wait_for_dir(pid: u32, expected: &std::path::Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if canonical(qcode_terminal::shell::process_cwd(pid))
            == canonical(Some(expected.to_path_buf()))
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn a_new_terminal_skips_a_directory_that_is_gone() {
    let missing = std::env::temp_dir().join(format!("qcode-cwd-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    let home = std::env::temp_dir().join(format!("qcode-cwd-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("home");

    // An inherited directory that was deleted must not be handed to the shell:
    // it would fail to start. Fall through to the configured one, then home.
    assert_eq!(
        qcode_terminal::shell::resolve_start_cwd(Some(missing.clone()), "", Some(home.clone())),
        Some(home.clone())
    );
    assert_eq!(
        qcode_terminal::shell::resolve_start_cwd(
            Some(missing),
            home.to_str().expect("utf-8"),
            None
        ),
        Some(home.clone())
    );
    assert_eq!(
        qcode_terminal::shell::resolve_start_cwd(None, "", Some(home.clone())),
        Some(home.clone())
    );
    assert!(
        qcode_terminal::shell::resolve_start_cwd(None, "/definitely/not/here", None).is_some(),
        "the last resort is the process cwd, not None"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn this_machine_answers_with_the_configured_login_shell() {
    // The real probe, not a fake: on macOS this is the test that fails when the
    // login shell is read from `/etc/passwd` (the account is in Open Directory).
    let choice = detect();
    assert!(
        choice.program.is_absolute() && choice.program.exists(),
        "detected shell {:?} ({:?}) should be a real binary",
        choice.program,
        choice.source
    );
    eprintln!(
        "detected shell: {} ({})",
        choice.program.display(),
        choice.source.label()
    );
}

#[test]
fn the_real_probe_reads_the_login_shell_from_the_user_database() {
    // Not the parent chain and not `$SHELL`: this is what a GUI-launched qc sees,
    // and on macOS it has to come from Open Directory rather than `/etc/passwd`.
    let shell = qcode_terminal::shell::System.login_shell();
    eprintln!("login shell from the user database: {shell:?}");
    let shell = shell.expect("the account should have a login shell");
    assert!(shell.is_absolute(), "{shell:?} should be an absolute path");
}
