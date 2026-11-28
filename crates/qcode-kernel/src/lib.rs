//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;
pub mod config_edit;
pub mod config_watch;
/// 日志初始化与 `qc.log` 的落点。
///
/// 在 `qcode-kernel` 而不是某个二进制里：`qc` 与 `qcd` 都要写同一份 `qc.log`，
/// 而 daemon 没有终端——日志是它唯一的现场（ADR-0082）。放在 kernel 是因为这里
/// 已经是「装配点」（`assemble`、`redact::install`），不是因为它和引擎有关。
pub mod logging;
pub mod provider_registry;
pub mod providers;
pub mod redact;
/// Engine-backed `subagent` runner (ADR-0087). It lives here, not in the binary,
/// because every frontend needs it: `qcode` could not lend a type to
/// `qcode-gui`, which depends on it and not the other way around.
pub mod subagent;
/// Config templates (ADR-0104). Embedded assets plus their validation; the
/// `qc config init` command that writes them lives in the binary.
pub mod template;

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
                return Err(qcode_i18n::trp(
                    "kernel.err.approval_policy_missing",
                    &[path.display().to_string()],
                )
                .into());
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
use std::time::Duration;

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
        .then(|| qcode_core::paths::log_dir(data_dir).join("network.jsonl"));
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
            return Err(qcode_i18n::tr("kernel.err.network_strict_macos").into());
        }
        let egress = egress.ok_or(qcode_i18n::tr("kernel.err.network_strict_proxy"))?;
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
    /// The config file this kernel was assembled from, if one exists.
    ///
    /// ADR-0103: frontends that do not re-assemble on change (TUI, `qcd`) use
    /// this to tell the user that the file no longer matches what is running.
    /// It is the *resolved* path — after `--config`, `QCODE_CONFIG`, portable
    /// mode and the XDG default — not whatever the caller passed in.
    pub config_path: Option<std::path::PathBuf>,
}

use std::path::PathBuf;

use qcode_core::provider::Provider;
use qcode_core::tool::{Tool, ToolRegistry};
use qcode_mcp::McpConfig;
use qcode_tools::builtin_registry_with_env;
use qcode_tools::sandbox::{Sandbox, SandboxMode, SandboxNetwork};
use qcode_tools::skills::{
    ServedFile, Skill, SkillDir, SkillEntry, SkillIndex, SkillSet, SkillSource, SkillTool,
};
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

/// Connect every configured MCP server and hand back what it produced.
///
/// Split out of [`build_tools`] by ADR-0108 §2, because the skill index needs
/// the servers' `skills/list` answers and the index is built *before* the tool
/// registry. Connecting inside `build_tools` would mean either connecting twice
/// (two processes per server) or indexing skills the tool registry has not seen
/// yet. One connection, read what it says, then build the registry.
pub async fn connect_mcp(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    sandbox: Arc<dyn Sandbox>,
    wiring: &ToolWiring,
) -> qcode_mcp::McpConnection {
    // ADR-0070: a bare `command` (`wordcraft-cli`) is the portable spelling, but
    // qcode's own `PATH` is launchd's minimal one when the GUI starts from the
    // Dock. Ask the shell for its environment — the same snapshot `run_shell`
    // uses — and resolve/start the servers with that. Sampled only when a
    // server actually needs it; an absolute `command` costs nothing.
    let search_path = mcp_search_path(mcp, &wiring.shell_env).await;
    // MCP servers run under a sandbox built from their own grant (ADR-0066),
    // not `run_shell`'s: the runtime directory is readable and executable,
    // writes are limited to the workspace plus a private HOME.
    let confine = mcp_confine(sandbox, workspace);
    // The remote half of ADR-0067: `Network::client()` is the only client a
    // streamable-http server may use, so its traffic is allowlisted, audited
    // and private-address-blocked like every other outbound request.
    let http = wiring.network.as_ref().map(|network| network.client());
    qcode_mcp::connect_detailed(
        mcp,
        workspace,
        data_dir,
        wiring.network_env.as_deref().unwrap_or(&[]),
        qcode_mcp::ConnectOptions {
            http: http.as_ref(),
            confine: Some(confine.as_ref()),
            path: search_path.as_deref(),
        },
    )
    .await
}

