//! Sandbox abstraction for process execution.
//!
//! v1 keeps "no sandbox + approval" as the default (ADR-0005). This module adds
//! the seam for an OS sandbox and one experimental backend:
//!
//! - [`NoSandbox`]: the default; approval and workspace path checks are the
//!   boundary.
//! - [`SandboxExec`] (macOS): wraps `run_shell` in `sandbox-exec` with a
//!   generated profile. It is opt-in because nested `sandbox-exec` calls return
//!   EPERM and because the profile is still experimental. Enabling it runs a
//!   preflight; a failure aborts startup instead of silently running
//!   unsandboxed.
//!
//! Linux (bubblewrap) is intentionally not implemented yet: it needs an
//! AppArmor profile on Ubuntu 24.04 and cannot be validated from a macOS host.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox `{0}` is not supported on this platform")]
    Unsupported(&'static str),
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    #[error("sandbox `{name}` failed its preflight: {detail}")]
    Preflight { name: &'static str, detail: String },
}

/// Selected by `[sandbox] mode` in the config file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// No OS sandbox; approval is the execution boundary.
    #[default]
    None,
    /// macOS `sandbox-exec` with a generated profile (experimental).
    SandboxExec,
}

impl SandboxMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

pub trait Sandbox: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;

    /// Human-readable description for `qc doctor` and logs.
    fn describe(&self) -> String;

    /// Build the command that runs `program args...` in `cwd`.
    ///
    /// Implementations may reject the call; callers must not fall back to an
    /// unconfined command on error.
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError>;

    /// Probe that the sandbox can actually be applied. Default: no-op.
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Build the configured sandbox. `SandboxExec` is preflighted here so a broken
/// sandbox fails before any tool runs.
pub fn build(
    mode: SandboxMode,
    workspace: &Path,
    network: bool,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    match mode {
        SandboxMode::None => Ok(Arc::new(NoSandbox)),
        SandboxMode::SandboxExec => {
            #[cfg(target_os = "macos")]
            {
                let sandbox = SandboxExec::new(workspace, network)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (workspace, network);
                Err(SandboxError::Unsupported("sandbox-exec"))
            }
        }
    }
}

/// The default: run the command as-is, subject to approval.
#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn describe(&self) -> String {
        "no OS sandbox; approval is the execution boundary".to_string()
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd);
        Ok(command)
    }
}

/// macOS `sandbox-exec` backend.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct SandboxExec {
    profile: String,
    network: bool,
}

#[cfg(target_os = "macos")]
impl SandboxExec {
    const EXEC: &'static str = "/usr/bin/sandbox-exec";

    pub fn new(workspace: &Path, network: bool) -> Result<Self, SandboxError> {
        Ok(Self {
            profile: profile_for(workspace, network)?,
            network,
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }
}

#[cfg(target_os = "macos")]
impl Sandbox for SandboxExec {
    fn name(&self) -> &'static str {
        "sandbox-exec"
    }

    fn describe(&self) -> String {
        format!(
            "sandbox-exec (network {})",
            if self.network { "allowed" } else { "denied" }
        )
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        let mut command = Command::new(Self::EXEC);
        command
            .arg("-p")
            .arg(&self.profile)
            .arg(program)
            .args(args)
            .current_dir(cwd);
        Ok(command)
    }

    fn check(&self) -> Result<(), SandboxError> {
        let output = Command::new(Self::EXEC)
            .arg("-p")
            .arg(&self.profile)
            .arg("/usr/bin/true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| SandboxError::Setup(format!("cannot run {}: {err}", Self::EXEC)))?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("exit code {:?}", output.status.code())
        } else {
            detail.to_string()
        };
        Err(SandboxError::Preflight {
            name: "sandbox-exec",
            detail,
        })
    }
}

/// Build the SBPL profile.
///
/// Read access covers the system toolchain; writes are limited to the workspace,
/// temporary directories and the Cargo/Rustup homes (needed for builds). Network
/// is denied unless explicitly enabled.
#[cfg(target_os = "macos")]
fn profile_for(workspace: &Path, network: bool) -> Result<String, SandboxError> {
    if workspace == Path::new("/") {
        return Err(SandboxError::Setup(
            "refusing to sandbox the filesystem root as the workspace".to_string(),
        ));
    }

    let mut readable: Vec<PathBuf> = vec![
        "/usr".into(),
        "/System".into(),
        "/Library".into(),
        "/bin".into(),
        "/sbin".into(),
        "/dev".into(),
        "/private/etc".into(),
        "/private/var/db".into(),
        "/private/var/select".into(),
        "/private/var/folders".into(),
        "/var/folders".into(),
        "/private/tmp".into(),
        "/tmp".into(),
        "/opt/homebrew".into(),
        "/usr/local".into(),
    ];
    readable.push(workspace.to_path_buf());

    let mut writable: Vec<PathBuf> = vec![
        workspace.to_path_buf(),
        "/private/tmp".into(),
        "/tmp".into(),
        "/private/var/folders".into(),
        "/var/folders".into(),
    ];
    for key in ["CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                readable.push(path.clone());
                writable.push(path);
            }
        }
    }

    let mut profile = String::from("(version 1)\n(deny default)\n");
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow signal (target self))\n");
    profile.push_str("(allow sysctl-read)\n");

    profile.push_str("(allow file-read*\n");
    // Path resolution itself needs the root directory; without this, dyld
    // aborts the child before it can even start (found while preflighting).
    profile.push_str("    (literal \"/\")\n");
    for path in &readable {
        profile.push_str(&format!("    (subpath {})\n", sbpl_path(path)?));
    }
    profile.push_str(")\n");

    profile.push_str("(allow file-write*\n");
    for path in &writable {
        profile.push_str(&format!("    (subpath {})\n", sbpl_path(path)?));
    }
    for device in ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"] {
        profile.push_str(&format!("    (literal \"{device}\")\n"));
    }
    profile.push_str(")\n");

    if network {
        profile.push_str("(allow network*)\n");
    }

    Ok(profile)
}

