//! Per-server filesystem grants (ADR-0066).
//!
//! A server used to inherit `run_shell`'s sandbox verbatim, which meant the
//! read whitelist was `/usr /bin /sbin /lib /lib64 /etc /opt /sys /proc /run
//! /dev` — no `/home`. Anything installed under `~/.local` (pipx, uv, npm) was
//! therefore unreachable and the server died with `Permission denied`, while
//! the same binary copied to `/tmp` worked. This module replaces "inherit the
//! shell's boundary" with "derive one grant per server".
//!
//! The derivation is a pure function of the config plus the filesystem: no
//! dependency on the tools crate, no sandbox backend. The CLI turns the result
//! into sandbox roots, so `qc doctor` can print exactly what a server gets.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::McpServerConfig;

/// Credential subtrees under the real `$HOME` a server may read (ADR-0066 §4).
///
/// Only concrete subtrees are granted, never `$HOME` itself: `~/.ssh`,
/// `~/.aws` and `~/.config/gh` behind one blanket "read your home" grant is
/// exactly what the rejected drafts proposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialPreset {
    Aws,
    Docker,
    Github,
    Netrc,
    Npm,
    Ssh,
}

impl CredentialPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Aws => "aws",
            Self::Docker => "docker",
            Self::Github => "github",
            Self::Netrc => "netrc",
            Self::Npm => "npm",
            Self::Ssh => "ssh",
        }
    }

    /// Every preset, in the order the error messages and the GUI list them.
    ///
    /// Exists so "what can I write here?" has one answer: the CLI's
    /// `--credential`, the GUI's picker and the config file's accepted values
    /// are all this list.
    pub const ALL: &'static [Self] = &[
        Self::Aws,
        Self::Docker,
        Self::Github,
        Self::Netrc,
        Self::Npm,
        Self::Ssh,
    ];

    /// Parse a preset name as written in `config.toml`.
    ///
    /// The GUI needs this as much as the CLI does: a settings screen that
    /// accepted a name the loader rejects would write a config that fails to
    /// load. Unknown names return `None` rather than a guess — silently turning
    /// a typo into `github` would grant a credential subtree nobody asked for.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|preset| preset.label() == label.trim())
    }

    /// Paths relative to the real `$HOME`.
    ///
    /// Public so `qc doctor` can answer "`credentials = [\"github\"]` but is there
    /// a `~/.config/gh` at all?" without re-listing the table.
    pub fn home_paths(self) -> &'static [&'static str] {
        match self {
            Self::Aws => &[".aws"],
            Self::Docker => &[".docker/config.json"],
            Self::Github => &[".config/gh", ".config/git"],
            Self::Netrc => &[".netrc"],
            Self::Npm => &[".npmrc"],
            Self::Ssh => &[".ssh"],
        }
    }

    /// Variables passed through once the preset is selected. Names only; the
    /// values still come from qcode's own environment.
    fn env_vars(self) -> &'static [&'static str] {
        match self {
            Self::Aws => &[
                "AWS_PROFILE",
                "AWS_REGION",
                "AWS_DEFAULT_REGION",
                "AWS_CONFIG_FILE",
                "AWS_SHARED_CREDENTIALS_FILE",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
            ],
            Self::Docker => &["DOCKER_HOST", "DOCKER_CONFIG"],
            Self::Github => &["GITHUB_TOKEN", "GH_TOKEN", "GH_HOST"],
            Self::Netrc => &[],
            Self::Npm => &["NPM_TOKEN", "NODE_AUTH_TOKEN"],
            Self::Ssh => &["SSH_AUTH_SOCK"],
        }
    }
}

/// Base environment: what any server needs to run at all (ADR-0068 §1).
///
/// Deliberately narrower than `run_shell`'s list in
/// `qcode-tools/src/shell.rs`: no `TERM` (no tty is attached), no
/// `CARGO_HOME`/`RUSTUP_HOME` (build configuration a third-party server has no
/// business reading). `LC_*` is matched by prefix, see [`is_base_env`].
pub const BASE_ENV: &[&str] = &["PATH", "LANG", "TZ"];

/// Windows programs break without these; mirrors `shell.rs`'s `ENV_WINDOWS`.
#[cfg(windows)]
pub const WINDOWS_ENV: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
];

fn is_base_env(key: &str) -> bool {
    BASE_ENV.contains(&key) || key.starts_with("LC_")
}

