//! Windows ACL sandbox: a write-restricted token plus per-root capability SIDs.
//!
//! The backend follows ADR-0027. `run_shell` does not launch the shell directly;
//! it launches `qc sandbox-run --workspace <w> --temp <t> -- <program> <args>`.
//! That hidden subcommand
//!
//! 1. derives a capability SID for the workspace (stable, one ACE per root) and
//!    one for a private temp directory (fresh per run, so two sessions cannot
//!    write each other's temp),
//! 2. grants both SIDs write access on their root (`FILE_ALL_ACCESS`, inherited
//!    by the whole tree),
//! 3. opens its own token, attaches the logon SID, `Everyone` and the two
//!    capability SIDs as *restricting* SIDs, and makes it write-restricted, so
//!    every write access check has to pass with those SIDs as well,
//! 4. appends the temp SID to the token default DACL, which is what new objects
//!    (anonymous pipes for grandchildren) get,
//! 5. spawns the program under that token in a kill-on-close job object.
//!
//! Why the token and not just the ACEs: a capability ACE grants, it never
//! denies. The write restriction is what makes every write outside the two roots
//! fail, including deletes through the parent directory's `FILE_DELETE_CHILD`
//! (which a restricting SID does not have to co-sign).
//!
//! Known boundaries, documented in the ADR and [`WindowsAcl::describe`]:
//!
//! - read access is not restricted, and neither is network access (the egress
//!   proxy environment variables remain advisory for a shell);
//! - hard links alias a file object, so a link inside the workspace can point at
//!   an outside file;
//! - a confined grandchild cannot open a *named* pipe (libuv's `stdio: pipe`
//!   does exactly that), so Node subprocesses that pipe stdio fail with EPERM;
//!   inherited and anonymous pipes work;
//! - the capability ACEs are permanent until `qc sandbox revoke`.
#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, REVOKE_ACCESS,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AllocateAndInitializeSid, CONTAINER_INHERIT_ACE,
    CreateRestrictedToken, DACL_SECURITY_INFORMATION, EqualSid, GetLengthSid, GetTokenInformation,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, SID_IDENTIFIER_AUTHORITY,
    SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL,
    TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    TOKEN_USER, TokenDefaultDacl, TokenElevation, TokenGroups, TokenIntegrityLevel,
    TokenIsAppContainer, TokenLogonSid, TokenRestrictedSids, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateMutexW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    INFINITE, OpenProcessToken, PROCESS_INFORMATION, ReleaseMutex, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};

use crate::sandbox::{Sandbox, SandboxError};

/// The hidden subcommand that [`WindowsAcl::command`] launches.
pub const HELPER_COMMAND: &str = "sandbox-run";

/// Exit code the helper uses when it cannot set the sandbox up. It never falls
/// back to an unconfined spawn.
const EXIT_SETUP_FAILED: i32 = 127;

/// `DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`.
///
/// The privileged flag is best effort: a plain user cannot always request it,
/// and the probe showed the capability SIDs alone already deny outside writes.
const RESTRICT_FLAGS: [u32; 4] = [0x1 | 0x4 | 0x8, 0x1 | 0x8, 0x8, 0];

const SECURITY_NON_UNIQUE_AUTHORITY: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
    Value: [0, 0, 0, 0, 0, 4],
};
const SECURITY_WORLD_SID: &str = "S-1-1-0";
/// `OICI`: inherit into child containers and objects.
const INHERIT_BOTH: u32 = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

/// Runs `run_shell` commands under a write-restricted token.
#[derive(Debug)]
pub struct WindowsAcl {
    workspace: PathBuf,
    temp: PathBuf,
    extra: Vec<PathBuf>,
    helper: PathBuf,
}

impl WindowsAcl {
    pub fn new(workspace: &Path, extra_writable: &[PathBuf]) -> Result<Self, SandboxError> {
        let workspace = std::fs::canonicalize(workspace).map_err(|err| {
            SandboxError::Setup(format!(
                "canonicalize workspace {}: {err}",
                workspace.display()
            ))
        })?;
        let temp = private_temp(&workspace)?;
        let helper = std::env::current_exe()
            .map_err(|err| SandboxError::Setup(format!("current_exe: {err}")))?;

        let mut extra = Vec::new();
        for path in extra_writable {
            let canonical = std::fs::canonicalize(path).map_err(|err| {
                SandboxError::Setup(format!("canonicalize {}: {err}", path.display()))
            })?;
            if canonical.starts_with(&workspace) || workspace.starts_with(&canonical) {
                return Err(SandboxError::Setup(format!(
                    "extra writable root {} overlaps the workspace {}; pick one",
                    canonical.display(),
                    workspace.display()
                )));
            }
            extra.push(canonical);
        }

        Ok(Self {
            workspace,
            temp,
            extra,
            helper,
        })
    }

