//! M0 probe for the Windows ACL sandbox.
//!
//! This is the throwaway experiment behind the `windows-acl` backend: it builds a
//! write-restricted token, grants a capability SID on a workspace and a private
//! temp directory, and asks the confined child what it can actually do. It
//! answers four questions that decide the backend's shape (see ADR-0027):
//!
//! 1. Does `CreateProcessAsUserW` work for a non-admin caller whose token is a
//!    restricted version of its own?
//! 2. Does a *naked* write-restricted token still delete outside the workspace,
//!    through the parent-directory delete right?
//! 3. Do anonymous pipes still work in the confined child (so `run_shell` can
//!    keep capturing output, including from grandchildren)?
//! 4. Can the label layer be applied by an ordinary user (the label lives in the
//!    SACL, which normally needs `WRITE_OWNER`/`SeSecurityPrivilege`)?
//!
//! `--mode token` grants only the capability-SID ACE; `--mode full` adds the
//! `Everyone` delete-child deny and the Low integrity label. Comparing the two
//! runs shows which layer closes which hole.
#![cfg(windows)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AllocateAndInitializeSid, CONTAINER_INHERIT_ACE, CreateRestrictedToken,
    DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, InitializeAcl, NO_INHERITANCE,
    OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
    SID_AND_ATTRIBUTES, SID_IDENTIFIER_AUTHORITY, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenDefaultDacl, TokenIntegrityLevel, TokenLogonSid, TokenRestrictedSids,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOW,
    WaitForSingleObject,
};

const CREATE_RESTRICTED_TOKEN_FLAGS_LUA: u32 = 0x1 | 0x4 | 0x8;
const CREATE_RESTRICTED_TOKEN_FLAGS_PLAIN: u32 = 0x1 | 0x8;
const SECURITY_NON_UNIQUE_AUTHORITY: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
    Value: [0, 0, 0, 0, 0, 4],
};
const SECURITY_MANDATORY_LOW_RID: &str = "S-1-16-4096";
const SECURITY_WORLD_SID: &str = "S-1-1-0";
const NO_WRITE_UP: u32 = 0x1;
/// `OICI`: inherit into child containers and objects.
const INHERIT_BOTH: u32 = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

/// Set `QCODE_PROBE_TRACE=1` to see how far the parent got before a crash.
macro_rules! trace {
    ($($arg:tt)*) => {
        if std::env::var_os("QCODE_PROBE_TRACE").is_some() {
            eprintln!("probe: {}", format!($($arg)*));
        }
    };
}

/// Runs `qc sandbox-probe`. Returns the process exit code.
pub fn run(args: &ProbeArgs) -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    if args.child {
        return Ok(std::process::ExitCode::from(run_child(args) as u8));
    }
    let report = run_parent(args)?;
    let passed = report.evaluate();
    println!("{}", report.render());
    Ok(std::process::ExitCode::from(if passed { 0 } else { 1 }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Token,
    Full,
}

impl Mode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "token" => Ok(Mode::Token),
            "full" => Ok(Mode::Full),
            other => Err(format!("unknown mode '{other}' (expected token or full)")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Token => "token",
            Mode::Full => "full",
        }
    }
}

/// One child observation. `expect` is what the probe believes the correct
/// behaviour is; the verdict compares it with `result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub check: String,
    pub expect: String,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Check {
    fn matches(&self) -> bool {
        self.expect == "measure" || self.expect == self.result
    }
}

#[derive(Debug)]
pub struct Report {
    mode: Mode,
    /// Token facts reported by the child (integrity level, restricted SID count).
    token: Vec<Check>,
    checks: Vec<Check>,
    spawn: HandleFacts,
}

#[derive(Debug, Default)]
struct HandleFacts {
    restricted_flags: u32,
    restricted_sid_count: u32,
    grant_workspace: String,
    grant_temp: String,
    token_low_label: String,
    default_dacl: String,
    child_exit: Option<u32>,
    notes: Vec<String>,
}

