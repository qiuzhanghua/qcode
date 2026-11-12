//! Layered configuration: CLI flags > environment > user config > built-ins.
//!
//! API keys are never stored in the config file. A provider entry only names
//! the environment variable that holds the secret, and resolved secrets live in
//! memory only. `OpenAiCompatConfig` redacts them in `Debug`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use qcode_core::cost::Pricing;
use qcode_core::event::Scope;
use qcode_core::policy::{Action, Rule};
use qcode_mcp::{McpConfig, McpConfigError};
use qcode_tools::sandbox::SandboxMode;
use qcode_tools::web_search::{SearchBackend, SearchSettings};

mod catalog;
mod edit;
mod load;
mod resolve;
mod schedule;
#[cfg(test)]
mod tests;

pub use catalog::*;
pub use edit::*;
#[cfg(test)]
pub(crate) use load::is_system_install;
pub use load::*;
pub use resolve::*;
pub use schedule::*;

pub const DEFAULT_CONFIG_RELATIVE: &str = ".config/qcode/config.toml";

/// Schema version this build writes and the newest one it accepts.
///
/// A file without `version` is the current version: every config written before
/// versioning existed must keep loading unchanged. A file that declares a
/// *newer* version is an error, because this build cannot know which of its
/// interpretations changed.
pub const CONFIG_VERSION: u32 = 2;

/// The schema a file without a version is read as. Files written by this build
/// carry [`CONFIG_VERSION`]; a `version = 1` file is still read (ADR-0065).
pub const LEGACY_CONFIG_VERSION: u32 = 1;

/// How a message typed while a turn is running is delivered (ADR-0042).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusyDelivery {
    /// Queue it as the follow-up turn (default).
    #[default]
    Queue,
    /// Steer it into the running turn's next step boundary.
    Steer,
}

/// UI delivery settings shared by the GUI and the TUI.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub busy_delivery: BusyDelivery,
}

/// Global default model section `[default-model]` (M1 two-tier selection).
///
/// The GUI persists the session-level model here when the user switches it
/// ("切即默认") or pins it via the provider card's "设为默认" button.
/// Read-time precedence: `default_model.provider` outranks the legacy
/// `default_provider` (V2.1), and `default_model.model` only applies to the
/// provider it names (or to any provider when it names none).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultModelConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Schema version of the file. Absent means [`CONFIG_VERSION`]. `version`
    /// is still accepted so a file written before ADR-0065 keeps loading.
    #[serde(default = "default_version", alias = "version")]
    pub schema: u32,
    /// Provider used when neither `--provider` nor `QCODE_PROVIDER` is set.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default provider+model for sessions that have none (`[default-model]`).
    #[serde(default, rename = "default-model")]
    pub default_model: DefaultModelConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub approval: ApprovalConfig,
    /// Token prices keyed by model name. Optional: without an entry, cost is
    /// unknown (reported as zero) and `--max-cost` refuses to run.
    #[serde(default)]
    pub pricing: BTreeMap<String, Pricing>,
    /// Context window used for compaction.
    #[serde(default)]
    pub context: ContextConfig,
    /// Turn budget for one user message.
    #[serde(default)]
    pub agent: AgentConfig,
    /// stdio MCP servers. Disabled entries are kept for quick toggling.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Agent Skills discovered from the user and project config trees.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// OS sandbox for `run_shell`. Defaults to the platform's backend
    /// ([`default_sandbox_mode`]); `none` keeps approval as the boundary.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Application-level egress policy (ADR-0023).
    #[serde(default)]
    pub network: NetworkConfig,
    /// `web_search` backend and key (v2.0).
    #[serde(default)]
    pub search: SearchConfig,
    /// Version checks and `qc update` (v2.0).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Delivery behaviour shared by the frontends.
    #[serde(default)]
    pub ui: UiConfig,
    /// Scheduled turns (ADR-0059): `[schedules.<name>]`. Optional, so an old
    /// binary reading this file simply ignores the table.
    #[serde(default)]
    pub schedules: BTreeMap<String, ScheduleConfig>,
    /// The built-in terminal (ADR-0062).
    #[serde(default)]
    pub terminal: TerminalConfig,
    /// How `run_shell` learns the user's shell environment (ADR-0064).
    #[serde(default)]
    pub shell: qcode_tools::shell_env::ShellSettings,
    /// Diagnostics: where the log goes and how loud it is (ADR-0077).
    #[serde(default)]
    pub log: LogConfig,
}

