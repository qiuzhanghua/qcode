//! MCP client (tools, resources and prompts).
//!
//! Enabled servers are started when the tool registry is built, because
//! `tools/list` has to run before the model can pick a tool. Servers are spoken
//! to over stdio with the `rmcp` SDK; a server that fails to start, times out
//! or dies only loses its own tools. It never aborts the session.
//!
//! Resources and prompts are optional capabilities: the client asks for the
//! matching list once during the handshake and registers list/read tools only
//! when the server answers. Sampling and elicitation stay out.
//!
//! Each server gets its own [`GrantSet`] (ADR-0066): what it may read, what it
//! may write, and a synthetic `$HOME`. The CLI turns the grant into sandbox
//! roots through the [`Confine`] callback; this crate never names a backend.

mod config;
mod extras;
mod grant;
mod tool;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use qcode_core::tool::Tool;
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

pub use config::{CredentialPreset, McpConfig, McpConfigError, McpSandboxMode, McpServerConfig};
pub use extras::{Extra, McpExtra};
pub use grant::{GrantError, GrantInput, GrantSet, real_home};
pub use tool::McpTool;

/// How long a server gets to answer `initialize`. `tools/list` and `tools/call`
/// use the per-server `timeout_secs` instead.
const HANDSHAKE_TIMEOUT_SECS: u64 = 20;

/// Builds the command that starts one server under a sandbox.
///
/// The CLI supplies this so a server runs inside a sandbox built from its own
/// grant; `None` starts it directly. Keeping it a callback (rather than a
/// dependency on the tools crate) keeps this crate to one job: speaking MCP.
pub type Confine = dyn Fn(&OsStr, &[OsString], &Path, &GrantSet) -> Result<std::process::Command, String>
    + Send
    + Sync;

fn hint_text(hint: &Option<String>) -> String {
    match hint {
        Some(hint) => format!("\n  note: {hint}"),
        None => String::new(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("server '{server}' failed to start: {source}{}", hint_text(.hint))]
    Spawn {
        server: String,
        source: std::io::Error,
        /// What to try next, when we can tell (ADR-0066 §7).
        hint: Option<String>,
    },
    #[error(transparent)]
    Grant(#[from] GrantError),
    #[error("server '{server}' did not finish the handshake within {secs}s")]
    HandshakeTimeout { server: String, secs: u64 },
    #[error("server '{server}' handshake failed: {source}")]
    Handshake {
        server: String,
        source: Box<rmcp::service::ClientInitializeError>,
    },
    #[error("server '{server}' tools/list timed out after {secs}s")]
    ListTimeout { server: String, secs: u64 },
    #[error("server '{server}' tools/list failed: {source}")]
    ListTools {
        server: String,
        source: rmcp::ServiceError,
    },
}

/// Connect every enabled server and return its tools, named
/// `mcp__<server>__<tool>`.
///
/// Runtime failures are logged with `warn` (visible at the default log level)
/// and skipped; the returned list may therefore be shorter than the config.
pub async fn connect_all(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    env: &[(String, String)],
) -> Vec<Arc<dyn Tool>> {
    connect_all_with(mcp, workspace, data_dir, env, None).await
}

/// `connect_all` with a sandbox: every server is started through `confine`.
///
/// `data_dir` holds both the per-server logs (`logs/`) and the per-server
/// synthetic HOME (`mcp/<name>/home`), which is why it is not called `log_dir`
/// any more.
pub async fn connect_all_with(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    env: &[(String, String)],
    confine: Option<&Confine>,
) -> Vec<Arc<dyn Tool>> {
    let log_dir = data_dir.join("logs");
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            %error,
            "cannot create the MCP log directory; server stderr will be inherited"
        );
    }

    let home = real_home();
    let mut tools = Vec::new();
    for server in mcp.servers.iter().filter(|server| server.enabled) {
        let mode = server.sandbox_mode(mcp.sandbox);
        let grant = match GrantSet::derive(GrantInput {
            server,
            workspace,
            server_state: server.state_dir(data_dir),
            real_home: home.as_deref(),
            confined: mode.is_confined(),
        }) {
            Ok(grant) => grant,
            Err(error) => {
                tracing::warn!(server = %server.name, %error, "skipping MCP server");
                continue;
            }
        };
        tracing::info!(server = %server.name, grant = %grant.describe(), "mcp server grant");
        if !mode.is_confined() {
            tracing::warn!(
                server = %server.name,
                "sandbox = \"off\": this server runs without an OS sandbox"
            );
        }
        match connect_server(server, &grant, workspace, &log_dir, env, confine).await {
            Ok(mut found) => {
                tracing::info!(
                    server = %server.name,
                    tools = found.len(),
                    "mcp server ready"
                );
                tools.append(&mut found);
            }
            Err(error) => {
                tracing::warn!(server = %server.name, %error, "skipping MCP server");
            }
        }
    }
    tools
}