    fn helper_command(&self, program: &OsStr, args: &[OsString], cwd: &Path) -> Command {
        let mut command = Command::new(&self.helper);
        command.arg(HELPER_COMMAND);
        command.arg("--workspace").arg(&self.workspace);
        command.arg("--temp").arg(&self.temp);
        command.arg("--cwd").arg(cwd);
        for root in &self.extra {
            command.arg("--extra").arg(root);
        }
        command.arg("--").arg(program).args(args);
        command.current_dir(cwd);
        command
    }
}

impl Sandbox for WindowsAcl {
    fn name(&self) -> &'static str {
        "windows-acl"
    }

    fn describe(&self) -> String {
        format!(
            "windows-acl (partial): write-restricted token; writes allowed only under {} and {}; \
             reads and network are not restricted; hard links and named pipes are not covered",
            self.workspace.display(),
            self.temp.display()
        )
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        Ok(self.helper_command(program, args, cwd))
    }

    /// End-to-end preflight: spawn a trivial command through the real helper.
    /// A failure here means the token or the grants do not work on this machine,
    /// and startup aborts instead of running unsandboxed.
    fn check(&self) -> Result<(), SandboxError> {
        let output = self
            .helper_command(
                OsStr::new("cmd.exe"),
                &[OsString::from("/c exit 0")],
                &self.workspace,
            )
            .output()
            .map_err(|err| SandboxError::Preflight {
                name: "windows-acl",
                detail: format!("spawn the helper: {err}"),
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SandboxError::Preflight {
            name: "windows-acl",
            detail: format!(
                "helper exited with {}: {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "no exit code".to_string()),
                stderr.trim()
            ),
        })
    }
}

impl Drop for WindowsAcl {
    fn drop(&mut self) {
        // Best effort: the directory is a throwaway private temp.
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

/// A fresh directory under `%LOCALAPPDATA%\qcode\sandbox\tmp`, falling back to
/// the session temp directory and finally to the workspace when neither accepts
/// new directories.
///
/// The directory is ours and gets a capability ACE of its own; the surrounding
/// temp directory is never granted, so this is not an implicit writable root.
fn private_temp(workspace: &Path) -> Result<PathBuf, SandboxError> {
    let unique = format!(
        "{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or_default()
    );
    let mut bases = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push(
            PathBuf::from(local)
                .join("qcode")
                .join("sandbox")
                .join("tmp"),
        );
    }
    bases.push(std::env::temp_dir().join("qcode-sandbox-tmp"));
    // Some machines refuse new directories under both `%LOCALAPPDATA%` and
    // `%TEMP%` — an ACL oddity observed in the wild — and the workspace is
    // already a granted root, so a private temp inside it keeps the boundary.
    bases.push(workspace.join(".tmp").join("qcode-sandbox-tmp"));

    let mut failures = Vec::new();
    for (index, base) in bases.into_iter().enumerate() {
        let temp = base.join(&unique);
        match std::fs::create_dir_all(&temp) {
            Ok(()) => {
                if index > 1 {
                    tracing::warn!(
                        path = %temp.display(),
                        "no writable system temp for the sandbox; using a private temp inside the workspace"
                    );
                }
                return Ok(temp);
            }
            Err(err) => failures.push(format!("create private temp {}: {err}", temp.display())),
        }
    }
    Err(SandboxError::Setup(if failures.is_empty() {
        "no usable temp directory for the sandbox".to_string()
    } else {
        failures.join("; ")
    }))
}

/// Runs `qc sandbox-run ...`. Returns the process exit code.
pub fn run_helper(argv: &[String]) -> i32 {
    match execute_helper(argv) {
        Ok(code) => code,
        Err(detail) => {
            eprintln!("qc-sandbox-run: {detail}");
            EXIT_SETUP_FAILED
        }
    }
}

fn execute_helper(argv: &[String]) -> Result<i32, String> {
    let mut workspace = None;
    let mut temp = None;
    let mut cwd = None;
    let mut extra = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--" => {
                index += 1;
                break;
            }
            "--workspace" => {
                workspace = Some(PathBuf::from(next(argv, &mut index)?));
            }
            "--temp" => {
                temp = Some(PathBuf::from(next(argv, &mut index)?));
            }
            "--cwd" => {
                cwd = Some(PathBuf::from(next(argv, &mut index)?));
            }
            "--extra" => {
                extra.push(PathBuf::from(next(argv, &mut index)?));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        index += 1;
    }
    let workspace = workspace.ok_or("missing --workspace")?;
    let temp = temp.ok_or("missing --temp")?;
    let cwd = cwd.unwrap_or_else(|| workspace.clone());
    let program = argv.get(index).ok_or("missing program after '--'")?.clone();
    let args: Vec<String> = argv[index + 1..].to_vec();