/// Build the tool registry from an already-connected MCP set.
///
/// `mcp` is a [`qcode_mcp::McpConnection`] rather than a config, so this
/// function never starts a server: see [`connect_mcp`] for why that matters.
pub async fn build_tools(
    mcp: qcode_mcp::McpConnection,
    search: &config::SearchConfig,
    skills: &Arc<SkillIndex>,
    sandbox: Arc<dyn Sandbox>,
    shell_env: Arc<qcode_tools::shell_env::ShellEnvCache>,
) -> ToolRegistry {
    let mut tools = builtin_registry_with_env(sandbox, shell_env);
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
    if !mcp.tools.is_empty() {
        tracing::info!(count = mcp.tools.len(), "registered MCP tools");
    }
    tools.extend(mcp.tools);
    // The tool table follows the server's `ttlMs` and `list_changed`
    // (ADR-0067 §5). Registering the source is what makes the engine re-read it
    // once per turn; without it the list would stay whatever it was at startup,
    // which is the pre-2026-07-28 behaviour.
    if let Some(source) = mcp.source.clone() {
        tools.add_source(source as std::sync::Arc<dyn qcode_core::tool::ToolSource>);
    }

    if !skills.is_empty() {
        tracing::info!(count = skills.len(), "registered the skill tool");
        // A served skill's body is fetched through the connection this registry
        // was built from (ADR-0108 §4), so the reader and the tools can never
        // disagree about which servers exist.
        let tool: Arc<dyn Tool> = match mcp.source.as_ref() {
            Some(source) => Arc::new(SkillTool::with_reader(
                skills.clone(),
                Arc::new(McpSkillReader {
                    source: source.clone(),
                }),
            )),
            None => Arc::new(SkillTool::new(skills.clone())),
        };
        tools.extend([tool]);
    }

    tools
}

/// The `PATH` MCP servers are resolved in and started with (ADR-0070).
///
/// `None` keeps the pre-ADR-0070 behaviour: qcode's own `PATH`. That is the
/// right answer in three cases — no server needs a lookup, sampling is off, or
/// the sample produced nothing — and in all three there is nothing better to
/// offer.
///
/// Sampling blocks for as long as the shell takes (bounded by `env_timeout_ms`),
/// so it runs on a blocking thread rather than on the caller's runtime. It is
/// also asked for only when [`needs_path_lookup`] says a bare `command` exists:
/// a config full of absolute paths must not pay a shell startup per launch.
async fn mcp_search_path(
    mcp: &McpConfig,
    shell_env: &std::sync::Arc<qcode_tools::shell_env::ShellEnvCache>,
) -> Option<std::ffi::OsString> {
    if !mcp_needs_path_lookup(mcp) {
        return None;
    }
    let cache = shell_env.clone();
    let snapshot = tokio::task::spawn_blocking(move || cache.current())
        .await
        .ok()
        .flatten()?;
    match mcp_path_from_snapshot(Some(snapshot.as_ref())) {
        Some(path) => {
            tracing::info!("resolving MCP server commands against the shell environment's PATH");
            Some(path)
        }
        None => {
            // The snapshot exists but has no `PATH` — a shell that unsets it.
            // Falling back silently would leave "not found" unexplained.
            tracing::warn!(
                "the shell environment snapshot has no PATH; MCP server commands are resolved \
                 against qcode's PATH"
            );
            None
        }
    }
}

/// Whether any enabled stdio server's `command` is a bare name.
///
/// A bare name (no `/`) is the only thing a `PATH` lookup helps: an absolute or
/// relative path is resolved from the string itself. Remote servers have no
/// process at all (ADR-0067 §7).
///
/// Public because `qc doctor` has to answer the same question before it can
/// report *which* `PATH` it used, and two copies of this predicate would drift
/// into "doctor samples, a run does not" without either side noticing.
pub fn mcp_needs_path_lookup(mcp: &McpConfig) -> bool {
    mcp.servers
        .iter()
        .filter(|server| server.enabled)
        .any(|server| {
            matches!(server.transport(), Some(qcode_mcp::Transport::Stdio))
                && server.command.as_deref().is_some_and(|command| {
                    !command.is_empty() && !command.contains('/') && !command.contains('\\')
                })
        })
}

