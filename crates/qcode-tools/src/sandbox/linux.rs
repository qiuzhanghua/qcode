#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

/// Bubblewrap argument construction.
///
/// Kept free of `cfg(target_os)` so the argument list is unit-testable on any
/// host; the Linux backend below is the only caller.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub(crate) struct BwrapRoot {
    path: PathBuf,
    writable: bool,
}

#[cfg(target_os = "linux")]
pub(crate) fn bwrap_args(
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
pub(crate) fn writable_paths() -> Vec<PathBuf> {
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

#[cfg(target_os = "linux")]
pub(crate) const LL_EXECUTE: u64 = 1 << 0;

#[cfg(target_os = "linux")]
pub(crate) const LL_READ_FILE: u64 = 1 << 2;

#[cfg(target_os = "linux")]
pub(crate) const LL_READ_DIR: u64 = 1 << 3;

/// Rights `landlock_add_rule` accepts for a node that is not a directory.
///
/// The kernel validates `allowed_access` against the file type instead of
/// masking the unsuitable bits off, so a `path_beneath` rule that asks for a
/// directory-only right (READ_DIR, the MAKE_* family, REMOVE_*, REFER) on a
/// file, char/block device, FIFO or socket fails with EINVAL. Device nodes
/// therefore need a mask of their own: handing them `landlock_handled(abi)`
/// made every device rule fail, and because a failed preflight is fatal, the
/// binary refused to start on Linux at all.
///
/// Kept as a filtered view of the handled mask so the bits an older ABI does
/// not know (TRUNCATE, IOCTL_DEV) stay out of the request.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn landlock_file_rights(handled: u64) -> u64 {
    // EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV
    const FILE_RIGHTS: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 15);
    handled & FILE_RIGHTS
}

/// Access bits this kernel's Landlock ABI can handle.
///
/// ABI 1 handles bits 0..=12 (the filesystem rights); 2 adds REFER, 3 adds
/// TRUNCATE, 5 adds IOCTL_DEV. Newer features (network, scopes, audit, threads)
/// are not requested here.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn landlock_handled(abi: i64) -> u64 {
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
pub(crate) const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;

#[cfg(target_os = "linux")]
pub(crate) const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;

#[cfg(target_os = "linux")]
pub(crate) const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

/// `struct landlock_ruleset_attr` (v1 shape: one u64).
#[cfg(target_os = "linux")]
#[repr(C)]
pub(crate) struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr`. The kernel declares it packed.
#[cfg(target_os = "linux")]
#[repr(C, packed)]
pub(crate) struct LandlockPathBeneathAttr {
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
        // They are not directories, so only file rights are legal here.
        let device_rights = landlock_file_rights(allowed);
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
                add_path_rule(self.ruleset_fd, path, device_rights)?;
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
pub(crate) fn landlock_abi() -> Result<i64, SandboxError> {
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
pub(crate) fn create_ruleset(handled_access_fs: u64) -> Result<RawFd, SandboxError> {
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
pub(crate) fn add_path_rule(
    ruleset_fd: RawFd,
    path: &Path,
    allowed_access: u64,
) -> Result<(), SandboxError> {
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
