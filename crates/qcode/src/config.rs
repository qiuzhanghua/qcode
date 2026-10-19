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

pub const DEFAULT_CONFIG_RELATIVE: &str = ".config/qcode/config.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
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
    /// stdio MCP servers. Disabled entries are kept for quick toggling.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Agent Skills discovered from the user and project config trees.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// OS sandbox for `run_shell`. Defaults to `none` (approval is the boundary).
    #[serde(default)]
    pub sandbox: SandboxConfig,
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Allow outbound/inbound network inside the sandbox. Default: deny.
    #[serde(default)]
    pub network: bool,
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
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the key. Empty means keyless.
    #[serde(default)]
    pub api_key_env: Option<String>,
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
            default_model: None,
            stream_usage: true,
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
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    // Fail before spawning anything if the MCP section is malformed.
    config.mcp.validate()?;
    Ok(config)
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
    let project = load(&path)?;
    merge_project(base, project);
    base.mcp.validate()?;
    Ok(())
}

/// Merge a project config over a user/explicit one.
///
/// Project approval rules are forced to `Scope::Project` and may only tighten:
/// an `allow` becomes `ask`, because a repository must not grant itself more
/// permission than the user configured.
pub fn merge_project(base: &mut Config, project: Config) {
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

pub fn default_path() -> Result<PathBuf, ConfigError> {
    let home = std::env::var("HOME").map_err(|_| ConfigError::NoHome)?;
    Ok(PathBuf::from(home).join(DEFAULT_CONFIG_RELATIVE))
}

/// The file `qc config edit` / `qc mcp add|remove` should modify.
pub fn edit_path(explicit: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Ok(path) = std::env::var("QCODE_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    default_path()
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
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub stream_usage: bool,
    pub is_mock: bool,
}

struct Builtin {
    base_url: &'static str,
    api_key_env: &'static str,
    default_model: Option<&'static str>,
}

fn builtin(name: &str) -> Option<Builtin> {
    match name {
        "openai" => Some(Builtin {
            base_url: "https://api.openai.com/v1",
            api_key_env: "OPENAI_API_KEY",
            default_model: None,
        }),
        "deepseek" => Some(Builtin {
            base_url: "https://api.deepseek.com/v1",
            api_key_env: "DEEPSEEK_API_KEY",
            default_model: Some("deepseek-flash"),
        }),
        "ollama" => Some(Builtin {
            base_url: "http://localhost:11434/v1",
            api_key_env: "",
            default_model: None,
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
            base_url: None,
            api_key: None,
            model,
            stream_usage: false,
            is_mock: true,
        });
    }

    let entry = config.providers.get(&name).cloned().unwrap_or_default();
    let builtin = builtin(&name);

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

    let api_key = if api_key_env.is_empty() {
        None
    } else {
        match env(&api_key_env) {
            Some(key) if !key.is_empty() => Some(key),
            _ => {
                return Err(ConfigError::MissingKey {
                    provider: name,
                    env: api_key_env,
                });
            }
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
        base_url: Some(base_url),
        api_key,
        model,
        stream_usage: overrides.stream_usage.unwrap_or(entry.stream_usage),
        is_mock: false,
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

        merge_project(&mut base, project);
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
}
