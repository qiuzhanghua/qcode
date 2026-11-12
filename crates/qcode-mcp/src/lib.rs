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
mod table;
mod tool;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::tool::{ServerPrompt, ServerToolsChanged, Tool, ToolRefresh, ToolSource};
use rmcp::ServiceExt;
use rmcp::model::{ProtocolVersion, ServerCapabilities};
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
pub use grant::{GrantError, GrantInput, GrantSet, SearchPath, real_home};
pub use table::{ListChangedHandler, ToolTable};
pub use tool::McpTool;

/// The connected-client type. Not `RunningService<RoleClient, ()>` any more:
/// the handler has to see `notifications/tools/list_changed` (ADR-0067 §5).
pub(crate) type Service = Arc<RunningService<rmcp::RoleClient, ListChangedHandler>>;

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
    /// The `PATH` a bare `command` is resolved in and the server is started
    /// with (ADR-0070). `None` means qcode's own `PATH`, which is what every
    /// caller got before shell environment sampling reached the MCP side.
    ///
    /// A `&OsStr` rather than the sampler's own type: this crate must not
    /// depend on `qcode-tools` (ADR-0066 §1), and the value is the whole of
    /// what is needed anyway.
    pub path: Option<&'a std::ffi::OsStr>,
}

impl<'a> ConnectOptions<'a> {
    pub fn with_http(http: &'a reqwest::Client) -> Self {
        Self {
            http: Some(http),
            confine: None,
            path: None,
        }
    }

    pub fn with_confine(confine: &'a Confine) -> Self {
        Self {
            http: None,
            confine: Some(confine),
            path: None,
        }
    }

    /// Resolve bare commands against `path` and start servers with it.
    pub fn with_path(mut self, path: &'a std::ffi::OsStr) -> Self {
        self.path = Some(path);
        self
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
    connect_detailed(mcp, workspace, data_dir, env, options)
        .await
        .tools
}

/// What a successful [`connect_detailed`] hands back.
pub struct McpConnection {
    /// The tools as of the moment of connection.
    pub tools: Vec<Arc<dyn Tool>>,
    /// The same servers as a refreshable source, for callers that want the tool
    /// table to follow `ttlMs` / `list_changed` (ADR-0067 §5).
    ///
    /// `None` when no server connected: adding an empty source would just make
    /// the engine do bookkeeping for nothing.
    pub source: Option<Arc<McpToolSource>>,
}

/// `connect_all_with`, but keeping the servers around so their tool tables can
/// be re-read later.
pub async fn connect_detailed(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    env: &[(String, String)],
    options: ConnectOptions<'_>,
) -> McpConnection {
    let log_dir = qcode_core::paths::log_dir(data_dir);
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            %error,
            "cannot create the MCP log directory; server stderr will be inherited"
        );
    }

    let mut tools = Vec::new();
    let mut servers = Vec::new();
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
            Ok(found) => {
                tracing::info!(
                    server = %server.name,
                    transport = %transport,
                    protocol = found.protocol_version.as_deref().unwrap_or("-"),
                    tools = found.tools().len(),
                    "mcp server ready"
                );
                tools.extend(found.tools());
                servers.push(found);
            }
            Err(error) => {
                tracing::warn!(server = %server.name, %transport, %error, "skipping MCP server");
            }
        }
    }
    let source = (!servers.is_empty()).then(|| Arc::new(McpToolSource { servers }));
    McpConnection { tools, source }
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
) -> Result<ServerHandle, McpError> {
    match transport {
        Transport::Stdio => {
            // `[mcp] sandbox` is crate-wide and the caller passed no config, so
            // the per-server setting is what applies here; `validate()` already
            // stopped a remote server from carrying one.
            let confined = server.sandbox.unwrap_or_default().is_confined();
            connect_stdio(server, workspace, data_dir, log_dir, env, confined, options).await
        }
        Transport::StreamableHttp => connect_http(server, options.http).await,
    }
}

