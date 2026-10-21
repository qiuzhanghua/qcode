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

mod config;
mod extras;
mod tool;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use qcode_core::tool::Tool;
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

pub use config::{McpConfig, McpConfigError, McpServerConfig};
pub use extras::{Extra, McpExtra};
pub use tool::McpTool;

/// How long a server gets to answer `initialize`. `tools/list` and `tools/call`
/// use the per-server `timeout_secs` instead.
const HANDSHAKE_TIMEOUT_SECS: u64 = 20;

/// Builds the command that starts one server.
///
/// The CLI supplies this so a server runs under the same sandbox as
/// `run_shell`; `None` starts it directly. Keeping it a callback (rather than a
/// dependency on the tools crate) keeps this crate to one job: speaking MCP.
pub type Confine =
    dyn Fn(&OsStr, &[OsString], &Path) -> Result<std::process::Command, String> + Send + Sync;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("server '{server}' failed to start: {source}")]
    Spawn {
        server: String,
        source: std::io::Error,
    },
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
    servers: &[McpServerConfig],
    workspace: &Path,
    log_dir: &Path,
    env: &[(String, String)],
) -> Vec<Arc<dyn Tool>> {
    connect_all_with(servers, workspace, log_dir, env, None).await
}

/// `connect_all` with a sandbox: every server is started through `confine`.
pub async fn connect_all_with(
    servers: &[McpServerConfig],
    workspace: &Path,
    log_dir: &Path,
    env: &[(String, String)],
    confine: Option<&Confine>,
) -> Vec<Arc<dyn Tool>> {
    if let Err(error) = std::fs::create_dir_all(log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            %error,
            "cannot create the MCP log directory; server stderr will be inherited"
        );
    }

    let mut tools = Vec::new();
    for server in servers.iter().filter(|server| server.enabled) {
        match connect_server(server, workspace, log_dir, env, confine).await {
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

    let program = OsStr::new(&server.command);
    let args: Vec<OsString> = server.args.iter().map(OsString::from).collect();
    let command = match confine {
        Some(confine) => confine(program, &args, workspace).map_err(|detail| McpError::Spawn {
            server: server.name.clone(),
            source: std::io::Error::other(format!("sandbox refused to start it: {detail}")),
        })?,
        None => {
            let mut command = std::process::Command::new(program);
            command.args(&args).current_dir(workspace);
            command
        }
    };
    let mut command = Command::from(command);
    command
        // Config env first, then the egress variables: the proxy token must
        // win over a server's own HTTP(S)_PROXY.
        .envs(server.env.iter())
        .envs(env.iter().map(|(key, value)| (key, value)))
        .kill_on_drop(true);

    let transport = TokioChildProcess::builder(command)
        .stderr(stderr_log(server, log_dir))
        .spawn()
        .map_err(|source| McpError::Spawn {
            server: server.name.clone(),
            source,
        })?
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
