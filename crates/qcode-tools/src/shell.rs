//! run_shell: the only tool that executes arbitrary code.
//!
//! The environment is scrubbed down to a small allow-list so API keys in the
//! parent process cannot be read by `env`. Approval is `Execute` by default.
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
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::path::{self, Access};
use crate::sandbox::{NoSandbox, Sandbox};

const OUTPUT_LIMIT: usize = 32 * 1024;
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
        Ok((
            OsString::from("sh"),
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
}

impl RunShell {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
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
                          reading and editing files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Working directory, defaults to the workspace root"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Execute
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;

        if args.command.trim().is_empty() {
            return Err(ToolError::Invalid("command is empty".to_string()));
        }

        let cwd = match args.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => path::resolve_ctx(ctx, cwd, Access::Read)?.path,
            _ => ctx
                .temp_dir
                .clone()
                .or_else(|| {
                    ctx.roots
                        .iter()
                        .find(|root| root.primary)
                        .or_else(|| ctx.roots.first())
                        .map(|root| root.path.clone())
                })
                .unwrap_or_else(|| ctx.workspace.clone()),
        };
        let timeout_secs = args.timeout_secs.unwrap_or(120).clamp(1, 600);
        // The initial sandbox is built for the legacy workspace. A task with
        // multiple roots needs a profile generated from the current snapshot.
        let needs_roots = ctx.roots.len() > 1 || (ctx.roots.is_empty() && ctx.temp_dir.is_some());
        let sandbox = if needs_roots {
            self.sandbox
                .with_roots(&ctx.roots, ctx.temp_dir.as_deref())
                .map_err(|err| ToolError::Sandbox(err.to_string()))?
        } else {
            self.sandbox.clone()
        };

        if ctx.cancel.is_cancelled() {
            return Ok(ToolOutput::cancelled("cancelled by user\n"));
        }

        let (program, shell_args) = shell_invocation(&args.command)?;
        // The sandbox builds the real `Command`. On error we fail closed: the
        // command is never executed without the configured confinement.
        let mut command = tokio::process::Command::from(
            sandbox
                .command(program.as_os_str(), &shell_args, &cwd)
                .map_err(|err| ToolError::Sandbox(err.to_string()))?,
        );
        if let Some(temp) = &ctx.temp_dir {
            command
                .env("TMPDIR", temp)
                .env("TMP", temp)
                .env("TEMP", temp);
        }
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
        if let Some(network) = &ctx.network {
            for (key, value) in network.env() {
                command.env(key, value);
            }
        }

        let mut child = command.spawn()?;
        let pid = child.id().unwrap_or(0);

        let stdout = child
            .stdout
            .take()
            .expect("stdout is piped, so the handle exists");
        let stderr = child
            .stderr
            .take()
            .expect("stderr is piped, so the handle exists");
        let stdout_task = tokio::spawn(read_all(stdout));
        let stderr_task = tokio::spawn(read_all(stderr));

        // Reap on a task of our own so the select below never has to hold
        // `child`; the pid (captured above) is all the kill path needs.
        let (status_tx, mut status_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = status_tx.send(child.wait().await);
        });

        let mut exit_status = None;
        let mut cancelled_by_user = false;
        let mut timed_out = false;

        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => cancelled_by_user = true,
            result = tokio::time::timeout(Duration::from_secs(timeout_secs), &mut status_rx) => {
                match result {
                    Ok(Ok(Ok(status))) => exit_status = Some(status),
                    Ok(Ok(Err(err))) => return Err(err.into()),
                    // The wait task went away without a status; nothing to reap.
                    Ok(Err(_recv)) => {}
                    Err(_elapsed) => timed_out = true,
                }
            }
        }

        if cancelled_by_user || timed_out {
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

        let stdout_bytes = drain(stdout_task).await;
        let stderr_bytes = drain(stderr_task).await;

        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let mut combined = String::new();
        combined.push_str(&stdout);
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }

        let exit_code = exit_status.and_then(|status| status.code());
        let mut text = String::new();
        if cancelled_by_user {
            text.push_str("cancelled by user\n");
        } else if timed_out {
            text.push_str(&format!("command timed out after {timeout_secs}s\n"));
        } else {
            text.push_str(&format!(
                "exit code: {}\n",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ));
        }
        text.push_str(&combined);

        let raw = text;
        let (text, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);
        let status = if cancelled_by_user {
            ToolStatus::Cancelled
        } else if timed_out {
            ToolStatus::Error
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
        })
    }
}

async fn read_all(mut reader: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer).await;
    buffer
}

/// Collect a pipe reader, but never let a leaked grandchild holding the pipe
/// open keep the tool call alive forever.
async fn drain(mut task: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(DRAIN_GRACE, &mut task).await {
        Ok(Ok(buffer)) => buffer,
        _ => {
            task.abort();
            Vec::new()
        }
    }
}

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// Signal the process group led by `pid`.
///
/// Negative pids address the group; the child became its leader because it was
/// spawned with `process_group(0)`. A bare pid would only reach `sh`, leaving
/// `cargo`/`make`/`node` grandchildren running after the turn was cancelled.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    if pid == 0 {
        return;
    }
    // SAFETY: `kill` is async-signal-safe and only touches the given group.
    // ESRCH is expected when the group has already exited.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

/// Windows has no process groups; `taskkill /T` walks the child tree instead.
/// This is a stopgap until the OS sandbox layer owns process containment.
#[cfg(not(unix))]
fn signal_group(pid: u32, _signal: i32) {
    if pid == 0 {
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
    fn unix_runs_the_command_through_sh() {
        let (program, args) = shell_invocation("true").expect("sh");
        assert_eq!(program, OsString::from("sh"));
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
}