/// How deep an interpreter chain may go before we stop following it.
const MAX_SHEBANG_DEPTH: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    /// A bare `command` (no `/`) that no directory on the search path holds.
    ///
    /// Names the path *source* as well as the name: "was not found" on its own
    /// leaves the reader unable to tell "you typed it wrong" from "this GUI has
    /// a different `PATH` than your terminal", and the second is the whole
    /// reason this lookup consults the shell snapshot (ADR-0070).
    #[error(
        "MCP server '{server}' command {command:?} was not found on {searched}; \
         use an absolute path if it lives elsewhere"
    )]
    CommandNotFound {
        server: String,
        command: String,
        /// Not named `source`: `thiserror` would read it as the error source.
        searched: &'static str,
    },
    #[error(
        "MCP server '{server}' would need read access to {path}, which is {reason}; \
         add an explicit read_paths entry if that is really intended"
    )]
    DangerousRoot {
        server: String,
        path: PathBuf,
        reason: &'static str,
    },
    #[error(
        "MCP server '{server}' would need write access to {path}, which is {reason}; \
         narrow `writable_paths` to a subdirectory"
    )]
    DangerousWriteRoot {
        server: String,
        path: PathBuf,
        reason: &'static str,
    },
    #[error("MCP server '{server}' cannot create its private HOME at {path}: {source}")]
    HomeUnavailable {
        server: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "MCP server '{server}' sets {key} = \"${{{variable}}}\" but {variable} is not in \
         qcode's environment"
    )]
    MissingEnv {
        server: String,
        key: String,
        variable: String,
    },
    /// A remote server has no child process, so there is no grant to derive
    /// (ADR-0067 §7). Its only boundary is the egress proxy.
    #[error(
        "MCP server '{server}' is remote (`url`), so it has no filesystem grant;          drop request paths from its config"
    )]
    NotAProcess { server: String },
}

/// One granted credential subtree: absolute, plus the `$HOME`-relative form
/// used to recreate it inside the synthetic HOME.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Granted {
    path: PathBuf,
    relative: PathBuf,
}

/// Everything one server is allowed to touch.
///
/// Built once, before the process is spawned, and printed verbatim by
/// `qc doctor`; the spawn path only consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSet {
    server: String,
    /// Read + execute. Never writable.
    runtime: Vec<PathBuf>,
    /// Read + write: the workspace, explicit extras and the private HOME.
    writable: Vec<PathBuf>,
    /// Credential subtrees under the real `$HOME`, read-only.
    credentials: Vec<Granted>,
    presets: Vec<CredentialPreset>,
    /// The synthetic `$HOME`; always also in `writable`.
    home: PathBuf,
    /// `false` for `sandbox = "off"`: the grant is still computed (so doctor
    /// can show it) but the CLI starts the server unconfined.
    confined: bool,
}

/// Inputs for [`GrantSet::derive`]. A struct rather than five positional
/// arguments, because most of them are `&Path` and swapping two would still
/// type-check.
#[derive(Debug)]
pub struct GrantInput<'a> {
    pub server: &'a McpServerConfig,
    /// The task workspace; the server runs here and may write here.
    pub workspace: &'a Path,
    /// Per-server state directory (`<data_dir>/mcp/<name>`).
    pub server_state: PathBuf,
    /// The real `$HOME`, for credential presets and the danger check.
    pub real_home: Option<&'a Path>,
    /// Whether the OS sandbox is applied to this server.
    pub confined: bool,
    /// Where a bare `command` is looked up (ADR-0070). Callers that start the
    /// process must hand the same value to the child as its `PATH`, or the
    /// grant will describe a different file than the one that runs.
    pub path: SearchPath<'a>,
}