impl Report {
    fn evaluate(&self) -> bool {
        self.token.iter().all(Check::matches) && self.checks.iter().all(Check::matches)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "qcode sandbox probe - windows-acl, mode={}\n",
            self.mode.label()
        ));
        out.push_str(&format!(
            "spawn: restricted token flags=0x{:x} ({} restricting SIDs), child exit={}\n",
            self.spawn.restricted_flags,
            self.spawn.restricted_sid_count,
            self.spawn
                .child_exit
                .map(|code| code.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
        ));
        out.push_str(&format!(
            "grant workspace: {}\n",
            self.spawn.grant_workspace
        ));
        out.push_str(&format!("grant temp:      {}\n", self.spawn.grant_temp));
        out.push_str(&format!(
            "token label:     {}\n",
            self.spawn.token_low_label
        ));
        out.push_str(&format!("token dacl:      {}\n", self.spawn.default_dacl));
        for note in &self.spawn.notes {
            out.push_str(&format!("note: {note}\n"));
        }
        out.push('\n');
        out.push_str(&format!(
            "{:<28} {:<10} {:<10} detail\n",
            "check", "expect", "result"
        ));
        for check in self.token.iter().chain(self.checks.iter()) {
            let mark = if check.matches() { " " } else { "!" };
            out.push_str(&format!(
                "{mark}{:<27} {:<10} {:<10} {}\n",
                check.check,
                check.expect,
                check.result,
                describe(check),
            ));
        }
        out.push('\n');
        let mismatches: Vec<&Check> = self
            .token
            .iter()
            .chain(self.checks.iter())
            .filter(|check| !check.matches())
            .collect();
        if mismatches.is_empty() {
            out.push_str("verdict: PASS (every observation matched its expectation)\n");
        } else {
            out.push_str(&format!(
                "verdict: MISMATCH ({} of {} observations differed)\n",
                mismatches.len(),
                self.token.len() + self.checks.len()
            ));
            for check in mismatches {
                out.push_str(&format!(
                    "  {}: expected {}, got {}\n",
                    check.check, check.expect, check.result
                ));
            }
        }
        out
    }
}

fn describe(check: &Check) -> String {
    match (&check.detail, check.code) {
        (Some(detail), Some(code)) => format!("{detail} (win32 {code})"),
        (Some(detail), None) => detail.clone(),
        (None, Some(code)) => format!("win32 {code}"),
        (None, None) => String::new(),
    }
}

/// Arguments of the hidden `sandbox-probe` subcommand.
#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    /// Run as the confined child (internal).
    #[arg(long, hide = true)]
    pub child: bool,
    /// Workspace the child may write; defaults to `.tmp/sandbox-probe/ws`.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Private temp directory; defaults to `.tmp/sandbox-probe/tmp`.
    #[arg(long)]
    pub temp: Option<PathBuf>,
    /// Directory outside both grants used for the escape attempts.
    #[arg(long)]
    pub escape_dir: Option<PathBuf>,
    /// Directory that `Everyone` can write (internal).
    #[arg(long, hide = true)]
    pub public_dir: Option<PathBuf>,
    /// `token` (capability ACE only) or `full` (ACE + delete deny + Low label).
    #[arg(long, default_value = "full")]
    pub mode: String,
    /// Also print the observations as JSONL.
    #[arg(long)]
    pub json: bool,
}

