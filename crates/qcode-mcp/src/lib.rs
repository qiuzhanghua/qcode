//! MCP client (tools, resources and prompts).
//!
//! Enabled servers are connected when the tool registry is built, because
//! `tools/list` has to run before the model can pick a tool. Servers are
//! spoken to with the `rmcp` SDK over one of two transports (ADR-0067): a
//! child process on stdio, or a remote Streamable HTTP endpoint named by
//! `url`. A server that fails to start, times out or dies only loses its own
//! tools. It never aborts the session.
//!
//! Resources and prompts are optional capabilities: the client asks for the
//! matching list once during the handshake and registers list/read tools only
//! when the server answers. Sampling and elicitation stay out.
//!
//! Each stdio server gets its own [`GrantSet`] (ADR-0066): what it may read,
//! what it may write, and a synthetic `$HOME`. The CLI turns the grant into
//! sandbox roots through the [`Confine`] callback; this crate never names a
//! backend. A remote server has no process and therefore no grant — its only
//! boundary is the egress proxy, which is why the HTTP client must be the
//! caller's ([`ConnectOptions::http`]) and never one built here (ADR-0067 §2).

mod config;
mod extras;
mod grant;
mod tool;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use qcode_core::tool::Tool;
use rmcp::ServiceExt;
use rmcp::model::ProtocolVersion;
use rmcp::service::{ClientLifecycleMode, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use tokio::process::Command;

pub use config::{
    CredentialPreset, McpConfig, McpConfigError, McpSandboxMode, McpServerConfig, Transport,
};
pub use extras::{Extra, McpExtra};
pub use grant::{GrantError, GrantInput, GrantSet, real_home};
pub use tool::McpTool;

/// How long a server gets to answer `initialize`. `tools/list` and `tools/call`
/// use the per-server `timeout_secs` instead.
const HANDSHAKE_TIMEOUT_SECS: u64 = 20;

/// Protocol versions a remote server is asked about, most preferred first
/// (ADR-0067 §3).
///
/// Only `2026-07-28` is listed, so a server that answers `server/discover` but
/// offers only older versions is a hard `NoCompatibleProtocolVersion` rather
/// than a quiet downgrade — see the ADR's 待定 note. Legacy servers are found
/// by the `-32601` / timeout path instead, which is the case that actually
/// exists in the wild.
const PREFERRED_VERSIONS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];

/// What a client that falls back to `initialize` may claim: `initialize` cannot
/// answer a higher version (lifecycle moved into per-request `_meta` from
/// 2026-07-28 on). This is only the ceiling we declare; the server picks the
/// version it actually speaks.
const LEGACY_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// Builds the command that starts one server under a sandbox.
///
/// The CLI supplies this so a server runs inside a sandbox built from its own
/// grant; `None` starts it directly. Keeping it a callback (rather than a
/// dependency on the tools crate) keeps this crate to one job: speaking MCP.
pub type Confine = dyn Fn(&OsStr, &[OsString], &Path, &GrantSet) -> Result<std::process::Command, String>
    + Send
    + Sync;

/// Everything the caller has to supply that this crate must not decide for
/// itself.
///
/// Both fields are `Option` and both fail closed: a remote server with no
/// `http` client is an error, not a direct connection (ADR-0067 §2), and a
/// stdio server with no `confine` runs unconfined, which is what
/// `sandbox = "off"` already means.
///
/// Grouped in a struct rather than passed as two more positional parameters:
/// `Option<&Confine>` and `Option<&reqwest::Client>` next to each other are
/// trivial to swap and still type-check.
#[derive(Default, Clone, Copy)]
pub struct ConnectOptions<'a> {
    /// The egress client for `streamable-http` servers. `None` refuses to
    /// connect: building one here would bypass the allowlist, the audit log
    /// and the private-address block, silently.
    pub http: Option<&'a reqwest::Client>,
    /// The sandbox seam for `stdio` servers.
    pub confine: Option<&'a Confine>,
}

impl<'a> ConnectOptions<'a> {
    pub fn with_http(http: &'a reqwest::Client) -> Self {
        Self {
            http: Some(http),
            confine: None,
        }
    }

    pub fn with_confine(confine: &'a Confine) -> Self {
        Self {
            http: None,
            confine: Some(confine),
        }
    }
}

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
    /// ADR-0067 §2: without the caller's client there is no audited path, so
    /// there is no path at all.
    #[error(
        "server '{server}' is remote but qcode has no egress client: [network] is disabled, \
         so the request could not be audited or allowlisted — enable the egress proxy, or use \
         a stdio server"
    )]
    NoHttpClient { server: String },
    #[error(
        "server '{server}' needs the bearer token in ${var}, but it is not set in qcode's \
         environment"
    )]
    MissingToken { server: String, var: String },
    #[error("server '{server}' url is unusable: {detail}")]
    BadUrl { server: String, detail: String },
    #[error("server '{server}' has an unusable header {name:?}: {detail}")]
    BadHeader {
        server: String,
        name: String,
        detail: String,
    },
    /// `2026-07-28` can answer `tools/call` with `InputRequired` (MRTR). qcode
    /// has no multi-round trip for tools (ADR-0067 后果), so say so instead of
    /// retrying or pretending the call returned nothing.
    #[error(
        "server '{server}' asked for a multi-round tool call (MRTR InputRequired), which qcode \
         does not support; the raw request state was logged"
    )]
    InputRequired { server: String },
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
    connect_all_with(mcp, workspace, data_dir, env, ConnectOptions::default()).await
}