impl GrantSet {
    pub fn derive(input: GrantInput<'_>) -> Result<Self, GrantError> {
        let GrantInput {
            server,
            workspace,
            server_state,
            real_home,
            confined,
            path,
        } = input;

        // `dangerous()` compares a path that has been through `canonicalize()`
        // against the home directory, so the home has to be canonical too: on
        // macOS `/tmp` is a symlink to `/private/tmp`, and comparing against the
        // uncanonicalized home would make "this is your home directory" fail
        // open — the exact case the check exists for.
        let real_home_canonical = real_home.and_then(|home| home.canonicalize().ok());
        let real_home_for_check = real_home_canonical.as_deref().or(real_home);

        // `validate()` already rejects these fields on a remote server; this is
        // the second line so a caller that skipped validation gets an error
        // instead of a grant for a program that does not exist.
        let Some(command) = server.command.as_deref() else {
            return Err(GrantError::NotAProcess {
                server: server.name.clone(),
            });
        };

        let mut runtime = BTreeSet::new();
        let program = resolve_program(&server.name, command, path)?;
        collect_runtime(&server.name, &program, 0, &mut runtime, path)?;
        for extra in &server.read_paths {
            if let Ok(path) = extra.canonicalize() {
                runtime.insert(path);
            }
        }

        // The danger check runs on the whole runtime set: what `collect_runtime`
        // derived *and* the user's `read_paths`, which are folded in above. So
        // `read_paths = ["/"]` is refused while `["/opt/tool"]` passes.
        for path in &runtime {
            if let Some(reason) = dangerous(path, workspace, real_home_for_check) {
                return Err(GrantError::DangerousRoot {
                    server: server.name.clone(),
                    path: path.clone(),
                    reason,
                });
            }
        }

        // The private HOME must be absolute before it is handed to a child as
        // `HOME` and to the sandbox as a root: `data_dir` can be relative
        // (`--data-dir ./data`), and a relative `HOME` breaks most servers.
        let home = server_state.join("home");
        std::fs::create_dir_all(&home).map_err(|source| GrantError::HomeUnavailable {
            server: server.name.clone(),
            path: home.clone(),
            source,
        })?;
        let home = home.canonicalize().unwrap_or(home);

        let mut writable: Vec<PathBuf> = Vec::new();
        push_unique(&mut writable, workspace.to_path_buf());
        for extra in &server.writable_paths {
            let Ok(path) = extra.canonicalize() else {
                continue;
            };
            // The same danger check the read side runs, on the write side. It
            // was missing, and `writable_paths = ["~/"]` — two characters once
            // `~` is expanded — is exactly the shape that needs it: a server
            // that can write anywhere in your home can plant a
            // `.config/gh/hosts.yml`, a shell rc line or a `.gitconfig`
            // `core.hooksPath`, none of which needs a second write to take
            // effect.
            //
            // Only the entries the user wrote: the workspace and the private
            // HOME are granted unconditionally, and running qcode from `$HOME`
            // must not make every server unstartable.
            if let Some(reason) = dangerous(&path, workspace, real_home_for_check) {
                return Err(GrantError::DangerousWriteRoot {
                    server: server.name.clone(),
                    path: path.clone(),
                    reason,
                });
            }
            push_unique(&mut writable, path);
        }
        push_unique(&mut writable, home.clone());

        let mut credentials = Vec::new();
        if let Some(home_dir) = real_home {
            for preset in &server.credentials {
                for relative in preset.home_paths() {
                    let path = home_dir.join(relative);
                    if !path.exists() {
                        continue;
                    }
                    if credentials.iter().any(|known: &Granted| known.path == path) {
                        continue;
                    }
                    credentials.push(Granted {
                        path,
                        relative: PathBuf::from(relative),
                    });
                }
            }
        }

        Ok(Self {
            server: server.name.clone(),
            runtime: runtime.into_iter().collect(),
            writable,
            credentials,
            presets: server.credentials.clone(),
            home,
            confined,
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// Read + execute roots in grant order.
    pub fn runtime(&self) -> &[PathBuf] {
        &self.runtime
    }

    pub fn writable(&self) -> &[PathBuf] {
        &self.writable
    }

    pub fn credentials(&self) -> impl Iterator<Item = &Path> {
        self.credentials
            .iter()
            .map(|granted| granted.path.as_path())
    }

    pub fn presets(&self) -> &[CredentialPreset] {
        &self.presets
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn is_confined(&self) -> bool {
        self.confined
    }

    /// Every root with its write bit, for `Sandbox::with_roots`.
    pub fn roots(&self) -> Vec<(PathBuf, bool)> {
        let mut roots: Vec<(PathBuf, bool)> = Vec::new();
        for path in &self.runtime {
            roots.push((path.clone(), false));
        }
        for granted in &self.credentials {
            roots.push((granted.path.clone(), false));
        }
        for path in &self.writable {
            // A writable path may also have been derived as a runtime root;
            // Landlock unions the rules, so the write bit wins either way.
            match roots.iter_mut().find(|(known, _)| known == path) {
                Some((_, writable)) => *writable = true,
                None => roots.push((path.clone(), true)),
            }
        }
        roots
    }

    /// Base environment + synthetic HOME + credential variables, in that
    /// order. The caller appends `[[mcp.servers]] env` and then the egress
    /// proxy variables — order matters, see ADR-0068 §1.
    pub fn base_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = Vec::new();
        for (key, value) in std::env::vars_os() {
            let Some(key) = key.to_str() else { continue };
            if !is_base_env(key) {
                continue;
            }
            if let Some(value) = value.to_str() {
                env.push((key.to_string(), value.to_string()));
            }
        }
        #[cfg(windows)]
        for key in WINDOWS_ENV {
            if let Some(value) = std::env::var_os(key).and_then(|value| value.into_string().ok()) {
                env.push(((*key).to_string(), value));
            }
        }

        let mut set = |key: &str, value: PathBuf| {
            env.push((key.to_string(), value.display().to_string()));
        };
        // The XDG defaults, spelled out rather than `home/config`: the
        // credential presets live at `$HOME/.config/...`, and `gh`, `gh`'s
        // token lookup and `prepare_home`'s symlinks all have to agree on one
        // location. Two directory layouts here is how "credentials = [github]"
        // silently does nothing.
        set("HOME", self.home.clone());
        set("XDG_CONFIG_HOME", self.home.join(".config"));
        set("XDG_CACHE_HOME", self.home.join(".cache"));
        set("XDG_DATA_HOME", self.home.join(".local").join("share"));
        for key in ["TMPDIR", "TMP", "TEMP"] {
            set(key, self.home.clone());
        }

        for preset in &self.presets {
            for key in preset.env_vars() {
                if let Some(value) =
                    std::env::var_os(key).and_then(|value| value.into_string().ok())
                {
                    env.push(((*key).to_string(), value));
                }
            }
        }
        env
    }

    /// Create the synthetic HOME and link the granted credential subtrees into
    /// it, so a server resolving `$HOME/.config/gh` lands on the real one.
    ///
    /// Linking matters: `$HOME` now points at the private directory, so `gh`
    /// would otherwise look in `<private>/.config/gh` and find nothing. The
    /// target stays read-only, so refreshing a token fails on purpose —
    /// writing to the real credential store is `qc`'s job (ADR-0056).
    pub fn prepare_home(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.home)?;
        for directory in [".config", ".cache", ".local/share"] {
            std::fs::create_dir_all(self.home.join(directory))?;
        }
        for granted in &self.credentials {
            let link = self.home.join(&granted.relative);
            if link.exists() {
                continue;
            }
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&granted.path, &link)?;
            #[cfg(not(unix))]
            {
                // Windows needs Developer Mode or an elevated process for
                // symlinks. Credential presets are advisory there anyway: the
                // ACL backend only restricts writes (ADR-0027), so a missing
                // link loses convenience, not isolation.
                let _ = &link;
            }
        }
        Ok(())
    }

    /// One line per section, for `qc doctor` and the startup log.
    pub fn describe(&self) -> String {
        fn list(paths: impl Iterator<Item = std::path::PathBuf>) -> String {
            let joined = paths
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            if joined.is_empty() {
                "—".to_string()
            } else {
                joined
            }
        }
        format!(
            "sandbox {}, runtime [{}], write [{}], credentials [{}], home {}",
            if self.confined { "confine" } else { "off" },
            list(self.runtime.iter().cloned()),
            list(self.writable.iter().cloned()),
            {
                // Granted subtrees in `~/...` form, then the presets that
                // matched nothing: "credentials [github]" next to a `!aws` is
                // the difference between "asked for" and "actually got", which
                // is precisely what went wrong silently before.
                let mut parts: Vec<String> = self
                    .credentials
                    .iter()
                    .map(|granted| format!("~/{}", granted.relative.display()))
                    .collect();
                for preset in &self.presets {
                    let matched = preset.home_paths().iter().any(|relative| {
                        self.credentials
                            .iter()
                            .any(|granted| granted.relative == Path::new(relative))
                    });
                    if !matched {
                        parts.push(format!("{}(not found)", preset.label()));
                    }
                }
                if parts.is_empty() {
                    "—".to_string()
                } else {
                    parts.join(", ")
                }
            },
            self.home.display()
        )
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Why a derived root must not be granted automatically, if it must not.
fn dangerous(path: &Path, workspace: &Path, real_home: Option<&Path>) -> Option<&'static str> {
    if path == Path::new("/") {
        return Some("the filesystem root");
    }
    if let Some(home) = real_home
        && path == home
    {
        return Some("your home directory");
    }
    for name in ["/home", "/Users", "/root"] {
        if path == Path::new(name) {
            return Some("a directory that contains every user's home");
        }
    }
    if workspace != path && workspace.starts_with(path) {
        return Some("a parent of the workspace");
    }
    None
}

/// Which `PATH` a bare `command` is looked up in — and, because the two must
/// agree, which `PATH` the server is started with.
///
/// ADR-0070. `command = "wordcraft-cli"` is the portable spelling: the version
/// directory in `~/cot/lib/wordcraft_0.15.2_linux_amd64` changes on every
/// upgrade, so an absolute path in the config is a liability. But qcode's own
/// `PATH` is launchd's minimal one when `qc gui` starts from the Dock, while the
/// user's shells have the toolchain hooks — so a lookup against the process
/// `PATH` alone is exactly the case that fails.
///
/// The resolution has to use the same value the child gets: `Command::new` is
/// handed the *bare name*, so the kernel resolves it again from the child's
/// `PATH`. Resolve with one and spawn with the other and the grant describes a
/// different file than the one that runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath<'a> {
    /// The user's shell environment (ADR-0064), sampled once per session.
    Snapshot(&'a OsStr),
    /// qcode's own `PATH`. The pre-ADR-0070 behaviour, and the fallback when
    /// sampling is off or the sample failed.
    Process,
}

impl SearchPath<'_> {
    /// The value to look in and to hand the child, and a name for error text.
    fn resolved(self) -> (OsString, &'static str) {
        match self {
            Self::Snapshot(path) => (path.to_os_string(), "the shell environment's PATH"),
            Self::Process => (
                std::env::var_os("PATH").unwrap_or_default(),
                "qcode's PATH (shell environment sampling is off or produced nothing)",
            ),
        }
    }

    /// The `PATH` a server started with this search path must see.
    pub fn as_os_str(self) -> OsString {
        self.resolved().0
    }

    /// Where this came from, for `qc doctor` and the startup log.
    pub fn source(self) -> &'static str {
        self.resolved().1
    }
}