/// `[log]`: file logging for both frontends (ADR-0077).
///
/// The file is not configurable on purpose. `logs/qc.log` is one stable path
/// next to the data the user already has, and `qc logs` prints it; a knob here
/// would only give users a way to lose track of their own logs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogConfig {
    /// Verbosity as an [`EnvFilter`] spec: `info`, `warn`, or per-module such as
    /// `qcode_gui=debug,warn`. `RUST_LOG` overrides this.
    ///
    /// `None` when the key is absent, and that difference is load-bearing: absent
    /// means "use the per-destination defaults" (the file at `info`, so the log is
    /// a usable record of a run; stderr at `warn`, so an interactive command stays
    /// quiet). A value here is an explicit ask and applies to both destinations —
    /// see ADR-0082.
    ///
    /// An unparsable value falls back to those same defaults rather than failing
    /// startup: a typo in the log level must never be the reason the agent cannot
    /// run.
    #[serde(default)]
    pub level: Option<String>,
}

/// `[terminal]`: how `qc gui` starts its shells (ADR-0062 §10).
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalConfig {
    /// Shell to run. Empty means "detect": the shell the user is actually in,
    /// then `$SHELL`, then the passwd entry, then `/bin/sh`. A configured value
    /// that is not on `PATH` falls back to detection rather than failing.
    #[serde(default)]
    pub shell: String,
    /// Directory a new terminal starts in. Empty means "inherit": the cwd of the
    /// terminal the user was last in, then home.
    #[serde(default)]
    pub start_cwd: String,
    /// Font family for the terminal, for every script.
    ///
    /// Empty means "follow the app": Latin takes the code font (`[gui] code_font`)
    /// and CJK takes the UI font, the one the composer's text uses. Both are drawn at
    /// the composer's size, so the terminal and the transcript agree.
    ///
    /// A name set here overrides both halves — someone who typed a family wants it
    /// used, not quietly split in two.
    #[serde(default)]
    pub font: String,
    /// Scrollback lines kept per terminal.
    #[serde(default = "default_terminal_scrollback")]
    pub max_scrollback: usize,
    /// How many terminals may be open at once.
    #[serde(default = "default_terminal_max")]
    pub max_terminals: usize,
}

fn default_terminal_scrollback() -> usize {
    10_000
}

fn default_terminal_max() -> usize {
    8
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: String::new(),
            start_cwd: String::new(),
            font: String::new(),
            max_scrollback: default_terminal_scrollback(),
            max_terminals: default_terminal_max(),
        }
    }
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_VERSION,
            default_provider: None,
            default_model: DefaultModelConfig::default(),
            providers: BTreeMap::new(),
            approval: ApprovalConfig::default(),
            pricing: BTreeMap::new(),
            context: ContextConfig::default(),
            agent: AgentConfig::default(),
            mcp: McpConfig::default(),
            skills: SkillsConfig::default(),
            sandbox: SandboxConfig::default(),
            network: NetworkConfig::default(),
            search: SearchConfig::default(),
            update: UpdateConfig::default(),
            ui: UiConfig::default(),
            schedules: BTreeMap::new(),
            terminal: TerminalConfig::default(),
            shell: qcode_tools::shell_env::ShellSettings::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchConfig {
    /// Expose the `web_search` tool. On by default; without a usable backend
    /// the tool is not registered and `qc doctor` says what is missing.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub backend: SearchBackend,
    /// Environment variable holding the API key. Empty means the backend's
    /// conventional variable (`BRAVE_API_KEY` / `TAVILY_API_KEY`); SearXNG is
    /// keyless.
    #[serde(default)]
    pub api_key_env: String,
    /// Command whose stdout is the key, used when the environment variable is
    /// absent (Keychain/libsecret/credential manager).
    #[serde(default)]
    pub api_key_command: Option<Vec<String>>,
    /// Results requested when the tool call does not say otherwise.
    #[serde(default = "default_search_results")]
    pub max_results: u32,
    /// API origin override, e.g. a self-hosted SearXNG instance.
    #[serde(default)]
    pub base_url: Option<String>,
}

