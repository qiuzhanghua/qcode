//! Which shell a new terminal should run.
//!
//! The obvious answer — `$SHELL` — is wrong. `$SHELL` is the *login* shell: an
//! inherited environment variable that says what the user was given at login, not
//! what they are using now. Measured: with zsh as the login shell, running a
//! program from bash still reports `/usr/bin/zsh`, so a terminal opened from bash
//! would come up as zsh.
//!
//! What the user expects is the shell they are typing in, which is discoverable
//! from the parent process chain. So the order on Unix is:
//!
//! ```text
//! configured shell > enclosing interactive shell > $SHELL > login shell > /bin/sh
//! ```
//!
//! Every layer is tolerant: a path that does not exist, is not executable, or a
//! parent that cannot be read is skipped, never an error. A typo in the config
//! must not make the terminal impossible to open.
//!
//! One limitation worth knowing: a non-interactive shell running a single `-c`
//! command may `exec` that command instead of forking, which removes itself from
//! the chain (`bash -c qc` → the chain starts at `qc`). Interactive shells always
//! wait for their child, which is the case that matters — someone typing `qc gui`
//! — so this only shows up in scripts, where the chain answers and `$SHELL` is
//! indistinguishable anyway.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Shells that count as "the user is working in this shell".
///
/// `sh` and `dash` are deliberately *not* here. They are script interpreters:
/// `sh -c …` and every `#!/bin/sh` launcher puts one in the parent chain, so
/// "nearest shell" would resolve to dash for almost every program started from a
/// script. Measured on this machine: the chain is `python3 → sh → qc`, which says
/// nothing about the user's shell. Skipping them loses nothing: someone whose
/// actual login shell is dash is caught by the `$SHELL` layer, which reports the
/// same dash.
const INTERACTIVE_SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "nu", "elvish", "xonsh", "tcsh", "csh", "ksh", "mksh", "oksh", "yash",
    "pwsh",
];

/// How far up the parent chain to look.
///
/// The chain from a GUI-launched process is short (shell → session manager →
/// systemd). The bound exists so a corrupted or recycled pid cannot turn the walk
/// into an infinite loop.
const MAX_CHAIN_DEPTH: usize = 12;

/// `PATH` on Windows is separated by `;`, everywhere else by `:`.
const PATH_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// Where the chosen shell came from. For logs, and for the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `[terminal] shell`, resolved and usable.
    Configured,
    /// An interactive shell in the parent chain, i.e. the one the user is in.
    ParentProcess,
    /// `$SHELL`, i.e. the login shell.
    Environment,
    /// The login shell in `/etc/passwd`.
    LoginShell,
    /// `/bin/sh`, or `cmd` on Windows.
    Fallback,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::ParentProcess => "parent process",
            Self::Environment => "$SHELL",
            Self::LoginShell => "login shell",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub program: PathBuf,
    pub source: Source,
}

/// One process, as much of it as the shell search needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    /// The short name (`comm`): reliable even when the executable is unreadable.
    pub comm: String,
    /// The full path, when the OS will tell us. `None` is normal for other users'
    /// processes.
    pub exe: Option<PathBuf>,
}

/// Looks processes up by pid.
///
/// A trait so the search can be tested against invented process trees: the
/// interesting cases — a wrapper `sh` in the chain, a cycle, a chain that is all
/// gone — cannot be produced with real processes.
pub trait ProcessSource {
    fn row(&self, pid: u32) -> Option<ProcessRow>;
}

/// Everything the search needs from the outside world.
///
/// One trait rather than four, because every layer of the search needs a different
/// piece (PATH lookup, environment, `/etc/passwd`, the process tree) and a test
/// wants to set them all at once.
pub trait Probe {
    fn self_pid(&self) -> u32;
    fn env(&self, key: &str) -> Option<String>;
    /// Resolve a configured program the way a shell would: as a path when it
    /// contains a separator, through `PATH` otherwise.
    fn resolve(&self, program: &str) -> Option<PathBuf>;
    /// The login shell from the user database.
    fn login_shell(&self) -> Option<PathBuf>;
    /// Whether the OS would let us execute this path.
    ///
    /// Part of the seam because the search's whole job is deciding which paths are
    /// usable, and a test that cannot make a path unusable cannot test that.
    fn is_executable(&self, path: &Path) -> bool;
    fn processes(&self) -> &dyn ProcessSource;
}

/// Pick the shell to run, from the real system.
pub fn detect() -> Choice {
    detect_with(&System, None)
}

