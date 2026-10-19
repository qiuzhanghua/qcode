//! MCP client (tools only).
//!
//! Enabled servers are started when the tool registry is built, because
//! `tools/list` has to run before the model can pick a tool. Servers are spoken
//! to over stdio with the `rmcp` SDK; a server that fails to start, times out
//! or dies only loses its own tools. It never aborts the session.
//!
//! v1 deliberately supports the tools capability only: resources, prompts,
//! sampling and elicitation stay out.

mod config;
mod tool;

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use qcode_core::tool::Tool;
use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command;

pub use config::{McpConfig, McpConfigError, McpServerConfig};
pub use tool::McpTool;

/// How long a server gets to answer `initialize`. `tools/list` and `tools/call`
/// use the per-server `timeout_secs` instead.
const HANDSHAKE_TIMEOUT_SECS: u64 = 20;

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
    if let Err(error) = std::fs::create_dir_all(log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            %error,
            "cannot create the MCP log directory; server stderr will be inherited"
        );
    }

    let mut tools = Vec::new();
    for server in servers.iter().filter(|server| server.enabled) {
        match connect_server(server, workspace, log_dir, env).await {
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

async fn connect_server(
    server: &McpServerConfig,
    workspace: &Path,
    log_dir: &Path,
    env: &[(String, String)],
) -> Result<Vec<Arc<dyn Tool>>, McpError> {
    tracing::info!(
        server = %server.name,
        command = %server.command,
        args = ?server.args,
        "starting MCP server"
    );

    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .current_dir(workspace)
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
    Ok(listed
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
        .collect())
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