fn default_search_results() -> u32 {
    5
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: SearchBackend::default(),
            api_key_env: String::new(),
            api_key_command: None,
            max_results: default_search_results(),
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Version checks at all. Enterprise images turn this off.
    pub enabled: bool,
    /// Check on startup (cached for `check_days`, never replaces anything).
    pub check: bool,
    pub check_days: u64,
    /// GitHub repository whose releases are the update channel.
    pub repo: String,
    /// API origin override: a mirror, or a local server in tests.
    pub base_url: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check: true,
            check_days: 7,
            repo: crate::DEFAULT_REPO.to_string(),
            base_url: None,
        }
    }
}

impl UpdateConfig {
    /// Whether `qc run` should spend a request on the startup notice.
    pub fn checks_on_startup(&self) -> bool {
        self.enabled && self.check
    }
}

impl SearchConfig {
    /// Resolve the environment variable name: explicit config wins, otherwise
    /// the backend's convention. `None` means keyless.
    pub fn key_env_name(&self) -> Option<String> {
        if !self.api_key_env.trim().is_empty() {
            return Some(self.api_key_env.trim().to_string());
        }
        self.backend.default_api_key_env().map(str::to_string)
    }

    pub fn settings(&self, env: &dyn Fn(&str) -> Option<String>) -> SearchSettings {
        let api_key = self.key_env_name().and_then(|name| env(&name)).or_else(|| {
            let argv = self.api_key_command.as_deref()?;
            match key_from_command("web_search", argv) {
                Ok(key) => Some(key),
                Err(error) => {
                    tracing::warn!(%error, "search key command failed");
                    None
                }
            }
        });
        SearchSettings {
            backend: self.backend,
            api_key,
            base_url: self
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            max_results: self
                .max_results
                .clamp(1, qcode_tools::web_search::MAX_RESULTS),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// Do not start the egress proxy.
    Off,
    /// Application-level loopback proxy (default). Shell/MCP get proxy env vars,
    /// but a raw socket can bypass them.
    #[default]
    Proxy,
    /// macOS hard mode: the proxy runs as usual and the Seatbelt profile only
    /// allows outbound TCP to its loopback port.
    Strict,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// Egress mode. `strict` requires `[sandbox] mode = "sandbox-exec"`.
    #[serde(default)]
    pub mode: NetworkMode,
    /// Start the local proxy. Disable only for offline/testing workflows.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Host patterns; provider hosts are added automatically.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Allow loopback/private/link-local destinations (local Ollama, tests).
    #[serde(default)]
    pub allow_private: bool,
    /// Unknown host policy. `ask` is enforced by the tool layer; the proxy
    /// itself can only deny or allow, because it cannot block on UI.
    #[serde(default)]
    pub unknown: UnknownHost,
    /// Append allowed/denied connections to `<data>/logs/network.jsonl`.
    #[serde(default = "default_true")]
    pub audit: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Proxy,
            enabled: true,
            allow: Vec::new(),
            allow_private: false,
            unknown: UnknownHost::Ask,
            audit: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownHost {
    #[default]
    Ask,
    Deny,
    Allow,
}

/// Skills are on by default; `enabled = false` skips discovery entirely.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra skill roots, loaded after the user root and before the project
    /// root (later roots win). `~/` is expanded; relative paths resolve against
    /// the workspace.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_context_window")]
    pub window_tokens: u64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            window_tokens: default_context_window(),
        }
    }
}