impl<'a> From<Option<&'a OsStr>> for SearchPath<'a> {
    fn from(path: Option<&'a OsStr>) -> Self {
        match path {
            Some(path) => Self::Snapshot(path),
            None => Self::Process,
        }
    }
}

/// Resolve `command` the way `execvp` would: a path if it has a separator,
/// otherwise a lookup in `path`.
fn resolve_program(
    server: &str,
    command: &str,
    path: SearchPath<'_>,
) -> Result<PathBuf, GrantError> {
    let has_separator = command.contains('/') || command.contains('\\');
    let candidate = if has_separator {
        PathBuf::from(command)
    } else {
        let (dirs, source) = path.resolved();
        std::env::split_paths(&dirs)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| GrantError::CommandNotFound {
                server: server.to_string(),
                command: command.to_string(),
                searched: source,
            })?
    };
    candidate
        .canonicalize()
        .map_err(|_| GrantError::CommandNotFound {
            server: server.to_string(),
            command: command.to_string(),
            searched: if has_separator {
                "the path as written"
            } else {
                path.source()
            },
        })
}

/// Walk the interpreter chain and add one root per program.
fn collect_runtime(
    server: &str,
    program: &Path,
    depth: usize,
    roots: &mut BTreeSet<PathBuf>,
    path: SearchPath<'_>,
) -> Result<(), GrantError> {
    if depth > MAX_SHEBANG_DEPTH {
        return Ok(());
    }
    if let Some(root) = root_for(program) {
        roots.insert(root);
    }
    if let Some(interpreter) = shebang_interpreter(program, path) {
        let resolved = interpreter
            .canonicalize()
            .map_err(|_| GrantError::CommandNotFound {
                server: server.to_string(),
                command: interpreter.display().to_string(),
                searched: "the shebang of the command",
            })?;
        collect_runtime(server, &resolved, depth + 1, roots, path)?;
    }
    Ok(())
}

