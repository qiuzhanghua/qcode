use qcode_core::task::Root;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

#[allow(unused_imports)]
use std::path::PathBuf;

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

/// What a sandbox grants **beyond** the roots it was given.
///
/// The two shapes exist because qcode has two very different callers, and until
/// this enum they silently shared one rule set (ADR-0066 §1 says an MCP server
/// gets exactly its grant — three disjoint sets, nothing else):
///
/// * [`Self::BuildCaches`] — `run_shell`. A compiler writes to `CARGO_HOME`,
///   `RUSTUP_HOME`, `~/.cargo`, `~/.rustup`, and anything that opens a socket or
///   a `mktemp` writes to the host `/tmp`. None of that is in the task roots, so
///   it is handed out on top.
/// * [`Self::Nothing`] — an MCP server. It is third-party code that was never
///   going to run `cargo build`; `TMPDIR` already points into its private HOME,
///   which *is* one of its roots (ADR-0068 §3), so a scratch directory is not
///   missing. Measured before this existed: a server could write
///   `/tmp/anything` and, on any machine with a Rust toolchain, rewrite
///   `~/.cargo/config.toml` — whose `[build] rustc-wrapper` is arbitrary code
///   execution on the next `cargo build` in that HOME.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutsideRoots {
    /// Build caches and the host scratch directories, on top of the roots.
    BuildCaches,
    /// Exactly the roots. Nothing is added.
    #[default]
    Nothing,
}

impl OutsideRoots {
    /// Whether the build caches / host scratch directories are handed out.
    pub fn grants_build_caches(self) -> bool {
        matches!(self, Self::BuildCaches)
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

/// Extra roots granted beyond the task roots, both lists recursive.
///
/// Grouped in a struct rather than passed as two more positional parameters: two
/// `&[PathBuf]`s next to each other swap silently and still type-check, and the
/// result is a sandbox that reads where it should write. (`qcode_mcp`'s
/// `ConnectOptions` was created for the same reason.)
///
/// The two lists are not symmetric across backends, and pretending otherwise
/// would be the "docs say A, code does B" failure this ADR set out to remove:
///
/// * `readable` is honoured by **every** backend. On Windows reads are not
///   restricted at all, so it is a no-op there rather than a promise.
/// * `writable` is honoured by **every** backend too, since ADR-0066 §2 修订
///   注记 2026-11-11: `sandbox-exec` and `windows-acl` had it first, and the two
///   Linux backends dropped the list on the floor (`let _ = extra_writable`)
///   until a user setting `extra_writable = ["~/Documents"]` found it read-only
///   with no way to tell whether the config or the code was wrong.
///
/// Both lists are recursive and both are refused for the same dangerous shapes
/// ([`qcode_core::dangerous_write_root`]): a config-wide grant has no per-task
/// audience, so `~/` or `/home` here is never what was meant.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxPaths<'a> {
    /// Extra roots granted read-write.
    pub writable: &'a [PathBuf],
    /// Extra roots granted read + execute, recursively.
    ///
    /// Exists because the system read allowlist is a fixed list of distro paths
    /// plus the task roots, so a toolchain installed anywhere else (`/opt/tool`,
    /// a package manager's own prefix, `$HOME/cot`) is invisible to a confined
    /// `run_shell` — `cargo`, `gh`, anything under it fails with `Permission
    /// denied` before it even starts. Measured: with this unset, a Landlocked
    /// agent could not read `/home/q/cot`, which is where the only Rust
    /// toolchain on that machine lives.
    pub readable: &'a [PathBuf],
}

/// Build the configured sandbox. `SandboxExec` is preflighted here so a broken
/// sandbox fails before any tool runs.
pub fn build(
    mode: SandboxMode,
    primary_root: &Path,
    network: SandboxNetwork,
    paths: SandboxPaths<'_>,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    let SandboxPaths { writable, readable } = paths;
    let extra_writable = writable;
    let extra_read = readable;
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
                let sandbox =
                    SandboxExec::new_with_extra(primary_root, network, extra_writable, extra_read)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (primary_root, network, extra_writable, extra_read);
                Err(SandboxError::Unsupported("sandbox-exec"))
            }
        }
        SandboxMode::Bwrap => {
            #[cfg(target_os = "linux")]
            {
                let sandbox = Bwrap::new(
                    primary_root,
                    matches!(network, SandboxNetwork::Allow),
                    extra_read,
                    extra_writable,
                )?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (primary_root, network, extra_writable, extra_read);
                Err(SandboxError::Unsupported("bwrap"))
            }
        }
        SandboxMode::Landlock => {
            #[cfg(target_os = "linux")]
            {
                let _ = network;
                let sandbox = Landlock::new(primary_root, extra_read, extra_writable)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (primary_root, network, extra_writable, extra_read);
                Err(SandboxError::Unsupported("landlock"))
            }
        }
        SandboxMode::WindowsAcl => {
            #[cfg(windows)]
            {
                let _ = network;
                // `extra_read` is a no-op here: this backend restricts writes
                // only, so reads are already unrestricted.
                let _ = extra_read;
                let sandbox = crate::windows_acl::WindowsAcl::new(primary_root, extra_writable)?;
                sandbox.check()?;
                Ok(Arc::new(sandbox))
            }
            #[cfg(not(windows))]
            {
                let _ = (primary_root, network, extra_writable, extra_read);
                Err(SandboxError::Unsupported("windows-acl"))
            }
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
    ///
    /// The returned `Command` keeps referring to this sandbox's state — the
    /// Landlock ruleset fd, the Seatbelt profile, the ACL helper — so **the
    /// sandbox must outlive `spawn()`**. Dropping it first (for example by
    /// calling `with_roots(..)?.command(..)` in one expression) makes the child
    /// die with `EBADFD` right before `exec`, which reads like a permissions
    /// bug and is not one.
    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError>;

    /// Rebuild this sandbox for a task's root set.
    ///
    /// `outside` is what the caller wants on top of those roots, and it has to
    /// be stated rather than inherited: the same backend serves `run_shell`
    /// (which needs build caches) and MCP servers (which must get nothing),
    /// and a default would make one of them silently wrong.
    ///
    /// The default backend does not support multiple roots; callers must fail
    /// closed rather than falling back to an unconfined command.
    fn with_roots(
        &self,
        _roots: &[Root],
        _temp: Option<&Path>,
        _outside: OutsideRoots,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Err(SandboxError::Unsupported("multi-root sandbox"))
    }

    /// Probe that the sandbox can actually be applied. Default: no-op.
    fn check(&self) -> Result<(), SandboxError> {
        Ok(())
    }
}

mod noop;
pub use noop::NoSandbox;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::SandboxExec;

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "linux")]
use linux::{Bwrap, Landlock};

#[cfg(test)]
mod tests;