fn run_parent(args: &ProbeArgs) -> Result<Report, String> {
    let mode = Mode::parse(&args.mode)?;
    let run_id = format!("{}-{}", std::process::id(), mode.label());
    let run_dir = PathBuf::from(".tmp").join("sandbox-probe").join(&run_id);
    let workspace = absolute(args.workspace.clone().unwrap_or_else(|| run_dir.join("ws")))?;
    let temp = absolute(args.temp.clone().unwrap_or_else(|| run_dir.join("tmp")))?;
    let escape = match &args.escape_dir {
        Some(path) => absolute(path.clone())?,
        None => absolute(pick_escape_dir(&run_id)?)?,
    };
    for dir in [&workspace, &temp] {
        std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    }
    std::fs::create_dir_all(&escape).map_err(|err| {
        format!(
            "create escape dir {}: {err}; pass --escape-dir <writable dir outside the two grants>",
            escape.display()
        )
    })?;
    if workspace.starts_with(&escape) || escape.starts_with(&workspace) || temp.starts_with(&escape)
    {
        return Err("probe directories must not contain each other".to_string());
    }

    // Fixtures the child deletes or renames: it cannot create them itself, which
    // is the point - a pre-existing file outside the grants is exactly what the
    // escape tests need.
    let workspace_victim = workspace.join("delete-me.txt");
    let escape_victim = escape.join("delete-me.txt");
    let escape_rename = escape.join("rename-me.txt");
    std::fs::write(&workspace_victim, b"inside").map_err(|err| err.to_string())?;
    std::fs::write(&escape_victim, b"outside").map_err(|err| err.to_string())?;
    std::fs::write(&escape_rename, b"outside").map_err(|err| err.to_string())?;

    // `PUBLIC` normally grants Everyone write access, so a parent-directory
    // FILE_DELETE_CHILD right there is the case that could survive the grants.
    let public = std::env::var_os("PUBLIC")
        .map(PathBuf::from)
        .and_then(|root| {
            let dir = root.join("qcode-sandbox-probe").join(&run_id);
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::write(dir.join("delete-me.txt"), b"public").ok()?;
            Some(dir)
        });

    let workspace_sid = derive_sid(&workspace, "ws")?;
    trace!("derived workspace sid");
    let temp_sid = derive_sid(&temp, "tmp")?;
    trace!("derived temp sid");
    let everyone = string_sid(SECURITY_WORLD_SID)?;
    let low = string_sid(SECURITY_MANDATORY_LOW_RID)?;
    trace!("resolved well-known sids");

    let token = TokenBuilder::open()?;
    let parent_integrity = token
        .integrity_string()
        .unwrap_or_else(|err| format!("unknown ({err})"));
    let caller_restricted = token.restrict_sid_count().unwrap_or(0);
    if caller_restricted > 0 {
        trace!("caller token already has {caller_restricted} restricting SID(s)");
    }
    let logon = token.logon_sid()?;
    trace!("read logon sid: {}", sid_string(logon.as_sid()));
    let mut sid_values = vec![
        logon.as_sid(),
        everyone.as_sid(),
        workspace_sid.as_sid(),
        temp_sid.as_sid(),
    ];
    let (restricted, flags) = match token.restrict(&sid_values) {
        Ok(created) => created,
        Err(err) if caller_restricted > 0 => {
            return Err(format!(
                "{err}; this process already runs under a restricted token ({caller_restricted} restricting SID(s)) - that is the harness sandbox. Run the probe from an unsandboxed shell."
            ));
        }
        Err(err) => return Err(err),
    };
    sid_values.clear();
    trace!("created restricted token flags=0x{flags:x}");

    let mut facts = HandleFacts {
        restricted_flags: flags,
        restricted_sid_count: 4,
        ..HandleFacts::default()
    };
    facts.notes.push(format!(
        "escape dir (outside both grants): {}",
        escape.display()
    ));
    facts
        .notes
        .push(format!("parent token integrity: {parent_integrity}"));
    if parent_integrity.ends_with("-4096") {
        facts.notes.push(
            "parent already runs at Low integrity, so the label layer cannot be isolated here; \
             the denials below come from the capability SIDs"
                .to_string(),
        );
    }
    match &public {
        Some(dir) => facts.notes.push(format!(
            "public escape dir (Everyone-writable): {}",
            dir.display()
        )),
        None => facts
            .notes
            .push("public escape not tested: %PUBLIC% not writable here".to_string()),
    }
    let caller_restricted = token.restrict_sid_count().unwrap_or(0);
    if caller_restricted > 0 {
        facts.notes.push(format!(
            "caller token already carries {caller_restricted} restricting SID(s): this probe inherits an outer confinement, so 'denied' results may be its work rather than the probe's"
        ));
    }

    facts.grant_workspace = grant(&workspace, workspace_sid.as_sid())?;
    trace!("granted workspace: {}", facts.grant_workspace);
    facts.grant_temp = grant(&temp, temp_sid.as_sid())?;
    trace!("granted temp: {}", facts.grant_temp);
    // The Low label sits in the SACL, so writing it needs WRITE_OWNER (normally
    // only the owner has it). A normal user cannot always grant that, so the
    // label is best-effort here: the ACE and the delete deny carry the probe.
    let mut label_applied = false;
    if mode == Mode::Full {
        let mut label_errors = Vec::new();
        for dir in [&workspace, &temp] {
            if let Err(err) = set_low_label(dir, low.as_sid()) {
                label_errors.push(format!("{}: {err}", dir.display()));
            }
        }
        if label_errors.is_empty() {
            label_applied = true;
            trace!("lowered labels");
            facts.notes.push(format!(
                "labels: {} and {} lowered to {SECURITY_MANDATORY_LOW_RID}",
                workspace.display(),
                temp.display()
            ));
        } else {
            facts.notes.push(format!(
                "Low label skipped ({}): the label lives in the SACL and needs a privilege a plain \
                 user does not hold; the capability-ACE layer is what the checks below measure",
                label_errors.join("; ")
            ));
        }
    }

    facts.token_low_label = if mode == Mode::Full && label_applied {
        token_set_integrity(restricted, low.as_sid())?;
        format!("set to {SECURITY_MANDATORY_LOW_RID} on the restricted token")
    } else if mode == Mode::Full {
        "not lowered (label unavailable); child stays Medium".to_string()
    } else {
        "untouched (Medium)".to_string()
    };

    // The confined child's pipe objects come from the token's default DACL, so
    // the temp capability SID must be full-access there. This is part of both
    // modes: it is a prerequisite for the pipe test, not a write grant.
    token_extend_default_dacl(restricted, temp_sid.as_sid())?;
    trace!("extended token default dacl");
    facts.default_dacl = format!(
        "existing default DACL kept; temp capability SID {} appended with file-all-access",
        sid_string(temp_sid.as_sid())
    );

    let exe = std::env::current_exe().map_err(|err| format!("current exe: {err}"))?;
    let log = workspace.join("child-stdout.log");
    let mut child_args = vec![
        "sandbox-probe".to_string(),
        "--child".to_string(),
        "--workspace".to_string(),
        workspace.display().to_string(),
        "--temp".to_string(),
        temp.display().to_string(),
        "--escape-dir".to_string(),
        escape.display().to_string(),
        "--mode".to_string(),
        mode.label().to_string(),
    ];
    if let Some(dir) = &public {
        child_args.push("--public-dir".to_string());
        child_args.push(dir.display().to_string());
    }
    facts.child_exit = Some(spawn_confined(
        &restricted,
        &exe,
        &child_args,
        &log,
        &workspace,
    )?);
    trace!("child exited: {:?}", facts.child_exit);

    let observations = read_observations(&log)?;
    let (mut token_facts, checks): (Vec<Check>, Vec<Check>) = observations
        .into_iter()
        .partition(|check| check.check.starts_with("token_"));
    if mode == Mode::Full && label_applied {
        for check in token_facts.iter_mut() {
            if check.check == "token_integrity" {
                check.expect = SECURITY_MANDATORY_LOW_RID.to_string();
            }
        }
    }

    if args.json {
        for check in token_facts.iter().chain(checks.iter()) {
            if let Ok(line) = serde_json::to_string(check) {
                println!("{line}");
            }
        }
    }
    unsafe { CloseHandle(restricted) };
    Ok(Report {
        mode,
        token: token_facts,
        checks,
        spawn: facts,
    })
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .map_err(|err| format!("cwd: {err}"))
    }
}