/// `connect_all` with the seams the caller owns: the egress client for remote
/// servers and the sandbox for stdio ones.
///
/// `data_dir` holds both the per-server logs (`logs/`) and the per-server
/// synthetic HOME (`mcp/<name>/home`), which is why it is not called `log_dir`
/// any more.
pub async fn connect_all_with(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    env: &[(String, String)],
    options: ConnectOptions<'_>,
) -> Vec<Arc<dyn Tool>> {
    let log_dir = data_dir.join("logs");
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            %error,
            "cannot create the MCP log directory; server stderr will be inherited"
        );
    }

    let mut tools = Vec::new();
    for server in mcp.servers.iter().filter(|server| server.enabled) {
        // `validate()` rejects both of these, so reaching either means the
        // caller skipped validation. Say so instead of guessing a transport.
        let Some(transport) = server.transport() else {
            tracing::warn!(
                server = %server.name,
                "sets both or neither of `command` and `url`; skipping (run the config check)"
            );
            continue;
        };
        match connect_server(
            server, transport, workspace, data_dir, &log_dir, env, options,
        )
        .await
        {
            Ok(mut found) => {
                tracing::info!(
                    server = %server.name,
                    transport = %transport,
                    protocol = found.protocol_version.as_deref().unwrap_or("-"),
                    tools = found.tools.len(),
                    "mcp server ready"
                );
                tools.append(&mut found.tools);
            }
            Err(error) => {
                tracing::warn!(server = %server.name, %transport, %error, "skipping MCP server");
            }
        }
    }
    tools
}

/// A connected server: its tools, and what the handshake settled on.
struct Connected {
    tools: Vec<Arc<dyn Tool>>,
    /// Negotiated protocol version, for the log. `None` when the server reports
    /// none, which only a non-conforming one does.
    protocol_version: Option<String>,
}

/// Dispatch on the transport, then hand both arms to [`finish`].
///
/// The grant is derived inside the stdio arm on purpose: a remote server has no
/// child process, so `GrantSet` does not apply to it and `validate()` rejects
/// carrying one (ADR-0067 §7).
#[allow(clippy::result_large_err)]
async fn connect_server(
    server: &McpServerConfig,
    transport: Transport,
    workspace: &Path,
    data_dir: &Path,
    log_dir: &Path,
    env: &[(String, String)],
    options: ConnectOptions<'_>,
) -> Result<Connected, McpError> {
    match transport {
        Transport::Stdio => {
            // `[mcp] sandbox` is crate-wide and the caller passed no config, so
            // the per-server setting is what applies here; `validate()` already
            // stopped a remote server from carrying one.
            let confined = server.sandbox.unwrap_or_default().is_confined();
            connect_stdio(
                server,
                workspace,
                data_dir,
                log_dir,
                env,
                options.confine,
                confined,
            )
            .await
        }
        Transport::StreamableHttp => connect_http(server, options.http).await,
    }
}