#[allow(clippy::result_large_err)]
async fn connect_stdio(
    server: &McpServerConfig,
    workspace: &Path,
    data_dir: &Path,
    log_dir: &Path,
    env: &[(String, String)],
    confined: bool,
    options: ConnectOptions<'_>,
) -> Result<ServerHandle, McpError> {
    let ConnectOptions { confine, path, .. } = options;
    // ADR-0070: one value for both halves. `Command::new` gets the bare name
    // below, so the child's `PATH` is what actually finds it; if the grant
    // resolved with a different `PATH`, the sandbox would be built for a file
    // other than the one that runs.
    let path = SearchPath::from(path);
    let command_line = server.command.as_deref().unwrap_or_default();
    let grant = GrantSet::derive(GrantInput {
        server,
        workspace,
        server_state: server.state_dir(data_dir),
        real_home: real_home().as_deref(),
        confined,
        path,
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
    // ADR-0070, and it has to sit here rather than inside `base_env`: this is
    // the `PATH` the grant was derived with, while `base_env`'s copy is
    // qcode's own. Replacing it after the whitelist but before `server.env`
    // keeps a server that writes `PATH` itself in charge of its own.
    //
    // Guarded on purpose: `PATH=""` is not the same as an absent `PATH` — an
    // empty value makes `execvp` find nothing at all, while an absent one lets
    // it fall back to the system default.
    let child_path = path.as_os_str();
    if !child_path.is_empty() {
        command.env("PATH", child_path);
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
        ListChangedHandler::new().serve(transport),
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
) -> Result<ServerHandle, McpError> {
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
            ListChangedHandler::new(),
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

/// One connected server, kept alive so its tool table can be re-read at a turn
/// boundary (ADR-0067 §5).
///
/// Holding the service and the table together is what makes the refresh
/// possible: a bare `Vec<Arc<dyn Tool>>` would have to be thrown away and
/// rebuilt by starting the server again.
pub(crate) struct ServerHandle {
    name: String,
    service: Service,
    /// The tool table and its freshness deadline.
    table: ToolTable,
    timeout: Duration,
    /// Resource/prompt tools, decided once at connect time. The capabilities do
    /// not change mid-session, and re-probing them every turn would cost two
    /// round trips per server for a set that never moves.
    extras: Vec<Extra>,
    /// What the handshake declared. Kept because `qc doctor --probe` compares it
    /// with what the wire actually answers (ADR-0068 §4, B-2).
    capabilities: ServerCapabilities,
    protocol_version: Option<String>,
    /// The server's own `instructions`, from whichever handshake answered
    /// (ADR-0068 §3). `None` when the server sent none.
    instructions: Option<String>,
}

impl ServerHandle {
    /// The tools this server currently offers, namespaced for the model.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = self
            .table
            .tools()
            .into_iter()
            .map(|definition| {
                let name = config::tool_name(&self.name, &definition.name);
                Arc::new(McpTool::new(
                    name,
                    self.name.clone(),
                    definition,
                    self.service.clone(),
                    self.timeout,
                )) as Arc<dyn Tool>
            })
            .collect();
        for kind in &self.extras {
            tools.push(Arc::new(McpExtra::new(
                config::tool_name(&self.name, kind.suffix()),
                self.name.clone(),
                *kind,
                self.service.clone(),
                self.timeout,
            )));
        }
        tools
    }

    /// Re-read `tools/list`. Returns the changed names (bare, server-side), or
    /// `None` when the set is identical.
    #[allow(clippy::result_large_err)]
    async fn reload(&self) -> Result<Option<(Vec<String>, Vec<String>)>, McpError> {
        let (tools, ttl) = fetch_tools(&self.name, &self.service, self.timeout).await?;
        Ok(self.table.replace(tools, ttl))
    }
}

/// Every connected server, exposed as one refreshable tool source.
pub struct McpToolSource {
    servers: Vec<ServerHandle>,
}

#[async_trait]
impl ToolSource for McpToolSource {
    fn prompts(&self) -> Vec<ServerPrompt> {
        self.servers
            .iter()
            .filter_map(|server| {
                let text = server.instructions.as_deref()?.trim();
                // An empty or whitespace-only string would produce an empty
                // section, which is tokens spent on a heading and nothing else.
                (!text.is_empty()).then(|| ServerPrompt {
                    server: server.name.clone(),
                    text: text.to_string(),
                })
            })
            .collect()
    }

    async fn refresh(&self) -> ToolRefresh {
        let mut tools = Vec::new();
        let mut changed = Vec::new();
        for server in &self.servers {
            if server.table.is_stale() {
                match server.reload().await {
                    Ok(Some((added, removed))) => changed.push(ServerToolsChanged {
                        server: server.name.clone(),
                        added: added
                            .iter()
                            .map(|name| config::tool_name(&server.name, name))
                            .collect(),
                        removed: removed
                            .iter()
                            .map(|name| config::tool_name(&server.name, name))
                            .collect(),
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        // SEP-2549 permits serving the stale result when a
                        // re-fetch fails, and losing every tool of a server
                        // because the network blipped would be worse than a
                        // table that is one turn old. `dirty` stays set, so the
                        // next turn tries again.
                        tracing::warn!(
                            server = %server.name,
                            %error,
                            "tools/list refresh failed; keeping the previous table"
                        );
                    }
                }
            }
            tools.extend(server.tools());
        }
        ToolRefresh { tools, changed }
    }
}

/// `tools/list`, following `nextCursor`.
///
/// Written out rather than using `Peer::list_all_tools`, which returns
/// `Vec<Tool>` and drops `ttl_ms` / `cache_scope` / `result_type` — the first of
/// which is the entire point (ADR-0067 §5).
#[allow(clippy::result_large_err)]
async fn fetch_tools(
    server: &str,
    service: &Service,
    timeout: Duration,
) -> Result<table::Fetched, McpError> {
    /// A server that keeps handing back new cursors would otherwise page until
    /// the turn budget dies. 64 pages of a normal table is far past "this is a
    /// real server".
    const MAX_PAGES: usize = 64;
    let mut accumulated: table::Fetched = (Vec::new(), None);
    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..MAX_PAGES {
        let result = tokio::time::timeout(
            timeout,
            service.list_tools(table::page_params(cursor.clone())),
        )
        .await
        .map_err(|_| McpError::ListTimeout {
            server: server.to_string(),
            secs: timeout.as_secs(),
        })?
        .map_err(|source| McpError::ListTools {
            server: server.to_string(),
            source,
        })?;
        table::absorb_page(&mut accumulated, result.ttl_ms, result.tools);
        let Some(next) = table::next_cursor(result.next_cursor) else {
            return Ok(accumulated);
        };
        // A cursor we have already followed would loop forever.
        if seen.contains(&next) {
            tracing::warn!(server, cursor = %next, "tools/list repeated a cursor; stopping");
            return Ok(accumulated);
        }
        seen.push(next.clone());
        cursor = Some(next);
    }
    tracing::warn!(
        server,
        pages = MAX_PAGES,
        "tools/list did not finish paging"
    );
    Ok(accumulated)
}

/// The transport-independent half: `tools/list`, then the optional resource and
/// prompt capabilities.
#[allow(clippy::result_large_err)]
async fn finish(
    server: &McpServerConfig,
    service: RunningService<rmcp::RoleClient, ListChangedHandler>,
) -> Result<ServerHandle, McpError> {
    let timeout = server.request_timeout();
    // `peer_info` carries what the handshake settled on, and both lifecycles
    // fill it: `InitializeResult` for legacy, `DiscoverResult` for 2026-07-28
    // (rmcp converts both into `ServerPeerInfo`). Reading it after the
    // handshake therefore covers either path without a second request.
    let info = service.peer_info();
    let protocol_version = info.as_ref().map(|info| info.protocol_version.to_string());
    let instructions = info.as_ref().and_then(|info| info.instructions.clone());
    // Taken before the service is moved into an `Arc`: the flag is what the
    // notification handler writes.
    let dirty = service.service().handle();

    let service = Arc::new(service);
    let (listed, ttl) = fetch_tools(&server.name, &service, timeout).await?;
    tracing::info!(
        server = %server.name,
        ttl_ms = ttl.map(|ttl| ttl.as_millis()).unwrap_or(0),
        // ADR-0067 §5: `cacheScope` has no extra meaning here — qcode keeps no
        // cross-process tool cache, so `public` and `private` behave the same.
        "mcp tool table fetched"
    );
    let table = ToolTable::new(listed, ttl, dirty);

    // Resources and prompts are optional, and the handshake already said which
    // ones exist (ADR-0068 §4, revised): the declaration is the answer for every
    // protocol version, not just 2026-07-28.
    //
    // Why the old per-version split went away: the reason for distrusting a
    // legacy server's `capabilities` was a guess, and wordcraft-cli 0.15.2 — a
    // real 2025-06-18 server — declares exactly what it serves. Meanwhile the
    // probe cost a `-32601` round trip per server on every connect and could
    // never detect the case that actually hurts (a server that *serves* a
    // capability it forgot to declare, whose tools were silently missing).
    // That case is what `qc doctor --probe` exists for, on demand.
    let capabilities = info
        .as_ref()
        .map(|info| info.capabilities.clone())
        .unwrap_or_default();
    if info.is_none() {
        // A successful handshake always fills `peer_info`; if it somehow did
        // not, nothing is registered rather than guessed. `qc doctor --probe`
        // is how to find out what the server really serves.
        tracing::warn!(
            server = %server.name,
            "handshake left no capabilities; registering no resource/prompt tools (try `qc doctor --probe`)"
        );
    }
    let (resources, prompts) = declared_extras(&capabilities);
    let mut extras = Vec::new();
    if resources {
        extras.extend([Extra::ListResources, Extra::ReadResource]);
    }
    if prompts {
        extras.extend([Extra::ListPrompts, Extra::GetPrompt]);
    }
    tracing::info!(
        server = %server.name,
        protocol = info
            .as_ref()
            .map(|info| info.protocol_version.to_string())
            .unwrap_or_else(|| "-".to_string()),
        resources,
        prompts,
        "mcp capabilities read from the handshake"
    );

    if let Some(text) = instructions.as_deref().map(str::trim) {
        // Name and size only: the text itself is third-party content and the
        // log outlives the session (the same reasoning as ADR-0064's variable
        // *names*).
        tracing::info!(
            server = %server.name,
            bytes = text.len(),
            "mcp server supplied instructions"
        );
    }

    Ok(ServerHandle {
        name: server.name.clone(),
        service,
        table,
        timeout,
        extras,
        capabilities,
        protocol_version,
        instructions,
    })
}

/// Which extra tools a declared capability set justifies (ADR-0068 §4).
///
/// Pure, so the mapping is testable without a server: the rule is one-way —
/// a declared capability registers the tools, an undeclared one does not. There
/// is no negotiation and no guessing.
fn declared_extras(capabilities: &ServerCapabilities) -> (bool, bool) {
    (
        capabilities.resources.is_some(),
        capabilities.prompts.is_some(),
    )
}

/// What the server said about itself in the handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

impl DeclaredCapabilities {
    fn from_wire(capabilities: &ServerCapabilities) -> Self {
        let (resources, prompts) = declared_extras(capabilities);
        Self {
            tools: capabilities.tools.is_some(),
            resources,
            prompts,
        }
    }

    /// The capabilities this declaration registers tools for, in the order the
    /// probe writes them.
    fn registered(&self) -> Vec<Extra> {
        let mut extras = Vec::new();
        if self.resources {
            extras.extend([Extra::ListResources, Extra::ReadResource]);
        }
        if self.prompts {
            extras.extend([Extra::ListPrompts, Extra::GetPrompt]);
        }
        extras
    }
}

/// What `*/list` actually answered when asked (ADR-0068 §4, B-2).
///
/// Distinguishing these three matters: `Refused` next to `declared: true` is a
/// server whose tools exist but fail at call time, while `Supported` next to
/// `declared: false` is a server whose tools are silently missing. Both are
/// mismatches, and both are invisible without this distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// The server answered `*/list` with a result.
    Supported,
    /// The server answered with a JSON-RPC error — how a server without the
    /// capability replies, and also how a broken one does.
    Refused(String),
    /// No answer inside the server's `timeout_secs`.
    TimedOut,
}

impl ProbeAnswer {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supported => "answers",
            Self::Refused(_) => "refuses",
            Self::TimedOut => "timed out",
        }
    }

    /// Whether declaration and wire agree.
    pub fn agrees_with(&self, declared: bool) -> bool {
        match self {
            Self::Supported => declared,
            Self::Refused(_) | Self::TimedOut => !declared,
        }
    }
}

/// One server as `qc doctor --probe` found it.
///
/// This is the diagnostic that replaced connect-time probing: the connection
/// itself trusts the handshake, and this reports where the handshake lies.
#[derive(Debug, Clone)]
pub struct CapabilityProbe {
    pub server: String,
    pub transport: Option<Transport>,
    /// Version actually negotiated, `None` when the handshake failed.
    pub protocol_version: Option<String>,
    pub declared: DeclaredCapabilities,
    /// The extra tools that declaration made qcode register.
    pub registered: Vec<Extra>,
    pub resources: ProbeAnswer,
    pub prompts: ProbeAnswer,
    pub instructions_bytes: Option<usize>,
    /// Why the server could not be reached at all. When set, every field above
    /// but `server`/`transport` is meaningless.
    pub error: Option<String>,
}

impl CapabilityProbe {
    fn unreachable(server: &McpServerConfig, error: String) -> Self {
        Self {
            server: server.name.clone(),
            transport: server.transport(),
            protocol_version: None,
            declared: DeclaredCapabilities::default(),
            registered: Vec::new(),
            resources: ProbeAnswer::TimedOut,
            prompts: ProbeAnswer::TimedOut,
            instructions_bytes: None,
            error: Some(error),
        }
    }
}

/// Connect to every enabled server and compare its declaration with the wire.
///
/// Deliberately separate from [`connect_detailed`]: a session must not pay two
/// round trips per server on every start (that probe is what ADR-0033 §3 did
/// and ADR-0068 §4 removed), but "is this server lying about what it serves?" is
/// a real question, asked rarely and on purpose.
///
/// Both lists are always asked, whatever the declaration says — including when
/// it says no. Skipping the undeclared ones would make the interesting case
/// (serves but does not declare) undetectable by construction.
pub async fn probe_capabilities(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    env: &[(String, String)],
    options: ConnectOptions<'_>,
) -> Vec<CapabilityProbe> {
    let log_dir = qcode_core::paths::log_dir(data_dir);
    let mut reports = Vec::new();
    for server in mcp.servers.iter().filter(|server| server.enabled) {
        let Some(transport) = server.transport() else {
            reports.push(CapabilityProbe::unreachable(
                server,
                "sets both or neither of `command` and `url`".to_string(),
            ));
            continue;
        };
        match connect_server(
            server, transport, workspace, data_dir, &log_dir, env, options,
        )
        .await
        {
            Ok(found) => {
                let timeout = found.timeout;
                let declared = DeclaredCapabilities::from_wire(&found.capabilities);
                let resources = probe_resources(&found.service, timeout).await;
                let prompts = probe_prompts(&found.service, timeout).await;
                reports.push(CapabilityProbe {
                    server: server.name.clone(),
                    transport: server.transport(),
                    protocol_version: found.protocol_version.clone(),
                    registered: declared.registered(),
                    declared,
                    resources,
                    prompts,
                    instructions_bytes: found
                        .instructions
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::len),
                    error: None,
                });
            }
            Err(error) => {
                reports.push(CapabilityProbe::unreachable(server, error.to_string()));
            }
        }
    }
    reports
}