#[allow(clippy::result_large_err)]
async fn connect_server(
    server: &McpServerConfig,
    grant: &GrantSet,
    workspace: &Path,
    log_dir: &Path,
    env: &[(String, String)],
    confine: Option<&Confine>,
) -> Result<Vec<Arc<dyn Tool>>, McpError> {
    tracing::info!(
        server = %server.name,
        command = %server.command,
        args = ?server.args,
        "starting MCP server"
    );

    // The synthetic HOME has to exist before the sandbox is built: the Windows
    // backend canonicalizes every writable root and fails on a missing one.
    grant.prepare_home().map_err(|source| McpError::Spawn {
        server: server.name.clone(),
        source,
        hint: Some(format!(
            "cannot create the private HOME at {}",
            grant.home().display()
        )),
    })?;

    let program = OsStr::new(&server.command);
    let args: Vec<OsString> = server.args.iter().map(OsString::from).collect();
    let base = || {
        let mut command = std::process::Command::new(program);
        command.args(&args).current_dir(workspace);
        command
    };
    let command = match confine {
        // `sandbox = "off"` is an explicit opt-out, not a fallback after an
        // error: the closure is simply not consulted.
        Some(confine) if grant.is_confined() => {
            confine(program, &args, workspace, grant).map_err(|detail| McpError::Spawn {
                server: server.name.clone(),
                source: std::io::Error::other(format!("sandbox refused to start it: {detail}")),
                hint: None,
            })?
        }
        Some(_) => {
            tracing::warn!(
                server = %server.name,
                "sandbox = \"off\": starting without an OS sandbox"
            );
            base()
        }
        None => base(),
    };
    let mut command = Command::from(command);
    // Order matters (ADR-0068 §1): the whitelist and the synthetic HOME first,
    // the server's own `env` next, and the egress proxy last so a server cannot
    // redirect its traffic past qcode's proxy.
    command.env_clear();
    for (key, value) in grant.base_env() {
        command.env(key, value);
    }
    for (key, value) in &server.env {
        let value = grant::expand_env(&server.name, key, value)?;
        command.env(key, value);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command.kill_on_drop(true);

    let transport = TokioChildProcess::builder(command)
        .stderr(stderr_log(server, log_dir))
        .spawn()
        .map_err(|source| spawn_error(server, grant, source))?
        .0;

    let timeout = server.request_timeout();
    let service = tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        ().serve(transport),
    )
    .await
    .map_err(|_| McpError::HandshakeTimeout {
        server: server.name.clone(),
        secs: HANDSHAKE_TIMEOUT_SECS,
    })?
    .map_err(|source| McpError::Handshake {
        server: server.name.clone(),
        source: Box::new(source),
    })?;

    let listed = tokio::time::timeout(timeout, service.list_all_tools())
        .await
        .map_err(|_| McpError::ListTimeout {
            server: server.name.clone(),
            secs: server.timeout_secs,
        })?
        .map_err(|source| McpError::ListTools {
            server: server.name.clone(),
            source,
        })?;

    let service = Arc::new(service);
    let mut tools: Vec<Arc<dyn Tool>> = listed
        .into_iter()
        .map(|definition| {
            let name = server.tool_name(&definition.name);
            Arc::new(McpTool::new(
                name,
                server.name.clone(),
                definition,
                service.clone(),
                timeout,
            )) as Arc<dyn Tool>
        })
        .collect();

    // Resources and prompts are optional. Asking once is the detection: a
    // server that does not implement the capability answers with an error and
    // simply gets no extra tools.
    match tokio::time::timeout(timeout, service.list_resources(None)).await {
        Ok(Ok(_)) => {
            for kind in [Extra::ListResources, Extra::ReadResource] {
                tools.push(extra_tool(server, kind, &service, timeout));
            }
        }
        Ok(Err(error)) => tracing::debug!(server = %server.name, %error, "no resources capability"),
        Err(_) => tracing::warn!(
            server = %server.name,
            secs = server.timeout_secs,
            "resources/list timed out; skipping resource tools"
        ),
    }

    match tokio::time::timeout(timeout, service.list_prompts(None)).await {
        Ok(Ok(_)) => {
            for kind in [Extra::ListPrompts, Extra::GetPrompt] {
                tools.push(extra_tool(server, kind, &service, timeout));
            }
        }
        Ok(Err(error)) => tracing::debug!(server = %server.name, %error, "no prompts capability"),
        Err(_) => tracing::warn!(
            server = %server.name,
            secs = server.timeout_secs,
            "prompts/list timed out; skipping prompt tools"
        ),
    }

    Ok(tools)
}

/// A bare `Permission denied` was the hardest part of the original bug report
/// (ADR-0066 §7). When the sandbox is on and the process failed to start, say
/// which entry is most likely missing and how to add it.
fn spawn_error(server: &McpServerConfig, grant: &GrantSet, source: std::io::Error) -> McpError {
    let hint = match source.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            if grant.is_confined() =>
        {
            let program = Path::new(&server.command);
            let candidate = program
                .canonicalize()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .or_else(|| program.parent().map(Path::to_path_buf));
            Some(match candidate {
                Some(directory) => format!(
                    "the OS sandbox may have denied {}; add read_paths = [\"{}\"] to this \
                     server if that is intended",
                    program.display(),
                    directory.display()
                ),
                None => format!(
                    "the OS sandbox may have denied {}; add a read_paths entry for it if that \
                     is intended",
                    program.display()
                ),
            })
        }
        _ => None,
    };
    McpError::Spawn {
        server: server.name.clone(),
        source,
        hint,
    }
}

fn extra_tool(
    server: &McpServerConfig,
    kind: Extra,
    service: &Arc<rmcp::service::RunningService<rmcp::RoleClient, ()>>,
    timeout: std::time::Duration,
) -> Arc<dyn Tool> {
    Arc::new(McpExtra::new(
        server.tool_name(kind.suffix()),
        server.name.clone(),
        kind,
        service.clone(),
        timeout,
    ))
}

/// One log file per server, append-only. If it cannot be opened, inherit the
/// parent's stderr instead of losing the diagnostics.
fn stderr_log(server: &McpServerConfig, log_dir: &Path) -> Stdio {
    let file_name: String = server
        .name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let path = log_dir.join(format!("mcp-{file_name}.log"));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => Stdio::from(file),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot open the MCP log file; inheriting stderr"
            );
            Stdio::inherit()
        }
    }
}
