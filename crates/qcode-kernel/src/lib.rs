//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;
pub mod config_edit;
pub mod provider_registry;

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
        None => {
            let mut rules = loaded.approval.rules.clone();
            rules.extend(loaded.approval.shorthand_rules());
            rules
        }
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
    if !loaded.network_plan().proxy {
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

/// Resolve the OS sandbox network policy from `[network]`/`[sandbox]`.
///
/// `strict` is the macOS hard mode: the Seatbelt profile may only connect to
/// the loopback egress proxy port. It needs both the proxy (for the port) and
/// `sandbox-exec` (the only backend that can enforce it today); anything else
/// fails closed instead of pretending to be confined.
fn sandbox_network(
    loaded: &config::Config,
    egress: Option<&Egress>,
) -> Result<SandboxNetwork, Box<dyn std::error::Error>> {
    if loaded.network_plan().strict {
        if loaded.sandbox.mode != SandboxMode::SandboxExec {
            return Err(
                "network.mode = \"strict\" requires [sandbox] mode = \"sandbox-exec\" (macOS)"
                    .into(),
            );
        }
        let egress = egress.ok_or(
            "network.mode = \"strict\" requires [network] enabled = true (the proxy is the only allowed destination)",
        )?;
        return Ok(SandboxNetwork::ProxyOnly {
            port: egress.port(),
        });
    }
    Ok(if loaded.network_plan().raw {
        SandboxNetwork::Allow
    } else {
        SandboxNetwork::Deny
    })
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
    /// The egress proxy as the network the tools and the engine share.
    pub network: Option<qcode_core::network::SharedNetwork>,
    /// The sandbox the tools run under, for status bars.
    pub sandbox: String,
    /// The environment `run_shell` injects (ADR-0064). Callers sample and take
    /// the audit before a turn so the log records what the agent actually saw.
    pub shell_env: Arc<qcode_tools::shell_env::ShellEnvCache>,
}

use std::path::PathBuf;

use qcode_core::provider::Provider;
use qcode_core::tool::{Tool, ToolRegistry};
use qcode_mcp::McpConfig;
use qcode_tools::builtin_registry_with_env;
use qcode_tools::sandbox::{Sandbox, SandboxMode, SandboxNetwork};
use qcode_tools::skills::{SkillSet, SkillSource, SkillTool};
use qcode_tools::web_search::WebSearch;
pub struct ToolWiring {
    /// Proxy variables the egress layer injects into every child.
    pub network_env: Option<Vec<(String, String)>>,
    /// The session's egress client, handed to remote MCP servers (ADR-0067
    /// §2). `None` when `[network]` is off, which makes a remote server fail
    /// with "no egress client" instead of connecting around the proxy.
    pub network: Option<qcode_core::network::SharedNetwork>,
    /// The user's shell environment, sampled once per session (ADR-0064).
    pub shell_env: Arc<qcode_tools::shell_env::ShellEnvCache>,
}

pub async fn build_tools(
    mcp: &McpConfig,
    search: &config::SearchConfig,
    workspace: &Path,
    data_dir: &Path,
    skills: &Arc<SkillSet>,
    sandbox: Arc<dyn Sandbox>,
    wiring: ToolWiring,
) -> ToolRegistry {
    let ToolWiring {
        network_env,
        network,
        shell_env,
    } = wiring;
    let mut tools = builtin_registry_with_env(sandbox.clone(), shell_env);
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
    // MCP servers run under a sandbox built from their own grant (ADR-0066),
    // not `run_shell`'s: the runtime directory is readable and executable,
    // writes are limited to the workspace plus a private HOME.
    let confine = mcp_confine(sandbox.clone(), workspace);
    // The remote half of ADR-0067: `Network::client()` is the only client a
    // streamable-http server may use, so its traffic is allowlisted, audited
    // and private-address-blocked like every other outbound request.
    let http = network.as_ref().map(|network| network.client());
    let mcp = qcode_mcp::connect_detailed(
        mcp,
        workspace,
        data_dir,
        network_env.as_deref().unwrap_or(&[]),
        qcode_mcp::ConnectOptions {
            http: http.as_ref(),
            confine: Some(confine.as_ref()),
        },
    )
    .await;
    if !mcp.tools.is_empty() {
        tracing::info!(count = mcp.tools.len(), "registered MCP tools");
    }
    tools.extend(mcp.tools);
    // The tool table follows the server's `ttlMs` and `list_changed`
    // (ADR-0067 §5). Registering the source is what makes the engine re-read it
    // once per turn; without it the list would stay whatever it was at startup,
    // which is the pre-2026-07-28 behaviour.
    if let Some(source) = mcp.source {
        tools.add_source(source as std::sync::Arc<dyn qcode_core::tool::ToolSource>);
    }

    if !skills.is_empty() {
        tracing::info!(count = skills.len(), "registered the skill tool");
        tools.extend([Arc::new(SkillTool::new(skills.clone())) as Arc<dyn Tool>]);
    }

    tools
}

/// The sandbox seam for MCP servers (ADR-0066): one sandbox per server, built
/// from that server's own grant.
///
/// Kept public so the integration test wires exactly what the kernel wires
/// instead of copying the closure and drifting.
pub fn mcp_confine(sandbox: Arc<dyn Sandbox>, workspace: &Path) -> Box<qcode_mcp::Confine> {
    // `Box<dyn Fn>` has no lifetime bound, so the closure owns the path.
    let workspace = workspace.to_path_buf();
    // Every `Command` keeps a reference to the sandbox that built it (see
    // `Sandbox::command`): dropping the sandbox before `spawn()` makes the
    // child fail with EBADFD. `run_shell` keeps its per-task sandbox in a local
    // for the same reason; here the locals die with this closure, so the
    // per-server sandboxes are parked in a list that lives as long as the
    // `Confine` box — which spans every spawn in `connect_all_with`.
    let alive: Arc<std::sync::Mutex<Vec<Arc<dyn Sandbox>>>> = Arc::default();
    Box::new(move |program, args, cwd, grant| {
        let roots = mcp_roots(&workspace, grant);
        let per_server = sandbox
            // `OutsideRoots::Nothing` is the whole point of ADR-0066 §1: an MCP
            // server writes to its grant (workspace, private HOME, declared
            // `writable_paths`) and nowhere else. `run_shell`'s build caches and
            // the host `/tmp` are *not* part of that grant, and handing them
            // over quietly made `~/.cargo/config.toml` writable by third-party
            // code — whose `[build] rustc-wrapper` runs on the next build.
            .with_roots(&roots, None, qcode_tools::sandbox::OutsideRoots::Nothing)
            .map_err(|error| format!("{error} (grant: {})", grant.describe()))?;
        let command = per_server
            .command(program, args, cwd)
            .map_err(|error| error.to_string())?;
        alive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(per_server);
        Ok(command)
    })
}

/// Turn one server's grant into task roots for `Sandbox::with_roots`.
///
/// The workspace is the primary root (it is also the server's cwd); everything
/// else is either read-only runtime or a credential subtree. `with_roots`
/// already understands `RootAccess::ReadOnly` on all three backends, so this
/// needs no new sandbox API (ADR-0066 实现).
pub fn mcp_roots(workspace: &Path, grant: &qcode_mcp::GrantSet) -> Vec<qcode_core::task::Root> {
    grant
        .roots()
        .into_iter()
        .enumerate()
        .map(|(index, (path, writable))| qcode_core::task::Root {
            id: format!("mcp-{index}"),
            label: format!("mcp-{index}"),
            primary: path == workspace,
            access: if writable {
                qcode_core::task::RootAccess::ReadWrite
            } else {
                qcode_core::task::RootAccess::ReadOnly
            },
            path,
        })
        .collect()
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

pub fn build_provider(
    resolved: &config::Resolved,
    proxy_url: Option<&str>,
) -> Result<Box<dyn Provider>, Box<dyn std::error::Error>> {
    provider_registry::ProviderRegistry::new(proxy_url.map(str::to_string)).build(resolved)
}

/// Build a provider for an explicit route + model, independent of the
/// currently assembled kernel (V2.1 grouped model picker: switching to another
/// provider's model rebuilds just the provider, keeping the session, tools and
/// policy intact). `proxy_url` is the egress proxy of the running kernel; a
/// same-process switch reuses it unchanged.
pub fn build_provider_for(
    config_path: Option<&std::path::Path>,
    cwd: &std::path::Path,
    provider: &str,
    model: &str,
    proxy_url: Option<&str>,
) -> Result<Box<dyn Provider>, Box<dyn std::error::Error>> {
    let loaded = load_config(config_path.map(Path::to_path_buf), cwd)?;
    let overrides = config::Overrides {
        provider: Some(provider.to_string()),
        model: Some(model.to_string()),
        ..Default::default()
    };
    let resolved = config::resolve(&loaded, &overrides, &|key| std::env::var(key).ok())?;
    build_provider(&resolved, proxy_url)
}

/// Everything a frontend needs, assembled once.
pub struct Assembled {
    pub config: config::Config,
    pub resolved: config::Resolved,
    pub policy: qcode_core::policy::Policy,
    pub store: qcode_core::store::SessionStore,
    pub tools: ToolRegistry,
    pub skills: Arc<SkillSet>,
    pub provider: Box<dyn Provider>,
    /// Kept so the proxy lives exactly as long as the kernel does.
    pub egress: Option<Arc<Egress>>,
    /// The egress proxy as the network the tools and the engine share.
    pub network: Option<qcode_core::network::SharedNetwork>,
    /// The sandbox the tools run under, for status bars.
    pub sandbox: String,
    /// The environment `run_shell` injects (ADR-0064). Callers sample and take
    /// the audit before a turn so the log records what the agent actually saw.
    pub shell_env: Arc<qcode_tools::shell_env::ShellEnvCache>,
}

/// Assemble the kernel for the directory qc was started in.
///
/// ADR-0036: this is the only place that wires configuration, policy, egress,
/// tools, skills and the provider together, so the TUI, ACP and GUI cannot
/// drift apart. `sandbox` and `network_env` stay parameters because they depend
/// on how the frontend was started (TUI probes, ACP does not).
pub async fn assemble(
    config_path: Option<std::path::PathBuf>,
    cwd: &Path,
    data_dir: Option<std::path::PathBuf>,
    approval_policy: Option<&Path>,
    overrides: config::Overrides,
) -> Result<Assembled, Box<dyn std::error::Error>> {
    let loaded = load_config(config_path, cwd)?;
    let resolved = config::resolve(&loaded, &overrides, &|key| std::env::var(key).ok())?;
    let policy = build_policy(&loaded, approval_policy)?;
    let data_dir = resolve_data_dir(data_dir);
    // The egress proxy must exist before the sandbox in strict mode: the
    // Seatbelt profile is built around the proxy's loopback port.
    let egress = build_egress(&loaded, &data_dir, resolved.base_url.as_deref()).await?;
    let sandbox_network = sandbox_network(&loaded, egress.as_deref())?;
    // The sandbox is configuration, not a frontend choice: mode, network
    // policy and extra writable roots all come from `[sandbox]`/`[network]`.
    let sandbox: Arc<dyn Sandbox> = qcode_tools::sandbox::build(
        loaded.sandbox.mode,
        cwd,
        sandbox_network,
        &loaded.sandbox.extra_writable,
    )?;
    let sandbox_name = sandbox.name().to_string();
    // The proxy doubles as the network the tools and the engine share, so its
    // environment is derived here instead of by every frontend.
    let network: Option<qcode_core::network::SharedNetwork> = egress
        .clone()
        .map(|egress| egress as qcode_core::network::SharedNetwork);
    let provider = build_provider(&resolved, egress.as_ref().map(|egress| egress.proxy_url()))?;
    let skills = load_skills(&loaded.skills, cwd);
    let shell_env = std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::new(
        loaded.shell.clone(),
    ));
    let tools = build_tools(
        &loaded.mcp,
        &loaded.search,
        cwd,
        &data_dir,
        &skills,
        sandbox,
        ToolWiring {
            network_env: network.as_ref().map(|network| network.env()),
            network: network.clone(),
            shell_env: shell_env.clone(),
        },
    )
    .await;
    let store = qcode_core::store::SessionStore::new(data_dir);
    Ok(Assembled {
        config: loaded,
        resolved,
        policy,
        store,
        tools,
        skills,
        shell_env,
        provider,
        egress,
        network,
        sandbox: sandbox_name,
    })
}

#[cfg(test)]
mod sandbox_network_tests {
    use super::*;
    use crate::config::NetworkMode;

    #[test]
    fn strict_requires_sandbox_exec() {
        let cfg: config::Config =
            toml::from_str("[network]\nmode = \"strict\"\n[sandbox]\nmode = \"none\"\n").unwrap();
        let error = sandbox_network(&cfg, None).unwrap_err();
        assert!(error.to_string().contains("sandbox-exec"), "{error}");
    }

    #[test]
    fn strict_requires_the_proxy() {
        let cfg: config::Config =
            toml::from_str("[network]\nmode = \"strict\"\n[sandbox]\nmode = \"sandbox-exec\"\n")
                .unwrap();
        let error = sandbox_network(&cfg, None).unwrap_err();
        assert!(error.to_string().contains("proxy"), "{error}");
    }

    #[test]
    fn proxy_mode_maps_the_coarse_sandbox_switch() {
        let cfg: config::Config = toml::from_str("[sandbox]\nnetwork = true\n").unwrap();
        assert!(matches!(
            sandbox_network(&cfg, None).unwrap(),
            SandboxNetwork::Allow
        ));
        assert_eq!(cfg.network.mode, NetworkMode::Proxy);
    }
}