/// The escape directory must sit outside both grants and be creatable by the
/// parent. A parent that is itself sandboxed can only write inside its own
/// workspace, so walk the usual user-writable roots and report what failed.
fn pick_escape_dir(run_id: &str) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        candidates.push(
            PathBuf::from(profile)
                .join("qcode-sandbox-probe")
                .join(run_id),
        );
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("qcode-sandbox-probe")
                .join(run_id),
        );
    }
    candidates.push(
        std::env::temp_dir()
            .join("qcode-sandbox-probe")
            .join(run_id),
    );
    // Last resort: inside the harness workspace. That is still a valid escape
    // target: the child's write access is decided by the restricting SIDs, and
    // none of them name this directory, so the probe's own layers decide.
    candidates.push(PathBuf::from(".tmp").join("probe-escape").join(run_id));
    let mut tried = Vec::new();
    for candidate in candidates {
        match std::fs::create_dir_all(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) => tried.push(format!("{}: {err}", candidate.display())),
        }
    }
    Err(format!(
        "no writable escape directory outside the workspace ({}); \
         run the probe from an unsandboxed shell, or pass --escape-dir <dir>",
        tried.join("; ")
    ))
}

fn read_observations(log: &Path) -> Result<Vec<Check>, String> {
    let text =
        std::fs::read_to_string(log).map_err(|err| format!("read {}: {err}", log.display()))?;
    let mut checks = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        match serde_json::from_str::<Check>(line) {
            Ok(check) => checks.push(check),
            Err(err) => {
                checks.push(Check {
                    check: "unparsable_child_line".to_string(),
                    expect: "none".to_string(),
                    result: "error".to_string(),
                    code: None,
                    detail: Some(format!("{err}: {line}")),
                });
            }
        }
    }
    if checks.is_empty() {
        return Err(format!(
            "child produced no observations (see {})",
            log.display()
        ));
    }
    Ok(checks)
}