fn default_context_window() -> u64 {
    128_000
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApprovalConfig {
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Shorthand for "allow this, no question" (ADR-0065): `"run_shell:echo *"`
    /// means `command_prefix = "echo "`, `"read_file:src/**"` means
    /// `path_glob = "src/**"`, a bare `"web_search"` matches the tool alone.
    #[serde(default)]
    pub allow: Vec<String>,
    /// The same shorthand for "never, not even with approval".
    #[serde(default)]
    pub deny: Vec<String>,
}

impl ApprovalConfig {
    /// Expand [`Self::allow`]/[`Self::deny`] into rules, in that order.
    ///
    /// The shorthand keeps `[[approval.rules]]` for the cases that need a
    /// scope, an id, or several predicates at once; everything else reads
    /// better as a list of `tool:pattern` lines.
    pub fn shorthand_rules(&self) -> Vec<Rule> {
        let mut rules = Vec::with_capacity(self.allow.len() + self.deny.len());
        for (action, entries) in [(Action::Allow, &self.allow), (Action::Deny, &self.deny)] {
            for (index, entry) in entries.iter().enumerate() {
                rules.push(shorthand_rule(action, entry, index));
            }
        }
        rules
    }
}

fn shorthand_rule(action: Action, entry: &str, index: usize) -> Rule {
    let (tool, pattern) = match entry.split_once(':') {
        Some((tool, pattern)) => (tool.trim(), Some(pattern.trim())),
        None => (entry.trim(), None),
    };
    let mut rule = Rule {
        id: format!(
            "{}-{index}",
            if action == Action::Allow {
                "allow"
            } else {
                "deny"
            }
        ),
        tool: if tool.is_empty() || tool == "*" {
            None
        } else {
            Some(tool.to_string())
        },
        command_prefix: None,
        path_glob: None,
        action,
        scope: None,
    };
    match pattern.filter(|pattern| !pattern.is_empty()) {
        Some(pattern) if tool == "run_shell" => {
            // `rm -rf *` and `rm -rf` mean the same thing: the prefix.
            rule.command_prefix = Some(pattern.trim_end_matches('*').to_string());
        }
        Some(pattern) => rule.path_glob = Some(pattern.to_string()),
        None => {}
    }
    rule
}

/// Turn budget for one user message.
///
/// Long analysis can spend dozens of model calls on reads, searches and
/// shell commands. The default leaves room for that while `--max-turns` and
/// `[agent] max_turns` still bound a runaway loop.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Stop the turn once the estimated cost passes this many USD. Needs a
    /// `[pricing."<model>"]` entry; the `--max-cost` flag wins over it.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Snapshot the whole workspace before `run_shell`, which can write
    /// anywhere. Costs one copy per shell turn (capped at 64 MB) and makes
    /// rollback cover shell- and MCP-driven edits; disable it on huge repos.
    #[serde(default = "default_true")]
    pub snapshot_shell: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            max_cost_usd: None,
            snapshot_shell: true,
        }
    }
}

fn default_max_turns() -> u32 {
    64
}

/// The sandbox backend a fresh config gets on this platform.
///
/// Each backend is self-contained for its OS: `windows-acl` (built into the
/// binary), `sandbox-exec` (macOS, present by default) and `landlock`
/// (Linux, unprivileged kernel confinement, no external binary). Startup
/// still preflights and fails closed when the OS cannot apply it.
fn default_sandbox_mode() -> SandboxMode {
    #[cfg(target_os = "windows")]
    {
        SandboxMode::WindowsAcl
    }
    #[cfg(target_os = "macos")]
    {
        SandboxMode::SandboxExec
    }
    #[cfg(target_os = "linux")]
    {
        SandboxMode::Landlock
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        SandboxMode::None
    }
}

/// One switch for "may the agent reach the network?" (ADR-0065).
///
/// Today it takes three cooperating knobs: `[network] enabled`, `[network]
/// mode` and `[sandbox] network`. They answer one question between them, and
/// answering it in two places is how a config ends up half-changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    /// No proxy, and the sandbox blocks outbound traffic.
    Off,
    /// The egress proxy runs; the sandbox only allows loopback to it.
    #[default]
    Proxy,
    /// Proxy plus "unknown hosts are denied" (macOS Seatbelt enforces it).
    Strict,
    /// Raw network inside the sandbox, no proxy: the agent talks to the
    /// internet directly.
    Open,
}

