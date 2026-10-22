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

pub const DEFAULT_CONFIG_RELATIVE: &str = ".config/qcode/config.toml";

/// Schema version this build writes and the newest one it accepts.
///
/// A file without `version` is the current version: every config written before
/// versioning existed must keep loading unchanged. A file that declares a
/// *newer* version is an error, because this build cannot know which of its
/// interpretations changed.
pub const CONFIG_VERSION: u32 = 1;

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
    /// OS sandbox for `run_shell`. Defaults to `none` (approval is the boundary).
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
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

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let document: toml::Table = text.parse().map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    check_version(path, config.version)?;
    warn_unknown_keys(path, &document);
    // Fail before spawning anything if the MCP section is malformed.
    config.mcp.validate()?;
    Ok(config)
}

/// Reject a version this build cannot interpret; anything older is migrated by
/// the current parser in place (version 1 has no predecessors).
fn check_version(path: &Path, version: u32) -> Result<(), ConfigError> {
    if version == 0 {
        return Err(ConfigError::InvalidDocument(format!(
            "{}: version must be 1 or greater",
            path.display()
        )));
    }
    if version > CONFIG_VERSION {
        return Err(ConfigError::VersionTooNew {
            path: path.to_path_buf(),
            found: version,
            supported: CONFIG_VERSION,
        });
    }
    Ok(())
}

/// Keys each table may contain, per table path. Tables whose children are
/// user-chosen names (`providers`, `pricing`, `mcp.servers` entries) are not
/// listed, since any key is a name there.
///
/// Exposed for the GUI schema-coverage test: every known key must have a form
/// control (or an explicit "auto" exception such as `version`).
#[doc(hidden)]
pub fn known_keys() -> &'static [(&'static str, &'static [&'static str])] {
    KNOWN_KEYS
}

const KNOWN_KEYS: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "version",
            "default_provider",
            "providers",
            "approval",
            "pricing",
            "context",
            "agent",
            "mcp",
            "skills",
            "sandbox",
            "network",
            "search",
            "update",
        ],
    ),
    ("approval", &["rules"]),
    ("context", &["window_tokens"]),
    ("agent", &["max_turns", "max_cost_usd", "snapshot_shell"]),
    ("mcp", &["servers"]),
    ("skills", &["enabled", "roots"]),
    ("sandbox", &["mode", "network", "extra_writable"]),
    (
        "network",
        &[
            "mode",
            "enabled",
            "allow",
            "allow_private",
            "unknown",
            "audit",
        ],
    ),
    (
        "search",
        &[
            "enabled",
            "backend",
            "api_key_env",
            "api_key_command",
            "max_results",
            "base_url",
        ],
    ),
    (
        "update",
        &["enabled", "check", "check_days", "repo", "base_url"],
    ),
];

/// Keys in `document` that this build does not read, as `section.key` paths.
///
/// Unknown keys are reported, never fatal: a config written for a newer qcode
/// keeps working under an older one as long as the new keys are optional. The
/// point is to catch typos, which serde would otherwise drop silently.
pub fn unknown_keys(document: &toml::Table) -> Vec<String> {
    let mut unknown = Vec::new();
    for (section, known) in KNOWN_KEYS {
        let table = if section.is_empty() {
            document
        } else {
            match document.get(*section).and_then(toml::Value::as_table) {
                Some(table) => table,
                None => continue,
            }
        };
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                unknown.push(if section.is_empty() {
                    key.clone()
                } else {
                    format!("{section}.{key}")
                });
            }
        }
    }
    unknown
}

fn warn_unknown_keys(path: &Path, document: &toml::Table) {
    let unknown = unknown_keys(document);
    if unknown.is_empty() {
        return;
    }
    tracing::warn!(
        path = %path.display(),
        keys = %unknown.join(", "),
        "unknown config keys are ignored"
    );
}

/// Project-level config, loaded from `<workspace>/.qcode/config.toml`.
pub const PROJECT_CONFIG_RELATIVE: &str = ".qcode/config.toml";

/// Load a config file and overlay the workspace's project config.
pub fn load_merged(path: &Path, workspace: &Path) -> Result<Config, ConfigError> {
    let mut config = load(path)?;
    merge_project_file(&mut config, workspace)?;
    Ok(config)
}

/// Config built from defaults plus the workspace project config.
pub fn load_project(workspace: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::default();
    merge_project_file(&mut config, workspace)?;
    Ok(config)
}

fn merge_project_file(base: &mut Config, workspace: &Path) -> Result<(), ConfigError> {
    let path = workspace.join(PROJECT_CONFIG_RELATIVE);
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let document: toml::Table = text.parse().map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    let project: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    check_version(&path, project.version)?;
    warn_unknown_keys(&path, &document);
    let sections = ProjectSections {
        context: document.contains_key("context"),
        sandbox: document.contains_key("sandbox"),
        search: document.contains_key("search"),
    };
    merge_project(base, project, sections);
    base.mcp.validate()?;
    Ok(())
}

/// Which optional sections the project file actually contains. Absent sections
/// must not reset user values to their defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectSections {
    pub context: bool,
    pub sandbox: bool,
    /// A project may point `web_search` at its own endpoint; it still cannot
    /// bypass tool approval or the egress allowlist.
    pub search: bool,
}

