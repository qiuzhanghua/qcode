//! run_shell: the only tool that executes arbitrary code.
//!
//! The environment is scrubbed down to a small allow-list so API keys in the
//! parent process cannot be read by `env`. Approval follows the write-scope
//! ladder (SANDBOX-MODE.md): a call without `sandbox_permissions` runs at the
//! session's scope without a prompt; only an escalation request asks.
//!
//! The child is spawned as the leader of a new process group, so cancellation
//! and timeout can take down the whole tree (`sh`, `cargo`, `make`, workers)
//! with one signal instead of orphaning grandchildren.

use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::{ToolMetadata, ToolStatus, Usage};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use qcode_core::write_scope::WriteScope;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::path::{self, Access};
use crate::sandbox::{NoSandbox, OutsideRoots, Sandbox};

const OUTPUT_LIMIT: usize = 32 * 1024;

/// How long `run_shell` waits in the foreground before handing a command to the
/// background (ADR-0106). The `spec` advertises the same number: changing one
/// without the other is how the model ends up guessing wrong.
pub const DEFAULT_WAIT_SECS: u64 = 60;

/// How long a cancelled or timed-out group gets to exit on SIGTERM before the
/// whole group is SIGKILLed. Matches the design note in docs/design/SPEC.md.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// How long we wait for the group leader after SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long we keep draining pipes after the process is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    // Build configuration, not secrets. Without these the agent's cargo uses
    // the wrong registry cache (and may fight the sandbox).
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
];

/// Windows programs break without these: `cmd.exe` wants `ComSpec`, most
/// installers want `TEMP`, and `PATH` lookups depend on `PATHEXT`.
#[cfg(windows)]
const ENV_WINDOWS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
];

/// The program and arguments that run `command` as a shell command line.
///
/// Windows has no `sh`. Spawning one anyway made every `run_shell` fail with a
/// bare "cannot find the file" — the tool was unusable there. Use the chain the
/// design notes describe: `pwsh`, then Windows PowerShell, then `cmd.exe`.
fn shell_invocation(command: &str) -> Result<(OsString, Vec<OsString>), ToolError> {
    #[cfg(not(windows))]
    {
        // ADR-0064: same probe as the terminal, so a command typed by the model
        // runs in the shell the user actually uses (`[[ ]]`, `setopt`,
        // `pipefail`). The environment does *not* come from a login shell — the
        // snapshot in `shell_env` provides it, and re-running rc per command
        // would be slow and noisy.
        let program = qcode_shell::detect().program;
        Ok((
            program.into_os_string(),
            vec![OsString::from("-c"), OsString::from(command)],
        ))
    }
    #[cfg(windows)]
    {
        if let Some(program) = find_program(&["pwsh.exe", "pwsh"]) {
            return Ok((
                program,
                vec![
                    OsString::from("-NoLogo"),
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from(command),
                ],
            ));
        }
        if let Some(program) = find_program(&["powershell.exe"]) {
            return Ok((
                program,
                vec![
                    OsString::from("-NoLogo"),
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from(command),
                ],
            ));
        }
        if let Some(program) = find_program(&["cmd.exe"]) {
            return Ok((program, vec![OsString::from("/C"), OsString::from(command)]));
        }
        Err(ToolError::Invalid(
            "no shell on PATH (tried pwsh, powershell.exe and cmd.exe)".to_string(),
        ))
    }
}

/// Absolute path of the first `name` found on `PATH`, so the child still finds
/// its shell after `env_clear`.
#[cfg(windows)]
fn find_program(names: &[&str]) -> Option<OsString> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

pub struct RunShell {
    sandbox: Arc<dyn Sandbox>,
    /// The user's shell environment, sampled once per session (ADR-0064).
    env: Arc<crate::shell_env::ShellEnvCache>,
    /// Where a command goes when its wait budget runs out (ADR-0106). Shared
    /// with the `job_*` tools, so a job started here can be read and killed
    /// there.
    jobs: Arc<crate::jobs::JobRegistry>,
}

