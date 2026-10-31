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

mod edit;
mod load;
mod resolve;
#[cfg(test)]
mod tests;

pub use edit::*;
#[cfg(test)]
pub(crate) use load::is_system_install;
pub use load::*;
pub use resolve::*;

pub const DEFAULT_CONFIG_RELATIVE: &str = ".config/qcode/config.toml";

/// Schema version this build writes and the newest one it accepts.
///
/// A file without `version` is the current version: every config written before
/// versioning existed must keep loading unchanged. A file that declares a
/// *newer* version is an error, because this build cannot know which of its
/// interpretations changed.
pub const CONFIG_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Schema version of the file. Absent means [`CONFIG_VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    /// Provider used when neither `--provider` nor `QCODE_PROVIDER` is set.
    #[serde(default)]
    pub default_provider: Option<String>,
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
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            default_provider: None,
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

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// OS sandbox for `run_shell`. Defaults to the platform backend; `none`
    /// keeps approval as the boundary.
    #[serde(default = "default_sandbox_mode")]
    pub mode: SandboxMode,
    /// Proxy mode: allow unrestricted network inside the OS sandbox. Ignored in
    /// strict mode, which always allows only the loopback proxy port.
    /// Enforced by `sandbox-exec` and `bwrap`; Landlock and Windows ACL only
    /// record the setting.
    #[serde(default)]
    pub network: bool,
    /// Extra roots the `windows-acl` backend grants write access to, besides the
    /// workspace and its private temp directory.
    #[serde(default)]
    pub extra_writable: Vec<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            network: false,
            extra_writable: Vec::new(),
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
    #[serde(default = "default_true")]
    pub stream_usage: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: None,
            api_key_command: None,
            default_model: None,
            stream_usage: true,
            extra_body: Default::default(),
        }
    }
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
    #[error("cannot parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot read approval policy {path}: {source}")]
    ReadPolicy {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse approval policy {path}: {source}")]
    ParsePolicy {
        path: PathBuf,
        source: toml::de::Error,
    },
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