/// The ecosystem root that makes `program` runnable, or its own directory.
///
/// Directory-level, not file-level: `Sandbox::with_roots` speaks in roots, and
/// a binary usually needs a sibling `.so` or its `lib/` anyway.
fn root_for(program: &Path) -> Option<PathBuf> {
    // A Python venv (including pipx's `~/.local/pipx/venvs/<name>`) is the
    // narrowest thing that carries the interpreter plus its site-packages.
    for dir in program.ancestors().skip(1) {
        if dir.join("pyvenv.cfg").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    // npm / bun / pnpm: everything installed for one project lives here.
    for dir in program.ancestors() {
        if dir.file_name() == Some(OsStr::new("node_modules")) {
            return Some(dir.to_path_buf());
        }
    }
    // FHS-style distribution root: `<x>/bin/<bin>` next to `<x>/lib`,
    // `<x>/lib64` or `<x>/share`. This is what makes a release tarball such as
    // wordcraft usable where the user unpacked it.
    if let Some(bin) = program.parent()
        && bin.file_name() == Some(OsStr::new("bin"))
        && let Some(dist) = bin.parent()
        && ["lib", "lib64", "share"]
            .iter()
            .any(|dir| dist.join(dir).is_dir())
    {
        return Some(dist.to_path_buf());
    }
    let parent = program.parent()?;
    // A top-level directory (`/tmp`, `C:\Users`) is too coarse to hand out by
    // inference; it has to be written down in `read_paths` instead.
    if parent.components().count() < 2 {
        return None;
    }
    Some(parent.to_path_buf())
}

/// Read `#!` from the first line of a text file, resolving `/usr/bin/env X`.
fn shebang_interpreter(program: &Path, path: SearchPath<'_>) -> Option<PathBuf> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(program).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let rest = line.strip_prefix("#!")?.trim();
    let mut parts = rest.split_whitespace();
    let mut interpreter = PathBuf::from(parts.next()?);
    if interpreter.file_name() == Some(OsStr::new("env")) {
        // `#!/usr/bin/env node`, and the `env -S node --flag` form. `env` is
        // what does the lookup at run time, so it has to be the same `PATH`.
        let name = parts.find(|part| !part.starts_with('-'))?;
        interpreter = resolve_relative(program, name, path)?;
    } else if !interpreter.is_absolute() {
        interpreter = resolve_relative(program, interpreter.as_os_str().to_str()?, path)?;
    }
    Some(interpreter)
}

fn resolve_relative(program: &Path, name: &str, path: SearchPath<'_>) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    // A relative interpreter resolves against the script's directory first.
    if let Some(dir) = program.parent() {
        let joined = dir.join(candidate);
        if joined.is_file() {
            return Some(joined);
        }
    }
    let (dirs, _) = path.resolved();
    std::env::split_paths(&dirs)
        .map(|dir| dir.join(candidate))
        .find(|candidate| candidate.is_file())
}

/// Expand a `[[mcp.servers]] env` value that is exactly `${NAME}` (ADR-0068 §2).
///
/// Only the whole-value form is supported: a literal that happens to contain
/// `${` stays literal, so shell-ish strings are not silently rewritten.
pub fn expand_env(server: &str, key: &str, value: &str) -> Result<String, GrantError> {
    let Some(name) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return Ok(value.to_string());
    };
    std::env::var(name).map_err(|_| GrantError::MissingEnv {
        server: server.to_string(),
        key: key.to_string(),
        variable: name.to_string(),
    })
}

