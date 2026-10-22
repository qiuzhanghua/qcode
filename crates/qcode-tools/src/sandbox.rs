//! Sandbox abstraction for process execution.
//!
//! v1 keeps "no sandbox + approval" as the default (ADR-0005). This module adds
//! the seam for OS sandboxes and three experimental backends:
//!
//! - [`NoSandbox`]: the default; approval and workspace path checks are the
//!   boundary.
//! - [`SandboxExec`] (macOS): wraps `run_shell` in `sandbox-exec` with a
//!   generated profile. It is opt-in because nested `sandbox-exec` calls return
//!   EPERM and because the profile is still experimental. Enabling it runs a
//!   preflight; a failure aborts startup instead of silently running
//!   unsandboxed.
//! - `Bwrap` (Linux): bubblewrap with a minimal read-only root.
//! - `Landlock` (Linux): unprivileged filesystem confinement applied in the
//!   child between `fork` and `exec` (no external binary, no mount namespace).
//!   It does not restrict network access; use bwrap when that matters.
//! - `WindowsAcl` (Windows): a write-restricted token whose write access is
//!   limited to the workspace and a private temp directory by capability SIDs
//!   (ADR-0027). Reads and network access are not confined; see
//!   [`crate::windows_acl`] for the boundaries.
//!
//! Every backend is preflighted at startup and fails closed.

use std::ffi::{OsStr, OsString};
use std::path::Path;
#[allow(unused_imports)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

use qcode_core::task::Root;
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
    /// Linux bubblewrap (experimental).
    Bwrap,
    /// Linux Landlock (experimental, filesystem only).
    Landlock,
    /// Windows write-restricted token (experimental, write confinement only).
    WindowsAcl,
}

impl SandboxMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SandboxExec => "sandbox-exec",
            Self::Bwrap => "bwrap",
            Self::Landlock => "landlock",
            Self::WindowsAcl => "windows-acl",
        }
    }
}

/// Network policy passed to an OS sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxNetwork {
    /// No network at all.
    Deny,
    /// Unrestricted network; only useful when approval is the boundary.
    Allow,
    /// Outbound TCP only to the loopback egress proxy. This is macOS "hard
    /// mode": raw sockets, DNS and direct connections are denied by Seatbelt.
    ProxyOnly { port: u16 },
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

    /// Rebuild this sandbox for a task's root set.
    ///
    /// The default backend does not support multiple roots; callers must fail
    /// closed rather than falling back to an unconfined command.
    fn with_roots(
        &self,
        _roots: &[Root],
        _temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Err(SandboxError::Unsupported("multi-root sandbox"))
    }

    /// Probe that the sandbox can actually be applied. Default: no-op.
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Build the configured sandbox. `SandboxExec` is preflighted here so a broken
/// sandbox fails before any tool runs.
pub fn build(
    mode: SandboxMode,
    primary_root: &Path,
    network: SandboxNetwork,
    extra_writable: &[PathBuf],
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    if matches!(network, SandboxNetwork::ProxyOnly { .. }) && mode != SandboxMode::SandboxExec {
        return Err(SandboxError::Setup(
            "network hard mode (proxy-only) is only implemented for macOS sandbox-exec".to_string(),
        ));
    }
    match mode {
        SandboxMode::None => Ok(Arc::new(NoSandbox)),
        SandboxMode::SandboxExec => {
            #[cfg(target_os = "macos")]
            {
                let sandbox = SandboxExec::new_with_extra(primary_root, network, extra_writable)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (primary_root, network, extra_writable);
                Err(SandboxError::Unsupported("sandbox-exec"))
            }
        }
        SandboxMode::Bwrap => {
            #[cfg(target_os = "linux")]
            {
                let _ = extra_writable;
                let sandbox = Bwrap::new(primary_root, matches!(network, SandboxNetwork::Allow))?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (primary_root, network, extra_writable);
                Err(SandboxError::Unsupported("bwrap"))
            }
        }
        SandboxMode::Landlock => {
            #[cfg(target_os = "linux")]
            {
                let _ = extra_writable;
                let _ = network;
                let sandbox = Landlock::new(primary_root)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (primary_root, network, extra_writable);
                Err(SandboxError::Unsupported("landlock"))
            }
        }
        SandboxMode::WindowsAcl => {
            #[cfg(windows)]
            {
                let _ = network;
                let sandbox = crate::windows_acl::WindowsAcl::new(primary_root, extra_writable)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(windows))]
            {
                let _ = (primary_root, network, extra_writable);
                Err(SandboxError::Unsupported("windows-acl"))
            }
        }
    }
}

