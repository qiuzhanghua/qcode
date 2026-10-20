//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;

/// Kept here so configuration stops reaching into the binary crate.
pub const DEFAULT_REPO: &str = "qiuzhanghua/qcode";

use std::path::Path;

use qcode_core::event::Scope;
use qcode_core::policy::Policy;

/// Turn configuration into the approval policy for one invocation.
///
/// A policy file is scoped to the invocation, so its rules always carry
/// [`Scope::Session`]; config rules keep their own scope.
pub fn build_policy(
    loaded: &config::Config,
    explicit: Option<&Path>,
) -> Result<Policy, Box<dyn std::error::Error>> {
    let rules = match explicit {
        Some(path) => {
            if !path.exists() {
                return Err(format!("approval policy not found: {}", path.display()).into());
            }
            tracing::info!(path = %path.display(), "loaded approval policy file");
            let mut rules = config::load_policy(path)?;
            for rule in &mut rules {
                rule.scope = Some(Scope::Session);
            }
            rules
        }
        None => loaded.approval.rules.clone(),
    };
    Ok(Policy::new(rules))
}

use std::sync::Arc;

use qcode_net::{Egress, EgressOptions, UnknownPolicy};

/// Start the loopback egress proxy when `[network] enabled` is set.
///
/// The provider's own host is added to the allowlist so the model endpoint works
/// without a separate entry; `unknown` maps "ask" onto deny because an egress
/// proxy cannot ask a human.
pub async fn build_egress(
    loaded: &config::Config,
    data_dir: &Path,
    base_url: Option<&str>,
) -> Result<Option<Arc<Egress>>, Box<dyn std::error::Error>> {
    if !loaded.network.enabled {
        return Ok(None);
    }
    let mut allow = loaded.network.allow.clone();
    if let Some(base_url) = base_url
        && let Ok(url) = reqwest::Url::parse(base_url)
        && let Some(host) = url.host_str()
    {
        let port = url.port_or_known_default().unwrap_or(443);
        allow.push(format!("{host}:{port}"));
    }
    let unknown = match loaded.network.unknown {
        config::UnknownHost::Allow => UnknownPolicy::Allow,
        config::UnknownHost::Ask | config::UnknownHost::Deny => UnknownPolicy::Deny,
    };
    let audit_path = loaded
        .network
        .audit
        .then(|| data_dir.join("logs").join("network.jsonl"));
    let egress = Egress::start(EgressOptions {
        allow,
        allow_private: loaded.network.allow_private,
        unknown,
        audit_path,
    })
    .await?;
    tracing::info!(port = egress.port(), "egress proxy started");
    Ok(Some(Arc::new(egress)))
}

/// The assembled kernel: everything a frontend needs, built once.
///
/// ADR-0036: the pieces are assembled in one place so the TUI, ACP and GUI
/// cannot drift apart. `egress` is kept here so the proxy lives exactly as long
/// as the kernel does.
pub struct Kernel {
    pub config: config::Config,
    pub resolved: config::Resolved,
    pub egress: Option<Arc<Egress>>,
}

use std::path::PathBuf;

use qcode_core::tool::{Tool, ToolRegistry};
use qcode_mcp::McpConfig;
use qcode_tools::builtin_registry_with;
use qcode_tools::sandbox::Sandbox;
use qcode_tools::skills::{SkillSet, SkillSource, SkillTool};
use qcode_tools::web_search::WebSearch;
pub async fn build_tools(
    mcp: &McpConfig,
    search: &config::SearchConfig,
    workspace: &Path,
    data_dir: &Path,
    skills: &Arc<SkillSet>,
    sandbox: Arc<dyn Sandbox>,
    network_env: Option<Vec<(String, String)>>,
) -> ToolRegistry {
    let confine_sandbox = sandbox.clone();
    let mut tools = builtin_registry_with(sandbox);
    if search.enabled {
        let settings = search.settings(&|key| std::env::var(key).ok());
        if settings.is_usable() {
            tracing::info!(backend = settings.backend.name(), "registered web_search");
            tools.extend([Arc::new(WebSearch::new(settings)) as Arc<dyn Tool>]);
        } else {
            tracing::warn!(
                backend = settings.backend.name(),
                "web_search is enabled but not configured; the tool is not registered"
            );
        }
    }
    // MCP servers get the same write boundary as `run_shell`. They are started
    // through the sandbox's command builder instead of `Command::new`, so a
    // server cannot write outside the workspace either.
    let confine: Box<qcode_mcp::Confine> = Box::new(move |program, args, cwd| {
        confine_sandbox
            .command(program, args, cwd)
            .map_err(|error| error.to_string())
    });
    let mcp_tools = qcode_mcp::connect_all_with(
        &mcp.servers,
        workspace,
        &data_dir.join("logs"),
        network_env.as_deref().unwrap_or(&[]),
        Some(confine.as_ref()),
    )
    .await;
    if !mcp_tools.is_empty() {
        tracing::info!(count = mcp_tools.len(), "registered MCP tools");
    }
    tools.extend(mcp_tools);

    if !skills.is_empty() {
        tracing::info!(count = skills.len(), "registered the skill tool");
        tools.extend([Arc::new(SkillTool::new(skills.clone())) as Arc<dyn Tool>]);
    }

    tools
}