// ---------------------------------------------------------------------------
// Child side: the confined process runs these checks and reports JSONL.
// ---------------------------------------------------------------------------

fn run_child(args: &ProbeArgs) -> i32 {
    let workspace = args.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    let temp = args.temp.clone().unwrap_or_else(std::env::temp_dir);
    let escape = args
        .escape_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    report(token_facts());
    let mut checks = vec![
        probe_write(
            "write_workspace",
            &workspace.join("child-write.txt"),
            "allowed",
        ),
        probe_write(
            "write_workspace_nested",
            &workspace
                .join("nested")
                .join("deep")
                .join("child-write.txt"),
            "allowed",
        ),
        probe_write("write_temp", &temp.join("child-write.txt"), "allowed"),
        probe_write("write_escape", &escape.join("write-attempt.txt"), "denied"),
        probe_delete(
            "delete_workspace",
            &workspace.join("delete-me.txt"),
            "allowed",
        ),
        probe_delete("delete_escape", &escape.join("delete-me.txt"), "denied"),
        probe_rename(
            "rename_escape",
            &escape.join("rename-me.txt"),
            &escape.join("moved.txt"),
            "denied",
        ),
        probe_pipe_child(),
        probe_inherited_child(),
    ];
    // A directory where `Everyone` has write access is where a parent-directory
    // FILE_DELETE_CHILD grant could still let the child delete a file that the
    // capability grants do not cover.
    if let Some(public) = &args.public_dir {
        checks.push(probe_delete(
            "delete_escape_public",
            &public.join("delete-me.txt"),
            "denied",
        ));
    }
    report(checks);
    0
}

fn report(checks: Vec<Check>) {
    for check in checks {
        if let Ok(line) = serde_json::to_string(&check) {
            println!("{line}");
        }
    }
}

fn token_facts() -> Vec<Check> {
    let integrity = TokenBuilder::open_query()
        .and_then(|token| token.integrity_string())
        .unwrap_or_else(|err| format!("error: {err}"));
    let restricted = TokenBuilder::open_query()
        .and_then(|token| token.restrict_sid_count())
        .unwrap_or(0);
    vec![
        Check {
            check: "token_integrity".to_string(),
            expect: "measure".to_string(),
            result: integrity.clone(),
            code: None,
            detail: Some("token integrity SID".to_string()),
        },
        Check {
            check: "token_restricted_sid_count".to_string(),
            expect: "measure".to_string(),
            result: restricted.to_string(),
            code: None,
            detail: Some("EntryCount of TokenRestrictedSids".to_string()),
        },
    ]
}

fn classify(outcome: &io::Result<()>) -> (String, Option<u32>, Option<String>) {
    match outcome {
        Ok(()) => ("allowed".to_string(), None, None),
        Err(err) => {
            let code = err.raw_os_error().map(|code| code as u32);
            let result = match code {
                Some(5) | Some(19) | Some(1314) | Some(0x8007_0005) => "denied".to_string(),
                _ => "error".to_string(),
            };
            (result, code, Some(err.to_string()))
        }
    }
}

fn probe_write(name: &str, path: &Path, expect: &str) -> Check {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let outcome = std::fs::write(path, b"probe").map(|_| ());
    let (result, code, detail) = classify(&outcome);
    Check {
        check: name.to_string(),
        expect: expect.to_string(),
        result,
        code,
        detail,
    }
}