/// Pick the shell to run, with an explicit probe and configuration.
///
/// `configured` is `[terminal] shell`, empty or `None` when unset.
pub fn detect_with(probe: &dyn Probe, configured: Option<&str>) -> Choice {
    let configured = configured.map(str::trim).filter(|name| !name.is_empty());

    if let Some(configured) = configured {
        // A configured shell that is not usable falls through to detection
        // instead of failing: a typo must not make the terminal unopenable.
        if let Some(program) = probe.resolve(configured) {
            return Choice {
                program,
                source: Source::Configured,
            };
        }
    }

    if let Some(program) = platform_default(probe) {
        return program;
    }

    #[cfg(not(windows))]
    {
        if let Some(shell) = probe
            .env("SHELL")
            .map(PathBuf::from)
            .filter(|path| probe.is_executable(path))
        {
            return Choice {
                program: shell,
                source: Source::Environment,
            };
        }
        if let Some(shell) = probe.login_shell().filter(|path| probe.is_executable(path)) {
            return Choice {
                program: shell,
                source: Source::LoginShell,
            };
        }
        Choice {
            program: PathBuf::from("/bin/sh"),
            source: Source::Fallback,
        }
    }

    #[cfg(windows)]
    {
        // `cmd` is located by name, not by path: it is always on the system path.
        let program = probe
            .env("COMSPEC")
            .map(PathBuf::from)
            .filter(|path| probe.is_executable(path))
            .unwrap_or_else(|| PathBuf::from("cmd"));
        Choice {
            program,
            source: Source::Fallback,
        }
    }
}

/// The platform's own idea of a default shell, before the environment is
/// consulted.
fn platform_default(probe: &dyn Probe) -> Option<Choice> {
    #[cfg(not(windows))]
    {
        // The shell the user is actually in, which is not `$SHELL`.
        let program = enclosing_shell(probe, probe.self_pid())?;
        Some(Choice {
            program,
            source: Source::ParentProcess,
        })
    }

    #[cfg(windows)]
    {
        // There is no shell hierarchy to walk: a GUI process is started by the
        // shell's parent, which is often not a shell at all. Probe for the shells
        // that exist instead, newest first.
        for name in ["pwsh", "powershell", "cmd"] {
            if let Some(program) = probe.resolve(name) {
                return Some(Choice {
                    program,
                    source: Source::ParentProcess,
                });
            }
        }
        None
    }
}

/// Walk up from `start` and return the first interactive shell found.
///
/// Stops at a parent that is itself, at pid 0 (init), or after
/// [`MAX_CHAIN_DEPTH`] steps.
pub fn enclosing_shell(probe: &dyn Probe, start: u32) -> Option<PathBuf> {
    let source = probe.processes();
    let mut pid = start;
    let mut visited = Vec::new();

    for _ in 0..MAX_CHAIN_DEPTH {
        if pid == 0 || visited.contains(&pid) {
            return None;
        }
        visited.push(pid);

        let row = source.row(pid)?;

        // The executable path, when readable. A process whose path we cannot read
        // is skipped rather than guessed at: guessing would mean running whatever
        // happens to be first on PATH with that name.
        if let Some(exe) = row
            .exe
            .as_deref()
            .map(clean_path)
            .filter(|path| probe.is_executable(path))
            && INTERACTIVE_SHELLS.contains(&basename(&exe).as_str())
        {
            return Some(exe);
        }

        pid = row.ppid;
    }

    None
}

/// Drop the suffixes the OS appends to a path that is no longer there.
///
/// When a binary is replaced or removed while running, Linux reports its link
/// target as `/usr/bin/zsh (deleted)`. The path as reported cannot be executed,
/// but the shell the user is in may have been reinstalled at the same path — so
/// the suffix comes off and the clean path is what gets checked.
pub fn clean_path(path: &Path) -> PathBuf {
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return path.to_path_buf();
    };
    match name.strip_suffix(" (deleted)") {
        Some(stripped) => path.with_file_name(stripped),
        None => path.to_path_buf(),
    }
}

/// The file name without the directory, without a ` (deleted)` suffix and
/// without a Windows `.exe`.
fn basename(path: &Path) -> String {
    let cleaned = clean_path(path);
    let mut name = cleaned
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let lowered = name.to_ascii_lowercase();
    if let Some(stripped) = lowered.strip_suffix(".exe") {
        name.truncate(stripped.len());
    }

    name
}