/// The default: run the command as-is, subject to approval.
#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn with_roots(
        &self,
        _roots: &[Root],
        _temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(NoSandbox))
    }

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
    network: SandboxNetwork,
    extra_writable: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
impl SandboxExec {
    const EXEC: &'static str = "/usr/bin/sandbox-exec";

    pub fn new(workspace: &Path, network: SandboxNetwork) -> Result<Self, SandboxError> {
        Self::new_with_extra(workspace, network, &[])
    }

    pub fn new_with_extra(
        workspace: &Path,
        network: SandboxNetwork,
        extra_writable: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let roots = vec![Root {
            id: qcode_core::task::LEGACY_ROOT_LABEL.to_string(),
            label: qcode_core::task::LEGACY_ROOT_LABEL.to_string(),
            path: workspace.to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        Ok(Self {
            profile: profile_for_roots(&roots, None, network, extra_writable)?,
            network,
            extra_writable: extra_writable.to_vec(),
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }
}

#[cfg(target_os = "macos")]
impl Sandbox for SandboxExec {
    fn with_roots(
        &self,
        roots: &[Root],
        temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(Self {
            profile: profile_for_roots(roots, temp, self.network, &self.extra_writable)?,
            network: self.network,
            extra_writable: self.extra_writable.clone(),
        }))
    }

    fn name(&self) -> &'static str {
        "sandbox-exec"
    }

    fn describe(&self) -> String {
        match self.network {
            SandboxNetwork::Deny => "sandbox-exec (network denied)".to_string(),
            SandboxNetwork::Allow => "sandbox-exec (network allowed)".to_string(),
            SandboxNetwork::ProxyOnly { port } => {
                format!("sandbox-exec (network proxy-only on localhost:{port})")
            }
        }
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

/// Bubblewrap argument construction.
///
/// Kept free of `cfg(target_os)` so the argument list is unit-testable on any
/// host; the Linux backend below is the only caller.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct BwrapRoot {
    path: PathBuf,
    writable: bool,
}

#[cfg(target_os = "linux")]
fn bwrap_args(
    roots: &[BwrapRoot],
    cwd: &Path,
    network: bool,
    writable: &[PathBuf],
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();

    for flag in [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
    ] {
        args.push(flag.into());
    }
    if !network {
        args.push("--unshare-net".into());
    }

    // A minimal read-only root. `/` itself is intentionally not bound: the
    // workspace and the toolchain directories are all the agent may read.
    for path in [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/sys", "/run",
    ] {
        if Path::new(path).exists() {
            args.push("--ro-bind".into());
            args.push(path.into());
            args.push(path.into());
        }
    }
    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    // A workspace under /tmp cannot be bound after a tmpfs is mounted over the
    // source path, so only give /tmp a private tmpfs when it is safe.
    let touches_tmp = roots.iter().any(|root| root.path.starts_with("/tmp"));
    if !touches_tmp {
        args.push("--tmpfs".into());
        args.push("/tmp".into());
    }
    // The host may point TMPDIR at a path that is not mounted in here.
    args.push("--setenv".into());
    args.push("TMPDIR".into());
    args.push("/tmp".into());

    // Each root is bound exactly where it lives: read-write roots share the
    // host directory, read-only roots are copied as immutably visible.
    for root in roots {
        let flag = if root.writable { "--bind" } else { "--ro-bind" };
        args.push(flag.into());
        args.push(root.path.clone().into());
        args.push(root.path.clone().into());
    }
    for path in writable {
        if roots.iter().any(|root| &root.path == path) {
            continue;
        }
        args.push("--bind".into());
        args.push(path.into());
        args.push(path.into());
    }

    args.push("--chdir".into());
    args.push(cwd.into());
    args
}

/// Cargo/Rustup homes are build caches: bind them read-write, like the macOS
/// profile. They are outside the workspace, so they are called out in the docs.
#[cfg(target_os = "linux")]
fn writable_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut push = |path: PathBuf| {
        if path.is_dir() && !paths.contains(&path) {
            paths.push(path);
        }
    };

    for key in ["CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                push(path);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        push(home.join(".cargo"));
        push(home.join(".rustup"));
    }
    paths
}

/// Linux bubblewrap backend.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct Bwrap {
    roots: Vec<BwrapRoot>,
    network: bool,
}

#[cfg(target_os = "linux")]
impl Bwrap {
    pub fn new(workspace: &Path, network: bool) -> Result<Self, SandboxError> {
        if workspace == Path::new("/") {
            return Err(SandboxError::Setup(
                "refusing to sandbox the filesystem root as the workspace".to_string(),
            ));
        }
        Ok(Self {
            roots: vec![BwrapRoot {
                path: workspace.to_path_buf(),
                writable: true,
            }],
            network,
        })
    }