impl SandboxNetworkMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Proxy => "proxy",
            Self::Strict => "strict",
            Self::Open => "open",
        }
    }
}

/// What [`Config::network_plan`] decided, after folding the legacy keys in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkPlan {
    /// Start the egress proxy.
    pub proxy: bool,
    /// Deny unknown hosts (the strict mode's policy half).
    pub strict: bool,
    /// Hand the OS sandbox a raw network allowance instead of loopback-only.
    pub raw: bool,
}

/// Accept `network = true|false` (files before ADR-0065) and the new names.
fn network_mode_from_any<'de, D>(deserializer: D) -> Result<Option<SandboxNetworkMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Flag(bool),
        Name(SandboxNetworkMode),
    }
    Ok(match Option::<Either>::deserialize(deserializer)? {
        None => None,
        // `network = true` used to mean "raw network inside the sandbox".
        Some(Either::Flag(true)) => Some(SandboxNetworkMode::Open),
        Some(Either::Flag(false)) => None,
        Some(Either::Name(name)) => Some(name),
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// OS sandbox for `run_shell`. Defaults to the platform backend; `none`
    /// keeps approval as the boundary.
    #[serde(default = "default_sandbox_mode")]
    pub mode: SandboxMode,
    /// May the agent reach the network? `None` means "derive it from the legacy
    /// `[network]` section", which is what files written before ADR-0065 have.
    #[serde(
        default,
        rename = "network",
        deserialize_with = "network_mode_from_any"
    )]
    pub network: Option<SandboxNetworkMode>,
    /// Extra roots every backend grants **write** access to, besides the task
    /// roots and (for an MCP server) its private temp directory. `~/x` is
    /// expanded at load time.
    ///
    /// Honoured by `sandbox-exec`, `windows-acl`, `bwrap` and `landlock`. The
    /// two Linux backends used to drop this list on the floor — `landlock`
    /// because the backend had no write list at all — so a user who set it got
    /// a read-only directory, no error, and no way to tell whether the config
    /// or the code was wrong. Measured: `touch ~/Documents/x` gave `Permission
    /// denied` while `cat` on a file in the same directory worked, which is
    /// exactly "the setting is ignored".
    ///
    /// The entry gets the same rights as a read-write task root, which makes
    /// "listing a directory here = the agent owns it" true; recursive, so
    /// `~/Documents` and not `~/Documents/a/b`. The shapes that are never meant
    /// are refused at load time by [`SandboxConfig::validate`]: `/`, the home
    /// directory itself, and `/home` / `/Users` / `/root`. There is no
    /// workspace to narrow them to — this section is config-wide, so an
    /// `extra_writable = ["~/"]` typo applies to every task including an MCP
    /// server under `sandbox = "on"`.
    ///
    /// Note the Landlock caveat: it unions rules, so an entry here also covers
    /// any read-only task root underneath it. That is not new (two nested task
    /// roots already behaved that way), but this list is invisible per task,
    /// which is the other reason the dangerous shapes are refused.
    #[serde(default)]
    pub extra_writable: Vec<PathBuf>,
    /// Extra roots every backend grants **read** (and execute) access to,
    /// besides the fixed system allowlist and the task roots.
    ///
    /// The system allowlist is a fixed list of distro paths (`/usr`, `/bin`,
    /// `/lib`, `/etc`, …), so a toolchain installed anywhere else is invisible
    /// to a confined `run_shell`. This machine is the example: Rust lives in
    /// `~/cot/lib/rust_*`, and with `mode = "landlock"` and no `extra_read` the
    /// agent could not read it — `cargo`, `gh` and every other tool under that
    /// prefix failed with `Permission denied` before starting. Listing the
    /// prefix here fixes that without opening the write side.
    ///
    /// `~/cot` is expanded against the real home at load time, so the entry
    /// reads the same on every machine. Relative paths are not expanded: the
    /// config is user-wide and the workspace is per-task, so "relative to what"
    /// has no stable answer.
    ///
    /// Recursive: each entry grants its whole subtree. Granting a directory is
    /// therefore a real decision — `extra_read = ["~"]` hands the agent
    /// `~/.ssh` and `~/.aws` too.
    #[serde(default)]
    pub extra_read: Vec<PathBuf>,
    /// May a confined `run_shell` allocate a pseudo-terminal? Default `false`.
    ///
    /// A PTY is how anything terminal-shaped starts: this repository's own
    /// `qcode-terminal` integration suite spawns its child through `openpty`,
    /// and inside a Landlocked `run_shell` all 23 of its tests failed with
    /// `EACCES` on `/dev/ptmx` (the node is 0666 — Landlock was the refusal).
    ///
    /// `mode = "bwrap"` does not need this and ignores it: `--dev` mounts a
    /// **new** devpts (`newinstance`), so the pty is private to the sandbox. The
    /// flag exists for `landlock`, which has no mount namespace and therefore
    /// can only share the host's devpts. That is a real widening and the reason
    /// the default is off: `/dev/pts/N` for the user's other terminal windows is
    /// writable by the same uid, so the grant is enough to type into an
    /// unsandboxed shell — a way out of a sandbox whose whole point is "the
    /// workspace and nothing else". Turn it on when the workload genuinely needs
    /// a terminal, and prefer `mode = "bwrap"` if it is available.
    ///
    /// `mode = "none"` has nothing to gate; `windows-acl` restricts writes only,
    /// so PTYs already work there.
    #[serde(default)]
    pub allow_pty: bool,
}