    let workspace = workspace
        .canonicalize()
        .map_err(|err| format!("canonicalize workspace {}: {err}", workspace.display()))?;
    let temp = temp
        .canonicalize()
        .map_err(|err| format!("canonicalize temp {}: {err}", temp.display()))?;
    let cwd = cwd
        .canonicalize()
        .map_err(|err| format!("canonicalize cwd {}: {err}", cwd.display()))?;

    if !workspace.is_dir() {
        return Err(format!(
            "workspace {} is not a directory",
            workspace.display()
        ));
    }
    if !temp.is_dir() {
        return Err(format!("temp {} is not a directory", temp.display()));
    }
    if !cwd.is_dir() {
        return Err(format!("cwd {} is not a directory", cwd.display()));
    }

    let workspace_sid = derive_sid(&workspace, "ws")?;
    let temp_sid = derive_sid(&temp, "tmp")?;
    let mut roots = vec![workspace.clone(), temp.clone()];
    roots.extend(extra.iter().cloned());
    for root in &roots {
        grant_root(root, workspace_sid.as_sid())?;
    }

    let token = build_token(&workspace_sid, &temp_sid)?;
    // The child inherits this process's environment block (`lpEnvironment` has
    // to stay NULL), so the only way to hand it a writable temp is to point
    // TMP/TEMP at the granted private directory here. `shell.rs` clears the
    // environment before spawning the helper, so setting this on the command
    // line would not survive; the helper is a short-lived `sandbox-run`
    // invocation whose other threads do not read the environment.
    unsafe {
        std::env::set_var("TMP", &temp);
        std::env::set_var("TEMP", &temp);
        std::env::set_var("TMPDIR", &temp);
    }
    let code = spawn_confined(
        &token.handle,
        Path::new(&program),
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
        &cwd,
    )
    .map_err(|err| err.to_string())?;
    Ok(code as i32)
}

fn next(argv: &[String], index: &mut usize) -> Result<String, String> {
    *index += 1;
    argv.get(*index)
        .cloned()
        .ok_or_else(|| format!("missing value for '{}'", argv[*index - 1]))
}

/// A SID copy that keeps the bytes 4-byte aligned for the Win32 APIs.
struct SidBuf {
    words: Vec<u32>,
}

impl SidBuf {
    fn from_sid(sid: PSID) -> Self {
        let size = unsafe { GetLengthSid(sid) as usize };
        let mut words = vec![0u32; size.div_ceil(4)];
        unsafe {
            std::ptr::copy_nonoverlapping(sid as *const u8, words.as_mut_ptr() as *mut u8, size);
        }
        Self { words }
    }

    fn as_sid(&self) -> PSID {
        self.words.as_ptr() as PSID
    }
}

/// Non-unique capability SID `S-1-4-x-y`, stable per (root, domain).
///
/// A renamed workspace gets a new SID; the old ACE stays behind as an inert
/// leftover until `qc sandbox revoke`.
fn derive_sid(path: &Path, domain: &str) -> Result<SidBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    sid_for_root(&canonical.display().to_string().to_lowercase(), domain)
}

/// Capability SID for one root, derived from its canonical (lowercased) path and
/// the domain it is used in.
fn sid_for_root(root: &str, domain: &str) -> Result<SidBuf, String> {
    let seed = format!("qcode-sandbox|{domain}|{root}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    let high = ((hash >> 32) as u32 & 0x7fff_ffff) | 1;
    let low = (hash as u32 & 0x7fff_ffff) | 1;
    let mut sid: PSID = std::ptr::null_mut();
    let ok = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NON_UNIQUE_AUTHORITY,
            2,
            high,
            low,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut sid,
        )
    };
    if ok == 0 {
        return Err(last_error("AllocateAndInitializeSid"));
    }
    let buffer = SidBuf::from_sid(sid);
    unsafe { LocalFree(sid) };
    Ok(buffer)
}