/// Discover skills unless the config disables them. A missing `HOME` only
/// loses the user-level root; configured and project roots still work.
pub fn load_skills(config: &config::SkillsConfig, workspace: &Path) -> Arc<SkillSet> {
    if !config.enabled {
        return Arc::new(SkillSet::default());
    }
    Arc::new(SkillSet::discover(&skill_roots(config, workspace)))
}

/// All discovery roots in increasing precedence: global, user, configured,
/// project. The `.agents/skills` paths are what `npx skills` manages.
pub fn skill_roots(config: &config::SkillsConfig, workspace: &Path) -> Vec<(SkillSource, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(home) = config::home_dir() {
        roots.push((SkillSource::Global, home.join(".agents/skills")));
        roots.push((SkillSource::Global, home.join(".config/agents/skills")));
        roots.push((SkillSource::User, home.join(".config/qcode/skills")));
    }
    roots.extend(config_roots(config, workspace));
    roots.push((SkillSource::Project, workspace.join(".agents/skills")));
    roots.push((SkillSource::Project, workspace.join(".qcode/skills")));
    roots
}

pub fn config_roots(
    config: &config::SkillsConfig,
    workspace: &Path,
) -> Vec<(SkillSource, PathBuf)> {
    let home = config::home_dir();
    config
        .roots
        .iter()
        .map(|root| {
            (
                SkillSource::Config,
                expand_root(root, workspace, home.as_deref()),
            )
        })
        .collect()
}

pub fn expand_root(path: &Path, workspace: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if path == Path::new("~") {
            return home.to_path_buf();
        }
        if let Some(rest) = path.to_str().and_then(|text| text.strip_prefix("~/")) {
            return home.join(rest);
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

pub fn load_config(
    explicit: Option<PathBuf>,
    workspace: &Path,
) -> Result<config::Config, Box<dyn std::error::Error>> {
    let explicit = explicit.or_else(|| std::env::var("QCODE_CONFIG").ok().map(PathBuf::from));

    if let Some(path) = explicit {
        if !path.exists() {
            return Err(format!("config file not found: {}", path.display()).into());
        }
        return Ok(config::load_merged(&path, workspace)?);
    }

    let path = config::default_path()?;
    if path.exists() {
        Ok(config::load_merged(&path, workspace)?)
    } else {
        Ok(config::load_project(workspace)?)
    }
}

pub fn resolve_data_dir(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Ok(path) = std::env::var("QCODE_DATA_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(path).join("qcode");
    }
    // Windows has no `HOME` in pwsh; the config path already resolves
    // HOME-or-USERPROFILE, and the store must use the same home.
    let preferred = match config::home_dir() {
        Some(home) => home.join(".local").join("share").join("qcode"),
        None => PathBuf::from(".local").join("share").join("qcode"),
    };
    if writable_dir(&preferred) {
        return preferred;
    }
    // A sandboxed shell that may only write its workspace cannot create the
    // store under the user profile. Losing sessions is worse than keeping them
    // beside the code, so fall back once, loudly, instead of failing at startup.
    let fallback = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".qcode-data");
    if writable_dir(&fallback) {
        // resolve_data_dir runs from several subcommands; say this once.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                preferred = %preferred.display(),
                fallback = %fallback.display(),
                "data dir is not writable; using a workspace-local store (\
                 set --data-dir or QCODE_DATA_DIR to choose another)"
            );
        });
        return fallback;
    }
    // Neither candidate works: hand back the preferred one so the caller's
    // error names the path the user would expect.
    preferred
}

pub fn writable_dir(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(format!(".write-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"probe").is_ok();
    if ok {
        let _ = std::fs::remove_file(&probe);
    }
    ok
}