#[allow(clippy::result_large_err, clippy::too_many_arguments)]
async fn connect_stdio(
    server: &McpServerConfig,
    workspace: &Path,
    data_dir: &Path,
    log_dir: &Path,
    env: &[(String, String)],
    confine: Option<&Confine>,
    confined: bool,
) -> Result<Connected, McpError> {
    let command_line = server.command.as_deref().unwrap_or_default();
    let grant = GrantSet::derive(GrantInput {
        server,
        workspace,
        server_state: server.state_dir(data_dir),
        real_home: real_home().as_deref(),
        confined,
    })?;
    tracing::info!(server = %server.name, grant = %grant.describe(), "mcp server grant");
    if !confined {
        tracing::warn!(
            server = %server.name,
            "sandbox = \"off\": this server runs without an OS sandbox"
        );
    }
    tracing::info!(
        server = %server.name,
        command = %command_line,
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

    let program = OsStr::new(command_line);
    let args: Vec<OsString> = server.args.iter().map(OsString::from).collect();
    let base = || {
        let mut command = std::process::Command::new(program);
        command.args(&args).current_dir(workspace);
        command
    };
    let command = match confine {
        // `sandbox = "off"` is an explicit opt-out, not a fallback after an
        // error: the closure is simply not consulted.
        Some(confine) if grant.is_confined() => confine(program, &args, workspace, &grant)
            .map_err(|detail| McpError::Spawn {
                server: server.name.clone(),
                source: std::io::Error::other(format!("sandbox refused to start it: {detail}")),
                hint: None,
            })?,
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
        .map_err(|source| spawn_error(server, &grant, source))?
        .0;

    let service = tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        // `Auto` stays off for stdio (ADR-0067 实现注记): a stdio server that
        // answers an unknown method with a JSON-RPC error is detected as legacy
        // and falls back, but one that answers `{}` hits rmcp's
        // `ExpectedInitResult` arm — which is a hard error, not a fallback — and
        // one that stays silent costs the full 10s discover timeout on every
        // connection. `Initialize` is also exactly what the P1 acceptance for
        // wordcraft asks for.
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

    finish(server, service).await
}

/// Connect to a remote server over Streamable HTTP.
///
/// The client is the caller's (ADR-0067 §2): `reqwest::Client::default()` here
/// would bypass the egress allowlist, the `network.jsonl` audit and the
/// private-address block — silently, with "fewer lines than expected in the
/// audit file" as the only symptom. So a missing client is an error.
#[allow(clippy::result_large_err)]
async fn connect_http(
    server: &McpServerConfig,
    http: Option<&reqwest::Client>,
) -> Result<Connected, McpError> {
    let Some(client) = http else {
        return Err(McpError::NoHttpClient {
            server: server.name.clone(),
        });
    };
    let url = server.url.as_deref().unwrap_or_default();
    // Validated at load time; parse again only to fail closed if that was
    // skipped, and to keep the message useful.
    reqwest::Url::parse(url).map_err(|error| McpError::BadUrl {
        server: server.name.clone(),
        detail: error.to_string(),
    })?;

    let token = match &server.bearer_token_env_var {
        Some(var) => Some(std::env::var(var).map_err(|_| McpError::MissingToken {
            server: server.name.clone(),
            var: var.clone(),
        })?),
        None => None,
    };

    let mut headers: HashMap<http::HeaderName, http::HeaderValue> = HashMap::new();
    for (name, value) in &server.headers {
        let name =
            http::HeaderName::try_from(name.as_str()).map_err(|error| McpError::BadHeader {
                server: server.name.clone(),
                name: name.clone(),
                detail: error.to_string(),
            })?;
        let value =
            http::HeaderValue::try_from(value.as_str()).map_err(|error| McpError::BadHeader {
                server: server.name.clone(),
                name: name.to_string(),
                detail: error.to_string(),
            })?;
        headers.insert(name, value);
    }

    tracing::info!(
        server = %server.name,
        url = %url,
        token = server.bearer_token_env_var.is_some(),
        headers = headers.len(),
        "connecting to remote MCP server"
    );

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    // ADR-0067 §4. Both are rmcp's defaults today; stated explicitly because a
    // silent flip would break stateless servers (`allow_stateless`) and long
    // sessions against restarting servers (`reinit_on_expired_session`) with no
    // error message to go on.
    config.allow_stateless = true;
    config.reinit_on_expired_session = true;
    config.auth_header = token;
    config.custom_headers = headers;

    let transport =
        StreamableHttpClientTransport::<reqwest::Client>::with_client(client.clone(), config);

    let service = tokio::time::timeout(
        std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        // ADR-0067 §3: probe with `server/discover`, fall back to `initialize`
        // when the peer answers a correlated non-modern JSON-RPC error (the
        // `-32601` a 2025-06-18 server sends) or does not answer within rmcp's
        // own 10s discover timeout. Transport errors and modern rejections do
        // not fall back: retrying the handshake would fail the same way.
        rmcp::service::serve_client_with_lifecycle(
            (),
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: PREFERRED_VERSIONS.to_vec(),
                legacy_version: Some(LEGACY_VERSION),
            },
        ),
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

    finish(server, service).await
}

/// The transport-independent half: `tools/list`, then the optional resource and
/// prompt capabilities.
#[allow(clippy::result_large_err)]
async fn finish(
    server: &McpServerConfig,
    service: RunningService<rmcp::RoleClient, ()>,
) -> Result<Connected, McpError> {
    let timeout = server.request_timeout();
    let protocol_version = service
        .peer_info()
        .map(|info| info.protocol_version.to_string());

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

    Ok(Connected {
        tools,
        protocol_version,
    })
}

/// A bare `Permission denied` was the hardest part of the original bug report
/// (ADR-0066 §7). When the sandbox is on and the process failed to start, say
/// which entry is most likely missing and how to add it.
fn spawn_error(server: &McpServerConfig, grant: &GrantSet, source: std::io::Error) -> McpError {
    let hint = match source.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            if grant.is_confined() =>
        {
            let program = Path::new(server.command.as_deref().unwrap_or_default());
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