fn string_sid(value: &str) -> Result<SidBuf, String> {
    let mut raw: PSID = std::ptr::null_mut();
    let ok = unsafe { ConvertStringSidToSidW(wide(OsStr::new(value)).as_ptr(), &mut raw) };
    if ok == 0 {
        return Err(last_error(&format!("ConvertStringSidToSidW({value})")));
    }
    let buffer = SidBuf::from_sid(raw);
    unsafe { LocalFree(raw) };
    Ok(buffer)
}

fn sid_string(sid: PSID) -> String {
    unsafe {
        let bytes = sid as *const u8;
        let revision = *bytes as usize;
        let count = *bytes.add(1) as usize;
        let authority = std::slice::from_raw_parts(bytes.add(2), 6);
        let mut value: u64 = 0;
        for byte in authority {
            value = (value << 8) | u64::from(*byte);
        }
        let subs = std::slice::from_raw_parts(bytes.add(8) as *const u32, count);
        let mut out = format!("S-{revision}-{value}");
        for sub in subs {
            out.push_str(&format!("-{sub}"));
        }
        out
    }
}

fn trustee(sid: PSID) -> TRUSTEE_W {
    TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    }
}

fn explicit_access(sid: PSID, mask: u32, mode: i32, inheritance: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: trustee(sid),
    }
}

/// Grants `sid` write access under `root`, inherited by the whole tree.
///
/// Returns `false` when the ACE was already there, which means the tree is
/// already granted: the eager walk that `SetNamedSecurityInfoW` does only has to
/// happen once per root.
fn grant_root(root: &Path, sid: PSID) -> Result<bool, String> {
    let _guard = GrantLock::acquire(&sid_string(sid))?;
    if tree_granted(root, sid)? {
        return Ok(false);
    }
    let name = wide(root.as_os_str());
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(format!(
                "GetNamedSecurityInfoW({}): win32 {rc}",
                root.display()
            ));
        }

        let entries = [explicit_access(
            sid,
            FILE_ALL_ACCESS,
            GRANT_ACCESS,
            INHERIT_BOTH,
        )];
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let rc = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut new_acl);
        if rc != 0 {
            LocalFree(sd);
            return Err(format!("SetEntriesInAclW({}): win32 {rc}", root.display()));
        }
        let rc = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null_mut(),
        );
        LocalFree(new_acl as *mut std::ffi::c_void);
        LocalFree(sd);
        if rc != 0 {
            let hint = if rc == 5 {
                ": access denied; the sandbox needs write-DACL on this root (own it, or grant \
                 `extra_writable` on a root you control)"
            } else {
                ""
            };
            return Err(format!(
                "SetNamedSecurityInfoW({}): win32 {rc}{hint}",
                root.display()
            ));
        }
    }
    Ok(true)
}

/// Grants the workspace capability ACE on `path` and everything below it.
///
/// This is what `qc sandbox grant` calls: it makes a directory writable for the
/// sandbox without running a command in it first.
pub fn grant_path(path: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(path)
        .map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    let sid = derive_sid(&root, "ws")?;
    grant_root(&root, sid.as_sid())?;
    Ok(root)
}

/// Removes the workspace capability ACE from `path` and everything below it.
///
/// Returns the canonical root and how many objects lost an ACE. Directories are
/// walked explicitly because removing the inheritable ACE from the root does not
/// reach the copies that inheritance already wrote onto existing children.
pub fn revoke_path(path: &Path) -> Result<(PathBuf, usize), String> {
    let root = std::fs::canonicalize(path)
        .map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    let sid = derive_sid(&root, "ws")?;
    let mut removed = 0usize;
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        removed += revoke_object(&dir, sid.as_sid())?;
        let entries =
            std::fs::read_dir(&dir).map_err(|err| format!("read_dir {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("read_dir {}: {err}", dir.display()))?;
            let kind = entry
                .file_type()
                .map_err(|err| format!("file_type {}: {err}", entry.path().display()))?;
            if kind.is_dir() {
                if !kind.is_symlink() {
                    pending.push(entry.path());
                }
            } else {
                removed += revoke_object(&entry.path(), sid.as_sid())?;
            }
        }
    }
    Ok((root, removed))
}