/// The `PATH` out of a shell snapshot, or `None` when there is nothing usable.
///
/// Split out from [`mcp_search_path`] so the synchronous caller (`qc doctor`)
/// shares the reading rules with the async one instead of restating them.
pub fn mcp_path_from_snapshot(
    snapshot: Option<&qcode_tools::shell_env::ShellEnv>,
) -> Option<std::ffi::OsString> {
    match snapshot.and_then(|env| env.vars().get("PATH")) {
        Some(path) if !path.is_empty() => Some(path.into()),
        _ => None,
    }
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
            .map_err(|error| {
                qcode_i18n::trp(
                    "kernel.err.grant_failed",
                    &[error.to_string(), grant.describe()],
                )
            })?;
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

/// The servers the config lets contribute skills, by label (ADR-0108 §1).
///
/// Both switches must agree: `[skills] mcp` (or the server's own `skills`) says
/// the source is allowed, and `enabled` says the server is running at all. An
/// allow-listed but disabled server contributes nothing because nothing asked
/// it — it never connected.
fn skill_allowed_servers(
    mcp: &McpConfig,
    default_allowed: bool,
) -> std::collections::BTreeSet<String> {
    mcp.servers
        .iter()
        .filter(|server| server.enabled && server.skills_allowed(default_allowed))
        .map(|server| server.name.clone())
        .collect()
}

/// Ask the servers for their skills and turn them into index entries.
///
/// Returns an empty vector — not an error — when nothing is allowed or nothing
/// declares the extension, which is the common case. A listing that fails is
/// logged and skipped: one server's skills are not worth a failed session.
pub async fn collect_served_skills(
    connection: &qcode_mcp::McpConnection,
    mcp: &McpConfig,
    default_allowed: bool,
) -> Vec<qcode_mcp::ServedSkill> {
    let Some(source) = connection.source.as_ref() else {
        return Vec::new();
    };
    let allowed = skill_allowed_servers(mcp, default_allowed);
    if allowed.is_empty() {
        return Vec::new();
    }
    let (listings, errors) = source.list_skills(&allowed).await;
    for error in errors {
        tracing::warn!(
            server = %error.server,
            message = %error.message,
            "a server's skills are not in the index"
        );
    }
    listings
        .into_iter()
        .flat_map(|listing| listing.skills)
        .collect()
}

/// The listings, plus each server's freshness hint.
///
/// The hint is what the refresher sleeps on: SEP-2640 has no `list_changed`
/// notification, so `ttlMs` is the only signal a server can give that its
/// catalogue moved (ADR-0108 §2).
pub async fn collect_served_skill_listings(
    connection: &qcode_mcp::McpConnection,
    mcp: &McpConfig,
    default_allowed: bool,
) -> (Vec<qcode_mcp::ServedSkill>, Vec<(String, Option<u64>)>) {
    let Some(source) = connection.source.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    let allowed = skill_allowed_servers(mcp, default_allowed);
    if allowed.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let (listings, errors) = source.list_skills(&allowed).await;
    for error in errors {
        tracing::warn!(
            server = %error.server,
            message = %error.message,
            "a server's skills are not in the index"
        );
    }
    let mut ttls = Vec::new();
    let mut skills = Vec::new();
    for listing in listings {
        ttls.push((listing.server.clone(), listing.ttl_ms));
        skills.extend(listing.skills);
    }
    (skills, ttls)
}

/// Re-list one server every time its advertised TTL runs out (ADR-0108 §2).
///
/// Public so the ADR-0108 §2 test can drive it without assembling a whole
/// session; `assemble` is the only production caller.
///
/// The session's `SkillIndex` is held weakly: when the session ends the index
/// drops and the task exits at its next tick. A failed re-list keeps the old
/// entries — a server that is briefly unavailable must not make its skills
/// disappear from the prompt (the model would lose a capability mid-session for
/// a transient reason).
pub fn spawn_skill_refresh(
    source: Arc<qcode_mcp::McpToolSource>,
    ttls: Vec<(String, Option<u64>)>,
    index: Arc<SkillIndex>,
) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime (a synchronous caller): the listing is a snapshot, which is
        // exactly what a one-shot command wants anyway.
        return;
    };
    for (server, ttl_ms) in ttls {
        let Some(mut delay) = ttl_ms.filter(|ms| *ms > 0).map(Duration::from_millis) else {
            continue;
        };
        let source = source.clone();
        let weak = Arc::downgrade(&index);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(delay).await;
                let Some(index) = weak.upgrade() else {
                    return;
                };
                let only = std::collections::BTreeSet::from([server.clone()]);
                let (listings, errors) = source.list_skills(&only).await;
                for error in &errors {
                    tracing::warn!(
                        server = %error.server,
                        message = %error.message,
                        "skills refresh failed; keeping the previous listing"
                    );
                }
                if let Some(listing) = listings.into_iter().find(|item| item.server == server) {
                    let served = served_skills(listing.skills);
                    let revision = index.replace_served(&server, served);
                    tracing::info!(
                        server = %server,
                        revision,
                        "skills refreshed after their ttl"
                    );
                    if let Some(ttl) = listing.ttl_ms.filter(|ms| *ms > 0) {
                        delay = Duration::from_millis(ttl);
                    }
                }
                drop(index);
            }
        });
    }
}