/// Merge a project config over a user/explicit one.
///
/// Project approval rules are forced to `Scope::Project` and may only tighten:
/// an `allow` becomes `ask`, because a repository must not grant itself more
/// permission than the user configured.
pub fn merge_project(base: &mut Config, project: Config, sections: ProjectSections) {
    if project.default_provider.is_some() {
        base.default_provider = project.default_provider;
    }
    base.providers.extend(project.providers);
    base.pricing.extend(project.pricing);
    base.mcp.servers.extend(project.mcp.servers);
    if !project.skills.enabled {
        base.skills.enabled = false;
    }
    base.skills.roots.extend(project.skills.roots);
    if sections.context {
        base.context = project.context;
    }
    if sections.sandbox {
        base.sandbox = project.sandbox;
    }
    if sections.search {
        base.search = project.search;
    }

    for mut rule in project.approval.rules {
        rule.scope = Some(Scope::Project);
        if rule.action == Action::Allow {
            tracing::warn!(
                rule = %rule.id,
                "project approval rules cannot allow; downgraded to ask"
            );
            rule.action = Action::Ask;
        }
        base.approval.rules.push(rule);
    }
}

/// Load a standalone `--approval-policy` file.
///
/// The returned rules replace (not extend) `[[approval.rules]]` from the config
/// for this invocation: a policy file is an explicit, auditable decision.
pub fn load_policy(path: &Path) -> Result<Vec<Rule>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadPolicy {
        path: path.to_path_buf(),
        source,
    })?;
    let file: PolicyFile = toml::from_str(&text).map_err(|source| ConfigError::ParsePolicy {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file.rules)
}

/// Home directory across platforms. Windows has no `HOME` by default.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// A resolved config location plus where it came from.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub source: ConfigSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config`
    Flag,
    /// `QCODE_CONFIG`
    Env,
    /// `<exe dir>/data/config.toml`, opted into by `qcode.portable`
    Portable,
    /// `$XDG_CONFIG_HOME/qcode/config.toml` or `~/.config/qcode/config.toml`
    User,
}

impl ConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "env",
            Self::Portable => "portable",
            Self::User => "user",
        }
    }
}

pub fn default_path() -> Result<PathBuf, ConfigError> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("qcode").join("config.toml"));
    }
    let home = home_dir().ok_or(ConfigError::NoHome)?;
    Ok(home.join(DEFAULT_CONFIG_RELATIVE))
}

/// The file `qc config edit` / `qc mcp add|remove` should modify.
pub fn edit_path(explicit: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
    discover(explicit).map(|found| found.path)
}

/// Resolve the config file once, keeping where it came from for diagnostics.
pub fn discover(explicit: Option<PathBuf>) -> Result<Found, ConfigError> {
    if let Some(path) = explicit {
        return Ok(Found {
            path,
            source: ConfigSource::Flag,
        });
    }
    if let Ok(path) = std::env::var("QCODE_CONFIG") {
        return Ok(Found {
            path: PathBuf::from(path),
            source: ConfigSource::Env,
        });
    }
    if let Some(path) = portable_path() {
        return Ok(Found {
            path,
            source: ConfigSource::Portable,
        });
    }
    Ok(Found {
        path: default_path()?,
        source: ConfigSource::User,
    })
}

/// Portable mode: a `qcode.portable` marker next to the executable opts in, and
/// the config then lives in `<exe dir>/data/` so installing a new build over the
/// old directory keeps it.
fn portable_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if !dir.join("qcode.portable").exists() || is_system_install(dir) {
        return None;
    }
    Some(dir.join("data").join("config.toml"))
}

/// Locations that are never a portable directory, even with a marker: writing
/// there either fails (root-owned, read-only image) or, on macOS, breaks the
/// bundle signature.
fn is_system_install(dir: &Path) -> bool {
    #[cfg(target_os = "macos")]
    if dir
        .components()
        .any(|part| part.as_os_str() == "Contents" || part.as_os_str() == "MacOS")
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    for prefix in ["/usr", "/opt", "/app", "/nix", "/snap"] {
        if dir.starts_with(prefix) {
            return true;
        }
    }
    let _ = dir;
    false
}

fn read_document(path: &Path) -> Result<toml::Value, ConfigError> {
    if !path.exists() {
        return Ok(toml::Value::Table(toml::Table::new()));
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse::<toml::Table>()
        .map(toml::Value::Table)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

fn write_document(path: &Path, document: &toml::Value) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let text = toml::to_string_pretty(document)
        .map_err(|err| ConfigError::InvalidDocument(err.to_string()))?;
    let temp = path.with_extension("qcode-tmp");
    std::fs::write(&temp, text).map_err(|source| ConfigError::Read {
        path: temp.clone(),
        source,
    })?;
    std::fs::rename(&temp, path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_document(document: &toml::Value) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(document)
        .map_err(|err| ConfigError::InvalidDocument(err.to_string()))?;
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: PathBuf::from("<edit>"),
        source,
    })?;
    config.mcp.validate()?;
    Ok(())
}

