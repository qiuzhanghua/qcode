//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;
pub mod config_edit;

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
    if !loaded.network.enabled || loaded.network.mode == config::NetworkMode::Off {
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
    if loaded.network.mode == config::NetworkMode::Strict {
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
    Ok(if loaded.sandbox.network {
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
}

use std::path::PathBuf;

use qcode_core::provider::Provider;
use qcode_core::provider::{StopReason, StreamEvent};
use qcode_core::tool::{Tool, ToolRegistry};
use qcode_mcp::McpConfig;
use qcode_providers::{
    AnthropicConfig, AnthropicProvider, MockProvider, OpenAiCompatConfig, OpenAiCompatProvider,
};
use qcode_tools::builtin_registry_with;
use qcode_tools::sandbox::{Sandbox, SandboxMode, SandboxNetwork};
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

pub fn build_provider(
    resolved: &config::Resolved,
    proxy_url: Option<&str>,
) -> Result<Box<dyn Provider>, Box<dyn std::error::Error>> {
    Ok(match resolved.name.as_str() {
        "mock" => Box::new(MockProvider::text(
            "Hello from the mock provider. The event log and replay path are working.",
        )),
        // Offline rehearsal of the approval UI: one run_shell call (ask by
        // default) followed by a closing message. Deliberately tiny so the
        // manual M1-10 walkthrough stays readable.
        "mock-approval" => Box::new(MockProvider::scripted(vec![
            vec![
                StreamEvent::Started,
                StreamEvent::ToolCallDone {
                    call_id: "approval-1".to_string(),
                    name: "run_shell".to_string(),
                    arguments: serde_json::json!({"command": "echo qcode-approval-demo"}),
                },
                StreamEvent::Ended {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                StreamEvent::Started,
                StreamEvent::TextDelta("审批流程演示结束。".to_string()),
                StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ])),
        "mock-tools" => Box::new(MockProvider::scripted(vec![
            vec![
                StreamEvent::Started,
                StreamEvent::ToolCallDone {
                    call_id: "demo-1".to_string(),
                    name: "list_dir".to_string(),
                    arguments: serde_json::json!({"path": "."}),
                },
                StreamEvent::Ended {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                StreamEvent::Started,
                StreamEvent::TextDelta("工具循环跑通了。".to_string()),
                StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ])),
        "mock-todo" => Box::new(MockProvider::scripted(vec![
            vec![
                StreamEvent::Started,
                StreamEvent::ToolCallDone {
                    call_id: "todo-1".to_string(),
                    name: "todo_write".to_string(),
                    arguments: serde_json::json!({
                        "items": [
                            {"content": "读 SPEC 的工具契约", "status": "completed"},
                            {"content": "实现 todo_write", "status": "in_progress"},
                            {"content": "跑测试与 TUI 验收"}
                        ]
                    }),
                },
                StreamEvent::Ended {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                StreamEvent::Started,
                StreamEvent::TextDelta("待办清单已更新。".to_string()),
                StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ])),
        "mock-subagent" => Box::new(MockProvider::scripted(vec![
            vec![
                StreamEvent::Started,
                StreamEvent::ToolCallDone {
                    call_id: "sub-1".to_string(),
                    name: "subagent".to_string(),
                    arguments: serde_json::json!({
                        "task": "在子会话里总结仓库结构",
                        "max_turns": 4
                    }),
                },
                StreamEvent::Ended {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            vec![
                StreamEvent::Started,
                StreamEvent::TextDelta("子 agent：仓库有 core/store/cli 三个 crate。".to_string()),
                StreamEvent::Usage(qcode_core::event::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(6),
                    ..qcode_core::event::Usage::default()
                }),
                StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                },
            ],
            vec![
                StreamEvent::Started,
                StreamEvent::TextDelta("父 agent 收到子任务结果。".to_string()),
                StreamEvent::Usage(qcode_core::event::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(6),
                    ..qcode_core::event::Usage::default()
                }),
                StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ])),
        "anthropic" | "claude" => Box::new(AnthropicProvider::new(AnthropicConfig {
            name: resolved.name.clone(),
            base_url: resolved
                .base_url
                .clone()
                .expect("non-mock provider always has a base URL"),
            api_key: resolved.api_key.clone(),
            proxy_url: proxy_url.map(str::to_string),
        })?),
        _ => Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
            name: resolved.name.clone(),
            base_url: resolved
                .base_url
                .clone()
                .expect("non-mock provider always has a base URL"),
            api_key: resolved.api_key.clone(),
            stream_usage: resolved.stream_usage,
            proxy_url: proxy_url.map(str::to_string),
            extra_body: resolved.extra_body.clone(),
        })?),
    })
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
    let tools = build_tools(
        &loaded.mcp,
        &loaded.search,
        cwd,
        &data_dir,
        &skills,
        sandbox,
        network.as_ref().map(|network| network.env()),
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