/// Build the index entries from what the servers listed.
///
/// The mapping is deliberately thin: everything that decides whether a skill may
/// be *loaded* stays in `qcode-tools` (`SkillEntry` + the verification there),
/// because that is the code that runs when the model asks.
pub fn served_skills(served: Vec<qcode_mcp::ServedSkill>) -> Vec<Skill> {
    served
        .into_iter()
        .map(|served| {
            let files: Vec<ServedFile> = served
                .files
                .into_iter()
                .map(|file| ServedFile {
                    uri: file.uri,
                    digest: file.digest,
                    size: file.size,
                })
                .collect();
            // The index shows exactly these two, and they must be the same
            // strings the body is later checked against — so they are read out
            // of the frontmatter rather than carried alongside it.
            let name = qcode_tools::skills::frontmatter_str(&served.frontmatter, "name")
                .unwrap_or_default()
                .to_string();
            let description =
                qcode_tools::skills::frontmatter_str(&served.frontmatter, "description")
                    .unwrap_or_default()
                    .to_string();
            Skill {
                name,
                description,
                source: SkillSource::Mcp(served.server),
                dir: SkillDir::Remote {
                    uri: served.uri.clone(),
                },
                // No body: SEP-2640 forbids picking it up before the model
                // asks, and the index only ever shows `name` + `description`.
                body: String::new(),
                files: files
                    .iter()
                    .map(|file| file.uri.clone())
                    .filter(|uri| uri != &served.uri)
                    .collect(),
                files_truncated: false,
                entry: Some(SkillEntry {
                    uri: served.uri,
                    files,
                    dynamic: served.dynamic,
                    listed: served.frontmatter,
                }),
            }
        })
        .collect()
}

/// Reads a served skill's files through the session's own MCP connection
/// (ADR-0108 §4).
///
/// Lives in the kernel because it is the only crate that sees both sides: the
/// tool layer must not depend on `qcode-mcp`, and `qcode-mcp` must not know what
/// a skill is.
struct McpSkillReader {
    source: Arc<qcode_mcp::McpToolSource>,
}

/// The reader a session's skill tool gets for its served skills.
pub fn mcp_skill_reader(
    source: Arc<qcode_mcp::McpToolSource>,
) -> Arc<dyn qcode_tools::skills::RemoteSkillReader> {
    Arc::new(McpSkillReader { source })
}

#[async_trait::async_trait]
impl qcode_tools::skills::RemoteSkillReader for McpSkillReader {
    async fn read_uri(&self, server: &str, uri: &str) -> Result<Vec<u8>, String> {
        self.source.read_resource(server, uri).await
    }