impl RunShell {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self::with_env(
            sandbox,
            Arc::new(crate::shell_env::ShellEnvCache::disabled()),
        )
    }

    pub fn with_env(sandbox: Arc<dyn Sandbox>, env: Arc<crate::shell_env::ShellEnvCache>) -> Self {
        Self::with_jobs(sandbox, env, Arc::new(crate::jobs::JobRegistry::new()))
    }

    pub fn with_jobs(
        sandbox: Arc<dyn Sandbox>,
        env: Arc<crate::shell_env::ShellEnvCache>,
        jobs: Arc<crate::jobs::JobRegistry>,
    ) -> Self {
        Self { sandbox, env, jobs }
    }

    pub fn unsandboxed() -> Self {
        Self::new(Arc::new(NoSandbox))
    }
}

impl Default for RunShell {
    fn default() -> Self {
        Self::unsandboxed()
    }
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
    /// Return immediately and let the command run detached (ADR-0106). For work
    /// that is known not to end on its own — a dev server, a watcher, `tail -f`.
    #[serde(default)]
    background: Option<bool>,
    /// Escalation request (SANDBOX-MODE.md): ask to run at a wider write scope.
    /// Only set when the command genuinely needs it — e.g. writes outside the
    /// workspace, direct network, or system-level changes. Must carry a
    /// `justification`; the user sees both before approving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_permissions: Option<String>,
    /// The model's reason for escalating; required with `sandbox_permissions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    justification: Option<String>,
}

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &str {
        "run_shell"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".to_string(),
            description: "Run a shell command inside the workspace. Use it for builds, \
                          tests, git and package managers; use the dedicated tools for \
                          reading and editing files. A command that outlives \
                          `timeout_secs` is not killed: it moves to the background and \
                          this returns a job handle, which `job_read` then follows."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Working directory, defaults to the workspace root"},
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600,
                        "description": "How long to wait before handing the command to the background (default 60). Not a kill deadline: see `background`."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Return as soon as the command starts instead of waiting. Use it for anything that will not exit on its own (dev server, watcher, tail -f). Read its output with job_read."
                    },
                    "sandbox_permissions": {
                        "type": "string",
                        "enum": ["read_only", "workspace_write", "full_access"],
                        "description": "Ask to run at a wider write scope. Only set this when the command needs it (writes outside the workspace, direct network, system-level changes); you will be prompted for approval. Omit it for normal workspace commands."
                    },
                    "justification": {
                        "type": "string",
                        "description": "Required with sandbox_permissions: the reason the user will see before approving."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, args: &Value) -> ApprovalClass {
        // A call that asks to widen its write scope is the only one that
        // prompts (SANDBOX-MODE.md); everything else runs at the session's
        // scope without a per-command question.
        if args
            .get("sandbox_permissions")
            .and_then(Value::as_str)
            .is_some()
        {
            ApprovalClass::Escalation
        } else {
            ApprovalClass::Shell
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;

        if args.command.trim().is_empty() {
            return Err(ToolError::Invalid("command is empty".to_string()));
        }

        // An escalation request must name a valid scope and say why. The scope
        // itself is enforced by the approval flow (and, in a future step, by
        // the sandbox backend mapping); here we only reject malformed calls.
        if let Some(scope) = &args.sandbox_permissions {
            let parsed = WriteScope::parse(scope).ok_or_else(|| {
                ToolError::Invalid(format!("unknown sandbox_permissions: {scope}"))
            })?;
            if args
                .justification
                .as_deref()
                .is_none_or(|text| text.trim().is_empty())
            {
                return Err(ToolError::Invalid(
                    "sandbox_permissions requires a non-empty justification".to_string(),
                ));
            }
            tracing::info!(scope = parsed.label(), "run_shell escalation approved");
        }
        let cwd = match args.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => path::resolve_ctx(ctx, cwd, Access::Read)?.path,
            _ => ctx.shell_cwd().ok_or_else(|| {
                ToolError::Invalid(
                    "no root and no turn temp: there is nowhere to run the command".to_string(),
                )
            })?,
        };
        let timeout_secs = args.timeout_secs.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, 600);
        // The initial sandbox is built for the legacy launch workspace, but a
        // task's roots may point somewhere else (a working directory selected
        // in the GUI). Rebuild from the current root set whenever roots exist,
        // so grants and write scope follow the task's actual working directory
        // instead of the directory qc happened to start from.
        let needs_roots = !ctx.roots.is_empty() || ctx.temp_dir.is_some();
        let sandbox = if needs_roots {
            // `run_shell` is the caller that needs the build caches: a build
            // inside the workspace writes to CARGO_HOME / ~/.cargo, and it may
            // write to the host /tmp. An MCP server explicitly does not (see
            // `OutsideRoots`).
            self.sandbox
                .with_roots(
                    &ctx.roots,
                    ctx.temp_dir.as_deref(),
                    OutsideRoots::BuildCaches,
                )
                .map_err(|err| ToolError::Sandbox(err.to_string()))?
        } else {
            self.sandbox.clone()
        };

        if ctx.cancel.is_cancelled() {
            return Ok(ToolOutput::cancelled(format!(
                "{}\n",
                qcode_i18n::tr("tool.cancelled")
            )));
        }

        let (program, shell_args) = shell_invocation(&args.command)?;
        // The sandbox builds the real `Command`. On error we fail closed: the
        // command is never executed without the configured confinement.
        let mut command = tokio::process::Command::from(
            sandbox
                .command(program.as_os_str(), &shell_args, &cwd)
                .map_err(|err| ToolError::Sandbox(err.to_string()))?,
        );
        command
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            // New process group led by this child. Negative pids in
            // `signal_group` then address the shell and everything it started.
            command.process_group(0);
        }
        // ...and if qcode itself dies without reaching its exit hook (a crash,
        // an OOM kill, `kill -9`), the kernel takes this child down with it. The
        // hook in `child_groups` covers the orderly way out; this covers the
        // rest.
        //
        // **Linux only**, and not for lack of trying: `prctl` exists only under
        // libc's `linux_like`, so `PR_SET_PDEATHSIG` has no macOS spelling at
        // all — the constant is not defined for Apple targets. macOS gets a
        // guard process instead (`procguard`), Windows a job object
        // (`win_job`); both are attached elsewhere because neither is a
        // per-child flag set before `exec`.
        //
        // Scope, so nobody is surprised later: `PR_SET_PDEATHSIG` is not
        // inherited across `fork`, so it reaches the group leader (the sandbox
        // wrapper, or `sh`) and stops there. A grandchild is reparented and
        // survives *this mechanism* — which is why Linux also registers the
        // group with `procguard`, whose `killpg` does reach the grandchildren.
        // The two layers are not redundant: this one needs no other process to
        // be alive and fires without waiting, and it keeps working if the guard
        // has been killed.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                // SAFETY: `prctl` is async-signal-safe and only affects the
                // calling (just forked) process. Nothing here allocates, which
                // is what makes it legal between fork and exec.
                //
                // A failure is not worth failing the command over: without it
                // qcode behaves exactly as it did before this line.
                libc::prctl(libc::PR_SET_PDEATHSIG, SIGKILL);
                Ok(())
            });
        }

        command.env_clear();
        for key in ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in ENV_WINDOWS {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        // The user's shell environment sits *above* the allow-list: it is what
        // `c`, nvm and friends put there, and the allow-list cannot know about
        // it. Values from this process (`env_include`) come next, and the two
        // things the tool itself depends on — the temp dir and the network
        // proxy — win over all of it (ADR-0064).
        if let Some(snapshot) = self.env.current_with(Some(self.sandbox.as_ref())) {
            for (key, value) in snapshot.vars() {
                command.env(key, value);
            }
        }
        for pattern in &self.env.settings().env_include {
            for (key, value) in std::env::vars() {
                if crate::shell_env::matches_pattern(pattern, &key) {
                    command.env(key, value);
                }
            }
        }
        if let Some(temp) = &ctx.temp_dir {
            command
                .env("TMPDIR", temp)
                .env("TMP", temp)
                .env("TEMP", temp);
        }
        if let Some(network) = &ctx.network {
            for (key, value) in network.env() {
                command.env(key, value);
            }
        }
        // 启动标记不许传给用户的 shell（ADR-0089 §5）。
        //
        // 它上面的三步都可能把它带回来：用户 rc 里自己 export 过、`env_include`
        // 配了 `QCODE_*`、或者一份在本次改动之前采样的旧快照。正常路径上
        // `boot::consume_env_marker()` 已经在启动时把它删了，这一行是边界处的
        // 明确表态——「这里是构造**用户**环境的地方，我们的内部标记到此为止」。
        command.env_remove(qcode_core::launch::GUI_MARKER_ENV);

        let mut child = command.spawn()?;
        let pid = child.id().unwrap_or(0);
        // Registered for the exit hook from here until the command is reaped:
        // if the process dies between this line and the drop, the group is still
        // in the registry and `kill_all` takes it out. See `child_groups`.
        //
        // On the detach path this guard is moved into the job rather than
        // dropped: that is the whole of what "a background job is still
        // cleaned up when qcode quits" amounts to (ADR-0106 §5).
        #[cfg(not(windows))]
        let live_group = crate::child_groups::LiveGroup::register(pid);
        // Windows registers a job object rather than a bare pid, and it has to
        // happen here: the job is what the kernel kills when qcode dies, so it
        // must exist before this child can start anything of its own.
        //
        // `raw_handle` rather than `AsRawHandle`: `tokio::process::Child` has no
        // trait impl for it, only the inherent method, which also answers `None`
        // once the child has exited (nothing left to contain, then).
        #[cfg(windows)]
        let live_group = crate::child_groups::LiveGroup::register_in_job(pid, child.raw_handle());

        let stdout = child
            .stdout
            .take()
            .expect("stdout is piped, so the handle exists");
        let stderr = child
            .stderr
            .take()
            .expect("stderr is piped, so the handle exists");
        // Pumped into a ring rather than read to completion: the same buffer has
        // to keep working if this call detaches, and a reader that only answered
        // at EOF could not say what a still-running command has printed so far.
        // With eviction off (`Stream::new`) the foreground result is
        // byte-for-byte what it was before (ADR-0106).
        let out = crate::jobs::Stream::new();
        let err = crate::jobs::Stream::new();
        let stdout_task = tokio::spawn(pump(stdout, out.clone()));
        let stderr_task = tokio::spawn(pump(stderr, err.clone()));

        // Reap on a task of our own so the select below never has to hold
        // `child`; the pid (captured above) is all the kill path needs. On the
        // detach path the receiver moves into the job, and `kill_on_drop` never
        // fires early because this task keeps owning the child until it exits.
        let (status_tx, mut status_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = status_tx.send(child.wait().await);
        });

        let mut exit_status = None;
        let mut cancelled_by_user = false;
        // `background` means "do not wait at all"; it uses the same hand-over
        // path as a spent budget, so there is one detach implementation.
        let background = args.background.unwrap_or(false);
        let mut handed_over = background;

        if !handed_over {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => cancelled_by_user = true,
                result = tokio::time::timeout(Duration::from_secs(timeout_secs), &mut status_rx) => {
                    match result {
                        Ok(Ok(Ok(status))) => exit_status = Some(status),
                        Ok(Ok(Err(err))) => return Err(err.into()),
                        // The wait task went away without a status; nothing to reap.
                        Ok(Err(_recv)) => {}
                        // The budget ran out. This is not a kill order any more:
                        // the command keeps running and the model gets a handle
                        // for it. Cancelling by hand still kills — see below.
                        Err(_elapsed) => handed_over = true,
                    }
                }
            }
        }

        // Cancelling the turn is an explicit "stop", so it stays a kill. It is
        // checked before the hand-over, which is why a cancelled call can never
        // end up as a running job.
        if cancelled_by_user {
            signal_group(pid, SIGTERM);
            match tokio::time::timeout(TERM_GRACE, &mut status_rx).await {
                Ok(Ok(Ok(status))) => exit_status = Some(status),
                _ => {
                    // Something in the group ignored SIGTERM (or the shell is
                    // gone but a worker is not); SIGKILL takes the whole group.
                    signal_group(pid, SIGKILL);
                    if let Ok(Ok(Ok(status))) =
                        tokio::time::timeout(KILL_GRACE, &mut status_rx).await
                    {
                        exit_status = Some(status);
                    }
                }
            }
        }

        if handed_over && !cancelled_by_user {
            // Advance the cursors past what this result shows, so the first
            // `job_read` does not replay it; only then bound the buffers, since
            // evicting before reading would lose exactly the bytes the model is
            // about to be handed.
            let (seen_out, _) = out.read(crate::jobs::READ_LIMIT);
            let (seen_err, _) = err.read(crate::jobs::READ_LIMIT);
            out.cap(crate::jobs::STREAM_CAP);
            err.cap(crate::jobs::STREAM_CAP);

            let combined = combine(&seen_out, &seen_err);
            let job = self.jobs.detach(crate::jobs::Detached {
                pid,
                command: args.command.clone(),
                cwd,
                group: live_group,
                status: status_rx,
                out: out.clone(),
                err: err.clone(),
            });

            let mut text = String::new();
            match job {
                Ok(job) => {
                    let seconds = timeout_secs.to_string();
                    text.push_str(&format!(
                        "{}\n",
                        if background {
                            qcode_i18n::trp("tool.jobs.started", &[job.id()])
                        } else {
                            qcode_i18n::trp("tool.jobs.detached", &[seconds.as_str(), job.id()])
                        }
                    ));
                    text.push_str(&format!(
                        "{}\n",
                        qcode_i18n::trp("tool.jobs.hint", &[job.id()])
                    ));
                }
                Err(crate::jobs::Rejected::TooManyLive(max)) => {
                    // `detach` has already killed the group: with no slot for the
                    // job, letting it run is the leak the cap exists to prevent.
                    text.push_str(&format!(
                        "{}\n",
                        qcode_i18n::trp("tool.jobs.too_many", &[max.to_string().as_str()])
                    ));
                }
            }
            text.push_str(&combined);

            let raw = text;
            let (text, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);
            return Ok(ToolOutput {
                status: ToolStatus::Ok,
                output: text,
                metadata: ToolMetadata {
                    truncated,
                    bytes: combined.len() as u64,
                    exit_code: None,
                    ..ToolMetadata::default()
                },
                usage: Usage::default(),
                cost_usd: 0.0,
                full_output: truncated.then(|| raw.clone()),
                images: Vec::new(),
                diff: None,
            });
        }

        drain(stdout_task).await;
        drain(stderr_task).await;

        let stdout_bytes = out.contents();
        let stderr_bytes = err.contents();

        let combined = combine(&stdout_bytes, &stderr_bytes);

        let exit_code = exit_status.and_then(|status| status.code());
        let mut text = String::new();
        if cancelled_by_user {
            text.push_str(&format!("{}\n", qcode_i18n::tr("tool.cancelled")));
        } else {
            let code = exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| qcode_i18n::tr("tool.shell_result.signal").into_owned());
            text.push_str(&format!(
                "{}\n",
                qcode_i18n::trp("tool.shell_result.exit_code", &[code.as_str()])
            ));
        }
        text.push_str(&combined);

        let raw = text;
        let (text, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);
        let status = if cancelled_by_user {
            ToolStatus::Cancelled
        } else {
            ToolStatus::Ok
        };

        Ok(ToolOutput {
            status,
            output: text,
            metadata: ToolMetadata {
                truncated,
                bytes: combined.len() as u64,
                exit_code,
                ..ToolMetadata::default()
            },
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: truncated.then(|| raw.clone()),
            images: Vec::new(),
            diff: None,
        })
    }
}