    fn build(&self, program: &OsStr, args: &[OsString], cwd: &Path) -> Command {
        let mut command = Command::new("bwrap");
        command.args(bwrap_args(
            &self.roots,
            cwd,
            self.network,
            &writable_paths(),
        ));
        command.arg(program).args(args);
        command
    }
}

#[cfg(target_os = "linux")]
impl Sandbox for Bwrap {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    /// A task's roots become binds: writable roots are shared, read-only roots
    /// are mounted read-only. The turn temp joins as a writable root.
    fn with_roots(
        &self,
        roots: &[Root],
        temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut mapped: Vec<BwrapRoot> = roots
            .iter()
            .map(|root| BwrapRoot {
                path: root.path.clone(),
                writable: root.access == qcode_core::task::RootAccess::ReadWrite,
            })
            .collect();
        if let Some(temp) = temp {
            mapped.push(BwrapRoot {
                path: temp.to_path_buf(),
                writable: true,
            });
        }
        if mapped.is_empty() {
            return Err(SandboxError::Unsupported(
                "bwrap needs at least one root or a turn temp",
            ));
        }
        Ok(Arc::new(Self {
            roots: mapped,
            network: self.network,
        }))
    }

    fn describe(&self) -> String {
        format!(
            "bubblewrap (network {})",
            if self.network { "allowed" } else { "denied" }
        )
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        Ok(self.build(program, args, cwd))
    }

    fn check(&self) -> Result<(), SandboxError> {
        let cwd = self
            .roots
            .first()
            .map(|root| root.path.clone())
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut command = self.build(OsStr::new("/bin/true"), &[], &cwd);
        let output = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| SandboxError::Setup(format!("cannot run bwrap: {err}")))?;

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
            name: "bwrap",
            detail,
        })
    }
}

// ---------------------------------------------------------------------------
// Landlock (Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
const LL_EXECUTE: u64 = 1 << 0;
#[cfg(target_os = "linux")]
const LL_READ_FILE: u64 = 1 << 2;
#[cfg(target_os = "linux")]
const LL_READ_DIR: u64 = 1 << 3;

/// Access bits this kernel's Landlock ABI can handle.
///
/// ABI 1 handles bits 0..=12 (the filesystem rights); 2 adds REFER, 3 adds
/// TRUNCATE, 5 adds IOCTL_DEV. Newer features (network, scopes, audit, threads)
/// are not requested here.
#[cfg(any(target_os = "linux", test))]
fn landlock_handled(abi: i64) -> u64 {
    let mut bits = (1 << 13) - 1;
    if abi >= 2 {
        bits |= 1 << 13;
    }
    if abi >= 3 {
        bits |= 1 << 14;
    }
    if abi >= 5 {
        bits |= 1 << 15;
    }
    bits
}