fn revoke_object(path: &Path, sid: PSID) -> Result<usize, String> {
    let name = wide(path.as_os_str());
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(format!(
                "GetNamedSecurityInfoW({}): win32 {rc}",
                path.display()
            ));
        }
        let owned = if dacl.is_null() {
            0
        } else {
            count_aces_for(&*dacl, sid)
        };
        if owned == 0 {
            LocalFree(sd);
            return Ok(0);
        }
        // A blank `grfAccessPermissions` with REVOKE_ACCESS drops every ACE that
        // names this trustee, whatever mask or inheritance flags it carries.
        let entries = [explicit_access(sid, 0, REVOKE_ACCESS, 0)];
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let rc = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut new_acl);
        if rc != 0 {
            LocalFree(sd);
            return Err(format!(
                "SetEntriesInAclW(revoke {}, {}): win32 {rc}",
                path.display(),
                sid_string(sid)
            ));
        }
        let rc = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null_mut(),
        );
        LocalFree(new_acl as *mut std::ffi::c_void);
        LocalFree(sd);
        if rc != 0 {
            return Err(format!(
                "SetNamedSecurityInfoW({}): win32 {rc}",
                path.display()
            ));
        }
        Ok(owned)
    }
}

/// Counts the ACEs that name `sid`, ignoring type and inheritance.
fn count_aces_for(acl: &ACL, sid: PSID) -> usize {
    let mut count = 0;
    unsafe {
        let mut cursor = (acl as *const ACL as *const u8).add(std::mem::size_of::<ACL>());
        for _ in 0..acl.AceCount {
            let header = &*(cursor as *const ACE_HEADER);
            let ace = &*(cursor as *const ACCESS_ALLOWED_ACE);
            if EqualSid(&ace.SidStart as *const u32 as PSID, sid) != 0 {
                count += 1;
            }
            if header.AceSize == 0 {
                break;
            }
            cursor = cursor.add(header.AceSize as usize);
        }
    }
    count
}

/// True when `root` already carries an inheritable allow ACE for `sid`.
fn tree_granted(root: &Path, sid: PSID) -> Result<bool, String> {
    let name = wide(root.as_os_str());
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(format!(
                "GetNamedSecurityInfoW({}): win32 {rc}",
                root.display()
            ));
        }
        let mut found = false;
        if !dacl.is_null() {
            let acl = &*dacl;
            let mut cursor = (dacl as *const u8).add(std::mem::size_of::<ACL>());
            for _ in 0..acl.AceCount {
                let header = &*(cursor as *const ACE_HEADER);
                if header.AceType == 0 && u32::from(header.AceFlags) & INHERIT_BOTH == INHERIT_BOTH
                {
                    let ace = &*(cursor as *const ACCESS_ALLOWED_ACE);
                    if EqualSid(&ace.SidStart as *const u32 as PSID, sid) != 0 {
                        found = true;
                        break;
                    }
                }
                if header.AceSize == 0 {
                    break;
                }
                cursor = cursor.add(header.AceSize as usize);
            }
        }
        LocalFree(sd);
        Ok(found)
    }
}

/// Serialises the read-merge-write of grants across processes.
///
/// Two `qc` processes starting on the same workspace would otherwise both read
/// the old DACL and the last writer would drop the other's ACE.
struct GrantLock(HANDLE);

impl GrantLock {
    fn acquire(sid: &str) -> Result<Self, String> {
        let name = wide(OsStr::new(&format!("Local\\qcode-sandbox-grant-{sid}")));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error("CreateMutexW"));
        }
        let rc = unsafe { WaitForSingleObject(handle, INFINITE) };
        // WAIT_OBJECT_0 = 0, WAIT_ABANDONED = 0x80: both own the mutex.
        if rc != 0 && rc != 0x80 {
            unsafe { CloseHandle(handle) };
            return Err(format!("WaitForSingleObject(mutex): {rc}"));
        }
        Ok(Self(handle))
    }
}

impl Drop for GrantLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

struct TokenBuilder {
    handle: HANDLE,
}

impl Drop for TokenBuilder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Creates the write-restricted token the child runs under.
///
fn build_token(workspace_sid: &SidBuf, temp_sid: &SidBuf) -> Result<TokenBuilder, String> {
    let owner = TokenBuilder::open()?;
    let logon = owner.logon_sid()?;
    let everyone = string_sid(SECURITY_WORLD_SID)?;
    let sids = [
        logon.as_sid(),
        everyone.as_sid(),
        workspace_sid.as_sid(),
        temp_sid.as_sid(),
    ];
    let (handle, flags) = owner.restrict(&sids)?;
    tracing::debug!(flags, "windows-acl token restricted");

    let token = TokenBuilder { handle };
    // Anonymous pipes created by the child carry the token default DACL, and the
    // client end is opened by the child itself; without the capability SID the
    // second access check fails and piped stdio breaks.
    token.extend_default_dacl(temp_sid.as_sid())?;
    Ok(token)
}