impl SandboxConfig {
    /// Refuse write roots that are never what the user meant.
    ///
    /// `[sandbox] extra_writable` is config-wide: it applies to every task, so
    /// unlike an MCP server's derived grant there is no workspace to narrow it
    /// to. `~/Documents` is the shape people actually want; `~/` and `/home`
    /// arrive by typo or by a copy-pasted example, and both quietly hand a
    /// confined `run_shell` every file the user can write.
    ///
    /// Called from every load path, after `~` expansion — before expansion a
    /// `~/` entry is the relative path `~/`, which compares equal to nothing.
    /// Only these three shapes are refused, not "must be absolute": a relative
    /// entry grants nothing (it never matches a resolved root) and turning it
    /// into a load failure would reject configs that load fine today.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let home = home_dir();
        for path in &self.extra_writable {
            if let Some(reason) = qcode_core::dangerous_write_root(path, home.as_deref()) {
                return Err(ConfigError::InvalidDocument(format!(
                    "[sandbox] extra_writable: {} is {reason}; \
                     list a subdirectory such as \"~/Documents\" instead",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// The mode this file asks for, folding the pre-ADR-0065 keys in.
    ///
    /// Precedence, most specific first: `[sandbox] network`, then
    /// `[sandbox] network = true` (read as `open`), then `[network] mode`,
    /// then `[network] enabled`.
    pub fn network_mode_with(&self, network: &NetworkConfig) -> SandboxNetworkMode {
        if let Some(mode) = self.network {
            return mode;
        }
        match network.mode {
            NetworkMode::Off => SandboxNetworkMode::Off,
            NetworkMode::Strict => SandboxNetworkMode::Strict,
            NetworkMode::Proxy if !network.enabled => SandboxNetworkMode::Off,
            NetworkMode::Proxy => SandboxNetworkMode::Proxy,
        }
    }
}

impl Config {
    /// The single answer to "may the agent reach the network?".
    pub fn network_plan(&self) -> NetworkPlan {
        match self.sandbox.network_mode_with(&self.network) {
            SandboxNetworkMode::Off => NetworkPlan {
                proxy: false,
                strict: false,
                raw: false,
            },
            SandboxNetworkMode::Proxy => NetworkPlan {
                proxy: true,
                strict: false,
                raw: false,
            },
            SandboxNetworkMode::Strict => NetworkPlan {
                proxy: true,
                strict: true,
                raw: false,
            },
            SandboxNetworkMode::Open => NetworkPlan {
                proxy: false,
                strict: false,
                raw: true,
            },
        }
    }

    /// The mode the plan came from, for messages.
    pub fn sandbox_network_mode(&self) -> SandboxNetworkMode {
        self.sandbox.network_mode_with(&self.network)
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            network: None,
            extra_writable: Vec::new(),
            extra_read: Vec::new(),
            allow_pty: false,
        }
    }
}

/// Standalone approval policy file (`--approval-policy`).
///
/// The shape matches the config's rule list, but at the root so a policy file
/// can be checked in and audited on its own:
///
/// ```toml
/// [[rules]]
/// id = "allow-cargo-test"
/// tool = "run_shell"
/// command_prefix = "cargo test"
/// action = "allow"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Extra JSON fields merged into the Chat Completions body, for provider
    /// extensions such as MiniMax `reasoning_split`.
    #[serde(default)]
    pub extra_body: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the key. Empty means keyless.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Command whose stdout is the key (Keychain, libsecret, credential
    /// manager). Used when [`Self::api_key_env`] is unset or empty at runtime.
    #[serde(default)]
    pub api_key_command: Option<Vec<String>>,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Wire protocol override. Absent means the catalog route's protocol (or
    /// OpenAI-compatible for a route the catalog does not ship).
    #[serde(default)]
    pub protocol: Option<Protocol>,
    /// Whether the route's models accept image input. Absent means the catalog
    /// route's answer (see [`super::CatalogEntry::images`]); set it for a
    /// gateway that differs from the vendor it fronts, or for a model newer
    /// than this build's catalog.
    #[serde(default)]
    pub images: Option<bool>,
    #[serde(default = "default_true")]
    pub stream_usage: bool,
    /// Deployment-level request headers (gateway auth, custom headers). Applied
    /// before the mandatory headers, so reserved names like `authorization` and
    /// `content-type` can never be overridden.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// This route's own model catalog (U0). An empty list means "serve the
    /// catalog's built-in models"; any entry replaces that catalog, so a row is
    /// only ever added deliberately.
    #[serde(default, deserialize_with = "models_from_entries_or_ids")]
    pub models: Vec<ModelEntry>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: None,
            api_key_command: None,
            default_model: None,
            protocol: None,
            images: None,
            stream_usage: true,
            extra_body: Default::default(),
            headers: Default::default(),
            models: Vec::new(),
        }
    }
}

/// One model a provider route serves. Hand-declared routes carry their own
/// list (hand-filled or adopted from an endpoint probe); catalog routes
/// inherit their static catalog entries until the user overrides them.
/// Accept both spellings of a model list (ADR-0065):
///
/// ```toml
/// models = ["deepseek-flash", "deepseek-v4-pro"]      # the shorthand
/// models = [{ id = "deepseek-flash", name = "…" }]    # when overriding
/// ```
///
/// Mixed lists are fine: a bare string is a model with no overrides.
fn models_from_entries_or_ids<'de, D>(deserializer: D) -> Result<Vec<ModelEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EntryOrId {
        Id(String),
        Entry(ModelEntryInput),
    }

