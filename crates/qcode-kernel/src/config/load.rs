use super::*;

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

/// Whether the directory accepts a new file. A policy can allow rewriting an
/// existing config while denying new files beside it, which breaks the atomic
/// save, so callers probe instead of guessing from permissions.
pub fn dir_accepts_new_files(dir: &Path) -> bool {
    let probe = dir.join(".qcode-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The advice to show when the config directory refuses new files.
pub const CONFIG_DIR_HINT: &str = "this directory refuses new files; either allow qc in your AV/EDR policy, set QCODE_CONFIG \
     to a writable path, or create a `qcode.portable` file next to qc.exe to keep the config there";

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
pub(crate) fn is_system_install(dir: &Path) -> bool {
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

pub(crate) fn read_document(path: &Path) -> Result<toml::Value, ConfigError> {
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

pub(crate) fn write_document(path: &Path, document: &toml::Value) -> Result<(), ConfigError> {
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

pub(crate) fn validate_document(document: &toml::Value) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(document)
        .map_err(|err| ConfigError::InvalidDocument(err.to_string()))?;
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: PathBuf::from("<edit>"),
        source,
    })?;
    config.mcp.validate()?;
    Ok(())
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

pub fn known_keys() -> &'static [(&'static str, &'static [&'static str])] {
    KNOWN_KEYS
}
