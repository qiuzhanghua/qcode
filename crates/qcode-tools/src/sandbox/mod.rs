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