impl TokenBuilder {
    fn open() -> Result<Self, String> {
        let mut handle: HANDLE = std::ptr::null_mut();
        let access = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) };
        if ok == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        Ok(Self { handle })
    }

    fn logon_sid(&self) -> Result<SidBuf, String> {
        let mut needed: u32 = 0;
        unsafe {
            GetTokenInformation(
                self.handle,
                TokenLogonSid,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenLogonSid, size)"));
        }
        let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
        let ok = unsafe {
            GetTokenInformation(
                self.handle,
                TokenLogonSid,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetTokenInformation(TokenLogonSid)"));
        }
        // `TokenLogonSid` returns a `TOKEN_GROUPS` with the single logon SID.
        let groups =
            unsafe { &*(buffer.as_ptr() as *const windows_sys::Win32::Security::TOKEN_GROUPS) };
        if groups.GroupCount == 0 {
            return Err("TokenLogonSid: empty group list".to_string());
        }
        Ok(SidBuf::from_sid(unsafe { (*groups.Groups.as_ptr()).Sid }))
    }

    fn restrict(&self, sids: &[PSID]) -> Result<(HANDLE, u32), String> {
        let attributes: Vec<SID_AND_ATTRIBUTES> = sids
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: *sid,
                Attributes: 0,
            })
            .collect();
        let mut last = 0;
        for flags in RESTRICT_FLAGS {
            let mut out: HANDLE = std::ptr::null_mut();
            let ok = unsafe {
                CreateRestrictedToken(
                    self.handle,
                    flags,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    attributes.len() as u32,
                    attributes.as_ptr(),
                    &mut out,
                )
            };
            if ok != 0 {
                return Ok((out, flags));
            }
            last = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        }
        Err(format!(
            "CreateRestrictedToken failed (win32 {last}); this user or an already-restricted parent \
             token cannot be restricted, so `windows-acl` cannot be enforced on this machine"
        ))
    }

    /// Appends the temp capability SID to the token's existing default DACL.
    ///
    /// The default DACL must not be *replaced*: the child still has to open its
    /// own objects with ambient access, so dropping the inherited entries breaks
    /// every pipe.
    fn extend_default_dacl(&self, temp_sid: PSID) -> Result<(), String> {
        let mut needed: u32 = 0;
        unsafe {
            GetTokenInformation(
                self.handle,
                TokenDefaultDacl,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenDefaultDacl, size)"));
        }
        let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
        let ok = unsafe {
            GetTokenInformation(
                self.handle,
                TokenDefaultDacl,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetTokenInformation(TokenDefaultDacl)"));
        }
        let current = unsafe { (*(buffer.as_ptr() as *const TOKEN_DEFAULT_DACL)).DefaultDacl };
        let entries = [explicit_access(temp_sid, FILE_ALL_ACCESS, GRANT_ACCESS, 0)];
        let mut merged: *mut ACL = std::ptr::null_mut();
        let rc = unsafe {
            SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), current, &mut merged)
        };
        if rc != 0 {
            return Err(format!("SetEntriesInAclW(token default dacl): win32 {rc}"));
        }
        let dacl = TOKEN_DEFAULT_DACL {
            DefaultDacl: merged,
        };
        let ok = unsafe {
            SetTokenInformation(
                self.handle,
                TokenDefaultDacl,
                &dacl as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
            )
        };
        unsafe { LocalFree(merged as *mut std::ffi::c_void) };
        if ok == 0 {
            return Err(last_error("SetTokenInformation(TokenDefaultDacl)"));
        }
        Ok(())
    }
}