/// Whether a path is something the OS would let us execute.
#[cfg(unix)]
fn usable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn usable(path: &Path) -> bool {
    // Windows has no executable bit; the extension is what decides, and
    // `resolve_in_path` has already appended each `PATHEXT` entry before asking.
    path.is_file()
}

// ---------------------------------------------------------------- the real world

/// The system, as the shell search sees it.
pub struct System;

impl Probe for System {
    fn self_pid(&self) -> u32 {
        std::process::id()
    }

    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn resolve(&self, program: &str) -> Option<PathBuf> {
        resolve_in_path(
            program,
            self.env("PATH").as_deref(),
            self.env("PATHEXT").as_deref(),
        )
    }

    fn login_shell(&self) -> Option<PathBuf> {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        login_shell_from(&passwd, current_uid())
    }

    fn is_executable(&self, path: &Path) -> bool {
        usable(path)
    }

    fn processes(&self) -> &dyn ProcessSource {
        proc_source()
    }
}

/// Resolve a program name, as `execvp` would.
pub fn resolve_in_path(
    program: &str,
    path: Option<&str>,
    pathext: Option<&str>,
) -> Option<PathBuf> {
    let candidate = Path::new(program);

    // A program with a separator is a path, not a name to look up. Relative paths
    // are resolved against the current directory, which is what a shell does.
    if program.contains('/') || program.contains('\\') {
        return usable(candidate).then(|| candidate.to_path_buf());
    }

    let extensions: Vec<String> = match pathext {
        Some(pathext) => pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| ext.to_ascii_lowercase())
            .collect(),
        None => vec![String::new()],
    };

    for directory in path
        .unwrap_or("")
        .split(PATH_SEPARATOR)
        .filter(|dir| !dir.is_empty())
    {
        for extension in &extensions {
            let candidate = Path::new(directory).join(format!("{program}{extension}"));
            if usable(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

/// Find the login shell for `uid` in `/etc/passwd`.
///
/// Parsed rather than looked up through `getpwuid` so it can be tested against
/// fixed input; the format is fixed and simple (`name:passwd:uid:gid:gecos:home:shell`).
pub fn login_shell_from(passwd: &str, uid: u32) -> Option<PathBuf> {
    for line in passwd.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split(':');
        // name, passwd, uid
        let _name = fields.next()?;
        let _passwd = fields.next()?;
        let Some(field_uid) = fields.next() else {
            continue;
        };
        if field_uid.parse::<u32>().ok() != Some(uid) {
            continue;
        }
        // gid, gecos, home, shell
        let shell = fields.nth(3)?;
        if shell.is_empty() || shell == "/bin/false" || shell == "/sbin/nologin" {
            return None;
        }
        return Some(PathBuf::from(shell));
    }

    None
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // `getuid` cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(target_os = "linux")]
fn proc_source() -> &'static dyn ProcessSource {
    static SOURCE: OnceLock<ProcFs> = OnceLock::new();
    SOURCE.get_or_init(|| ProcFs)
}

#[cfg(not(target_os = "linux"))]
fn proc_source() -> &'static dyn ProcessSource {
    static SOURCE: OnceLock<PsTable> = OnceLock::new();
    SOURCE.get_or_init(PsTable::new)
}

/// `/proc`, which is on any Linux worth running on and needs no subprocess.
#[cfg(target_os = "linux")]
struct ProcFs;

#[cfg(target_os = "linux")]
impl ProcessSource for ProcFs {
    fn row(&self, pid: u32) -> Option<ProcessRow> {
        let base = PathBuf::from(format!("/proc/{pid}"));
        let status = std::fs::read_to_string(base.join("status")).ok()?;

        let mut ppid = 0;
        let mut comm = String::new();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                ppid = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Name:") {
                comm = rest.trim().to_string();
            }
        }

        // `exe` is a symlink; it is unreadable for other users' processes and
        // after the binary is replaced, which is why it is optional.
        let exe = std::fs::read_link(base.join("exe")).ok();

        Some(ProcessRow {
            pid,
            ppid,
            comm,
            exe,
        })
    }
}

/// The process table from `ps`, for macOS and anything else without `/proc`.
///
/// `ps` rather than `sysctl`: `libc` does not expose Apple's `kinfo_proc`, and
/// declaring that struct's layout by hand — without being able to compile or run
/// it here — would read whatever happens to be at the wrong offset. One `ps`
/// invocation per session is cheap, and its output format is stable enough that
/// the parser can be unit-tested against real output on any Unix.
#[cfg(not(target_os = "linux"))]
struct PsTable {
    rows: std::collections::HashMap<u32, ProcessRow>,
}