    #[derive(Deserialize)]
    struct ModelEntryInput {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        context_window: Option<u64>,
        #[serde(default)]
        max_tokens: Option<u64>,
    }

    let raw = Vec::<EntryOrId>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|item| match item {
            EntryOrId::Id(id) => ModelEntry {
                id,
                ..Default::default()
            },
            EntryOrId::Entry(entry) => ModelEntry {
                id: entry.id,
                name: entry.name,
                context_window: entry.context_window,
                max_tokens: entry.max_tokens,
            },
        })
        .collect())
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ModelEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    // Boxed so that `ConfigError` stays well under the 128-byte threshold that
    // `clippy::result_large_err` enforces on `Result` returns (a bare
    // `toml::de::Error` is 88 bytes on its own).
    #[error("cannot parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("cannot read approval policy {path}: {source}")]
    ReadPolicy {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse approval policy {path}: {source}")]
    ParsePolicy {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    /// A rule whose `tool` or `path_glob` does not parse as a glob.
    ///
    /// Fatal rather than skipped: a `deny` rule with a typo would otherwise
    /// never match, which reads as "denied" on paper and "asked" (or allowed)
    /// at runtime. The message names the file so the fix is obvious.
    #[error("{path}: {detail}")]
    InvalidPolicyRule { path: PathBuf, detail: String },
    #[error("unknown provider '{0}'; add a [providers.{0}] section or pass --base-url and --model")]
    UnknownProvider(String),
    #[error(
        "provider '{provider}' requires ${env}; export it, or set \
         providers.{provider}.api_key_env = \"\" for a keyless server"
    )]
    MissingKey { provider: String, env: String },
    #[error("provider '{provider}': key command failed ({detail})")]
    KeyCommand { provider: String, detail: String },
    #[error(
        "provider '{provider}' has no model; pass --model or set \
         providers.{provider}.default_model"
    )]
    MissingModel { provider: String },
    #[error("provider '{provider}' has an invalid base URL: {url}")]
    InvalidBaseUrl { provider: String, url: String },
    #[error("cannot determine home directory; pass --config explicitly")]
    NoHome,
    #[error(
        "--max-cost requires pricing for model '{model}'; add a [pricing.\"{model}\"] \
         section with input_usd_per_mtok and output_usd_per_mtok"
    )]
    PricingRequired { model: String },
    #[error("pricing for model '{model}' must be finite and non-negative")]
    InvalidPricing { model: String },
    #[error("--max-cost must be a finite, non-negative USD amount")]
    InvalidMaxCost,
    #[error("invalid MCP configuration: {0}")]
    Mcp(#[from] McpConfigError),
    #[error("invalid config document: {0}")]
    InvalidDocument(String),
    #[error(
        "config {} declares version {found}, but this build supports up to {supported}; \
         upgrade qcode or delete the version key",
        path.display()
    )]
    VersionTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("MCP server '{0}' already exists")]
    McpExists(String),
    #[error("MCP server '{0}' not found")]
    McpMissing(String),
}