    /// `skills/get` (ADR-0108 §4): the entry for a skill the listing may not
    /// contain. Its digests are what makes the body verifiable.
    async fn get_skill(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<qcode_tools::skills::RemoteEntry, String> {
        let served = self.source.get_skill(server, uri).await?;
        Ok(qcode_tools::skills::RemoteEntry {
            uri: served.uri,
            frontmatter: served.frontmatter,
            files: served
                .files
                .into_iter()
                .map(|file| qcode_tools::skills::ServedFile {
                    uri: file.uri,
                    digest: file.digest,
                    size: file.size,
                })
                .collect(),
            dynamic: served.dynamic,
        })
    }
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

/// Expand one `[skills] roots` entry.
///
/// `~` handling is [`config::expand_tilde`], the same helper every other
/// configured path goes through, so there is one answer to "what is `~`".
/// Unlike those, a relative entry is allowed here and resolves against the
/// workspace: a skill root is a *directory to scan*, not a grant, and
/// `.qcode/skills` in a repository is a useful thing to write.
pub fn expand_root(path: &Path, workspace: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = config::expand_tilde(path, home);
    if expanded != path || path.is_absolute() {
        return expanded;
    }
    workspace.join(path)
}

pub fn load_config(
    explicit: Option<PathBuf>,
    workspace: &Path,
) -> Result<config::Config, Box<dyn std::error::Error>> {
    let explicit = explicit.or_else(|| std::env::var("QCODE_CONFIG").ok().map(PathBuf::from));

    if let Some(path) = explicit {
        if !path.exists() {
            return Err(qcode_i18n::trp(
                "kernel.err.config_missing",
                &[path.display().to_string()],
            )
            .into());
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

/// 配置里给的语言，供**在 clap 之前**就要定语言的调用方使用（GUI 三个入口）。
///
/// 读不到、文件不存在、解析失败一律返回 `None` —— 语言是「有就用，没有就跟随
/// 环境/系统」，绝不能让一次配置读失败把界面卡住。首次运行的向导（`run_setup`）
/// 就是文件还不存在的情况。
///
/// 与 `main.rs` 读 `[log] level` 是同一个形状（`load_config(...).ok()`），代价也一样：
/// 这个函数会把配置读第二遍，其间的警告此时没有订阅者接。调用方本来就要再读一次，
/// 那次会重放同样的警告，所以不丢信息。
pub fn configured_language(
    explicit: Option<std::path::PathBuf>,
    workspace: &std::path::Path,
) -> Option<String> {
    load_config(explicit, workspace)
        .ok()
        .and_then(|config| config.ui.language)
}

/// Where the data directory *would* be, without touching the filesystem.
///
/// Logging needs a destination before anything else runs, and it must not be the
/// reason a directory appears: `qc skills list` never touches the store, so
/// resolving it eagerly would create `~/.local/share/qcode` — or, in a read-only
/// sandbox, fall back to `.qcode-data` inside the user's repo — for a command that
/// has nothing to store. Callers that need the real, writable answer use
/// [`resolve_data_dir`]; this one only predicts it.
pub fn data_dir_hint(explicit: Option<PathBuf>) -> PathBuf {
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
    match config::home_dir() {
        Some(home) => home.join(".local").join("share").join("qcode"),
        None => PathBuf::from(".local").join("share").join("qcode"),
    }
}

pub fn resolve_data_dir(explicit: Option<PathBuf>) -> PathBuf {
    // A `--data-dir` or env var is taken at its word, writable or not: the point
    // of naming a directory is that the answer is not up for negotiation.
    if explicit.is_some()
        || std::env::var_os("QCODE_DATA_DIR").is_some()
        || std::env::var_os("XDG_DATA_HOME").is_some()
    {
        return data_dir_hint(explicit);
    }
    let preferred = data_dir_hint(None);
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
    /// The working directory this kernel was assembled for — the same value the
    /// sandbox was built around and config lookup used.
    ///
    /// Stored rather than re-read by callers: `qcd` needs it as the fallback for
    /// a task without a `cwd`, and asking `current_dir()` a second time is how a
    /// process ends up with two answers to "where am I". `qcd` is the case that
    /// proves it — spawned from a GUI whose cwd is `/`, its second read would be
    /// the filesystem root, which is precisely what the first read had to
    /// replace (see `qcd::launch_workspace`).
    pub workspace: std::path::PathBuf,
    pub tools: ToolRegistry,
    pub skills: Arc<SkillIndex>,
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
    /// The config file this kernel was assembled from, if one exists.
    ///
    /// ADR-0103: frontends that do not re-assemble on change (TUI, `qcd`) use
    /// this to tell the user that the file no longer matches what is running.
    /// It is the *resolved* path — after `--config`, `QCODE_CONFIG`, portable
    /// mode and the XDG default — not whatever the caller passed in.
    pub config_path: Option<std::path::PathBuf>,
}

/// Register every credential this configuration resolved for redaction.
///
/// Split out of [`assemble`] so it can be tested: the property worth asserting is
/// not "masking works" (that is [`redact::Redactor`]'s own unit tests) but **that
/// each credential is actually handed to it**. A secret left out here is invisible
/// in every other test and only shows up in a log file.
///
/// Today that is two: the provider API key, and the OAuth2 client secret
/// (ADR-0107 补记). The OAuth2 one matters for a specific shape — a token endpoint
/// that echoes its request back in an error body puts the secret in the text we
/// log, and nothing upstream of this point knows it is a secret.
fn install_secrets(resolved: &config::Resolved) {
    redact::install(
        resolved
            .api_key
            .clone()
            .into_iter()
            .chain(resolved.oauth2_client_secret.clone()),
    );
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
    let loaded = load_config(config_path.clone(), cwd)?;
    // ADR-0103: the frontends that never re-assemble need the resolved path to
    // notice the file changing under them. `discover` is the same resolver
    // `load_config` used above (flag → env → portable → user), so the two agree;
    // a failure here is not fatal — it only costs the warning.
    let watched_config = config::discover(config_path).ok().map(|found| found.path);
    let resolved = config::resolve(&loaded, &overrides, &|key| std::env::var(key).ok())?;
    // The one place every frontend passes through, and the moment the key becomes
    // known. Installing here is what keeps `qc acp` — a long-lived headless server
    // whose provider errors all go to the log — from being the one caller that
    // forgets (ADR-0078).
    install_secrets(&resolved);
    let policy = build_policy(&loaded, approval_policy)?;
    let data_dir = resolve_data_dir(data_dir);
    // The egress proxy must exist before the sandbox in strict mode: the
    // Seatbelt profile is built around the proxy's loopback port.
    let egress = build_egress(&loaded, &data_dir, resolved.base_url.as_deref()).await?;
    let sandbox_network = sandbox_network(&loaded, egress.as_deref())?;
    // The sandbox is configuration, not a frontend choice: mode, network
    // policy and the extra read/write roots all come from `[sandbox]`/`[network]`.
    let sandbox: Arc<dyn Sandbox> = qcode_tools::sandbox::build(
        loaded.sandbox.mode,
        cwd,
        sandbox_network,
        qcode_tools::sandbox::SandboxPaths {
            writable: &loaded.sandbox.extra_writable,
            readable: &loaded.sandbox.extra_read,
            allow_pty: loaded.sandbox.allow_pty,
        },
    )?;
    let sandbox_name = sandbox.name().to_string();
    // The proxy doubles as the network the tools and the engine share, so its
    // environment is derived here instead of by every frontend.
    let network: Option<qcode_core::network::SharedNetwork> = egress
        .clone()
        .map(|egress| egress as qcode_core::network::SharedNetwork);
    let provider = build_provider(&resolved, egress.as_ref().map(|egress| egress.proxy_url()))?;
    let shell_env = std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::new(
        loaded.shell.clone(),
    ));
    let wiring = ToolWiring {
        network_env: network.as_ref().map(|network| network.env()),
        network: network.clone(),
        shell_env: shell_env.clone(),
    };
    // ADR-0108 §2: connect first, so the skill index can include what the
    // servers serve, then build the registry from that same connection. The
    // order is the whole point — connecting inside `build_tools` would start
    // every server twice.
    let mcp = connect_mcp(&loaded.mcp, cwd, &data_dir, sandbox.clone(), &wiring).await;
    // ADR-0108 §2: the index is built from the same connection, so a served
    // skill and the tool that loads it always come from one handshake.
    let (served, ttls) = collect_served_skill_listings(&mcp, &loaded.mcp, loaded.skills.mcp).await;
    // One index per session: `SkillSet` stays the immutable value it always
    // was, and the index is what lets a later `list_changed` swap one server's
    // contribution without rebuilding this assembly (ADR-0108 P1).
    let set = if served.is_empty() {
        (*load_skills(&loaded.skills, cwd)).clone()
    } else {
        let local = load_skills(&loaded.skills, cwd);
        (*local).clone().with_served(served_skills(served))
    };
    let skills = Arc::new(SkillIndex::new(set));
    // SEP-2640 defines no `list_changed` notification; a server's `ttlMs` is the
    // only hint that its catalogue moved. Refresh on that hint, and let the
    // revision bump tell the next turn that the prompt changed.
    if let Some(source) = mcp.source.clone() {
        spawn_skill_refresh(source, ttls, skills.clone());
    }
    let tools = build_tools(mcp, &loaded.search, &skills, sandbox, shell_env.clone()).await;
    let store = qcode_core::store::SessionStore::new(data_dir);
    Ok(Assembled {
        config: loaded,
        resolved,
        policy,
        store,
        workspace: cwd.to_path_buf(),
        tools,
        skills,
        shell_env,
        provider,
        egress,
        network,
        sandbox: sandbox_name,
        config_path: watched_config,
    })
}

#[cfg(test)]
mod install_secrets_tests {
    use super::*;

    /// 每个解析出来的凭据都必须进 redact —— 漏一个在任何其它测试里都看不见，
    /// 只会出现在日志文件里。
    ///
    /// 两条都要断言：只测 OAuth2 那条的话，`api_key` 哪天被漏掉同样没人发现；
    /// 只测 api_key 那条则正是 `log_redaction.rs` 已有的覆盖，而它对 OAuth2
    /// 一无所知。
    ///
    /// 用 `redact::current().secrets()` 读回集合，所以测的是「装进去了」而不是
    /// 「安装函数被调过」。同一个测试会污染进程级集合，因此两个值各带进程号
    /// 后缀，避免与别的测试里的短串互相吃（`Redactor` 会丢掉长度 < 4 的项）。
    #[test]
    fn both_the_api_key_and_the_oauth2_secret_reach_the_redactor() {
        let tag = std::process::id();
        let api_key = format!("sk-api-{tag}-MUST-NOT-LEAK");
        let oauth2_secret = format!("oauth2-secret-{tag}-MUST-NOT-LEAK");

        let mut resolved: config::Resolved = config::resolve(
            &config::Config::default(),
            &config::Overrides::default(),
            &|_| None,
        )
        .expect("mock resolves");
        resolved.api_key = Some(api_key.clone());
        resolved.oauth2_client_secret = Some(oauth2_secret.clone());

        install_secrets(&resolved);

        let redactor = redact::current();
        let registered = redactor.secrets();
        assert!(
            registered.iter().any(|secret| secret == &api_key),
            "the API key was never installed: {registered:?}"
        );
        assert!(
            registered.iter().any(|secret| secret == &oauth2_secret),
            "the OAuth2 client secret was never installed: {registered:?}"
        );
        // 端到端：真的被掩掉，而不只是躺在集合里。
        assert_eq!(redactor.redact(&api_key), "<redacted>");
        assert_eq!(redactor.redact(&oauth2_secret), "<redacted>");
    }

    /// 没配 OAuth2 时不该凭空注册什么（`None` 不能被当成空串塞进去）。
    #[test]
    fn a_configuration_without_oauth2_installs_nothing_extra() {
        let tag = std::process::id();
        let api_key = format!("sk-only-{tag}-MUST-NOT-LEAK");

        let mut resolved: config::Resolved = config::resolve(
            &config::Config::default(),
            &config::Overrides::default(),
            &|_| None,
        )
        .expect("mock resolves");
        resolved.api_key = Some(api_key.clone());
        resolved.oauth2_client_secret = None;

        install_secrets(&resolved);

        let redactor = redact::current();
        assert_eq!(redactor.redact(&api_key), "<redacted>");
        assert!(
            !redactor.secrets().iter().any(String::is_empty),
            "an empty string was registered as a secret"
        );
    }
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
        // 断言可操作的那一段（配置键），而不是某个语言的措辞：这条错误走
        // `tr()`，措辞跟着界面语言变，而中文里没有 "proxy" 这个词。
        assert!(error.to_string().contains("[network] enabled"), "{error}");
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
