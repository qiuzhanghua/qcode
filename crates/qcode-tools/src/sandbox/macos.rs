use super::*;

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
    pub(crate) const EXEC: &'static str = "/usr/bin/sandbox-exec";

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

/// Build the SBPL profile.
///
/// Read access covers the system toolchain; writes are limited to the workspace,
/// temporary directories and the Cargo/Rustup homes (needed for builds). Network
/// is denied unless explicitly enabled.
#[cfg(target_os = "macos")]
pub(crate) fn profile_for_roots(
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
pub(crate) fn sbpl_path(path: &Path) -> Result<String, SandboxError> {
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