#[cfg(not(target_os = "linux"))]
impl PsTable {
    fn new() -> Self {
        let output = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,comm="])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();

        Self {
            rows: parse_ps_table(&output),
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl ProcessSource for PsTable {
    fn row(&self, pid: u32) -> Option<ProcessRow> {
        let mut row = self.rows.get(&pid)?.clone();
        // `comm` is a path on macOS and a bare name on Linux; `proc_pidpath` gives
        // the authoritative path either way.
        if let Some(exe) = process_path(pid) {
            row.exe = Some(exe);
        }
        Some(row)
    }
}

/// Parse `ps -A -o pid=,ppid=,comm=`.
///
/// `comm` may contain spaces, so it is everything after the second field. Lines
/// that do not parse are skipped: the table is advisory.
///
/// Compiled on every platform, not just the ones that use it: the parser is what
/// can silently mis-parse, and this way the macOS path is still covered by tests
/// that run in CI on Linux.
pub fn parse_ps_table(output: &str) -> std::collections::HashMap<u32, ProcessRow> {
    let mut rows = std::collections::HashMap::new();
    for line in output.lines() {
        // `ps` pads its columns with runs of spaces, so splitting on single
        // whitespace characters yields empty fields. Take the runs instead.
        let (pid, rest) = take_field(line);
        let (ppid, rest) = take_field(rest);
        let comm = rest.trim();

        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if comm.is_empty() {
            continue;
        }
        let comm = comm.to_string();
        let exe = Path::new(&comm).is_absolute().then(|| PathBuf::from(&comm));
        rows.insert(
            pid,
            ProcessRow {
                pid,
                ppid,
                comm,
                exe,
            },
        );
    }
    rows
}

/// Split off the first whitespace-delimited field, returning it and the rest.
fn take_field(input: &str) -> (&str, &str) {
    let input = input.trim_start();
    match input.find(char::is_whitespace) {
        Some(at) => (&input[..at], &input[at..]),
        None => (input, ""),
    }
}

/// The real process row for `pid`.
///
/// Exposed so a caller (and a test) can check the live table against another
/// source instead of trusting the search's own output.
pub fn system_process_row(pid: u32) -> Option<ProcessRow> {
    proc_source().row(pid)
}

/// The executable path for a pid, via `proc_pidpath` where it exists.
#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Option<PathBuf> {
    // `PROC_PIDPATHINFO_MAXSIZE`
    let mut buffer = [0u8; 4096];
    let written = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        return None;
    }

    let bytes = &buffer[..written as usize];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(PathBuf::from)
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn process_path(_pid: u32) -> Option<PathBuf> {
    None
}

/// The working directory a live process is actually in (ADR-0062 §3).
///
/// This is the *real* cwd, not the one recorded at spawn: after the user types
/// `cd`, the shell's own directory is what a new terminal should inherit.
///
/// Known limits, both accepted in the ADR: a symlinked path comes back resolved
/// (`/proc/<pid>/cwd` and `proc_pidinfo` both report the physical path), and this
/// is the shell's directory, not the foreground job's.
pub fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStrExt as _;

        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // `proc_pidinfo` returns the bytes written, or <= 0 on failure.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written < size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        // libc models the path as `[[c_char; 32]; 32]` (an old-rustc workaround for
        // a 1024-byte array), so flatten it before reading it as a C string.
        let path =
            unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast::<libc::c_char>()) };
        let bytes = path.to_bytes();
        if bytes.is_empty() {
            return None;
        }
        Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Where a new terminal should start (ADR-0062 §3).
///
/// `inherited` is the live cwd of the terminal the user was last in; `configured`
/// is `[terminal] start_cwd`. A directory that no longer exists is skipped rather
/// than handed to `spawn_command`, which would leave the shell unable to start.
pub fn resolve_start_cwd(
    inherited: Option<std::path::PathBuf>,
    configured: &str,
    home: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    if let Some(path) = inherited.filter(|path| path.is_dir()) {
        return Some(path);
    }
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = std::path::PathBuf::from(configured);
        if path.is_dir() {
            return Some(path);
        }
    }
    if let Some(home) = home.filter(|path| path.is_dir()) {
        return Some(home);
    }
    std::env::current_dir().ok().filter(|path| path.is_dir())
}