/// How long a key command may run before it is killed.
const KEY_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Read a secret from a command's stdout.
///
/// No shell is involved, so there is nothing to quote or escape; the output is
/// never echoed, because it is the secret. The child is killed if it outlives
/// [`KEY_COMMAND_TIMEOUT`].
fn key_from_command(provider: &str, argv: &[String]) -> Result<String, ConfigError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let Some((program, rest)) = argv.split_first() else {
        return Err(ConfigError::KeyCommand {
            provider: provider.to_string(),
            detail: "the command is empty".to_string(),
        });
    };
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ConfigError::KeyCommand {
            provider: provider.to_string(),
            detail: error.to_string(),
        })?;

    // Read in a thread so a chatty child cannot deadlock on a full pipe.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        let _ = tx.send(buffer);
    });

    let buffer = match rx.recv_timeout(KEY_COMMAND_TIMEOUT) {
        Ok(buffer) => buffer,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ConfigError::KeyCommand {
                provider: provider.to_string(),
                detail: format!("timed out after {KEY_COMMAND_TIMEOUT:?}"),
            });
        }
    };
    let status = child.wait().map_err(|error| ConfigError::KeyCommand {
        provider: provider.to_string(),
        detail: error.to_string(),
    })?;
    if !status.success() {
        return Err(ConfigError::KeyCommand {
            provider: provider.to_string(),
            detail: format!("exited with {status}"),
        });
    }
    let key = buffer.trim().to_string();
    if key.is_empty() {
        return Err(ConfigError::KeyCommand {
            provider: provider.to_string(),
            detail: "printed nothing on stdout".to_string(),
        });
    }
    Ok(key)
}

/// Doctor-friendly description of where a key came from.
fn command_source(argv: &[String]) -> String {
    argv.first()
        .map(|program| format!("command {program}"))
        .unwrap_or_else(|| "command".to_string())
}
