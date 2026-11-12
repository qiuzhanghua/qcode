#[allow(unused_imports)]
use super::*;

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
    pub(crate) path: PathBuf,
    pub(crate) writable: bool,
}

#[cfg(target_os = "linux")]
pub(crate) fn bwrap_args(
    roots: &[BwrapRoot],
    cwd: &Path,
    network: bool,
    writable: &[PathBuf],
    outside: OutsideRoots,
    extra_read: &[PathBuf],
    extra_write: &[PathBuf],
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
    // `[sandbox] extra_read`: read-only binds, same shape as the system list
    // above. bwrap creates the mount point, so these may live outside the
    // minimal root (typically `$HOME`, which is deliberately not bound).
    for path in extra_read {
        if roots.iter().any(|root| &root.path == path) {
            continue;
        }
        args.push("--ro-bind".into());
        args.push(path.into());
        args.push(path.into());
    }
    args.push("--proc".into());
    args.push("/proc".into());
    args.push("--dev".into());
    args.push("/dev".into());
    // A workspace under /tmp cannot be bound after a tmpfs is mounted over the
    // source path, so only give /tmp a private tmpfs when it is safe.
    //
    // A private tmpfs is kept for both `OutsideRoots` shapes: it is *not* the
    // host's /tmp, so an MCP server writing `/tmp/x` writes into a directory
    // that vanishes with the process and that nothing else can see. Denying it
    // outright would break runtimes that ignore TMPDIR for no security gain.
    let touches_tmp = roots.iter().any(|root| root.path.starts_with("/tmp"))
        || extra_write.iter().any(|path| path.starts_with("/tmp"));
    if !touches_tmp {
        args.push("--tmpfs".into());
        args.push("/tmp".into());
    }
    // The host may point TMPDIR at a path that is not mounted in here. When the
    // caller granted no build caches, TMPDIR is left as qcode set it — for an
    // MCP server that is its private HOME (ADR-0068 §3), which is a bound,
    // writable root, so this keeps the scratch directory inside the grant.
    if outside.grants_build_caches() {
        args.push("--setenv".into());
        args.push("TMPDIR".into());
        args.push("/tmp".into());
    }

    // Each root is bound exactly where it lives: read-write roots share the
    // host directory, read-only roots are copied as immutably visible.
    for root in roots {
        let flag = if root.writable { "--bind" } else { "--ro-bind" };
        args.push(flag.into());
        args.push(root.path.clone().into());
        args.push(root.path.clone().into());
    }
    if outside.grants_build_caches() {
        for path in writable {
            if roots.iter().any(|root| &root.path == path) {
                continue;
            }
            args.push("--bind".into());
            args.push(path.into());
            args.push(path.into());
        }
    }
    // `[sandbox] extra_writable`: shared rw binds, not gated on the build-cache
    // switch — that switch is about what qcode adds on its own, while these are
    // what the user asked for (ADR-0066 §2 修订注记 2026-11-11 补记). A path
    // that is already a task root is skipped so one directory is not bound
    // twice.
    for path in extra_write {
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

/// Validate `[sandbox] extra_writable` for a Linux backend.
///
/// Shared by `Bwrap` and `Landlock` rather than written twice: the two would
/// drift into "bubblewrap refuses `/` but landlock grants it", and neither copy
/// would look wrong on its own (ADR-0068's lesson).
///
/// Two failures, both of which the user cannot diagnose from inside a sandbox:
///
/// * A path that does not exist. `Landlock` would install no rule and stay
///   denied; `bubblewrap` would fail with its own message about a mount. Either
///   way it reads as "the setting is ignored", which is the exact symptom
///   `extra_writable` already spent a release having.
/// * A dangerous shape. `SandboxConfig::validate` refuses these at load time;
///   this second check covers `Sandbox::build`'s direct callers, because a rule
///   installed here cannot be taken back.
#[cfg(target_os = "linux")]
fn check_extra_write(extra_write: &[PathBuf]) -> Result<(), SandboxError> {
    if let Some(missing) = extra_write.iter().find(|path| !path.exists()) {
        return Err(SandboxError::Setup(format!(
            "[sandbox] extra_writable: {} does not exist",
            missing.display()
        )));
    }
    let home = qcode_core::real_home();
    for path in extra_write {
        if let Some(reason) = qcode_core::dangerous_write_root(path, home.as_deref()) {
            return Err(SandboxError::Setup(format!(
                "[sandbox] extra_writable: {} is {reason}; narrow it to a subdirectory",
                path.display()
            )));
        }
    }
    Ok(())
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
    outside: OutsideRoots,
    /// `[sandbox] extra_read`, carried so `with_roots` keeps it.
    extra_read: Vec<PathBuf>,
    /// `[sandbox] extra_writable`, carried so `with_roots` keeps it.
    extra_write: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl Bwrap {
    pub fn new(
        workspace: &Path,
        network: bool,
        extra_read: &[PathBuf],
        extra_write: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        if workspace == Path::new("/") {
            return Err(SandboxError::Setup(
                "refusing to sandbox the filesystem root as the workspace".to_string(),
            ));
        }
        check_extra_write(extra_write)?;
        Ok(Self {
            roots: vec![BwrapRoot {
                path: workspace.to_path_buf(),
                writable: true,
            }],
            network,
            // The base sandbox is only a factory and a preflight: every real
            // command comes from `with_roots`, which states this for itself.
            outside: OutsideRoots::BuildCaches,
            extra_read: extra_read.to_vec(),
            extra_write: extra_write.to_vec(),
        })
    }

    fn build(&self, program: &OsStr, args: &[OsString], cwd: &Path) -> Command {
        let mut command = Command::new("bwrap");
        command.args(bwrap_args(
            &self.roots,
            cwd,
            self.network,
            &writable_paths(),
            self.outside,
            &self.extra_read,
            &self.extra_write,
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
        outside: OutsideRoots,
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
            outside,
            extra_read: self.extra_read.clone(),
            extra_write: self.extra_write.clone(),
        }))
    }

    fn describe(&self) -> String {
        // The write scope is part of the description because ADR-0066 §1 is
        // exactly about not letting the printed grant and the enforced grant
        // drift apart: "bubblewrap" alone does not say whether build caches
        // came along.
        let mut text = format!(
            "bubblewrap (network {}, writes: {})",
            if self.network { "allowed" } else { "denied" },
            if self.outside.grants_build_caches() {
                "roots + build caches + host /tmp"
            } else {
                "roots only"
            }
        );
        if !self.extra_read.is_empty() {
            let list: Vec<String> = self
                .extra_read
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            text.push_str(&format!(", extra read: [{}]", list.join(", ")));
        }
        if !self.extra_write.is_empty() {
            let list: Vec<String> = self
                .extra_write
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            text.push_str(&format!(", extra write: [{}]", list.join(", ")));
        }
        text
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
    /// Build caches and the host `/tmp`, or neither. Carried rather than read
    /// from the environment at install time so the answer is a property of this
    /// ruleset, visible in `describe()` and in the tests.
    outside: OutsideRoots,
    /// `[sandbox] extra_read`: roots granted read + execute recursively, on top
    /// of the fixed system allowlist and the task roots. Carried on the ruleset
    /// (not re-read at install time) so `with_roots` keeps them and
    /// `describe()` can show them.
    extra_read: Vec<PathBuf>,
    /// `[sandbox] extra_writable`: roots granted the full right set, exactly as
    /// a read-write task root is (ADR-0066 §2 修订注记 2026-11-11 补记).
    ///
    /// Landlock unions overlapping rules and the more permissive one wins, so an
    /// entry here also covers any read-only task root underneath it. That is
    /// the same behaviour two nested task roots already had; the difference is
    /// that this list is config-wide and invisible per task, which is why the
    /// dangerous shapes are refused at load time.
    extra_write: Vec<PathBuf>,
    ruleset_fd: RawFd,
}

#[cfg(target_os = "linux")]
impl Landlock {
    pub fn new(
        workspace: &Path,
        extra_read: &[PathBuf],
        extra_write: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        Self::for_roots(
            vec![(workspace.to_path_buf(), true)],
            OutsideRoots::BuildCaches,
            extra_read.to_vec(),
            extra_write.to_vec(),
        )
    }

    /// Build a ruleset for a set of roots and a directory to run the preflight
    /// in. Read-only roots only get the read bits, which is all Landlock needs:
    /// unlike the other backends there is no mount table to prepare.
    fn for_roots(
        roots: Vec<(PathBuf, bool)>,
        outside: OutsideRoots,
        extra_read: Vec<PathBuf>,
        extra_write: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        // A read root that does not exist is a configuration mistake the user
        // cannot see from the inside: Landlock would install no rule and the
        // path stays denied, which looks exactly like the feature not working.
        // `add_path_rule` would fail with ENOENT instead, which is louder but
        // arrives as a raw errno at startup. Name it.
        if let Some(missing) = extra_read.iter().find(|path| !path.exists()) {
            return Err(SandboxError::Setup(format!(
                "[sandbox] extra_read: {} does not exist",
                missing.display()
            )));
        }
        // Same reasoning for the write side, and it matters more — see
        // `check_extra_write`.
        check_extra_write(&extra_write)?;
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
            outside,
            extra_read,
            extra_write,
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

        // `[sandbox] extra_read`: read + execute for whole subtrees. Execute is
        // included on purpose — a directory of binaries nobody may execute is
        // useless, and it is what lets a toolchain outside the distro prefix
        // (`$HOME/cot`, `/opt/tool`, a package manager's own root) be usable by
        // a confined `run_shell` at all.
        for path in &self.extra_read {
            add_path_rule(self.ruleset_fd, path, read_only)?;
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

        // `[sandbox] extra_writable`: the full set, identical to a read-write
        // task root. `allowed` rather than a hand-picked write mask on purpose —
        // anything narrower breaks the ordinary use (write a file, rename it
        // inside the tree, mkdir, delete), and REFER in particular has to be on
        // both parents for a rename to succeed.
        for path in &self.extra_write {
            add_path_rule(self.ruleset_fd, path, allowed)?;
        }

        // Only when the caller asked for them. Landlock has no "private tmpfs"
        // trick: granting `/tmp` here grants the host's `/tmp`, which is why an
        // MCP server could write `/tmp/anything` before `OutsideRoots::Nothing`
        // existed. The build caches are worse — `~/.cargo/config.toml` decides
        // the next `cargo build`'s `rustc-wrapper`.
        if self.outside.grants_build_caches() {
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
        outside: OutsideRoots,
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
        Ok(Arc::new(Self::for_roots(
            mapped,
            outside,
            self.extra_read.clone(),
            self.extra_write.clone(),
        )?))
    }

    fn describe(&self) -> String {
        // `qc doctor` prints this and nothing else about the ruleset, so the
        // extra read roots have to appear here or "what can this actually
        // reach?" is unanswerable from the outside (ADR-0066 §1).
        let mut text = format!(
            "Landlock filesystem confinement (network not restricted, writes: {})",
            if self.outside.grants_build_caches() {
                "roots + build caches + host /tmp"
            } else {
                "roots only"
            }
        );
        if !self.extra_read.is_empty() {
            let list: Vec<String> = self
                .extra_read
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            text.push_str(&format!(", extra read: [{}]", list.join(", ")));
        }
        if !self.extra_write.is_empty() {
            let list: Vec<String> = self
                .extra_write
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            text.push_str(&format!(", extra write: [{}]", list.join(", ")));
        }
        text
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