/// Append one `[[mcp.servers]]` entry to the config file.
pub fn mcp_add(
    path: &Path,
    name: &str,
    command: &str,
    args: &[String],
    enabled: bool,
    timeout_secs: Option<u64>,
) -> Result<(), ConfigError> {
    let mut document = read_document(path)?;
    let root = document
        .as_table_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("root must be a table".to_string()))?;
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("mcp must be a table".to_string()))?;
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("mcp.servers must be an array".to_string()))?;

    if servers
        .iter()
        .any(|server| server.get("name").and_then(toml::Value::as_str) == Some(name))
    {
        return Err(ConfigError::McpExists(name.to_string()));
    }

    let mut server = toml::Table::new();
    server.insert("name".to_string(), toml::Value::String(name.to_string()));
    server.insert(
        "command".to_string(),
        toml::Value::String(command.to_string()),
    );
    if !args.is_empty() {
        server.insert(
            "args".to_string(),
            toml::Value::Array(
                args.iter()
                    .map(|arg| toml::Value::String(arg.clone()))
                    .collect(),
            ),
        );
    }
    if !enabled {
        server.insert("enabled".to_string(), toml::Value::Boolean(false));
    }
    if let Some(seconds) = timeout_secs {
        server.insert(
            "timeout_secs".to_string(),
            toml::Value::Integer(seconds as i64),
        );
    }
    servers.push(toml::Value::Table(server));

    validate_document(&document)?;
    write_document(path, &document)
}

/// Remove `[[mcp.servers]]` by name.
pub fn mcp_remove(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut document = read_document(path)?;
    let servers = document
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(toml::Value::as_array_mut)
        .ok_or_else(|| ConfigError::McpMissing(name.to_string()))?;
    let before = servers.len();
    servers.retain(|server| server.get("name").and_then(toml::Value::as_str) != Some(name));
    if servers.len() == before {
        return Err(ConfigError::McpMissing(name.to_string()));
    }
    validate_document(&document)?;
    write_document(path, &document)
}

#[derive(Debug, Default)]
pub struct Overrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub stream_usage: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    /// `env NAME`, `command NAME`, or `keyless`, for doctor output.
    pub api_key_source: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub stream_usage: bool,
    pub is_mock: bool,
    /// Provider-specific body fields, already merged with the builtin defaults.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

/// Default models of the builtin providers, for forms that need to tell a
/// stale value (left over from another provider) apart from a real choice.
pub fn builtin_models() -> Vec<(&'static str, &'static str)> {
    ["openai", "deepseek", "ollama", "anthropic", "minimax"]
        .into_iter()
        .filter_map(|name| builtin(name).map(|b| (name, b.default_model.unwrap_or(""))))
        .filter(|(_, model)| !model.is_empty())
        .collect()
}
struct Builtin {
    base_url: &'static str,
    api_key_env: &'static str,
    default_model: Option<&'static str>,
    /// MiniMax ships its thinking inside `content` unless asked to split it out.
    reasoning_split: bool,
}

fn builtin(name: &str) -> Option<Builtin> {
    match name {
        "openai" => Some(Builtin {
            base_url: "https://api.openai.com/v1",
            api_key_env: "OPENAI_API_KEY",
            default_model: None,
            reasoning_split: false,
        }),
        "deepseek" => Some(Builtin {
            base_url: "https://api.deepseek.com/v1",
            api_key_env: "DEEPSEEK_API_KEY",
            default_model: Some("deepseek-flash"),
            reasoning_split: false,
        }),
        "ollama" => Some(Builtin {
            base_url: "http://localhost:11434/v1",
            api_key_env: "",
            default_model: None,
            reasoning_split: false,
        }),
        // MiniMax exposes an OpenAI-compatible Chat Completions API, so it needs
        // no new provider code, only the endpoint and key variable.
        // MiniMax exposes an OpenAI-compatible Chat Completions API, so it needs
        // no new provider code, only the endpoint and key variable. The default
        // host is the mainland platform; international accounts override
        // `base_url` to https://api.minimax.io/v1.
        "minimax" => Some(Builtin {
            base_url: "https://api.minimaxi.com/v1",
            api_key_env: "MINIMAX_API_KEY",
            default_model: Some("MiniMax-M3"),
            reasoning_split: true,
        }),
        // Anthropic Messages API. `claude` is accepted as a friendlier alias;
        // both resolve to the same provider and key.
        "anthropic" | "claude" => Some(Builtin {
            base_url: "https://api.anthropic.com",
            api_key_env: "ANTHROPIC_API_KEY",
            default_model: Some("claude-sonnet-4-5"),
            reasoning_split: false,
        }),
        _ => None,
    }
}

/// Resolve the price table for a model and validate the budget pair.
///
/// `--max-cost` without a price entry is a hard error: silently treating the
/// cost as zero would make the flag a no-op.
pub fn pricing_for(
    config: &Config,
    model: &str,
    max_cost: Option<f64>,
) -> Result<Option<Pricing>, ConfigError> {
    if let Some(limit) = max_cost
        && !(limit.is_finite() && limit >= 0.0)
    {
        return Err(ConfigError::InvalidMaxCost);
    }

    match config.pricing.get(model) {
        Some(pricing) => {
            if !pricing.validate() {
                return Err(ConfigError::InvalidPricing {
                    model: model.to_string(),
                });
            }
            Ok(Some(*pricing))
        }
        None if max_cost.is_some() => Err(ConfigError::PricingRequired {
            model: model.to_string(),
        }),
        None => Ok(None),
    }
}

