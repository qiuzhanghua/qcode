//! The environment a `run_shell` command sees (ADR-0064).
//!
//! A GUI started from the Dock inherits launchd's environment: no `JAVA_HOME`,
//! no `GOROOT`, a minimal `PATH`. The user's shell has them — they come from rc
//! files and toolchain hooks — so the cheapest correct answer is to ask that
//! shell once and reuse the answer:
//!
//! ```text
//! {shell} -lic 'env -0'     # zsh / bash / fish
//! {shell} -lc  'env -0'     # sh, dash, ash: no rc to read
//! pwsh -NoProfile ...       # Windows: -lic is POSIX-only, env does not exist
//! ```
//!
//! Things this module deliberately does **not** do:
//!
//! - filter secrets. A half-hearted `*_KEY` filter gives false confidence and
//!   misses `DATABASE_URL`. Either the snapshot is taken or it is not.
//! - sample per command. That re-runs the rc files for every tool call.
//! - log values. Reports carry names; `qc shell-env` is where values live.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// Whether `run_shell` should ask the user's shell for its environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvSampling {
    /// Sample once per session (and again when the rc files change).
    #[default]
    Auto,
    /// Keep the pre-ADR-0064 behaviour: allow-list only.
    Off,
}

/// How much of a sample goes into the session log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellAudit {
    /// Variable names and what changed — never values.
    #[default]
    Names,
    Off,
}

/// `[shell]` in the config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSettings {
    pub env_sampling: EnvSampling,
    /// Overrides the derived sampling command. `{shell}` is replaced by the
    /// detected shell, quoted for the shell that runs it.
    pub env_command: String,
    pub env_timeout_ms: u64,
    /// `0` means "do not expire on a timer"; rc changes still invalidate.
    pub env_ttl_secs: u64,
    /// Variables copied straight from this process, `PREFIX*` or an exact name.
    pub env_include: Vec<String>,
    pub env_exclude: Vec<String>,
    /// Sampling inside the sandbox sees a smaller world; default is outside it.
    pub env_sample_sandboxed: bool,
    pub audit: ShellAudit,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            env_sampling: EnvSampling::Auto,
            env_command: String::new(),
            env_timeout_ms: 5_000,
            env_ttl_secs: 600,
            env_include: vec![
                "COT_*".to_string(),
                "JAVA_HOME".to_string(),
                "GOROOT".to_string(),
                "M2_HOME".to_string(),
            ],
            env_exclude: vec![
                "_".to_string(),
                "PWD".to_string(),
                "OLDPWD".to_string(),
                "SHLVL".to_string(),
                "RANDOM".to_string(),
                "SECONDS".to_string(),
            ],
            env_sample_sandboxed: false,
            audit: ShellAudit::Names,
        }
    }
}

/// One snapshot of the user's shell environment.
#[derive(Debug)]
pub struct ShellEnv {
    vars: BTreeMap<String, String>,
    program: PathBuf,
    sampled_at: Instant,
}

impl ShellEnv {
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn sampled_at(&self) -> Instant {
        self.sampled_at
    }
}

/// What the UI and the session log get to see about a sample.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SampleReport {
    pub program: String,
    pub count: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub duration_ms: u64,
    pub cached: bool,
    pub error: Option<String>,
}

/// Parse `env -0` output.
///
/// NUL separated, `NAME=VALUE`, split on the *first* `=` so values may contain
/// more of them. Blocks without an `=` — the rc files' banners, `echo` calls,
/// prompts — are dropped, which is what makes `-lic` usable.
pub fn parse_env_nul(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for block in bytes.split(|byte| *byte == 0) {
        if block.is_empty() {
            continue;
        }
        let Ok(block) = std::str::from_utf8(block) else {
            continue;
        };
        // A banner printed before `env -0` can share the first block with the
        // first variable (`Welcome!\nPATH=/...`). Keep the last line in that
        // case instead of dropping a real variable.
        let block = match block.rsplit_once('\n') {
            Some((_, last)) if !last.is_empty() && valid_entry(last) => last,
            _ => block,
        };
        let Some((name, value)) = block.split_once('=') else {
            continue;
        };
        if !is_variable_name(name) {
            continue;
        }
        vars.insert(name.to_string(), value.to_string());
    }
    vars
}