/// Ask one server for `resources/list` and report what came back.
///
/// `false`-shaped answers are not errors here: "this server has no resources" is
/// an ordinary state that the caller reports, not a problem to log at `warn`.
async fn probe_resources(service: &Service, timeout: std::time::Duration) -> ProbeAnswer {
    match tokio::time::timeout(timeout, service.list_resources(None)).await {
        Ok(Ok(_)) => ProbeAnswer::Supported,
        Ok(Err(error)) => ProbeAnswer::Refused(error.to_string()),
        Err(_) => ProbeAnswer::TimedOut,
    }
}

/// The same for `prompts/list`.
async fn probe_prompts(service: &Service, timeout: std::time::Duration) -> ProbeAnswer {
    match tokio::time::timeout(timeout, service.list_prompts(None)).await {
        Ok(Ok(_)) => ProbeAnswer::Supported,
        Ok(Err(error)) => ProbeAnswer::Refused(error.to_string()),
        Err(_) => ProbeAnswer::TimedOut,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0068 §4: the declared capability is the whole answer, and the
    /// mapping is one-to-one. Written as a table because the failure mode to
    /// catch later is "someone added a third capability and forgot it here".
    #[test]
    fn declared_capabilities_map_to_extras() {
        let caps = |resources: bool, prompts: bool| {
            let mut capabilities = ServerCapabilities::default();
            if resources {
                capabilities.resources = Some(rmcp::model::ResourcesCapability::default());
            }
            if prompts {
                capabilities.prompts = Some(rmcp::model::PromptsCapability::default());
            }
            capabilities
        };

        assert_eq!(declared_extras(&caps(false, false)), (false, false));
        assert_eq!(declared_extras(&caps(true, false)), (true, false));
        assert_eq!(declared_extras(&caps(false, true)), (false, true));
        assert_eq!(declared_extras(&caps(true, true)), (true, true));
    }

    /// A modern peer is defined by the lifecycle, not by a version string we
    /// compare ourselves: `has_initialize()` is rmcp's answer to "did
    /// `server/discover` answer this connection".
    #[test]
    fn only_the_no_initialize_lifecycle_counts_as_modern() {
        assert!(!ProtocolVersion::V_2026_07_28.has_initialize());
        for legacy in [
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2024_11_05,
        ] {
            assert!(legacy.has_initialize(), "{legacy} must probe");
        }
    }
}