pub fn resolve(
    config: &Config,
    overrides: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Resolved, ConfigError> {
    let name = overrides
        .provider
        .clone()
        .or_else(|| env("QCODE_PROVIDER"))
        .or_else(|| config.default_provider.clone())
        .unwrap_or_else(|| "mock".to_string());

    if name == "mock"
        || name == "mock-tools"
        || name == "mock-approval"
        || name == "mock-todo"
        || name == "mock-subagent"
    {
        let model = overrides
            .model
            .clone()
            .or_else(|| env("QCODE_MODEL"))
            .unwrap_or_else(|| "mock-1".to_string());
        return Ok(Resolved {
            name,
            api_key_source: "mock".to_string(),
            base_url: None,
            api_key: None,
            model,
            stream_usage: false,
            is_mock: true,
            extra_body: serde_json::Map::new(),
        });
    }

    let entry = config.providers.get(&name).cloned().unwrap_or_default();
    let builtin = builtin(&name);
    // Builtin defaults first, then whatever the config says: a user can always
    // turn a field off.
    let mut extra_body = serde_json::Map::new();
    if let Some(builtin) = &builtin
        && builtin.reasoning_split
    {
        extra_body.insert("reasoning_split".to_string(), serde_json::json!(true));
    }
    for (key, value) in &entry.extra_body {
        extra_body.insert(key.clone(), value.clone());
    }

    let base_url = overrides
        .base_url
        .clone()
        .or_else(|| env("QCODE_BASE_URL"))
        .or_else(|| entry.base_url.clone())
        .or_else(|| builtin.as_ref().map(|builtin| builtin.base_url.to_string()))
        .ok_or_else(|| ConfigError::UnknownProvider(name.clone()))?;

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ConfigError::InvalidBaseUrl {
            provider: name,
            url: base_url,
        });
    }

    let api_key_env = entry
        .api_key_env
        .clone()
        .or_else(|| {
            builtin
                .as_ref()
                .map(|builtin| builtin.api_key_env.to_string())
        })
        .unwrap_or_default();

    let command = entry.api_key_command.as_deref();
    let (api_key, api_key_source) = if api_key_env.is_empty() {
        match command {
            Some(argv) => (Some(key_from_command(&name, argv)?), command_source(argv)),
            None => (None, "keyless".to_string()),
        }
    } else {
        match env(&api_key_env) {
            Some(key) if !key.is_empty() => (Some(key), format!("env {api_key_env}")),
            _ => match command {
                Some(argv) => (Some(key_from_command(&name, argv)?), command_source(argv)),
                None => {
                    return Err(ConfigError::MissingKey {
                        provider: name,
                        env: api_key_env,
                    });
                }
            },
        }
    };

    let model = overrides
        .model
        .clone()
        .or_else(|| env("QCODE_MODEL"))
        .or_else(|| entry.default_model.clone())
        .or_else(|| {
            builtin
                .as_ref()
                .and_then(|builtin| builtin.default_model.map(str::to_string))
        })
        .ok_or_else(|| ConfigError::MissingModel {
            provider: name.clone(),
        })?;

    Ok(Resolved {
        name,
        api_key_source,
        base_url: Some(base_url),
        api_key,
        model,
        stream_usage: overrides.stream_usage.unwrap_or(entry.stream_usage),
        is_mock: false,
        extra_body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn sandbox_mode_parses_and_defaults_to_none() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.sandbox.mode, SandboxMode::None);
        assert!(!config.sandbox.network);

        let config: Config = toml::from_str(
            r#"
            [sandbox]
            mode = "sandbox-exec"
            network = true
            "#,
        )
        .unwrap();
        assert_eq!(config.sandbox.mode, SandboxMode::SandboxExec);
        assert!(config.sandbox.network);

        let config: Config = toml::from_str("[sandbox]\nmode = \"bwrap\"\n").unwrap();
        assert_eq!(config.sandbox.mode, SandboxMode::Bwrap);

        let config: Config = toml::from_str("[sandbox]\nmode = \"landlock\"\n").unwrap();
        assert_eq!(config.sandbox.mode, SandboxMode::Landlock);

        let error = toml::from_str::<Config>("[sandbox]\nmode = \"seccomp\"\n").unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }

    #[test]
    fn loads_a_standalone_policy_file() {
        use qcode_core::policy::Action;

        let dir = std::env::temp_dir().join(format!("qcode-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("policy.toml");
        std::fs::write(
            &path,
            r#"
            [[rules]]
            id = "allow-shell"
            tool = "run_shell"
            action = "allow"
            "#,
        )
        .unwrap();

        let rules = load_policy(&path).expect("load policy");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "allow-shell");
        assert_eq!(rules[0].tool.as_deref(), Some("run_shell"));
        assert_eq!(rules[0].action, Action::Allow);
    }

    #[test]
    fn policy_file_errors_name_the_path() {
        let dir = std::env::temp_dir().join(format!("qcode-policy-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.toml");
        match load_policy(&missing) {
            Err(ConfigError::ReadPolicy { path, .. }) => assert_eq!(path, missing),
            other => panic!("unexpected result: {other:?}"),
        }

        let invalid = dir.join("invalid.toml");
        std::fs::write(&invalid, "rules = 1").unwrap();
        match load_policy(&invalid) {
            Err(ConfigError::ParsePolicy { path, .. }) => assert_eq!(path, invalid),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn defaults_to_mock() {
        let resolved = resolve(&Config::default(), &Overrides::default(), &env_from(&[])).unwrap();
        assert!(resolved.is_mock);
        assert_eq!(resolved.name, "mock");
        assert_eq!(resolved.model, "mock-1");
    }

    #[test]
    fn cli_beats_env_beats_config() {
        let config: Config = toml::from_str(
            r#"
            default_provider = "openai"

            [providers.openai]
            default_model = "config-model"
            "#,
        )
        .unwrap();
        let overrides = Overrides {
            model: Some("cli-model".to_string()),
            ..Overrides::default()
        };
        let resolved = resolve(
            &config,
            &overrides,
            &env_from(&[("OPENAI_API_KEY", "sk-test"), ("QCODE_MODEL", "env-model")]),
        )
        .unwrap();

        assert_eq!(resolved.name, "openai");
        assert_eq!(resolved.model, "cli-model");
        assert_eq!(resolved.api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn env_model_used_when_cli_is_silent() {
        let config: Config = toml::from_str(
            r#"
            default_provider = "deepseek"

            [providers.deepseek]
            default_model = "config-model"
            "#,
        )
        .unwrap();
        let resolved = resolve(
            &config,
            &Overrides::default(),
            &env_from(&[
                ("DEEPSEEK_API_KEY", "sk-test"),
                ("QCODE_MODEL", "env-model"),
            ]),
        )
        .unwrap();
        assert_eq!(resolved.model, "env-model");
    }

    #[test]
    fn deepseek_defaults_to_flash() {
        let config: Config = toml::from_str(r#"default_provider = "deepseek""#).unwrap();
        let resolved = resolve(
            &config,
            &Overrides::default(),
            &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
        )
        .unwrap();
        assert_eq!(resolved.model, "deepseek-flash");
    }

    #[test]
    fn anthropic_defaults_and_claude_alias() {
        let config: Config = toml::from_str(r#"default_provider = "anthropic""#).unwrap();
        let resolved = resolve(
            &config,
            &Overrides::default(),
            &env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test")]),
        )
        .unwrap();
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(resolved.model, "claude-sonnet-4-5");
        assert_eq!(resolved.api_key.as_deref(), Some("sk-ant-test"));

        let alias: Config = toml::from_str(r#"default_provider = "claude""#).unwrap();
        let resolved = resolve(
            &alias,
            &Overrides::default(),
            &env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test")]),
        )
        .unwrap();
        assert_eq!(resolved.name, "claude");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
    }

    #[test]
    fn explicit_model_overrides_deepseek_default() {
        let config = Config::default();
        let overrides = Overrides {
            provider: Some("deepseek".to_string()),
            model: Some("custom-model".to_string()),
            ..Overrides::default()
        };
        let resolved = resolve(
            &config,
            &overrides,
            &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
        )
        .unwrap();
        assert_eq!(resolved.model, "custom-model");
    }

    #[test]
    fn missing_key_is_actionable() {
        let config: Config = toml::from_str(r#"default_provider = "deepseek""#).unwrap();
        let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
        assert!(matches!(error, ConfigError::MissingKey { .. }));
        assert!(error.to_string().contains("DEEPSEEK_API_KEY"));
    }

    #[test]
    fn the_key_comes_from_the_environment_before_the_command() {
        let config: Config = toml::from_str(
            r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "echo from-command"]
"#,
        )
        .unwrap();
        let resolved = resolve(
            &config,
            &Overrides::default(),
            &env_from(&[("DS_KEY", "from-env")]),
        )
        .unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("from-env"));
        assert_eq!(resolved.api_key_source, "env DS_KEY");
    }

    #[test]
    fn a_missing_environment_variable_falls_back_to_the_command() {
        let config: Config = toml::from_str(
            r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "echo from-command"]
"#,
        )
        .unwrap();
        if cfg!(windows) {
            let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
            assert_eq!(resolved.api_key.as_deref(), Some("from-command"));
            assert_eq!(resolved.api_key_source, "command cmd");
        }
    }

    #[test]
    fn a_failing_key_command_reports_the_reason_without_its_output() {
        if !cfg!(windows) {
            return;
        }
        let config: Config = toml::from_str(
            r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "exit 3"]
"#,
        )
        .unwrap();
        let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("key command failed"), "{text}");
        assert!(text.contains("exited with"), "{text}");
    }
    #[test]
    fn keyless_provider_needs_no_key() {
        let config: Config = toml::from_str(
            r#"
            default_provider = "ollama"

            [providers.ollama]
            default_model = "qwen"
            "#,
        )
        .unwrap();
        let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
        assert!(resolved.api_key.is_none());
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn unknown_provider_without_base_url_fails() {
        let config: Config = toml::from_str(r#"default_provider = "mystery""#).unwrap();
        let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
        assert!(matches!(error, ConfigError::UnknownProvider(_)));
    }

    #[test]
    fn context_window_defaults_and_overrides() {
        let config = Config::default();
        assert_eq!(config.context.window_tokens, 128_000);

        let config: Config = toml::from_str("[context]\nwindow_tokens = 32000\n").unwrap();
        assert_eq!(config.context.window_tokens, 32_000);
    }

    #[test]
    fn the_turn_budget_defaults_to_eight_and_can_be_raised() {
        let config = Config::default();
        assert_eq!(config.agent.max_turns, 64);
        assert_eq!(config.agent.max_cost_usd, None);
        assert!(
            config.agent.snapshot_shell,
            "workspace snapshots are on by default"
        );

        let config: Config = toml::from_str(
            "[agent]\nmax_turns = 32\nmax_cost_usd = 0.25\nsnapshot_shell = false\n",
        )
        .unwrap();
        assert_eq!(config.agent.max_turns, 32);
        assert_eq!(config.agent.max_cost_usd, Some(0.25));
        assert!(!config.agent.snapshot_shell);
    }

    #[test]
    fn pricing_table_parses_and_resolves() {
        let config: Config = toml::from_str(
            r#"
            [pricing."deepseek-flash"]
            input_usd_per_mtok = 0.28
            output_usd_per_mtok = 0.42
            cache_read_usd_per_mtok = 0.028
            "#,
        )
        .unwrap();
        let pricing = pricing_for(&config, "deepseek-flash", Some(1.0))
            .unwrap()
            .expect("pricing entry");
        assert_eq!(pricing.input_usd_per_mtok, 0.28);
        assert_eq!(pricing.output_usd_per_mtok, 0.42);
        assert_eq!(pricing.cache_read_usd_per_mtok, 0.028);
        assert_eq!(pricing.cache_write_usd_per_mtok, 0.0);
    }

    #[test]
    fn max_cost_without_pricing_is_an_error() {
        let error = pricing_for(&Config::default(), "mystery", Some(0.5)).unwrap_err();
        assert!(matches!(error, ConfigError::PricingRequired { .. }));
        assert!(error.to_string().contains("[pricing.\"mystery\"]"));
    }

    #[test]
    fn no_budget_needs_no_pricing() {
        assert!(
            pricing_for(&Config::default(), "mystery", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_budget_or_rates_are_rejected() {
        assert!(matches!(
            pricing_for(&Config::default(), "m", Some(-1.0)),
            Err(ConfigError::InvalidMaxCost)
        ));
        assert!(matches!(
            pricing_for(&Config::default(), "m", Some(f64::NAN)),
            Err(ConfigError::InvalidMaxCost)
        ));

        let config: Config = toml::from_str(
            r#"
            [pricing.m]
            input_usd_per_mtok = -0.1
            output_usd_per_mtok = 1.0
            "#,
        )
        .unwrap();
        assert!(matches!(
            pricing_for(&config, "m", Some(1.0)),
            Err(ConfigError::InvalidPricing { .. })
        ));
    }

    #[test]
    fn skills_are_enabled_by_default_and_can_be_disabled() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.skills.enabled);
        assert!(config.skills.roots.is_empty());

        let config: Config = toml::from_str(
            "[skills]\nenabled = false\nroots = [\"/tmp/skills\", \"~/space/skills\"]\n",
        )
        .unwrap();
        assert!(!config.skills.enabled);
        assert_eq!(config.skills.roots.len(), 2);
    }

    #[test]
    fn mcp_servers_parse_with_defaults() {
        let config: Config = toml::from_str(
            r#"
            [[mcp.servers]]
            name = "filesystem"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
            "#,
        )
        .unwrap();
        config.mcp.validate().unwrap();

        assert_eq!(config.mcp.servers.len(), 1);
        let server = &config.mcp.servers[0];
        assert_eq!(server.name, "filesystem");
        assert_eq!(server.args.len(), 3);
        assert!(server.enabled);
        assert_eq!(server.timeout_secs, 120);
    }

    #[test]
    fn duplicate_mcp_server_names_are_rejected() {
        let config: Config = toml::from_str(
            r#"
            [[mcp.servers]]
            name = "fs"
            command = "a"

            [[mcp.servers]]
            name = "fs"
            command = "b"
            "#,
        )
        .unwrap();
        assert!(matches!(
            config.mcp.validate(),
            Err(McpConfigError::DuplicateName(_))
        ));
    }

    #[test]
    fn model_override_works_for_unknown_provider() {
        let config = Config::default();
        let overrides = Overrides {
            provider: Some("local".to_string()),
            model: Some("m".to_string()),
            base_url: Some("http://127.0.0.1:1234/v1".to_string()),
            stream_usage: None,
        };
        let resolved = resolve(&config, &overrides, &env_from(&[])).unwrap();
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("http://127.0.0.1:1234/v1")
        );
        assert!(resolved.api_key.is_none());
    }

    #[test]
    fn project_rules_are_scoped_and_cannot_allow() {
        let mut base: Config = toml::from_str(
            r#"
            [[approval.rules]]
            id = "user-allow"
            tool = "run_shell"
            action = "allow"
            "#,
        )
        .unwrap();
        let project: Config = toml::from_str(
            r#"
            [[approval.rules]]
            id = "proj-allow"
            tool = "read_file"
            action = "allow"

            [[approval.rules]]
            id = "proj-deny"
            tool = "run_shell"
            command_prefix = "git push"
            action = "deny"
            "#,
        )
        .unwrap();

        merge_project(&mut base, project, ProjectSections::default());
        assert_eq!(base.approval.rules.len(), 3);
        let proj_allow = base
            .approval
            .rules
            .iter()
            .find(|rule| rule.id == "proj-allow")
            .expect("project allow");
        assert_eq!(proj_allow.action, Action::Ask);
        assert_eq!(proj_allow.scope, Some(Scope::Project));
        let proj_deny = base
            .approval
            .rules
            .iter()
            .find(|rule| rule.id == "proj-deny")
            .expect("project deny");
        assert_eq!(proj_deny.action, Action::Deny);
    }

    #[test]
    fn project_config_loads_from_the_workspace() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".qcode")).unwrap();
        std::fs::write(
            dir.path().join(".qcode/config.toml"),
            r#"
            [[approval.rules]]
            id = "repo-deny"
            tool = "run_shell"
            action = "deny"
            "#,
        )
        .unwrap();

        let config = load_project(dir.path()).expect("project config");
        assert_eq!(config.approval.rules.len(), 1);
        assert_eq!(config.approval.rules[0].scope, Some(Scope::Project));
    }

    #[test]
    fn network_defaults_and_overrides() {
        let defaults: Config = toml::from_str("").unwrap();
        assert_eq!(defaults.network.mode, NetworkMode::Proxy);
        assert!(defaults.network.enabled);
        assert!(defaults.network.audit);
        assert_eq!(defaults.network.unknown, UnknownHost::Ask);
        assert!(!defaults.network.allow_private);

        let config: Config = toml::from_str(
            r#"
            [network]
            mode = "strict"
            enabled = false
            allow = ["api.example.com", "*.internal.test"]
            allow_private = true
            unknown = "deny"
            audit = false
            "#,
        )
        .unwrap();
        assert_eq!(config.network.mode, NetworkMode::Strict);
        assert!(!config.network.enabled);
        assert_eq!(config.network.allow.len(), 2);
        assert!(config.network.allow_private);
        assert_eq!(config.network.unknown, UnknownHost::Deny);
        assert!(!config.network.audit);
    }

    #[test]
    fn search_defaults_and_overrides() {
        let defaults: Config = toml::from_str("").unwrap();
        assert!(defaults.search.enabled);
        assert_eq!(defaults.search.backend, SearchBackend::Brave);
        assert_eq!(defaults.search.max_results, 5);
        assert_eq!(
            defaults.search.key_env_name().as_deref(),
            Some("BRAVE_API_KEY")
        );

        let config: Config = toml::from_str(
            r#"
            [search]
            enabled = false
            backend = "searxng"
            max_results = 100
            base_url = "http://localhost:8888/"
            "#,
        )
        .unwrap();
        assert!(!config.search.enabled);
        assert_eq!(config.search.backend, SearchBackend::Searxng);
        assert_eq!(config.search.key_env_name(), None);
        assert_eq!(config.search.max_results, 100);
        let settings = config.search.settings(&|_| None);
        assert_eq!(settings.max_results, 20);
        assert_eq!(settings.base_url.as_deref(), Some("http://localhost:8888/"));
        assert!(settings.is_usable());
    }

    #[test]
    fn search_resolves_the_configured_key_variable() {
        let config: Config = toml::from_str(
            r#"
            [search]
            backend = "tavily"
            api_key_env = "MY_SEARCH_KEY"
            "#,
        )
        .unwrap();
        assert!(!config.search.settings(&|_| None).is_usable());
        let settings = config
            .search
            .settings(&|name| (name == "MY_SEARCH_KEY").then(|| "tvly-x".to_string()));
        assert!(settings.is_usable());
        assert_eq!(settings.api_key.as_deref(), Some("tvly-x"));
    }

    #[test]
    fn project_search_section_replaces_only_when_present() {
        let base = Config {
            search: SearchConfig {
                base_url: Some("https://api.search.brave.com".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let project = Config {
            search: SearchConfig {
                backend: SearchBackend::Searxng,
                base_url: Some("http://host.docker.internal:8888".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut merged = base.clone();
        merge_project(
            &mut merged,
            project.clone(),
            ProjectSections {
                context: false,
                sandbox: false,
                search: false,
            },
        );
        assert_eq!(merged.search.backend, SearchBackend::Brave);

        merge_project(
            &mut merged,
            project,
            ProjectSections {
                context: false,
                sandbox: false,
                search: true,
            },
        );
        assert_eq!(merged.search.backend, SearchBackend::Searxng);
        assert_eq!(
            merged.search.base_url.as_deref(),
            Some("http://host.docker.internal:8888")
        );
    }

    #[test]
    fn mcp_add_and_remove_edit_the_config_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");

        mcp_add(
            &path,
            "filesystem",
            "npx",
            &["-y".to_string(), "server".to_string()],
            false,
            Some(30),
        )
        .expect("add");
        let config = load(&path).expect("load");
        assert_eq!(config.mcp.servers.len(), 1);
        let server = &config.mcp.servers[0];
        assert_eq!(server.name, "filesystem");
        assert_eq!(server.args, vec!["-y", "server"]);
        assert!(!server.enabled);
        assert_eq!(server.timeout_secs, 30);

        assert!(matches!(
            mcp_add(&path, "filesystem", "npx", &[], true, None),
            Err(ConfigError::McpExists(_))
        ));

        mcp_remove(&path, "filesystem").expect("remove");
        let config = load(&path).expect("load after remove");
        assert!(config.mcp.servers.is_empty());
        assert!(matches!(
            mcp_remove(&path, "filesystem"),
            Err(ConfigError::McpMissing(_))
        ));
    }

    #[test]
    fn project_context_and_sandbox_only_override_when_present() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base_path = dir.path().join("config.toml");
        std::fs::write(
            &base_path,
            "[context]\nwindow_tokens = 100000\n[sandbox]\nnetwork = false\n",
        )
        .unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join(".qcode")).unwrap();

        std::fs::write(
            workspace.join(".qcode/config.toml"),
            "[context]\nwindow_tokens = 32000\n[sandbox]\nnetwork = true\n",
        )
        .unwrap();
        let merged = load_merged(&base_path, &workspace).expect("merged");
        assert_eq!(merged.context.window_tokens, 32_000);
        assert!(merged.sandbox.network);

        // A project file without these sections must not reset the user values.
        std::fs::write(workspace.join(".qcode/config.toml"), "\n").unwrap();
        let merged = load_merged(&base_path, &workspace).expect("merged without sections");
        assert_eq!(merged.context.window_tokens, 100_000);
        assert!(!merged.sandbox.network);
    }

    #[test]
    fn a_config_without_a_version_is_the_current_schema() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(Config::default().version, CONFIG_VERSION);
    }

    #[test]
    fn a_newer_config_version_is_rejected_with_the_supported_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 99\n").unwrap();

        let error = load(&path).expect_err("newer version").to_string();
        assert!(error.contains("declares version 99"), "{error}");
        assert!(
            error.contains(&format!("up to {CONFIG_VERSION}")),
            "{error}"
        );
        assert!(error.contains("upgrade qcode"), "{error}");
    }

    #[test]
    fn a_zero_config_version_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 0\n").unwrap();

        let error = load(&path).expect_err("zero version").to_string();
        assert!(error.contains("version must be 1"), "{error}");
    }

    #[test]
    fn a_project_config_cannot_declare_a_newer_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".qcode")).unwrap();
        std::fs::write(dir.path().join(".qcode/config.toml"), "version = 7\n").unwrap();

        let error = load_merged(&path, dir.path())
            .expect_err("newer project version")
            .to_string();
        assert!(error.contains("declares version 7"), "{error}");
    }

    #[test]
    fn typos_are_reported_as_unknown_keys_but_still_load() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "defualt_provider = \"mock\"\n[search]\nbakend = \"tavily\"\n",
        )
        .unwrap();

        // Serde ignores both keys, so the file loads; the typo list is what a
        // caller reports. Unknown keys must never be fatal: a newer qcode may
        // add optional keys that this build simply does not read.
        load(&path).expect("unknown keys are not fatal");
        let document: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let mut unknown = unknown_keys(&document);
        unknown.sort();
        assert_eq!(unknown, vec!["defualt_provider", "search.bakend"]);
    }

    #[test]
    fn every_section_key_the_build_reads_is_known() {
        let document: toml::Table = r#"
             version = 1
             default_provider = "mock"

             [approval]
             rules = []

             [context]
             window_tokens = 64000

             [agent]
             max_turns = 24
             max_cost_usd = 0.50

             [skills]
             enabled = true
             roots = []

             [sandbox]
             mode = "none"
             network = false
             extra_writable = []

             [network]
             enabled = true
             allow = []
             allow_private = false
             unknown = "ask"
             audit = true

             [search]
             enabled = true
             backend = "brave"
             api_key_env = "BRAVE_API_KEY"
             max_results = 5
             base_url = "https://api.search.brave.com"

             [update]
             enabled = true
             check = false
             check_days = 7
             repo = "qiuzhanghua/qcode"
             base_url = "https://api.github.com"

             [providers.ollama]
             base_url = "http://127.0.0.1:11434/v1"

             [[mcp.servers]]
             name = "filesystem"
             command = "npx"
        "#
        .parse()
        .expect("document");

        assert!(
            unknown_keys(&document).is_empty(),
            "{:?}",
            unknown_keys(&document)
        );
    }
    #[test]
    fn minimax_defaults_to_the_m3_model() {
        let config: Config = toml::from_str(r#"default_provider = "minimax""#).unwrap();
        let resolved = resolve(
            &config,
            &Overrides::default(),
            &env_from(&[("MINIMAX_API_KEY", "sk-test")]),
        )
        .expect("resolve");
        assert_eq!(resolved.name, "minimax");
        assert_eq!(resolved.model, "MiniMax-M3");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://api.minimaxi.com/v1")
        );
        assert_eq!(resolved.api_key.as_deref(), Some("sk-test"));
    }
    #[test]
    fn a_plain_directory_is_not_a_system_install() {
        assert!(!is_system_install(Path::new("relative/tools/qcode")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_system_directories_are_never_portable() {
        assert!(is_system_install(Path::new("/usr/local/bin")));
        assert!(is_system_install(Path::new("/opt/qcode")));
        assert!(!is_system_install(Path::new("/home/q/tools/qcode")));
    }

    #[test]
    fn the_user_config_honours_xdg_config_home() {
        // The resolver reads the env, so this checks the branch shape rather
        // than the value: an explicit path always wins over any env var.
        let explicit = PathBuf::from("/tmp/explicit.toml");
        let found = discover(Some(explicit.clone())).expect("discover");
        assert_eq!(found.path, explicit);
        assert_eq!(found.source, ConfigSource::Flag);
    }
}