/// SBPL string literal. The profile is built from trusted paths, but a workspace
/// may still contain characters that would break the filter syntax.
#[cfg(target_os = "macos")]
fn sbpl_path(path: &Path) -> Result<String, SandboxError> {
    let text = path
        .to_str()
        .ok_or_else(|| SandboxError::Setup(format!("path is not UTF-8: {}", path.display())))?;
    if text.contains(['"', '\\', '\n', '\r']) {
        return Err(SandboxError::Setup(format!(
            "path contains characters the sandbox profile cannot represent: {}",
            path.display()
        )));
    }
    Ok(format!("\"{text}\""))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        PathBuf::from("/tmp/qcode-sandbox-tests/workspace")
    }

    #[test]
    fn profile_denies_by_default_and_scopes_writes() {
        let sandbox = SandboxExec::new(&workspace(), false).expect("profile");
        let profile = sandbox.profile();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(subpath \"/tmp/qcode-sandbox-tests/workspace\")"));
        assert!(!profile.contains("(allow network*)"));
        assert!(!profile.contains("(allow default)"));
    }

    #[test]
    fn network_is_opt_in() {
        let sandbox = SandboxExec::new(&workspace(), true).expect("profile");
        assert!(sandbox.profile().contains("(allow network*)"));
        assert!(sandbox.describe().contains("network allowed"));
    }

    #[test]
    fn command_wraps_the_program() {
        let sandbox = SandboxExec::new(&workspace(), false).expect("profile");
        let command = sandbox
            .command(
                OsStr::new("sh"),
                &[OsString::from("-c"), OsString::from("echo hi")],
                &workspace(),
            )
            .expect("command");

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), SandboxExec::EXEC);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], sandbox.profile());
        assert_eq!(&args[2..], ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn rejects_paths_that_break_the_profile() {
        let error = SandboxExec::new(Path::new("/tmp/has\"quote"), false).expect_err("rejected");
        assert!(error.to_string().contains("cannot represent"), "{error}");
    }

    #[test]
    fn no_sandbox_runs_the_command() {
        let mut command = NoSandbox
            .command(
                OsStr::new("sh"),
                &[OsString::from("-c"), OsString::from("printf hi")],
                Path::new("/tmp"),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
    }

    /// These two tests execute `sandbox-exec` for real, so they only prove
    /// anything outside a nested sandbox. Inside one (CI containers, the DSH
    /// runner) the preflight fails with EPERM and the test returns early.
    fn sandbox_or_skip(workspace: &Path) -> Option<SandboxExec> {
        let sandbox = SandboxExec::new(workspace, false).expect("profile");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: sandbox-exec unavailable here: {err}");
                None
            }
        }
    }

    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = sandbox_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("sh"),
                &[
                    OsString::from("-c"),
                    OsString::from("printf hi > inside.txt && cat inside.txt"),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    #[test]
    fn denies_writes_outside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = sandbox_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-sandbox-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        assert!(
            !output.status.success(),
            "sandbox allowed a write outside the workspace"
        );
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}