/// Spawns `program` under `token`, inheriting the helper's own std handles.
fn spawn_confined(
    token: &HANDLE,
    program: &Path,
    args: &[OsString],
    cwd: &Path,
) -> io::Result<u32> {
    let program = resolve_program(program.as_os_str());
    let mut command_line = quote_arg(program.as_os_str());
    for arg in args {
        command_line.push(' ');
        command_line.push_str(&quote_arg(arg));
    }
    let mut command_line: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let exe_name = wide(program.as_os_str());
    let cwd_name = wide(plain_path(cwd).as_os_str());

    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };

    unsafe {
        let stdin = inherit_std(STD_INPUT_HANDLE, GENERIC_READ | GENERIC_WRITE, &attributes)?;
        let stdout = inherit_std(STD_OUTPUT_HANDLE, GENERIC_WRITE, &attributes)?;
        let stderr = inherit_std(STD_ERROR_HANDLE, GENERIC_WRITE, &attributes)?;

        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin.handle,
            hStdOutput: stdout.handle,
            hStdError: stderr.handle,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let ok = CreateProcessAsUserW(
            *token,
            exe_name.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED,
            // `lpEnvironment` must stay NULL: an explicit block makes the call
            // fail with ERROR_INVALID_PARAMETER for a restricted token.
            std::ptr::null(),
            cwd_name.as_ptr(),
            &startup,
            &mut process,
        );
        if ok == 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::other(format!(
                "start {}: {err}",
                program.display()
            )));
        }

        // Kill the whole tree when the helper dies, including on a panic.
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            AssignProcessToJobObject(job, process.hProcess);
        }
        ResumeThread(process.hThread);

        WaitForSingleObject(process.hProcess, INFINITE);
        let mut code: u32 = 0;
        GetExitCodeProcess(process.hProcess, &mut code);

        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
        if !job.is_null() {
            CloseHandle(job);
        }
        Ok(code)
    }
}

struct InheritedHandle {
    handle: HANDLE,
    close: bool,
}

/// Makes one of the helper's std handles inheritable, or substitutes `NUL`.
fn inherit_std(
    which: u32,
    fallback_access: u32,
    attributes: &windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
) -> io::Result<InheritedHandle> {
    unsafe {
        let handle = GetStdHandle(which);
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            windows_sys::Win32::Foundation::SetHandleInformation(
                handle,
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            );
            return Ok(InheritedHandle {
                handle,
                close: false,
            });
        }
        let nul_name = wide(OsStr::new("NUL"));
        let handle = CreateFileW(
            nul_name.as_ptr(),
            fallback_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(InheritedHandle {
            handle,
            close: true,
        })
    }
}