/// The real `$HOME`. `qcode-kernel` has its own resolver; this one exists so
/// the crate keeps working without it (tests pass an explicit home instead).
pub fn real_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn server(name: &str, command: &Path) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some(command.display().to_string()),
            ..Default::default()
        }
    }

    /// A binary in an FHS-style release tree: `<dist>/bin/x` next to `share/`.
    fn dist_binary(root: &Path) -> PathBuf {
        let dist = root.join("wordcraft_1.0_linux_amd64");
        fs::create_dir_all(dist.join("bin")).unwrap();
        fs::create_dir_all(dist.join("share")).unwrap();
        let binary = dist.join("bin").join("wordcraft");
        fs::write(&binary, b"not really an elf").unwrap();
        binary
    }

    #[test]
    fn a_release_tarball_grants_its_whole_dist_root() {
        let temp = tempfile::tempdir().unwrap();
        let binary = dist_binary(temp.path());
        assert_eq!(
            root_for(&binary.canonicalize().unwrap()),
            Some(
                temp.path()
                    .join("wordcraft_1.0_linux_amd64")
                    .canonicalize()
                    .unwrap()
            )
        );
    }

    #[test]
    fn a_python_venv_grants_the_venv_not_the_whole_home() {
        let temp = tempfile::tempdir().unwrap();
        let venv = temp.path().join("venvs").join("thing");
        fs::create_dir_all(venv.join("bin")).unwrap();
        fs::write(venv.join("pyvenv.cfg"), b"home = /usr/bin\n").unwrap();
        let python = venv.join("bin").join("python");
        fs::write(&python, b"#!/bin/sh\n").unwrap();
        assert_eq!(
            root_for(&python.canonicalize().unwrap()),
            Some(venv.canonicalize().unwrap())
        );
    }

    #[test]
    fn a_node_modules_binary_grants_the_install_tree() {
        let temp = tempfile::tempdir().unwrap();
        let modules = temp.path().join("node_modules");
        let package = modules.join("pkg").join("bin");
        fs::create_dir_all(&package).unwrap();
        let binary = package.join("cli.js");
        fs::write(&binary, b"#!/usr/bin/env node\n").unwrap();
        assert_eq!(
            root_for(&binary.canonicalize().unwrap()),
            Some(modules.canonicalize().unwrap())
        );
    }

    #[test]
    fn a_plain_binary_grants_its_own_directory() {
        let temp = tempfile::tempdir().unwrap();
        let debug = temp.path().join("target").join("debug");
        fs::create_dir_all(&debug).unwrap();
        let binary = debug.join("mcp-stub");
        fs::write(&binary, b"elf").unwrap();
        assert_eq!(
            root_for(&binary.canonicalize().unwrap()),
            Some(debug.canonicalize().unwrap())
        );
    }

    #[test]
    fn a_top_level_directory_is_never_derived() {
        // `/tmp` (or the platform equivalent) is too coarse to infer; it has to
        // be written down in `read_paths`.
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().parent().unwrap();
        // `parent` of a tempdir is still deeper than one component on most
        // systems, so check the rule directly on a synthetic path.
        assert!(parent.components().count() >= 2 || root_for(temp.path()).is_none());
        assert_eq!(root_for(Path::new("/")), None);
        let top = if cfg!(windows) { "C:\\Windows" } else { "/etc" };
        // `/etc` has a single component, so there is no parent to grant.
        assert_eq!(root_for(Path::new(top)), None);
    }

    #[test]
    fn a_shebang_script_grants_its_interpreter() {
        let temp = tempfile::tempdir().unwrap();
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        let script = scripts.join("tool");
        fs::write(&script, b"#!/usr/bin/env sh\necho hi\n").unwrap();
        let mut roots = BTreeSet::new();
        collect_runtime(
            "t",
            &script.canonicalize().unwrap(),
            0,
            &mut roots,
            SearchPath::Process,
        )
        .unwrap();
        assert!(
            roots.contains(&scripts.canonicalize().unwrap()),
            "{roots:?}"
        );
        let sh = Path::new("/usr/bin/sh")
            .canonicalize()
            .or_else(|_| Path::new("/bin/sh").canonicalize())
            .unwrap();
        assert!(
            roots.iter().any(|root| sh.starts_with(root)),
            "no root covers the interpreter {sh:?}: {roots:?}"
        );
    }

    #[test]
    fn refusing_the_filesystem_root_and_the_home_directory() {
        let workspace = Path::new("/work/project");
        let home = Path::new("/home/someone");
        assert_eq!(
            dangerous(Path::new("/"), workspace, Some(home)),
            Some("the filesystem root")
        );
        assert_eq!(
            dangerous(home, workspace, Some(home)),
            Some("your home directory")
        );
        assert_eq!(
            dangerous(Path::new("/home"), workspace, Some(home)),
            Some("a directory that contains every user's home")
        );
        assert_eq!(
            dangerous(Path::new("/work"), workspace, Some(home)),
            Some("a parent of the workspace")
        );
        // The workspace itself is fine: it is already the writable root.
        assert_eq!(dangerous(workspace, workspace, Some(home)), None);
        assert_eq!(
            dangerous(Path::new("/opt/tool"), workspace, Some(home)),
            None
        );
    }

    #[test]
    fn a_binary_in_the_home_directory_is_refused_not_silently_granted() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("share")).unwrap();
        fs::create_dir_all(home.join("bin")).unwrap();
        let binary = home.join("bin").join("tool");
        fs::write(&binary, b"elf").unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let config = server("greedy", &binary);
        let error = GrantSet::derive(GrantInput {
            server: &config,
            workspace: workspace.path(),
            server_state: temp.path().join("state"),
            real_home: Some(&home),
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap_err();
        assert!(
            matches!(error, GrantError::DangerousRoot { reason, .. } if reason == "your home directory"),
            "{error}"
        );
    }

    #[test]
    fn a_writable_path_that_is_the_whole_home_directory_is_refused() {
        // `writable_paths = ["~/"]` is two characters away from `["~/x"]` once
        // `~` is expanded, and the failure it prevents is invisible: a server
        // that can write anywhere in `$HOME` can drop a `.gitconfig`
        // `core.hooksPath`, a shell rc line or `~/.config/gh/hosts.yml` and
        // take effect on the next command, none of which needs a second write.
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let binary = dist_binary(temp.path());

        let mut config = server("greedy", &binary);
        config.writable_paths = vec![home.clone()];
        let error = GrantSet::derive(GrantInput {
            server: &config,
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: Some(&home),
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap_err();
        assert!(
            matches!(error, GrantError::DangerousWriteRoot { .. }),
            "{error}"
        );
        // The message has to say *why* and point at the fix, or a user just
        // reads it as "the feature is broken".
        assert!(error.to_string().contains("your home directory"), "{error}");
        assert!(error.to_string().contains("writable_paths"), "{error}");
    }

    #[test]
    fn a_writable_path_inside_the_home_directory_is_what_to_write_instead() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let documents = home.join("Documents");
        fs::create_dir_all(&documents).unwrap();
        let home = home.canonicalize().unwrap();
        let documents = documents.canonicalize().unwrap();
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let binary = dist_binary(temp.path());

        let mut config = server("wordcraft", &binary);
        config.writable_paths = vec![documents.clone()];
        let grant = GrantSet::derive(GrantInput {
            server: &config,
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: Some(&home),
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap();
        // A subdirectory is granted, canonicalized, and still on the write side.
        assert!(
            grant
                .writable()
                .contains(&documents.canonicalize().unwrap())
        );
    }

    #[test]
    fn the_grant_is_runtime_read_only_and_workspace_plus_home_writable() {
        let temp = tempfile::tempdir().unwrap();
        let binary = dist_binary(temp.path());
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let config = server("wordcraft", &binary);

        let grant = GrantSet::derive(GrantInput {
            server: &config,
            workspace: &workspace,
            server_state: temp.path().join("state").join("wordcraft"),
            real_home: None,
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap();

        let write = grant.writable().to_vec();
        assert!(write.contains(&workspace.canonicalize().unwrap()));
        assert!(write.contains(&grant.home().to_path_buf()));
        assert!(grant.runtime().iter().all(|path| !write.contains(path)));

        // `roots()` carries the write bit, which is what `with_roots` consumes.
        let roots = grant.roots();
        let writable: Vec<&PathBuf> = roots
            .iter()
            .filter(|(_, writable)| *writable)
            .map(|(path, _)| path)
            .collect();
        assert!(writable.iter().any(|path| **path == grant.home()));
        assert!(
            roots
                .iter()
                .any(|(path, writable)| !*writable && grant.runtime().contains(path))
        );
    }

    #[test]
    fn the_private_home_is_created_and_holds_the_xdg_directories() {
        let temp = tempfile::tempdir().unwrap();
        let binary = dist_binary(temp.path());
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let grant = GrantSet::derive(GrantInput {
            server: &server("s", &binary),
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: None,
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap();

        grant.prepare_home().unwrap();
        for directory in ["", ".config", ".cache", ".local/share"] {
            assert!(grant.home().join(directory).is_dir(), "{directory}");
        }

        let env = grant.base_env();
        let value = |key: &str| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("HOME"), Some(grant.home().display().to_string()));
        // The preset layout and `XDG_CONFIG_HOME` must agree, or `gh` looks in
        // one directory and the credential link sits in the other.
        assert_eq!(
            value("XDG_CONFIG_HOME"),
            Some(grant.home().join(".config").display().to_string())
        );
        assert_eq!(
            grant.home().join(".config").display().to_string(),
            format!("{}/.config", grant.home().display())
        );
        // The temp dir follows the private home, so a server's scratch files
        // stay inside the grant.
        assert_eq!(value("TMPDIR"), Some(grant.home().display().to_string()));
    }

    #[test]
    fn credentials_only_grant_what_exists_and_are_linked_into_the_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(home.join(".config").join("gh")).unwrap();
        // `~/.aws` is deliberately missing: the preset must not invent it.
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let mut config = server("s", &dist_binary(temp.path()));
        config.credentials = vec![
            CredentialPreset::Ssh,
            CredentialPreset::Github,
            CredentialPreset::Aws,
        ];

        let grant = GrantSet::derive(GrantInput {
            server: &config,
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: Some(&home),
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap();

        let granted: Vec<PathBuf> = grant.credentials().map(Path::to_path_buf).collect();
        assert!(granted.contains(&home.join(".ssh")));
        assert!(granted.contains(&home.join(".config").join("gh")));
        assert!(!granted.iter().any(|path| path.ends_with(".aws")));
        // `$HOME` itself is never granted.
        assert!(!granted.contains(&home));

        grant.prepare_home().unwrap();
        let link = grant.home().join(".ssh");
        assert!(link.exists(), "credential link missing");
        assert_eq!(link.read_link().unwrap(), home.join(".ssh"));
    }

    #[test]
    fn sandbox_off_still_produces_a_grant_but_marks_it_unconfined() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let grant = GrantSet::derive(GrantInput {
            server: &server("s", &dist_binary(temp.path())),
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: None,
            confined: false,
            path: SearchPath::Process,
        })
        .unwrap();
        assert!(!grant.is_confined());
        assert!(!grant.runtime().is_empty());
        assert!(grant.describe().starts_with("sandbox off"));
    }

    #[test]
    fn env_values_may_reference_the_parent_environment() {
        // SAFETY: single-threaded test process; the variable is restored below.
        unsafe { std::env::set_var("QCODE_GRANT_TEST_TOKEN", "abc") };
        assert_eq!(
            expand_env("s", "TOKEN", "${QCODE_GRANT_TEST_TOKEN}").unwrap(),
            "abc"
        );
        // Only the whole-value form is expanded.
        assert_eq!(expand_env("s", "TOKEN", "x${NOPE}y").unwrap(), "x${NOPE}y");
        let error = expand_env("s", "TOKEN", "${QCODE_GRANT_TEST_MISSING}").unwrap_err();
        assert!(
            matches!(&error, GrantError::MissingEnv { key, variable, .. }
                if key == "TOKEN" && variable == "QCODE_GRANT_TEST_MISSING"),
            "{error}"
        );
        unsafe { std::env::remove_var("QCODE_GRANT_TEST_TOKEN") };
    }

    #[test]
    fn a_missing_command_names_itself() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let error = GrantSet::derive(GrantInput {
            server: &server("gone", Path::new("/nonexistent/definitely/missing")),
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: None,
            confined: true,
            path: SearchPath::Process,
        })
        .unwrap_err();
        assert!(
            matches!(error, GrantError::CommandNotFound { ref server, .. } if server == "gone"),
            "{error}"
        );
    }

    /// ADR-0070: a bare `command` is looked up in the `PATH` the caller names,
    /// not in qcode's own. This is what makes `command = "wordcraft-cli"`
    /// portable across a version upgrade.
    #[test]
    fn a_bare_command_is_resolved_in_the_given_path() {
        let temp = tempfile::tempdir().unwrap();
        // Two "installations", only the second on the search path. The name is
        // the same, so a lookup that ignored the argument would still find
        // *something* — and fail the assertion below.
        let elsewhere = temp.path().join("elsewhere");
        let installed = temp.path().join("installed/bin");
        for dir in [&elsewhere, &installed] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("tool"), b"not really an elf").unwrap();
        }
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let search = std::ffi::OsString::from(installed.display().to_string());
        let grant = GrantSet::derive(GrantInput {
            server: &McpServerConfig {
                name: "tool".to_string(),
                command: Some("tool".to_string()),
                ..Default::default()
            },
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: None,
            confined: true,
            path: SearchPath::Snapshot(&search),
        })
        .expect("the bare name is on the given PATH");

        // The grant describes the file that would run: its directory became the
        // runtime root, and the other installation did not.
        assert!(
            grant
                .runtime()
                .iter()
                .any(|root| installed.canonicalize().unwrap().starts_with(root)),
            "{:?}",
            grant.runtime()
        );
        assert!(
            !grant
                .runtime()
                .iter()
                .any(|root| elsewhere.canonicalize().unwrap().starts_with(root)),
            "{:?}",
            grant.runtime()
        );
    }

    /// The failure has to name the `PATH` it searched: "not found" alone cannot
    /// distinguish a typo from "this GUI has a smaller `PATH` than your shell",
    /// and the second is the case that brings people here.
    #[test]
    fn a_bare_command_that_is_missing_names_the_path_it_searched() {
        let temp = tempfile::tempdir().unwrap();
        let empty = temp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let workspace = temp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let search = std::ffi::OsString::from(empty.display().to_string());
        let error = GrantSet::derive(GrantInput {
            server: &McpServerConfig {
                name: "gone".to_string(),
                command: Some("definitely-not-here".to_string()),
                ..Default::default()
            },
            workspace: &workspace,
            server_state: temp.path().join("state"),
            real_home: None,
            confined: true,
            path: SearchPath::Snapshot(&search),
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("shell environment's PATH"),
            "{error}"
        );
        assert!(error.to_string().contains("definitely-not-here"), "{error}");
    }

    /// `SearchPath::from(None)` is the pre-ADR-0070 behaviour, and the fallback
    /// when sampling is off or produced nothing.
    #[test]
    fn a_missing_snapshot_falls_back_to_the_process_path() {
        assert_eq!(SearchPath::from(None), SearchPath::Process);
        let snapshot = std::ffi::OsString::from("/snapshot/bin");
        assert_eq!(
            SearchPath::from(Some(snapshot.as_os_str())),
            SearchPath::Snapshot(snapshot.as_os_str())
        );
        // What the child gets is exactly what the lookup used.
        assert_eq!(
            SearchPath::Snapshot(snapshot.as_os_str()).as_os_str(),
            snapshot
        );
        assert_eq!(
            SearchPath::Process.as_os_str(),
            std::env::var_os("PATH").unwrap_or_default()
        );
    }
}