#[cfg(target_os = "linux")]
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
#[cfg(target_os = "linux")]
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
#[cfg(target_os = "linux")]
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

/// `struct landlock_ruleset_attr` (v1 shape: one u64).
#[cfg(target_os = "linux")]
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr`. The kernel declares it packed.
#[cfg(target_os = "linux")]
#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: libc::c_int,
}

/// Linux Landlock backend: filesystem confinement without an external binary.
///
/// The ruleset is built in the parent; the child applies it with
/// `landlock_restrict_self()` in a `pre_exec` hook, so only `run_shell` and its
/// descendants are confined.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct Landlock {
    /// Each root with its mode: `true` is read-write, `false` is read-only.
    roots: Vec<(PathBuf, bool)>,
    /// Where the preflight runs; the first root (or the turn temp).
    cwd: PathBuf,
    ruleset_fd: RawFd,
}

#[cfg(target_os = "linux")]
impl Landlock {
    pub fn new(workspace: &Path) -> Result<Self, SandboxError> {
        Self::for_roots(vec![(workspace.to_path_buf(), true)])
    }

    /// Build a ruleset for a set of roots and a directory to run the preflight
    /// in. Read-only roots only get the read bits, which is all Landlock needs:
    /// unlike the other backends there is no mount table to prepare.
    fn for_roots(roots: Vec<(PathBuf, bool)>) -> Result<Self, SandboxError> {
        if let Some((path, _)) = roots.iter().find(|(path, _)| path == Path::new("/")) {
            return Err(SandboxError::Setup(format!(
                "refusing to sandbox the filesystem root as a root: {}",
                path.display()
            )));
        }
        let cwd = roots
            .first()
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| PathBuf::from("/"));

        let abi = landlock_abi()?;
        let handled = landlock_handled(abi);
        let ruleset_fd = create_ruleset(handled)?;
        let sandbox = Self {
            roots,
            cwd,
            ruleset_fd,
        };
        // `Drop` closes the fd on error.
        sandbox.install_rules(abi)?;
        Ok(sandbox)
    }

    fn install_rules(&self, abi: i64) -> Result<(), SandboxError> {
        let allowed = landlock_handled(abi);
        let read_only = LL_EXECUTE | LL_READ_FILE | LL_READ_DIR;

        for path in [
            "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/sys", "/proc", "/run",
            "/dev",
        ] {
            let path = Path::new(path);
            if path.exists() {
                add_path_rule(self.ruleset_fd, path, read_only)?;
            }
        }

        // Devices need writes; grant them explicitly instead of making /dev rw.
        for path in [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
        ] {
            let path = Path::new(path);
            if path.exists() {
                add_path_rule(self.ruleset_fd, path, allowed)?;
            }
        }

        // Task roots: read-only ones get the read bits, read-write ones the
        // full set.
        for (path, writable) in &self.roots {
            if path.exists() {
                let access = if *writable { allowed } else { read_only };
                add_path_rule(self.ruleset_fd, path, access)?;
            }
        }

        let mut writable = vec![PathBuf::from("/tmp")];
        if Path::new("/var/tmp").exists() {
            writable.push(PathBuf::from("/var/tmp"));
        }
        writable.extend(writable_paths());
        for path in writable {
            if path.exists() {
                add_path_rule(self.ruleset_fd, &path, allowed)?;
            }
        }

        Ok(())
    }

    fn build(&self, program: &OsStr, args: &[OsString], cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd);

        let ruleset_fd = self.ruleset_fd;
        // SAFETY: between fork and exec; only raw syscalls and no allocation.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(
                    SYS_LANDLOCK_RESTRICT_SELF,
                    ruleset_fd as libc::c_long,
                    0 as libc::c_long,
                ) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
}

#[cfg(target_os = "linux")]
impl Drop for Landlock {
    fn drop(&mut self) {
        // SAFETY: the fd is owned by this struct.
        unsafe {
            libc::close(self.ruleset_fd);
        }
    }
}

#[cfg(target_os = "linux")]
impl Sandbox for Landlock {
    fn name(&self) -> &'static str {
        "landlock"
    }

    /// Rebuild the ruleset for a task's roots plus its turn temp.
    fn with_roots(
        &self,
        roots: &[Root],
        temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut mapped: Vec<(PathBuf, bool)> = roots
            .iter()
            .map(|root| {
                (
                    root.path.clone(),
                    root.access == qcode_core::task::RootAccess::ReadWrite,
                )
            })
            .collect();
        if let Some(temp) = temp {
            mapped.push((temp.to_path_buf(), true));
        }
        if mapped.is_empty() {
            return Err(SandboxError::Unsupported(
                "landlock needs at least one root or a turn temp",
            ));
        }
        Ok(Arc::new(Self::for_roots(mapped)?))
    }

    fn describe(&self) -> String {
        "Landlock filesystem confinement (network not restricted)".to_string()
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        Ok(self.build(program, args, cwd))
    }

    fn check(&self) -> Result<(), SandboxError> {
        let mut command = self.build(OsStr::new("/bin/true"), &[], &self.cwd);
        let output = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| SandboxError::Setup(format!("cannot apply Landlock: {err}")))?;

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
            name: "landlock",
            detail,
        })
    }
}

#[cfg(target_os = "linux")]
fn landlock_abi() -> Result<i64, SandboxError> {
    // SAFETY: the version query takes a null attribute pointer and size 0.
    let version = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    if version < 0 {
        return Err(SandboxError::Setup(format!(
            "Landlock is unavailable: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(version)
}

#[cfg(target_os = "linux")]
fn create_ruleset(handled_access_fs: u64) -> Result<RawFd, SandboxError> {
    let attr = LandlockRulesetAttr { handled_access_fs };
    // SAFETY: `attr` lives for the duration of the syscall and the struct
    // layout matches the kernel's v1 ABI.
    let fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(SandboxError::Setup(format!(
            "cannot create Landlock ruleset: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(fd as RawFd)
}

#[cfg(target_os = "linux")]
fn add_path_rule(ruleset_fd: RawFd, path: &Path, allowed_access: u64) -> Result<(), SandboxError> {
    let path_bytes = path.as_os_str().as_bytes();
    let c_path = std::ffi::CString::new(path_bytes).map_err(|_| {
        SandboxError::Setup(format!("path contains a NUL byte: {}", path.display()))
    })?;

    // SAFETY: `open` with O_PATH only creates a reference, no IO happens.
    let parent_fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if parent_fd < 0 {
        return Err(SandboxError::Setup(format!(
            "cannot open {} for Landlock: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }

    let attr = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd,
    };
    // SAFETY: the ruleset fd is open and the attribute struct matches the
    // kernel's packed layout.
    let result = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd,
            1u32, // LANDLOCK_RULE_PATH_BENEATH
            &attr as *const LandlockPathBeneathAttr,
            0u32,
        )
    };
    // SAFETY: this function owns the reference from `open`.
    unsafe {
        libc::close(parent_fd);
    }

    if result < 0 {
        return Err(SandboxError::Setup(format!(
            "cannot add Landlock rule for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Build the SBPL profile.
///
/// Read access covers the system toolchain; writes are limited to the workspace,
/// temporary directories and the Cargo/Rustup homes (needed for builds). Network
/// is denied unless explicitly enabled.
#[cfg(target_os = "macos")]
fn profile_for_roots(
    roots: &[Root],
    temp: Option<&Path>,
    network: SandboxNetwork,
    extra_writable: &[PathBuf],
) -> Result<String, SandboxError> {
    if roots.iter().any(|root| root.path == Path::new("/")) {
        return Err(SandboxError::Setup(
            "refusing to sandbox the filesystem root as a root".to_string(),
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
    let mut writable: Vec<PathBuf> = vec![
        "/private/tmp".into(),
        "/tmp".into(),
        "/private/var/folders".into(),
        "/var/folders".into(),
    ];
    for root in roots {
        readable.push(root.path.clone());
        if root.access == qcode_core::task::RootAccess::ReadWrite {
            writable.push(root.path.clone());
        }
    }
    if let Some(temp) = temp {
        readable.push(temp.to_path_buf());
        writable.push(temp.to_path_buf());
    }
    writable.extend(extra_writable.iter().cloned());
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
    match network {
        SandboxNetwork::Deny => {}
        SandboxNetwork::Allow => profile.push_str("(allow network*)\n"),
        SandboxNetwork::ProxyOnly { port } => {
            profile.push_str(&format!(
                "(allow network-outbound (remote tcp \"localhost:{port}\"))\n"
            ));
        }
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
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Deny).expect("profile");
        let profile = sandbox.profile();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(subpath \"/tmp/qcode-sandbox-tests/workspace\")"));
        assert!(!profile.contains("(allow network*)"));
        assert!(!profile.contains("(allow default)"));
    }

    #[test]
    fn multi_root_profile_scopes_writes_to_rw_roots() {
        let rw = PathBuf::from("/tmp/qcode-sandbox-tests/code");
        let ro = PathBuf::from("/tmp/qcode-sandbox-tests/docs");
        let profile = profile_for_roots(
            &[
                Root {
                    id: "code".into(),
                    label: "code".into(),
                    path: rw.clone(),
                    access: qcode_core::task::RootAccess::ReadWrite,
                    primary: true,
                },
                Root {
                    id: "docs".into(),
                    label: "docs".into(),
                    path: ro.clone(),
                    access: qcode_core::task::RootAccess::ReadOnly,
                    primary: false,
                },
            ],
            Some(Path::new("/tmp/qcode-sandbox-tests/tmp")),
            SandboxNetwork::Deny,
            &[],
        )
        .expect("profile");
        let write = profile
            .split("(allow file-write*")
            .nth(1)
            .expect("write block");
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/code\")"));
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/tmp\")"));
        assert!(!write.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
        let read = profile
            .split("(allow file-read*")
            .nth(1)
            .expect("read block");
        assert!(read.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
    }

    #[test]
    fn network_is_opt_in() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Allow).expect("profile");
        assert!(sandbox.profile().contains("(allow network*)"));
        assert!(sandbox.describe().contains("network allowed"));
    }

    #[test]
    fn hard_mode_allows_only_the_proxy_port() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::ProxyOnly { port: 43123 })
            .expect("profile");
        let profile = sandbox.profile();
        assert!(profile.contains("(allow network-outbound (remote tcp \"localhost:43123\"))"));
        assert!(!profile.contains("(allow network*)"));
        assert!(sandbox.describe().contains("proxy-only on localhost:43123"));
    }

    #[test]
    fn hard_mode_requires_an_os_sandbox() {
        let error = build(
            SandboxMode::None,
            &workspace(),
            SandboxNetwork::ProxyOnly { port: 43123 },
            &[],
        )
        .expect_err("rejected");
        assert!(
            error
                .to_string()
                .contains("only implemented for macOS sandbox-exec")
        );
    }

    #[test]
    fn command_wraps_the_program() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Deny).expect("profile");
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
        let error = SandboxExec::new(Path::new("/tmp/has\"quote"), SandboxNetwork::Deny)
            .expect_err("rejected");
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
        let sandbox = SandboxExec::new(workspace, SandboxNetwork::Deny).expect("profile");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: sandbox-exec unavailable here: {err}");
                None
            }
        }
    }

    /// Runtime check for the hard-mode profile: the proxy port is reachable,
    /// another live loopback port is not. Skipped when nested sandbox-exec is
    /// unavailable (CI containers, the DSH runner).
    #[test]
    fn hard_mode_denies_other_loopback_ports() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let proxy_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("proxy listener");
        let other_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("other listener");
        let proxy_port = proxy_listener.local_addr().expect("proxy addr").port();
        let other_port = other_listener.local_addr().expect("other addr").port();

        let sandbox = SandboxExec::new(
            workspace.path(),
            SandboxNetwork::ProxyOnly { port: proxy_port },
        )
        .expect("profile");
        if let Err(err) = sandbox.check() {
            let detail = err.to_string();
            // Only nested sandboxes are expected to refuse sandbox-exec. Any
            // other preflight failure (for example an invalid profile) is a
            // real failure, not a skip.
            if detail.contains("Operation not permitted") || detail.contains("EPERM") {
                eprintln!("skipping: nested sandbox-exec is not permitted here: {err}");
                return;
            }
            panic!("sandbox-exec preflight failed: {err}");
        }

        let connect = |port: u16| {
            sandbox
                .command(
                    OsStr::new("/usr/bin/nc"),
                    &[
                        OsString::from("-z"),
                        OsString::from("127.0.0.1"),
                        OsString::from(port.to_string()),
                    ],
                    workspace.path(),
                )
                .expect("command")
                .output()
                .expect("run nc")
        };

        let allowed = connect(proxy_port);
        assert!(
            allowed.status.success(),
            "the proxy port must stay reachable: {allowed:?}"
        );
        let denied = connect(other_port);
        assert!(
            !denied.status.success(),
            "a non-proxy loopback port must be denied: {denied:?}"
        );
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

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod bwrap_tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn workspace() -> PathBuf {
        PathBuf::from("/home/qcode-bwrap-tests/workspace")
    }

    fn cwd() -> PathBuf {
        workspace().join("sub")
    }

    fn rw_roots() -> Vec<BwrapRoot> {
        vec![BwrapRoot {
            path: workspace(),
            writable: true,
        }]
    }

    #[test]
    fn binds_workspace_read_write_and_chdirs() {
        let args = strings(&bwrap_args(&rw_roots(), &cwd(), false, &[]));
        assert!(
            args.windows(3).any(|window| window
                == [
                    "--bind",
                    "/home/qcode-bwrap-tests/workspace",
                    "/home/qcode-bwrap-tests/workspace"
                ]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--chdir", "/home/qcode-bwrap-tests/workspace/sub"]),
            "{args:?}"
        );
        assert!(args.contains(&"--tmpfs".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
    }

    #[test]
    fn workspace_under_tmp_skips_the_tmpfs_mount() {
        let workspace = PathBuf::from("/tmp/ws");
        let roots = vec![BwrapRoot {
            path: workspace.clone(),
            writable: true,
        }];
        let args = strings(&bwrap_args(&roots, &workspace, false, &[]));
        assert!(!args.contains(&"--tmpfs".to_string()), "{args:?}");
        assert!(
            args.windows(2)
                .any(|window| window == ["--setenv", "TMPDIR"])
        );
    }

    #[test]
    fn network_is_denied_unless_opted_in() {
        let denied = strings(&bwrap_args(&rw_roots(), &cwd(), false, &[]));
        assert!(denied.contains(&"--unshare-net".to_string()));

        let allowed = strings(&bwrap_args(&rw_roots(), &cwd(), true, &[]));
        assert!(!allowed.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn a_read_only_root_is_mounted_read_only() {
        let w = workspace();
        let roots = vec![
            BwrapRoot {
                path: w.clone(),
                writable: true,
            },
            BwrapRoot {
                path: PathBuf::from("/home/qcode-bwrap-tests/docs"),
                writable: false,
            },
        ];
        let args = strings(&bwrap_args(&roots, &cwd(), false, &[]));
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--bind" && window[1] == w.to_string_lossy()),
            "{args:?}"
        );
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--ro-bind"
                    && window[1] == "/home/qcode-bwrap-tests/docs"),
            "{args:?}"
        );
    }
    #[test]
    fn extra_writable_paths_are_bound_once() {
        let cargo = PathBuf::from("/tmp/qcode-bwrap-tests/cargo");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            std::slice::from_ref(&cargo),
        ));
        let binds = args
            .windows(3)
            .filter(|window| window[0] == "--bind" && window[1] == cargo.to_string_lossy())
            .count();
        assert_eq!(binds, 1, "{args:?}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    fn bwrap_or_skip(workspace: &Path) -> Option<Bwrap> {
        let sandbox = Bwrap::new(workspace, false).expect("sandbox");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: bwrap unavailable here: {err}");
                None
            }
        }
    }

    #[test]
    fn a_read_only_root_rejects_writes_next_to_the_writable_one() {
        let rw = tempfile::tempdir().expect("rw dir");
        let ro = tempfile::tempdir().expect("ro dir");
        let Some(seed) = bwrap_or_skip(rw.path()) else {
            return;
        };

        let roots = vec![
            Root {
                id: "rw".to_string(),
                label: "app".to_string(),
                path: rw.path().to_path_buf(),
                access: qcode_core::task::RootAccess::ReadWrite,
                primary: true,
            },
            Root {
                id: "ro".to_string(),
                label: "docs".to_string(),
                path: ro.path().to_path_buf(),
                access: qcode_core::task::RootAccess::ReadOnly,
                primary: false,
            },
        ];
        let sandbox = seed.with_roots(&roots, None).expect("multi-root sandbox");

        let write = |cwd: &Path, target: &Path| {
            let mut command = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!("printf x > '{}'", target.display())),
                    ],
                    cwd,
                )
                .expect("command");
            command.output().expect("run")
        };

        // The writable root accepts a file, on the host.
        let inside = rw.path().join("inside.txt");
        write(rw.path(), &inside);
        assert!(inside.is_file(), "read-write root refused a write");

        // The read-only root is mounted read-only, so the write cannot land.
        let denied = ro.path().join("denied.txt");
        let output = write(rw.path(), &denied);
        assert!(!output.status.success(), "read-only root accepted a write");
        assert!(!denied.exists(), "read-only root has a file in it");
    }
    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = bwrap_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
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
        let Some(sandbox) = bwrap_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-bwrap-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}

#[cfg(test)]
mod landlock_tests {
    use super::*;

    #[test]
    fn handled_access_grows_with_abi() {
        let v1 = landlock_handled(1);
        assert_eq!(v1, (1 << 13) - 1);
        assert_eq!(landlock_handled(2), v1 | (1 << 13));
        assert_eq!(landlock_handled(3), v1 | (1 << 13) | (1 << 14));
        assert_eq!(landlock_handled(5), v1 | (1 << 13) | (1 << 14) | (1 << 15));
        assert_eq!(landlock_handled(9), landlock_handled(5));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod landlock_runtime {
    use super::*;

    fn landlock_or_skip(workspace: &Path) -> Option<Landlock> {
        let sandbox = match Landlock::new(workspace) {
            Ok(sandbox) => sandbox,
            Err(err) => {
                eprintln!("skipping: landlock unavailable here: {err}");
                return None;
            }
        };
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: landlock unavailable here: {err}");
                None
            }
        }
    }

    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = landlock_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
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
        let Some(sandbox) = landlock_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-landlock-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}
#[cfg(all(test, target_os = "macos"))]
mod macos_profile_tests {
    use super::*;

    #[test]
    fn the_profile_writes_extra_roots() {
        let root = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let roots = vec![Root {
            id: "r1".to_string(),
            label: "app".to_string(),
            path: root.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        let profile = profile_for_roots(
            &roots,
            None,
            SandboxNetwork::Deny,
            &[extra.path().to_path_buf()],
        )
        .expect("profile");
        let write_section = profile
            .split("(allow file-write*")
            .nth(1)
            .expect("write section");
        assert!(
            write_section.contains(&extra.path().display().to_string()),
            "extra writable root missing from the profile: {profile}"
        );
    }
}