impl Drop for InheritedHandle {
    fn drop(&mut self) {
        if self.close && !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Turns a verbatim (`\\?\`) path back into a plain DOS path.
///
/// Grants need the verbatim form, but a child such as `cmd.exe` reads `\\?\C:\x`
/// as a UNC path, refuses it as its current directory and silently falls back to
/// the Windows directory. The workspace is canonicalized (which yields the
/// verbatim form), so strip it again right before the child starts.
fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// Resolves `program` against `PATH` only.
///
/// `CreateProcessAsUserW` searches the current directory before `PATH`, and the
/// current directory here is the workspace the agent is working in, so a file
/// named `sh.exe` in the repository would win over the real shell. Resolving
/// explicitly keeps the workspace out of the lookup.
fn resolve_program(program: &OsStr) -> PathBuf {
    let path = Path::new(program);
    let text = program.to_string_lossy();
    if path.is_absolute() || text.contains(['\\', '/']) {
        return path.to_path_buf();
    }
    let mut names = vec![program.to_os_string()];
    if path.extension().is_none() {
        let mut with_exe = program.to_os_string();
        with_exe.push(".exe");
        names.push(with_exe);
    }
    if let Some(entries) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&entries) {
            if dir.as_os_str().is_empty() || dir == Path::new(".") {
                continue;
            }
            for name in &names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    path.to_path_buf()
}

/// Quotes one argument for a `CreateProcessW` command line.
fn quote_arg(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.into_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes sit right before the closing quote, so they have to
    // be doubled or they would escape it.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

/// What the process token and the temp bases look like, for `qc whoami`.
///
/// This is a diagnostic, not part of the boundary: it exists because a shell and
/// the programs it starts can disagree about which directories accept a new
/// subdirectory, and the reason (integrity level, restricted SIDs, an
/// AppContainer, an object manager redirect) is invisible from the outside.
pub fn diagnose() -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("pid: {}", std::process::id()));
    lines.push(format!(
        "exe: {}",
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|err| format!("error: {err}"))
    ));
    lines.push(format!(
        "cwd: {}",
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|err| format!("error: {err}"))
    ));
    match token_facts() {
        Ok(facts) => lines.extend(facts),
        Err(err) => lines.push(format!("token: error: {err}")),
    }
    for key in ["TEMP", "TMP", "LOCALAPPDATA", "USERPROFILE"] {
        match std::env::var_os(key) {
            Some(value) => lines.push(format!("env {key}={}", value.to_string_lossy())),
            None => lines.push(format!("env {key}: unset")),
        }
    }
    let mut bases = vec![("temp_dir", std::env::temp_dir())];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push((
            "local_app_data",
            PathBuf::from(local)
                .join("qcode")
                .join("sandbox")
                .join("tmp"),
        ));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|span| span.subsec_nanos())
        .unwrap_or_default();
    for (label, base) in bases {
        let probe = base.join(format!("qcode-diagnose-{}-{nanos}", std::process::id()));
        match std::fs::create_dir_all(&probe) {
            Ok(()) => {
                lines.push(format!("create {label} {}: ok", probe.display()));
                let _ = std::fs::remove_dir_all(&probe);
            }
            Err(err) => lines.push(format!("create {label} {}: error: {err}", probe.display())),
        }
    }
    lines
}

fn token_facts() -> Result<Vec<String>, String> {
    struct Guard(HANDLE);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    let mut handle: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let _guard = Guard(handle);

    let mut lines = Vec::new();
    if let Ok(buffer) = query_token(handle, TokenUser) {
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        lines.push(format!("token user: {}", sid_string(user.User.Sid)));
    }
    match query_token(handle, TokenIntegrityLevel) {
        Ok(buffer) => {
            let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
            lines.push(format!("token integrity: {}", sid_string(label.Label.Sid)));
        }
        Err(err) => lines.push(format!("token integrity: error: {err}")),
    }
    match query_token(handle, TokenRestrictedSids) {
        Ok(buffer) => {
            let groups = unsafe { &*(buffer.as_ptr() as *const TOKEN_GROUPS) };
            let entries = groups.Groups.as_ptr();
            let mut sids = Vec::new();
            for index in 0..groups.GroupCount as usize {
                let sid = unsafe { (*entries.add(index)).Sid };
                sids.push(sid_string(sid));
            }
            lines.push(format!(
                "token restricted sids: {} [{}]",
                groups.GroupCount,
                sids.join(", ")
            ));
        }
        Err(err) => lines.push(format!("token restricted sids: error: {err}")),
    }
    match query_token(handle, TokenGroups) {
        Ok(buffer) => {
            let groups = unsafe { &*(buffer.as_ptr() as *const TOKEN_GROUPS) };
            lines.push(format!("token groups: {}", groups.GroupCount));
        }
        Err(err) => lines.push(format!("token groups: error: {err}")),
    }
    if let Ok(buffer) = query_token(handle, TokenIsAppContainer) {
        lines.push(format!("token app container: {}", unsafe {
            *buffer.as_ptr()
        }));
    }
    if let Ok(buffer) = query_token(handle, TokenElevation) {
        lines.push(format!("token elevated: {}", unsafe { *buffer.as_ptr() }));
    }
    Ok(lines)
}

fn query_token(handle: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u32>, String> {
    let mut needed: u32 = 0;
    unsafe { GetTokenInformation(handle, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(last_error("GetTokenInformation(size)"));
    }
    let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
    let ok = unsafe {
        GetTokenInformation(
            handle,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_arguments_with_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(quote_arg(OsStr::new("plain")), "plain");
        assert_eq!(quote_arg(OsStr::new("two words")), "\"two words\"");
        assert_eq!(quote_arg(OsStr::new("say \"hi\"")), r#""say \"hi\"""#);
        // Without a space or quote there is nothing to protect, so the trailing
        // backslash stays as it is; it only has to be doubled when the argument
        // is quoted and the backslash lands right before the closing quote.
        assert_eq!(quote_arg(OsStr::new("keep\\")), "keep\\");
        assert_eq!(quote_arg(OsStr::new("keep \\")), r#""keep \\""#);
        assert_eq!(quote_arg(OsStr::new("")), "\"\"");
    }

    #[test]
    fn verbatim_paths_lose_their_prefix_before_the_child_starts() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\work")), Path::new(r"C:\work"));
        assert_eq!(
            plain_path(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\server\share")
        );
        assert_eq!(plain_path(Path::new(r"C:\work")), Path::new(r"C:\work"));
    }

    #[test]
    fn capability_sids_are_stable_per_root_and_domain() {
        let a = sid_string(sid_for_root(r"c:\work", "ws").unwrap().as_sid());
        let b = sid_string(sid_for_root(r"c:\work", "ws").unwrap().as_sid());
        let c = sid_string(sid_for_root(r"c:\work", "tmp").unwrap().as_sid());
        let d = sid_string(sid_for_root(r"c:\other", "ws").unwrap().as_sid());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(a.starts_with("S-1-4-"), "{a}");
    }

    #[test]
    fn helper_rejects_unknown_arguments() {
        let err = execute_helper(&["--nope".to_string()]).unwrap_err();
        assert!(err.contains("unexpected argument"), "{err}");
    }
}