fn probe_delete(name: &str, path: &Path, expect: &str) -> Check {
    let outcome = std::fs::remove_file(path);
    let (result, code, detail) = classify(&outcome);
    Check {
        check: name.to_string(),
        expect: expect.to_string(),
        result,
        code,
        detail,
    }
}

fn probe_rename(name: &str, from: &Path, to: &Path, expect: &str) -> Check {
    let outcome = std::fs::rename(from, to);
    let (result, code, detail) = classify(&outcome);
    Check {
        check: name.to_string(),
        expect: expect.to_string(),
        result,
        code,
        detail,
    }
}

/// A grandchild with piped stdio: the case libuv gets wrong under a restricted
/// token, and the case `run_shell` depends on.
fn probe_pipe_child() -> Check {
    let spawn = Command::new("cmd")
        .args(["/c", "echo hi"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match spawn {
        Ok(child) => child,
        Err(err) => {
            let (result, code, detail) = classify(&Err(err));
            return Check {
                check: "pipe_grandchild".to_string(),
                expect: "allowed".to_string(),
                result,
                code,
                detail: detail.map(|detail| format!("spawn with piped stdio: {detail}")),
            };
        }
    };
    let outcome = child.wait_with_output();
    let (result, code, detail) = match outcome {
        Ok(output) if output.status.success() => (
            "allowed".to_string(),
            None,
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        ),
        Ok(output) => (
            "error".to_string(),
            output.status.code().map(|code| code as u32),
            Some(format!(
                "read pipes: exit {:?} stderr {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        ),
        Err(err) => {
            let (result, code, detail) = classify(&Err(err));
            (
                result,
                code,
                detail.map(|detail| format!("read pipes: {detail}")),
            )
        }
    };
    Check {
        check: "pipe_grandchild".to_string(),
        expect: "allowed".to_string(),
        result,
        code,
        detail,
    }
}

/// A grandchild with inherited (null) stdio: the documented working case.
fn probe_inherited_child() -> Check {
    let outcome = Command::new("cmd")
        .args(["/c", "exit 0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let (result, code, detail) = match outcome {
        Ok(status) if status.success() => ("allowed".to_string(), None, None),
        Ok(status) => (
            "error".to_string(),
            status.code().map(|code| code as u32),
            Some(format!("exit {:?}", status.code())),
        ),
        Err(err) => classify(&Err(err)),
    };
    Check {
        check: "null_stdio_grandchild".to_string(),
        expect: "allowed".to_string(),
        result,
        code,
        detail,
    }
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
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

/// `"S-1-5-21-..."` to an owned SID; the caller's `LocalFree` is done here.
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

/// Non-unique capability SID `S-1-4-x-y`, stable per (root, domain).
fn derive_sid(path: &Path, domain: &str) -> Result<SidBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    let seed = format!(
        "qcode-sandbox|{domain}|{}",
        canonical.display().to_string().to_lowercase()
    );
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

/// Grants `sid` write access under `path` (inherited to the whole tree).
fn grant(path: &Path, sid: PSID) -> Result<String, String> {
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
            return Err(format!("SetEntriesInAclW({}): win32 {rc}", path.display()));
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
    }
    Ok(format!("allow {} full-access (inherited)", sid_string(sid)))
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn AddMandatoryAce(
        pacl: *mut ACL,
        dwacerevision: u32,
        aceflags: u32,
        mandatoryrights: u32,
        psid: PSID,
    ) -> i32;
}

fn set_low_label(path: &Path, low: PSID) -> Result<(), String> {
    let size = 8 + 8 + 4 + unsafe { GetLengthSid(low) as usize };
    let size = size.div_ceil(4) * 4;
    let mut buffer = vec![0u32; size / 4];
    let acl = buffer.as_mut_ptr() as *mut ACL;
    unsafe {
        if InitializeAcl(acl, size as u32, ACL_REVISION) == 0 {
            return Err(last_error("InitializeAcl"));
        }
        if AddMandatoryAce(acl, ACL_REVISION, INHERIT_BOTH, NO_WRITE_UP, low) == 0 {
            return Err(last_error("AddMandatoryAce"));
        }
        let name = wide(path.as_os_str());
        let rc = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            SACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
        );
        if rc != 0 {
            return Err(format!(
                "SetNamedSecurityInfoW(sacl {}): win32 {rc}",
                path.display()
            ));
        }
    }
    Ok(())
}

struct TokenBuilder {
    handle: HANDLE,
    owned: bool,
}

impl TokenBuilder {
    fn open() -> Result<Self, String> {
        let mut handle: HANDLE = std::ptr::null_mut();
        let access = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) };
        if ok == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        trace!("opened own token");
        Ok(Self {
            handle,
            owned: true,
        })
    }

    /// Read-only handle: a restricted token does not grant the mutation rights
    /// to itself, and the child legitimately runs under such a token.
    fn open_query() -> Result<Self, String> {
        let mut handle: HANDLE = std::ptr::null_mut();
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) };
        if ok == 0 {
            return Err(last_error("OpenProcessToken(TOKEN_QUERY)"));
        }
        Ok(Self {
            handle,
            owned: true,
        })
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
        trace!("logon sid size query: needed={needed}");
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
        let groups = unsafe { &*(buffer.as_ptr() as *const TOKEN_GROUPS) };
        if groups.GroupCount == 0 {
            return Err("TokenLogonSid: empty group list".to_string());
        }
        let sid = SidBuf::from_sid(unsafe { (*groups.Groups.as_ptr()).Sid });
        trace!("logon sid copied");
        Ok(sid)
    }

    fn restrict(&self, sids: &[PSID]) -> Result<(HANDLE, u32), String> {
        let attributes: Vec<SID_AND_ATTRIBUTES> = sids
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: *sid,
                Attributes: 0,
            })
            .collect();
        let mut flags_used = 0;
        for flags in [
            CREATE_RESTRICTED_TOKEN_FLAGS_LUA, // 0xd: max-privilege + LUA + write-restricted
            CREATE_RESTRICTED_TOKEN_FLAGS_PLAIN, // 0x9: write-restricted without LUA
            0x8,                               // write-restricted only
            0x1 | 0x4,                         // max-privilege + LUA, no write restriction
            0,                                 // plain restricted token
        ] {
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
                trace!("CreateRestrictedToken accepted flags 0x{flags:x}");
                return Ok((out, flags));
            }
            let err = io::Error::last_os_error();
            trace!("CreateRestrictedToken flags 0x{flags:x}: {err}");
            flags_used = flags;
            if err.raw_os_error() == Some(1314) {
                continue; // ERROR_PRIVILEGE_NOT_HELD: try without the privileged flag
            }
        }
        Err(format!(
            "CreateRestrictedToken: no flag combination worked (last flags 0x{flags_used:x})"
        ))
    }

    fn restrict_sid_count(&self) -> Result<u32, String> {
        let mut needed: u32 = 0;
        unsafe {
            GetTokenInformation(
                self.handle,
                TokenRestrictedSids,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Ok(0);
        }
        let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
        let ok = unsafe {
            GetTokenInformation(
                self.handle,
                TokenRestrictedSids,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetTokenInformation(TokenRestrictedSids)"));
        }
        Ok(unsafe { (*(buffer.as_ptr() as *const TOKEN_GROUPS)).GroupCount })
    }

    fn integrity_string(&self) -> Result<String, String> {
        let mut needed: u32 = 0;
        unsafe {
            GetTokenInformation(
                self.handle,
                TokenIntegrityLevel,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(last_error("GetTokenInformation(TokenIntegrityLevel, size)"));
        }
        let mut buffer = vec![0u32; (needed as usize).div_ceil(4)];
        let ok = unsafe {
            GetTokenInformation(
                self.handle,
                TokenIntegrityLevel,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error("GetTokenInformation(TokenIntegrityLevel)"));
        }
        let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
        Ok(sid_string(label.Label.Sid))
    }
}

fn token_set_integrity(token: HANDLE, low: PSID) -> Result<(), String> {
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: low,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    let ok = unsafe {
        SetTokenInformation(
            token,
            TokenIntegrityLevel,
            &label as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("SetTokenInformation(TokenIntegrityLevel)"));
    }
    Ok(())
}

/// Appends the temp capability SID to the token's existing default DACL.
///
/// The default DACL is what new objects (anonymous pipes, events) get. It must
/// not be *replaced*: the confined process still has to open those objects with
/// its ambient access, so dropping the inherited entries breaks piped stdio.
fn token_extend_default_dacl(token: HANDLE, temp_sid: PSID) -> Result<(), String> {
    let mut needed: u32 = 0;
    unsafe {
        GetTokenInformation(
            token,
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
            token,
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
    let entries = [explicit_access(
        temp_sid,
        FILE_ALL_ACCESS,
        GRANT_ACCESS,
        NO_INHERITANCE,
    )];
    let mut merged: *mut ACL = std::ptr::null_mut();
    let rc =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), current, &mut merged) };
    if rc != 0 {
        return Err(format!("SetEntriesInAclW(token default dacl): win32 {rc}"));
    }
    let dacl = TOKEN_DEFAULT_DACL {
        DefaultDacl: merged,
    };
    let ok = unsafe {
        SetTokenInformation(
            token,
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

impl Drop for TokenBuilder {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// Replaces the child's stdout/stderr with a file handle, so the probe can read
/// the observations back without depending on pipes (which are what it tests).
fn spawn_confined(
    token: &HANDLE,
    exe: &Path,
    args: &[String],
    log: &Path,
    cwd: &Path,
) -> Result<u32, String> {
    let mut command_line = format!("\"{}\"", exe.display());
    for arg in args {
        command_line.push(' ');
        command_line.push('"');
        command_line.push_str(arg);
        command_line.push('"');
    }
    let mut command_line: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let exe_name = wide(exe.as_os_str());
    let cwd_name = wide(cwd.as_os_str());
    let log_name = wide(log.as_os_str());

    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };

    unsafe {
        let log_handle = CreateFileW(
            log_name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if log_handle.is_null()
            || log_handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
        {
            return Err(last_error("CreateFileW(log)"));
        }
        let nul_name = wide(OsStr::new("NUL"));
        let nul = CreateFileW(
            nul_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );

        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: if nul.is_null() {
                GetStdHandle(STD_INPUT_HANDLE)
            } else {
                nul
            },
            hStdOutput: log_handle,
            hStdError: log_handle,
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
            std::ptr::null(),
            cwd_name.as_ptr(),
            &startup,
            &mut process,
        );
        if ok == 0 {
            let err = last_error("CreateProcessAsUserW");
            CloseHandle(log_handle);
            if !nul.is_null() {
                CloseHandle(nul);
            }
            return Err(err);
        }

        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        AssignProcessToJobObject(job, process.hProcess);
        ResumeThread(process.hThread);

        WaitForSingleObject(process.hProcess, INFINITE);
        let mut code: u32 = 0;
        GetExitCodeProcess(process.hProcess, &mut code);

        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
        CloseHandle(log_handle);
        if !nul.is_null() {
            CloseHandle(nul);
        }
        CloseHandle(job);
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, expect: &str, result: &str) -> Check {
        Check {
            check: name.to_string(),
            expect: expect.to_string(),
            result: result.to_string(),
            code: None,
            detail: None,
        }
    }

    #[test]
    fn measure_expectations_never_fail_the_verdict() {
        assert!(check("delete_escape", "measure", "allowed").matches());
        assert!(check("delete_escape", "measure", "denied").matches());
        assert!(!check("delete_escape", "denied", "allowed").matches());
    }

    #[test]
    fn observations_skip_non_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(
            &log,
            "noise\n{\"check\":\"write_escape\",\"expect\":\"denied\",\"result\":\"denied\",\"code\":5}\n",
        )
        .unwrap();
        let checks = read_observations(&log).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].result, "denied");
        assert_eq!(checks[0].code, Some(5));
        assert!(checks[0].matches());
    }

    #[test]
    fn empty_logs_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(&log, "").unwrap();
        assert!(read_observations(&log).is_err());
    }

    #[test]
    fn capability_sids_are_stable_per_root_and_domain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = sid_string(derive_sid(root, "ws").unwrap().as_sid());
        let b = sid_string(derive_sid(root, "ws").unwrap().as_sid());
        let c = sid_string(derive_sid(root, "tmp").unwrap().as_sid());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("S-1-4-"), "{a}");
    }

    #[test]
    fn mode_parsing_is_strict() {
        assert_eq!(Mode::parse("token").unwrap(), Mode::Token);
        assert_eq!(Mode::parse("full").unwrap(), Mode::Full);
        assert!(Mode::parse("yes").is_err());
    }
}