/// Feed one pipe into a ring as it arrives.
///
/// No error path: a failed read means the pipe has nothing more to say, which is
/// the same thing EOF means. What the command already printed is what matters.
async fn pump(mut reader: impl tokio::io::AsyncRead + Unpin, stream: crate::jobs::Stream) {
    let mut chunk = vec![0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => stream.append(&chunk[..read]),
        }
    }
}

/// Wait for a pipe reader to finish, but never let a leaked grandchild holding
/// the pipe open keep the tool call alive forever.
async fn drain(mut task: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err() {
        task.abort();
    }
}

/// stdout then stderr, with a newline between them when stdout did not end in
/// one.
///
/// Shared by the finished-command result and the "here is what it has said so
/// far" preamble of a detach, so a command that moves to the background reads
/// the same way one that finishes does. The newline rule is the original one —
/// `run_shell`'s output for a foreground command is unchanged by ADR-0106.
fn combine(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut combined = String::new();
    combined.push_str(&stdout);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    combined
}

#[cfg(unix)]
pub(crate) const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
pub(crate) const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub(crate) const SIGTERM: i32 = 15;
#[cfg(not(unix))]
pub(crate) const SIGKILL: i32 = 9;

/// Signal the process group led by `pid`.
///
/// Negative pids address the group; the child became its leader because it was
/// spawned with `process_group(0)`. A bare pid would only reach `sh`, leaving
/// `cargo`/`make`/`node` grandchildren running after the turn was cancelled.
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: i32) {
    if pid == 0 {
        return;
    }
    // SAFETY: `kill` is async-signal-safe and only touches the given group.
    // ESRCH is expected when the group has already exited.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

/// Windows has no process groups. The command's tree is its job object
/// (`win_job`), so terminating the job is the whole-tree kill, and it is one
/// syscall instead of spawning `taskkill` and waiting on it.
///
/// `taskkill /T /F` remains as the fallback for a `pid` that has no job: a
/// registration that could not be set up, or a pid that was never registered. It
/// walks the child tree by pid, which is worse — it races the tree changing shape
/// and it costs a process — but it is the only thing left for that case.
///
/// The `signal` argument is ignored because a job has no gentle form. That was
/// already true of `taskkill /F`, so the SIGTERM-then-SIGKILL escalation in the
/// cancellation path behaves exactly as it did before: the second step is
/// what actually lands.
#[cfg(not(unix))]
pub(crate) fn signal_group(pid: u32, _signal: i32) {
    if pid == 0 {
        return;
    }
    // Gated to `windows` rather than left bare: this function is
    // `cfg(not(unix))`, which is wider than the set of platforms that have job
    // objects, and it has to keep compiling for any of them.
    #[cfg(windows)]
    if crate::child_groups::terminate_job(pid) {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_rootless_task_runs_in_its_turn_temp() {
        let root = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let ctx = qcode_core::tool::ToolContext::for_test(root.path())
            .with_roots(Vec::new())
            .with_temp_dir(temp.path().to_path_buf());
        assert_eq!(ctx.shell_cwd().expect("temp"), temp.path());

        let primary = qcode_core::task::Root {
            id: "r1".to_string(),
            label: "app".to_string(),
            path: root.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        };
        let ctx = ctx.with_roots(vec![primary]);
        assert_eq!(ctx.shell_cwd().expect("root"), root.path());
    }
    use super::*;

    #[test]
    fn the_command_line_reaches_the_shell_verbatim() {
        let command = "echo hi && echo bye";
        let (program, args) = shell_invocation(command).expect("a shell");
        assert!(!program.is_empty());
        let joined: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            joined.iter().any(|arg| arg == command),
            "{joined:?} should carry the command unchanged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_runs_the_command_through_the_detected_shell() {
        // ADR-0064: the same probe as the terminal, so `[[ ]]` and `pipefail`
        // behave the way the user's own shell behaves.
        let detected = qcode_shell::detect().program;
        let (program, args) = shell_invocation("true").expect("a shell");
        assert_eq!(program, detected.into_os_string());
        assert_eq!(args, vec![OsString::from("-c"), OsString::from("true")]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_a_real_shell_on_path() {
        let (program, _) = shell_invocation("echo hi").expect("pwsh, powershell or cmd");
        let name = program.to_string_lossy().to_lowercase();
        assert!(
            name.ends_with("pwsh.exe")
                || name.ends_with("powershell.exe")
                || name.ends_with("cmd.exe"),
            "{name} is not a shell"
        );
        // An absolute path, because the child runs with a cleared environment.
        assert!(std::path::Path::new(&program).is_absolute(), "{name}");
    }

    #[test]
    fn a_call_without_permissions_is_a_plain_shell_call() {
        let ctx = ToolContext::for_test(".");
        let args = json!({ "command": "ls" });
        assert_eq!(
            RunShell::unsandboxed().approval(&ctx, &args),
            ApprovalClass::Shell
        );
    }

    #[test]
    fn an_escalation_request_is_classified_as_such() {
        let ctx = ToolContext::for_test(".");
        let args = json!({
            "command": "sudo make install",
            "sandbox_permissions": "full_access",
            "justification": "installs a system package"
        });
        assert_eq!(
            RunShell::unsandboxed().approval(&ctx, &args),
            ApprovalClass::Escalation
        );
    }

    #[tokio::test]
    async fn escalation_requires_a_non_empty_justification() {
        let ctx = ToolContext::for_test(".");
        let args = json!({ "command": "echo hi", "sandbox_permissions": "full_access" });
        let err = RunShell::unsandboxed()
            .execute(&ctx, args)
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("justification"));
    }

    #[tokio::test]
    async fn an_unknown_scope_is_rejected_before_it_runs() {
        let ctx = ToolContext::for_test(".");
        let args = json!({
            "command": "echo hi",
            "sandbox_permissions": "sudo",
            "justification": "nope"
        });
        let err = RunShell::unsandboxed()
            .execute(&ctx, args)
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("unknown sandbox_permissions"));
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    /// ADR-0089 §5：启动标记不许出现在 `run_shell` 的子进程里。
    ///
    /// 这一条防的不是「我们自己忘了删」——`boot::consume_env_marker()` 在正常
    /// 启动路径上已经删了。它防的是标记**绕回来**：用户的 rc 里自己 export 过、
    /// `env_include` 配了 `QCODE_*`、或一份采样于改动之前的旧快照。三者都在
    /// 构造用户环境的那三步里，所以边界处必须无条件剔掉。
    #[tokio::test]
    async fn the_launch_marker_never_reaches_the_shell() {
        // 故意把它放进本进程环境，模拟「绕回来」的情形。`cargo test` 的用例是
        // 并发线程，写进程级环境不安全，所以只在没有别的用例读它时用 unsafe；
        // 这里通过 `env_include` 更干净地复现同一条路径——见下。
        let marker = qcode_core::launch::GUI_MARKER_ENV;
        let root = tempfile::tempdir().expect("root");

        // `env_include` 是「从本进程环境挑变量传给 shell」那一步：让 `QCODE_*`
        // 命中，标记就会在**边界的那三步之末**被加进 command。这正是要测的路径。
        let settings = crate::shell_env::ShellSettings {
            env_sampling: crate::shell_env::EnvSampling::Off,
            env_include: vec!["QCODE_*".to_string()],
            ..Default::default()
        };
        let env_cache = std::sync::Arc::new(crate::shell_env::ShellEnvCache::new(settings));
        {
            // SAFETY: 只有本模块的用例读这个变量；置值-运行-清除都在同一个
            // 串行块里，且 `run_shell` 的 `env_remove` 会让结果与并发无关。
            unsafe { std::env::set_var(marker, "1") };
        }
        let shell = RunShell::with_env(Arc::new(NoSandbox), env_cache);
        let ctx = qcode_core::tool::ToolContext::for_test(root.path());
        let command = format!("printf '%s' \"${{{marker}-unset}}\"");
        let output = shell
            .execute(&ctx, serde_json::json!({ "command": command }))
            .await
            .expect("execute");
        {
            // SAFETY: 同上。
            unsafe { std::env::remove_var(marker) };
        }

        let text = output.output;
        assert!(
            text.contains("unset"),
            "the shell inherited our launch marker: {text}"
        );
    }
}