fn valid_entry(text: &str) -> bool {
    text.split_once('=')
        .is_some_and(|(name, _)| is_variable_name(name))
}

/// A POSIX variable name, or at least a name `env` would print. `-lic` runs the
/// rc files first, so anything printed before the snapshot is noise.
fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(' ')
        && !name.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '='
        })
}

/// `PREFIX*` or an exact name.
pub fn matches_pattern(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn excluded(settings: &ShellSettings, name: &str) -> bool {
    settings
        .env_exclude
        .iter()
        .any(|pattern| matches_pattern(pattern, name))
}

/// `{shell}` in `env_command` becomes the detected program, single quoted.
fn quote_shell(program: &Path) -> String {
    let text = program.to_string_lossy();
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The command that produces the snapshot, or `None` when sampling is off.
pub fn sampling_command(
    settings: &ShellSettings,
    program: &Path,
) -> Option<(PathBuf, Vec<String>)> {
    if settings.env_sampling == EnvSampling::Off {
        return None;
    }

    let configured = settings.env_command.trim();
    if !configured.is_empty() {
        let script = configured.replace("{shell}", &quote_shell(program));
        #[cfg(windows)]
        {
            return Some((PathBuf::from("cmd.exe"), vec!["/C".to_string(), script]));
        }
        #[cfg(not(windows))]
        {
            return Some((PathBuf::from("/bin/sh"), vec!["-c".to_string(), script]));
        }
    }

    // Windows shells have no `-lic "env -0"`: pwsh rejects `-lic`, and `env`
    // does not exist. The environment lives in the registry, not a profile, so a
    // `-NoProfile` pwsh sees the same set the user sees. `[Console]::Out.Write`
    // bypasses the formatting pipeline so the `NAME=VALUE\0` blocks survive the
    // pipe; pinning `OutputEncoding` to UTF-8 also covers Windows PowerShell
    // 5.1, whose redirected stdout would otherwise be ANSI (GBK on a Chinese
    // system, which `parse_env_nul` cannot read).
    #[cfg(windows)]
    {
        let script = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; Get-ChildItem Env: | ForEach-Object { [Console]::Out.Write($_.Name + '=' + $_.Value + [char]0) }";
        Some((
            program.to_path_buf(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                script.to_string(),
            ],
        ))
    }

    // `env -0` needs a login, interactive shell to see what the user sees: `-l`
    // reads the profile, `-i` reads `.zshrc` / `.bashrc`, where the toolchain
    // hooks live. `sh` and its relatives have no rc worth reading, and `-i` there
    // only adds noise. (Windows never gets here: the pwsh branch above returns.)
    #[cfg(not(windows))]
    {
        let login_only = matches!(
            program
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
            "sh" | "dash" | "ash"
        );
        let flag = if login_only { "-lc" } else { "-lic" };
        Some((
            program.to_path_buf(),
            vec![flag.to_string(), "env -0".to_string()],
        ))
    }
}

/// Run the sampling command once.
///
/// `sandbox` is only consulted when `env_sample_sandboxed` is set: sampling inside
/// the sandbox sees a smaller world (macOS SBPL hides most paths), which is why
/// the default is outside it.
pub fn sample(
    settings: &ShellSettings,
    program: &Path,
    sandbox: Option<&dyn crate::sandbox::Sandbox>,
) -> Result<ShellEnv, String> {
    let (program, args) = sampling_command(settings, program)
        .ok_or_else(|| "environment sampling is off".to_string())?;
    let started = Instant::now();
    let mut command = match (settings.env_sample_sandboxed, sandbox) {
        (true, Some(sandbox)) => {
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
            sandbox
                .command(program.as_os_str(), &args, &cwd)
                .map_err(|error| error.to_string())?
        }
        _ => {
            let mut command = Command::new(&program);
            command.args(&args);
            command
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = run_with_timeout(
        command,
        Duration::from_millis(settings.env_timeout_ms.max(1)),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} exited with {}: {}",
            program.display(),
            output.status,
            stderr.trim()
        ));
    }

    let mut vars = parse_env_nul(&output.stdout);
    if vars.is_empty() {
        return Err(format!(
            "{} produced no environment entries",
            program.display()
        ));
    }
    vars.retain(|name, _| !excluded(settings, name));
    Ok(ShellEnv {
        vars,
        program,
        sampled_at: started,
    })
}

fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    #[cfg(unix)]
    detach_from_controlling_terminal(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the sampler: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("sampling timed out after {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Give the sampler its own session, and with it no controlling terminal.
///
/// The default sampler is `$SHELL -lic env -0`. `-i` is not decoration: `.zshrc`
/// and `.bashrc` are what define the toolchain hooks this snapshot exists to
/// capture, and a shell only reads them with `-i`. But an interactive shell also
/// opens `/dev/tty` *directly* rather than reading fd 0, so `Stdio::null()` on
/// stdin does not protect it.
///
/// That only bites when `qc` runs in a background process group of a terminal
/// session — `make check &`, `cargo test` under an IDE task runner, anything
/// piped. The `/dev/tty` read raises `SIGTTIN`, which stops the sampler *and*
/// the parent polling for it, so the `timeout` in [`run_with_timeout`] can never
/// fire: its loop is stopped too. The job hangs until someone `fg`s it.
///
/// A new session has no controlling terminal, so `/dev/tty` fails with `ENXIO`,
/// `-i` degrades to "no job control in this shell", and the sample still works.
/// Note this is deliberately *not* `process_group(0)`: a process-group leader
/// without a session still has the old session's terminal available.
#[cfg(unix)]
fn detach_from_controlling_terminal(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // SAFETY: `setsid` is async-signal-safe and touches only the forked child's
    // own session state, which is what `pre_exec` closures are restricted to.
    unsafe {
        command.pre_exec(|| {
            // Best effort, and silent on purpose: `EPERM` means the child is
            // already a process-group leader, and failing the spawn over it
            // would turn a fix for a hang into a broken sampler.
            libc::setsid();
            Ok(())
        });
    }
}

/// A file the shell reads at startup. Any change means the snapshot is stale.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn rc_files() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let zdotdir = std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .or_else(|| home.clone());
    let mut paths = Vec::new();
    if let Some(home) = home {
        for name in [
            ".zshrc",
            ".zprofile",
            ".zshenv",
            ".bash_profile",
            ".bashrc",
            ".profile",
        ] {
            paths.push(home.join(name));
        }
        paths.push(home.join(".config/qcode/config.toml"));
    }
    if let Some(zdotdir) = zdotdir {
        paths.push(zdotdir.join(".zshrc"));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn stamps() -> Vec<FileStamp> {
    rc_files()
        .into_iter()
        .map(|path| {
            let meta = std::fs::metadata(&path).ok();
            FileStamp {
                len: meta.as_ref().map(std::fs::Metadata::len).unwrap_or(0),
                modified: meta.and_then(|meta| meta.modified().ok()),
                path,
            }
        })
        .collect()
}

/// The snapshot plus the rules for when it stops being true.
#[derive(Debug)]
pub struct ShellEnvCache {
    /// The report of the last sample that no one has logged yet (ADR-0064).
    pending_audit: std::sync::Mutex<Option<SampleReport>>,
    settings: ShellSettings,
    state: RwLock<Option<Arc<ShellEnv>>>,
    report: RwLock<Option<SampleReport>>,
    stamps: Mutex<Vec<FileStamp>>,
}

impl From<&SampleReport> for qcode_core::event::ShellEnvSampled {
    fn from(report: &SampleReport) -> Self {
        Self {
            program: report.program.clone(),
            count: report.count,
            added: report.added.clone(),
            removed: report.removed.clone(),
            duration_ms: report.duration_ms,
            cached: report.cached,
            error: report.error.clone(),
        }
    }
}

impl ShellEnvCache {
    pub fn new(settings: ShellSettings) -> Self {
        Self {
            settings,
            pending_audit: std::sync::Mutex::new(None),
            state: RwLock::new(None),
            report: RwLock::new(None),
            stamps: Mutex::new(stamps()),
        }
    }

    /// The pre-ADR-0064 behaviour: no sampling, allow-list only.
    pub fn disabled() -> Self {
        Self::new(ShellSettings {
            env_sampling: EnvSampling::Off,
            ..ShellSettings::default()
        })
    }

    pub fn settings(&self) -> &ShellSettings {
        &self.settings
    }

    pub fn cached(&self) -> Option<Arc<ShellEnv>> {
        self.state.read().ok().and_then(|state| state.clone())
    }

    /// Take the report a sample produced, once.
    ///
    /// The session log wants each sample exactly once (ADR-0064), while the
    /// settings page wants the latest one whenever it renders — that is what
    /// `last_report` is for.
    pub fn take_audit(&self) -> Option<SampleReport> {
        self.pending_audit
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    pub fn last_report(&self) -> Option<SampleReport> {
        self.report.read().ok().and_then(|report| report.clone())
    }

    /// True when the next `current()` would have to run the sampler.
    pub fn needs_sampling(&self) -> bool {
        if self.settings.env_sampling == EnvSampling::Off {
            return false;
        }
        let cached = self.cached();
        match cached {
            None => true,
            Some(env) => {
                if self.settings.env_ttl_secs > 0
                    && env.sampled_at.elapsed() >= Duration::from_secs(self.settings.env_ttl_secs)
                {
                    return true;
                }
                let current = stamps();
                self.stamps
                    .lock()
                    .map(|stamps| *stamps != current)
                    .unwrap_or(false)
            }
        }
    }

    /// The snapshot, sampling it first when needed.
    ///
    /// Sampling blocks for as long as the command takes (bounded by
    /// `env_timeout_ms`), so callers on an async runtime hand the sandbox in and
    /// accept the wait only when `needs_sampling()` says so.
    pub fn current_with(
        &self,
        sandbox: Option<&dyn crate::sandbox::Sandbox>,
    ) -> Option<Arc<ShellEnv>> {
        if self.settings.env_sampling == EnvSampling::Off {
            return None;
        }
        if !self.needs_sampling() {
            if let Ok(mut report) = self.report.write()
                && let Some(report) = report.as_mut()
            {
                report.cached = true;
            }
            return self.cached();
        }
        self.refresh(sandbox)
    }

    /// Convenience for callers that have no sandbox to offer.
    pub fn current(&self) -> Option<Arc<ShellEnv>> {
        self.current_with(None)
    }

    /// Force a sample, regardless of the TTL.
    pub fn refresh(&self, sandbox: Option<&dyn crate::sandbox::Sandbox>) -> Option<Arc<ShellEnv>> {
        let program = qcode_shell::detect().program;
        let started = Instant::now();
        let previous = self.cached();
        let result = sample(&self.settings, &program, sandbox);

        let mut report = SampleReport {
            program: program.display().to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            cached: false,
            ..SampleReport::default()
        };

        let next = match result {
            Ok(env) => {
                report.count = env.len();
                report.added = names_only_in(&env, previous.as_deref());
                report.removed = names_only_in_previous(&env, previous.as_deref());
                if let Ok(mut state) = self.state.write() {
                    *state = Some(Arc::new(env));
                }
                if let Ok(mut stamps) = self.stamps.lock() {
                    *stamps = stamps_now();
                }
                self.cached()
            }
            Err(error) => {
                report.error = Some(error);
                previous
            }
        };

        // The audit line (ADR-0064 decision 6). Names only: the values are the
        // user's, and a log is the easiest place for them to leak from.
        match (&report.error, self.settings.audit) {
            (Some(error), _) => tracing::warn!(
                program = %report.program,
                duration_ms = report.duration_ms,
                %error,
                "shell environment sample failed; keeping the previous snapshot"
            ),
            (None, ShellAudit::Names) => tracing::info!(
                program = %report.program,
                count = report.count,
                added = report.added.len(),
                removed = report.removed.len(),
                duration_ms = report.duration_ms,
                "shell environment sampled"
            ),
            (None, ShellAudit::Off) => {}
        }

        if let Ok(mut slot) = self.pending_audit.lock() {
            *slot = Some(report.clone());
        }
        if let Ok(mut slot) = self.report.write() {
            *slot = Some(report);
        }
        next
    }
}

fn stamps_now() -> Vec<FileStamp> {
    stamps()
}

fn names_only_in(env: &ShellEnv, previous: Option<&ShellEnv>) -> Vec<String> {
    match previous {
        None => Vec::new(),
        Some(previous) => env
            .vars
            .keys()
            .filter(|name| !previous.vars.contains_key(*name))
            .cloned()
            .collect(),
    }
}

fn names_only_in_previous(env: &ShellEnv, previous: Option<&ShellEnv>) -> Vec<String> {
    match previous {
        None => Vec::new(),
        Some(previous) => previous
            .vars
            .keys()
            .filter(|name| !env.vars.contains_key(*name))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(command: &str) -> ShellSettings {
        ShellSettings {
            env_command: command.to_string(),
            ..ShellSettings::default()
        }
    }

    #[test]
    fn null_separated_entries_survive_spaces_and_newlines() {
        let bytes = b"A=1\0P=/usr/bin:/bin\0MULTI=one\ntwo\0";
        let vars = parse_env_nul(bytes);
        assert_eq!(vars.get("A").map(String::as_str), Some("1"));
        assert_eq!(vars.get("P").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(vars.get("MULTI").map(String::as_str), Some("one\ntwo"));
    }

    #[test]
    fn garbage_blocks_from_rc_chatter_are_dropped() {
        let bytes = b"Welcome to zsh!\nA=1\0this has no equals\0 =leading\0B=2\0";
        let vars = parse_env_nul(bytes);
        assert_eq!(vars.len(), 2);
        assert!(vars.contains_key("A"));
        assert!(vars.contains_key("B"));
    }

    #[test]
    fn later_duplicates_win() {
        let vars = parse_env_nul(b"A=1\0A=2\0");
        assert_eq!(vars.get("A").map(String::as_str), Some("2"));
    }

    #[test]
    fn values_may_contain_equals() {
        let vars = parse_env_nul(b"URL=https://example.com/?a=1\0");
        assert_eq!(
            vars.get("URL").map(String::as_str),
            Some("https://example.com/?a=1")
        );
    }

    #[test]
    fn include_patterns_match_prefixes_and_exact_names() {
        assert!(matches_pattern("COT_*", "COT_HOME"));
        assert!(!matches_pattern("COT_*", "JAVA_HOME"));
        assert!(matches_pattern("JAVA_HOME", "JAVA_HOME"));
    }

    #[test]
    fn exclude_removes_noise_but_keeps_everything_else() {
        let settings = ShellSettings::default();
        assert!(excluded(&settings, "PWD"));
        assert!(excluded(&settings, "_"));
        assert!(!excluded(&settings, "PATH"));
    }

    #[test]
    fn a_failing_sampler_reports_the_error() {
        let settings = settings_with("exit 3");
        let error = sample(&settings, Path::new("/bin/sh"), None).expect_err("should fail");
        assert!(error.contains("3"), "unexpected error: {error}");
    }

    #[test]
    fn sampling_off_returns_no_command() {
        let settings = ShellSettings {
            env_sampling: EnvSampling::Off,
            ..ShellSettings::default()
        };
        assert!(sampling_command(&settings, Path::new("/bin/zsh")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_default_command_asks_the_shell_for_a_null_separated_dump() {
        let (program, args) =
            sampling_command(&ShellSettings::default(), Path::new("/bin/zsh")).expect("command");
        assert_eq!(program, Path::new("/bin/zsh"));
        assert_eq!(args, vec!["-lic".to_string(), "env -0".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn sh_gets_the_login_flag_only() {
        let (_, args) =
            sampling_command(&ShellSettings::default(), Path::new("/bin/sh")).expect("cmd");
        assert_eq!(args, vec!["-lc".to_string(), "env -0".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_sampling_command_uses_pwsh_flags_and_null_separated_output() {
        let program = Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe");
        let (cmd, args) = sampling_command(&ShellSettings::default(), program).expect("command");
        assert_eq!(cmd, program);
        assert_eq!(
            args,
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; Get-ChildItem Env: | ForEach-Object { [Console]::Out.Write($_.Name + '=' + $_.Value + [char]0) }"
                    .to_string(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_custom_command_runs_through_sh_with_the_shell_substituted() {
        let settings = settings_with("{shell} -c 'echo Z=9'");
        let (program, args) = sampling_command(&settings, Path::new("/bin/zsh")).expect("cmd");
        assert_eq!(program, Path::new("/bin/sh"));
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("'/bin/zsh'"), "got: {args:?}");
    }
}
