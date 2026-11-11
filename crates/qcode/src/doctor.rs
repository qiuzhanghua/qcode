//! `qc doctor`: answer "why doesn't it run?" in one command.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use futures_util::StreamExt;
use qcode_core::Network;
use qcode_core::event::ContentBlock;
use qcode_core::provider::{CompletionRequest, Message, ProviderError, Role};
use qcode_mcp::McpServerConfig;
use qcode_tools::sandbox::{self, SandboxMode, SandboxNetwork};
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};

pub async fn run(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    probe: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut doctor = Doctor { failures: 0 };

    println!("qcode {}", env!("CARGO_PKG_VERSION"));
    println!();

    doctor.check_data_dir(&data_dir);
    let config = doctor.check_config(&config_path);
    doctor.check_search(&config);
    doctor.check_update(&config, &data_dir);
    doctor.check_provider(&config);
    doctor.check_connectivity(&config, &data_dir).await;
    doctor.check_toolchain();
    #[cfg(windows)]
    doctor.check_token();
    doctor.check_sandbox(&config);
    doctor.check_mcp(&config, &data_dir);
    if probe {
        doctor.check_mcp_capabilities(&config, &data_dir).await;
    }
    doctor.ok(
        "acp",
        format!(
            "protocol v{} over stdio (`qc acp`)",
            qcode_acp::PROTOCOL_VERSION
        ),
    );

    println!();
    if doctor.failures == 0 {
        println!("all checks passed");
    } else {
        println!("{} check(s) failed", doctor.failures);
    }

    Ok(doctor.failures == 0)
}

struct Doctor {
    failures: usize,
}

/// Where to keep the key so it is not a plaintext file or registry value.
///
/// The hints are platform-specific because the mechanisms are: Keychain on
/// macOS, libsecret on Linux, the credential store on Windows. The API key
/// itself is never printed, only the command that would store it.
fn key_store_hint() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "store it: security add-generic-password -a \"$USER\" -s qcode -w".to_string(),
            "then: api_key_command = [\"security\", \"find-generic-password\", \"-w\", \"-s\", \"qcode\", \"-a\", \"$USER\"]".to_string(),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            "store it: secret-tool store --label qcode service qcode user deepseek".to_string(),
            "then: api_key_command = [\"secret-tool\", \"lookup\", \"service\", \"qcode\", \"user\", \"deepseek\"]".to_string(),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![
            "store it: Settings > provider > key (CredWriteW, secret piped over stdin)".to_string(),
            "then: api_key_command = [\"powershell\", \"-NoProfile\", \"-Command\", \"...\"]"
                .to_string(),
            "avoid: cmdkey /pass:<key>, which puts the key in the command line".to_string(),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}
impl Doctor {
    /// Prove the config directory accepts a new file: a policy can allow
    /// rewriting config.toml while denying new files beside it, which is exactly
    /// the failure the settings dialog hit.
    fn write_probe(&mut self, path: &std::path::Path) {
        let Some(dir) = path.parent() else {
            self.fail("config writable", "config path has no parent directory");
            return;
        };
        if config::dir_accepts_new_files(dir) {
            self.ok("config writable", dir.display().to_string());
        } else {
            self.fail(
                "config writable",
                format!("{}: {}", dir.display(), config::CONFIG_DIR_HINT),
            );
        }
    }

    fn ok(&self, label: &str, detail: impl AsRef<str>) {
        println!("[ok]   {label}: {}", detail.as_ref());
    }

    fn warn(&self, label: &str, detail: impl AsRef<str>) {
        println!("[warn] {label}: {}", detail.as_ref());
    }

    fn fail(&mut self, label: &str, detail: impl AsRef<str>) {
        self.failures += 1;
        println!("[fail] {label}: {}", detail.as_ref());
    }

    fn check_data_dir(&mut self, explicit: &Option<PathBuf>) {
        let dir = qcode_kernel::resolve_data_dir(explicit.clone());
        let result = std::fs::create_dir_all(&dir).and_then(|()| {
            let probe = dir.join(".qcode-doctor-write");
            std::fs::write(&probe, b"ok")?;
            std::fs::remove_file(&probe)
        });

        match result {
            Ok(()) => self.ok("data dir", dir.display().to_string()),
            Err(err) => self.fail("data dir", format!("{}: {err}", dir.display())),
        }
    }

    fn check_config(&mut self, explicit: &Option<PathBuf>) -> Config {
        let explicit = explicit
            .clone()
            .or_else(|| std::env::var("QCODE_CONFIG").ok().map(PathBuf::from));

        let path = match &explicit {
            Some(path) => path.clone(),
            None => match config::default_path() {
                Ok(path) => path,
                Err(err) => {
                    self.fail("config", err.to_string());
                    return Config::default();
                }
            },
        };

        if !path.exists() {
            if explicit.is_some() {
                self.fail("config", format!("not found: {}", path.display()));
            } else {
                self.warn(
                    "config",
                    format!("not found: {} (using defaults)", path.display()),
                );
            }
            return Config::default();
        }

        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match config::load_merged(&path, &workspace) {
            Ok(config) => {
                self.ok("config", path.display().to_string());
                self.write_probe(&path);
                self.ok(
                    "config schema",
                    format!(
                        "version {} (this build supports {})",
                        config.schema,
                        config::CONFIG_VERSION
                    ),
                );
                if workspace.join(config::PROJECT_CONFIG_RELATIVE).exists() {
                    self.ok(
                        "project config",
                        workspace
                            .join(config::PROJECT_CONFIG_RELATIVE)
                            .display()
                            .to_string(),
                    );
                }
                config
            }
            Err(err) => {
                self.fail("config", err.to_string());
                Config::default()
            }
        }
    }

    fn check_search(&mut self, config: &Config) {
        if !config.search.enabled {
            self.ok("web_search", "disabled ([search] enabled = false)");
            return;
        }
        let settings = config.search.settings(&|key| std::env::var(key).ok());
        if settings.is_usable() {
            self.ok(
                "web_search",
                format!("backend '{}' is ready", settings.backend.name()),
            );
        } else if settings.backend == qcode_tools::web_search::SearchBackend::Searxng {
            self.warn(
                "web_search",
                "backend 'searxng' needs [search] base_url; the tool is not registered",
            );
        } else {
            self.warn(
                "web_search",
                format!(
                    "export {} (or set [search] api_key_env); the tool is not registered",
                    config.search.key_env_name().unwrap_or_default()
                ),
            );
        }
    }

    fn check_provider(&mut self, config: &Config) {
        match config::resolve(config, &config::Overrides::default(), &|key| {
            std::env::var(key).ok()
        }) {
            Ok(resolved) => {
                self.ok("provider", &resolved.name);
                self.ok("model", &resolved.model);
                if let Some(base_url) = &resolved.base_url {
                    self.ok("base url", base_url);
                }
                if resolved.is_mock {
                    self.warn("credentials", "mock provider; no network calls");
                } else if resolved.api_key.is_some() {
                    // The source, never the value: `env NAME`, `command NAME`.
                    self.ok(
                        "credentials",
                        format!("API key present ({})", resolved.api_key_source),
                    );
                } else {
                    self.ok("credentials", "keyless provider");
                }
                self.check_pricing(config, &resolved.model);
            }
            Err(err) => {
                let missing_key =
                    matches!(err, qcode_kernel::config::ConfigError::MissingKey { .. });
                self.fail("provider", err.to_string());
                if missing_key {
                    for line in key_store_hint() {
                        self.warn("key storage", &line);
                    }
                }
            }
        }
    }

    /// Prices are never guessed (ADR-0065 决定 7): say what `--max-cost` will
    /// do instead of inventing a number.
    fn check_pricing(&mut self, config: &Config, model: &str) {
        let limit = config.agent.max_cost_usd;
        match config.pricing.get(model) {
            Some(price) if price.validate() => {
                if limit.is_some() {
                    self.ok(
                        "pricing",
                        format!("{model} declared; `--max-cost` stops at the limit"),
                    );
                } else {
                    self.ok(
                        "pricing",
                        format!(
                            "{model} declared; set `--max-cost` or `[agent] max_cost_usd` to actually stop"
                        ),
                    );
                }
            }
            Some(_) => self.warn(
                "pricing",
                format!("the `[pricing.{model}]` entry is incomplete; it is ignored"),
            ),
            None => self.warn(
                "pricing",
                format!(
                    "no `[pricing.{model}]` entry; `--max-cost` cannot stop the run. \
                     Prices are not guessed — declare the rates or ignore this line"
                ),
            ),
        }
    }

    async fn check_connectivity(&mut self, config: &Config, data_dir: &Option<PathBuf>) {
        let resolved = match config::resolve(config, &config::Overrides::default(), &|key| {
            std::env::var(key).ok()
        }) {
            Ok(resolved) => resolved,
            // check_provider already reported the resolution failure.
            Err(_) => return,
        };
        if resolved.is_mock {
            self.ok("connectivity", "skipped (mock provider)");
            return;
        }

        let dir = qcode_kernel::resolve_data_dir(data_dir.clone());
        let egress =
            match qcode_kernel::build_egress(config, &dir, resolved.base_url.as_deref()).await {
                Ok(egress) => egress,
                Err(err) => {
                    self.fail("connectivity", format!("cannot start egress proxy: {err}"));
                    return;
                }
            };
        let provider = match qcode_kernel::build_provider(
            &resolved,
            egress.as_ref().map(|egress| egress.proxy_url()),
        ) {
            Ok(provider) => provider,
            Err(err) => {
                self.fail("connectivity", err.to_string());
                return;
            }
        };
        let request = CompletionRequest {
            model: resolved.model.clone(),
            system: String::new(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: "Reply with the single word ok.".to_string(),
                }],
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_output_tokens: Some(8),
        };
        let cancel = CancellationToken::new();
        let probe = async {
            let mut stream = provider.stream(request, cancel.clone()).await?;
            // One event (headers/first chunk) is enough to prove reachability.
            let _ = stream.next().await;
            Ok::<(), ProviderError>(())
        };

        match tokio::time::timeout(Duration::from_secs(10), probe).await {
            Ok(Ok(())) => self.ok("connectivity", "provider responded"),
            Ok(Err(ProviderError::Auth(err))) => {
                self.fail("connectivity", format!("authentication failed: {err}"));
            }
            Ok(Err(ProviderError::InvalidRequest(err))) => {
                self.fail("connectivity", format!("invalid request: {err}"));
            }
            Ok(Err(err)) => self.warn("connectivity", err.to_string()),
            Err(_) => self.warn("connectivity", "timed out after 10s"),
        }
    }

    fn check_toolchain(&self) {
        match Command::new("rustc").arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                self.ok("rustc", version);
            }
            Ok(_) => self.warn("rustc", "`rustc --version` failed"),
            Err(_) => self.warn("rustc", "not found on PATH (only needed to build)"),
        }
    }

    /// The token decides whether the sandbox can create its private temp, and a
    /// refused `create_dir_all` there is otherwise reported only as a warning
    /// from deep inside the helper. Print the facts next to the other checks.
    #[cfg(windows)]
    fn check_token(&self) {
        let lines = qcode_tools::windows_acl::diagnose();
        let field = |prefix: &str| {
            lines
                .iter()
                .find_map(|line| line.strip_prefix(prefix))
                .map(|value| value.trim().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        };
        self.ok(
            "token",
            format!(
                "integrity {}; restricted sids {}",
                field("token integrity:"),
                field("token restricted sids:")
            ),
        );
        for line in lines.iter().filter(|line| line.starts_with("create ")) {
            if line.contains(": error:") {
                self.warn(
                    "temp",
                    format!("{line}; a sandboxed shell falls back to a temp inside the workspace"),
                );
            }
        }
    }

    fn check_update(&mut self, config: &Config, data_dir: &Option<PathBuf>) {
        let update = &config.update;
        if !update.enabled {
            self.warn("update", "disabled; qc never looks for new releases");
            return;
        }

        let directory = qcode_kernel::resolve_data_dir(data_dir.clone());
        let schedule = if update.check {
            format!("every {} day(s)", update.check_days)
        } else {
            "only via `qc update`".to_string()
        };
        let detail = format!(
            "current {}, checks {schedule} (repo {})",
            crate::update::CURRENT_VERSION,
            update.repo
        );

        let Some((checked_at, latest)) = crate::update::cached(&directory) else {
            self.ok("update", format!("{detail}; no cached check yet"));
            return;
        };
        let age = crate::update::cache_age_days(checked_at);
        match latest {
            Some(latest) if crate::update::is_newer(&latest, crate::update::CURRENT_VERSION) => {
                self.warn(
                    "update",
                    format!(
                        "{detail}; {latest} is available (last check {age}d ago, run `qc update`)"
                    ),
                );
            }
            Some(latest) => self.ok(
                "update",
                format!("{detail}; latest seen {latest} (last check {age}d ago)"),
            ),
            None => self.ok(
                "update",
                format!("{detail}; last check {age}d ago found nothing"),
            ),
        }
    }

    fn check_sandbox(&mut self, config: &Config) {
        let strict = config.network.mode == config::NetworkMode::Strict;
        if config.sandbox.mode == SandboxMode::None {
            if strict {
                self.fail(
                    "network hard mode",
                    "requires [sandbox] mode = \"sandbox-exec\" (macOS)",
                );
                return;
            }
            self.warn(
                "sandbox",
                "none; approval and workspace path checks are the boundary",
            );
            return;
        }

        if strict {
            if config.sandbox.mode != SandboxMode::SandboxExec {
                self.fail(
                    "network hard mode",
                    "requires [sandbox] mode = \"sandbox-exec\" (macOS)",
                );
                return;
            }
            if !config.network.enabled {
                self.fail(
                    "network hard mode",
                    "requires [network] enabled = true (the proxy is the only allowed destination)",
                );
                return;
            }
        }

        // A Deny profile only exercises the filesystem rules; the proxy-only
        // profile is built at startup once the egress port is known. Keep the
        // preflight independent of a live proxy.
        let network = if strict {
            SandboxNetwork::Deny
        } else if config.network_plan().raw {
            SandboxNetwork::Allow
        } else {
            SandboxNetwork::Deny
        };

        let workspace = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        match sandbox::build(
            config.sandbox.mode,
            &workspace,
            network,
            sandbox::SandboxPaths {
                writable: &config.sandbox.extra_writable,
                readable: &config.sandbox.extra_read,
            },
        ) {
            Ok(sandbox) => {
                self.ok("sandbox", sandbox.describe());
                if strict {
                    self.ok(
                        "network hard mode",
                        "proxy-only via sandbox-exec; the proxy port is assigned at startup",
                    );
                }
            }
            Err(err) => self.fail("sandbox", err.to_string()),
        }
    }

    /// `qc doctor --probe`: connect for real and compare the handshake's
    /// `capabilities` with what the server actually answers (ADR-0068 §4).
    ///
    /// This is the other half of "trust the declaration at connect time": the
    /// trust is not free, so the check exists — just on demand, where it costs a
    /// probe the user asked for instead of two round trips per server on every
    /// start.
    ///
    /// Both directions are reported, because they fail differently:
    ///
    /// * **declares, refuses** — the resource/prompt tools are registered and
    ///   every call to them will fail. The user sees two broken tools.
    /// * **serves, does not declare** — the tools are silently absent. The user
    ///   sees nothing at all, which is the worse failure and the one the old
    ///   connect-time probe could not detect either (it only asked, and believed
    ///   the answer).
    async fn check_mcp_capabilities(&mut self, config: &Config, data_dir: &Option<PathBuf>) {
        if config.mcp.servers.iter().all(|server| !server.enabled) {
            return;
        }
        let dir = qcode_kernel::resolve_data_dir(data_dir.clone());
        let workspace = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        // The same two seams a run wires: a per-server sandbox from
        // `[sandbox] mode`, and the session egress proxy for remote servers
        // (ADR-0067 §2 forbids building a bare client here).
        let sandbox_network = if config.network_plan().proxy {
            SandboxNetwork::Allow
        } else {
            SandboxNetwork::Deny
        };
        let sandbox = match sandbox::build(
            config.sandbox.mode,
            &workspace,
            sandbox_network,
            sandbox::SandboxPaths {
                writable: &config.sandbox.extra_writable,
                readable: &config.sandbox.extra_read,
            },
        ) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                self.warn("mcp probe", format!("no sandbox: {error}"));
                return;
            }
        };
        let confine = qcode_kernel::mcp_confine(sandbox, &workspace);
        let egress = match qcode_kernel::build_egress(config, &dir, None).await {
            Ok(egress) => egress,
            Err(error) => {
                self.warn(
                    "mcp probe",
                    format!(
                        "cannot start the egress proxy, remote servers will be skipped: {error}"
                    ),
                );
                None
            }
        };
        let http = egress.as_ref().map(|egress| egress.client());
        let env = egress
            .as_ref()
            .map(|egress| Network::env(egress.as_ref()))
            .unwrap_or_default();

        let reports = qcode_mcp::probe_capabilities(
            &config.mcp,
            &workspace,
            &dir,
            &env,
            qcode_mcp::ConnectOptions {
                http: http.as_ref(),
                confine: Some(confine.as_ref()),
            },
        )
        .await;

        for report in &reports {
            let label = format!("mcp probe {}", report.server);
            if let Some(error) = &report.error {
                // `warn`, not `fail`: one unreachable server loses its own
                // tools and the session still runs (ADR-0066's rule for the
                // whole MCP section).
                self.warn(&label, error);
                continue;
            }
            let registered = if report.registered.is_empty() {
                "none".to_string()
            } else {
                report
                    .registered
                    .iter()
                    .map(|extra| extra.suffix())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            self.ok(
                &label,
                format!(
                    "protocol {} · declares tools={} resources={} prompts={} · registered {registered}{}",
                    report.protocol_version.as_deref().unwrap_or("-"),
                    report.declared.tools,
                    report.declared.resources,
                    report.declared.prompts,
                    match report.instructions_bytes {
                        Some(bytes) => format!(" · instructions {bytes} bytes"),
                        None => String::new(),
                    }
                ),
            );

            // `declares, refuses`: two tools that exist and always fail.
            for (name, declared, answer) in [
                ("resources", report.declared.resources, &report.resources),
                ("prompts", report.declared.prompts, &report.prompts),
            ] {
                if answer.agrees_with(declared) {
                    continue;
                }
                let detail = match (declared, answer) {
                    (true, qcode_mcp::ProbeAnswer::Refused(message)) => format!(
                        "declares {name} but `{name}/list` failed ({message}); the {name} tools \
                         are registered and every call to them will fail — this is a server bug"
                    ),
                    (true, qcode_mcp::ProbeAnswer::TimedOut) => format!(
                        "declares {name} but `{name}/list` did not answer in {}s; the {name} \
                         tools are registered and may hang",
                        config
                            .mcp
                            .servers
                            .iter()
                            .find(|server| server.name == report.server)
                            .map(|server| server.timeout_secs)
                            .unwrap_or(0)
                    ),
                    (false, qcode_mcp::ProbeAnswer::Supported) => format!(
                        "does not declare {name} but `{name}/list` answers; qcode registers no \
                         {name} tools for it, so they are missing from the model's tool list — \
                         the declaration in the handshake is what is wrong"
                    ),
                    // Covered by `agrees_with`; listed so the match is total.
                    _ => continue,
                };
                self.warn(&format!("mcp probe {} {name}", report.server), detail);
            }
        }
    }

    /// One line per remote server, plus the two things that stop it working
    /// silently: no proxy, and a host outside the allowlist (ADR-0067 §8).
    ///
    /// No live probe here. Connecting would mean starting every stdio server
    /// or opening a second egress proxy inside a diagnostic command, and the
    /// questions the user actually has — "is the proxy on", "is the host
    /// allowed", "is the token variable set" — are all answerable from the
    /// config. The negotiated protocol version is logged at startup instead.
    fn check_remote(&mut self, label: &str, server: &McpServerConfig, config: &Config) {
        let Some(url) = server.url.as_deref() else {
            self.warn(label, "no url; qcode will skip it");
            return;
        };
        let parsed = match reqwest::Url::parse(url) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.warn(label, format!("url {url:?} is unusable: {error}"));
                return;
            }
        };
        let host = parsed.host_str().unwrap_or_default().to_string();
        let port = parsed
            .port_or_known_default()
            .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });

        let token = match &server.bearer_token_env_var {
            Some(var) if std::env::var_os(var).is_some() => format!("{var} (set)"),
            Some(var) => format!("{var} (NOT set)"),
            None => "none".to_string(),
        };
        // Three states, and the middle one matters: `audit = true` is the
        // default, but with no proxy there is no audit log to write to, so
        // "audited" would be a claim about a mechanism that is not running.
        let proxy = config.network_plan().proxy;
        let audit = if !proxy {
            "no proxy, so nothing is audited"
        } else if config.network.audit {
            "audited"
        } else {
            "not audited ([network] audit = false)"
        };
        self.ok(
            label,
            format!(
                "transport streamable-http, {url}, credentials [bearer {token}], headers [{}], \
                 {audit}",
                server.headers.len()
            ),
        );

        // A named-but-unset variable is a hard stop for this server: the
        // request is never made, so nothing else here can compensate.
        if let Some(var) = &server.bearer_token_env_var
            && std::env::var_os(var).is_none()
        {
            self.warn(
                &format!("{label} bearer_token_env_var"),
                format!("${var} is not set in qcode's environment, so this server is skipped"),
            );
        }

        if !proxy {
            // The kernel hands MCP the `Network` the proxy provides; without
            // one there is no client, and a remote server is refused rather
            // than connected around the proxy (ADR-0067 §2).
            self.warn(
                &format!("{label} network"),
                "[network] enabled = false, so qcode has no egress client and will refuse this \
                 server instead of connecting around the proxy",
            );
            return;
        }
        if qcode_net::is_allowlisted(&config.network.allow, &host, port) {
            return;
        }
        if config.network.unknown == config::UnknownHost::Allow {
            // `unknown = "allow"` lets the proxy through, but it lets every
            // other host through too; worth one line so it is a choice.
            self.warn(
                &format!("{label} network"),
                format!(
                    "{host}:{port} is not in [network] allow, and it will be reached only \
                     because [network] unknown = \"allow\""
                ),
            );
            return;
        }
        self.warn(
            &format!("{label} network"),
            format!(
                "{host}:{port} is not in [network] allow, so the proxy will answer 403 and this \
                 server will contribute no tools; add allow = [\"{host}:{port}\"] under [network]"
            ),
        );
    }

    /// MCP servers: one line per server with the grant it would get
    /// (ADR-0066), plus the silent no-ops that make a server "configured but
    /// useless" (ADR-0068 §2).
    ///
    /// Nothing here counts as a failure. A broken server only loses its own
    /// tools and the session continues, so exiting non-zero would overstate it;
    /// what doctor owes the user is the *reason*, which is what the warnings
    /// are.
    fn check_mcp(&mut self, config: &Config, data_dir: &Option<PathBuf>) {
        if config.mcp.servers.is_empty() {
            return;
        }
        let workspace = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let data_dir = qcode_kernel::resolve_data_dir(data_dir.clone());
        let real_home = qcode_mcp::real_home();
        // `[mcp] sandbox` is not the last word: the kernel builds MCP sandboxes
        // out of `[sandbox] mode`, and `none` there means nothing is confined
        // whatever the per-server setting says. Saying "sandbox confine" in
        // that case is exactly the doc-says-one-thing, code-does-another bug
        // this ADR set out to remove.
        let global = config.sandbox.mode != SandboxMode::None;
        if !global && config.mcp.servers.iter().any(|server| server.enabled) {
            self.warn(
                "mcp",
                "[sandbox] mode = \"none\": no server is confined, whatever [mcp] sandbox says",
            );
        }

        for server in &config.mcp.servers {
            let label = format!("mcp {}", server.name);
            if !server.enabled {
                self.ok(&label, "disabled; skipped at startup");
                continue;
            }

            // A remote server has no process and therefore no grant: its only
            // boundary is the egress proxy (ADR-0067 后果). Everything the
            // grant section checks is inapplicable, and the proxy replaces it.
            match server.transport() {
                Some(qcode_mcp::Transport::StreamableHttp) => {
                    self.check_remote(&label, server, config);
                    continue;
                }
                Some(qcode_mcp::Transport::Stdio) => {}
                None => {
                    self.warn(
                        &label,
                        "sets both or neither of `command` and `url`; qcode will skip it",
                    );
                    continue;
                }
            }

            // The one way a token still reaches the config file.
            for (key, value) in &server.env {
                if !is_secret_name(key) || is_env_reference(value) {
                    continue;
                }
                self.warn(
                    &format!("{label} env {key}"),
                    "the value is a literal, so the secret is in the config file; write \
                     ${NAME} instead so it is read from qcode's environment",
                );
            }

            // `derive` drops explicit paths that do not exist and presets with
            // nothing behind them, which is correct (the server may create the
            // directory itself) but leaves the user with a server that fails
            // for no visible reason. Say it here.
            for (field, paths) in [
                ("read_paths", &server.read_paths),
                ("writable_paths", &server.writable_paths),
            ] {
                for path in paths {
                    if !path.exists() {
                        self.warn(
                            &format!("{label} {field}"),
                            format!("{} does not exist; the grant skips it", path.display()),
                        );
                    }
                }
            }
            for preset in &server.credentials {
                let granted = real_home.as_ref().is_some_and(|home| {
                    preset
                        .home_paths()
                        .iter()
                        .any(|relative| home.join(relative).exists())
                });
                if granted {
                    continue;
                }
                let wanted = preset
                    .home_paths()
                    .iter()
                    .map(|relative| format!("~/{relative}"))
                    .collect::<Vec<_>>()
                    .join(" or ");
                self.warn(
                    &format!("{label} credentials"),
                    format!(
                        "\"{}\" grants nothing on this machine (no {wanted})",
                        preset.label()
                    ),
                );
            }

            if !server.sandbox_mode(config.mcp.sandbox).is_confined() {
                self.warn(
                    &label,
                    "sandbox = \"off\"; the server is not confined, so it reads and writes \
                     whatever your user can",
                );
                continue;
            }

            let grant = qcode_mcp::GrantSet::derive(qcode_mcp::GrantInput {
                server,
                workspace: &workspace,
                server_state: server.state_dir(&data_dir),
                real_home: real_home.as_deref(),
                confined: global,
            });
            match grant {
                Ok(grant) => self.ok(&label, grant.describe()),
                Err(error) => self.warn(&label, error.to_string()),
            }
        }
    }
}

/// Does this variable name look like it holds a secret?
///
/// Matched per `_`/`-`-separated segment, not as a substring: the substring
/// version that ADR-0068 rejected hits `MONKEY` and `KEYBOARD_LAYOUT` while
/// missing nothing that the segment version catches. A false positive here is
/// only a warning, but it is a warning users learn to ignore.
fn is_secret_name(key: &str) -> bool {
    const SECRET_SEGMENTS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "KEY",
        "APIKEY",
        "CREDENTIAL",
        "CREDENTIALS",
    ];
    key.split(['_', '-'])
        .any(|segment| SECRET_SEGMENTS.contains(&segment.to_ascii_uppercase().as_str()))
}

/// `"${NAME}"` — the whole-value form `expand_env` understands.
fn is_env_reference(value: &str) -> bool {
    value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_matched_by_segment() {
        for name in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
            "NPM_TOKEN",
            "DOCKER_PASSWORD",
            "GH_TOKEN",
        ] {
            assert!(is_secret_name(name), "{name} should look like a secret");
        }
        // The substring approach ADR-0068 rejected caught these.
        for name in ["MONKEY", "KEYBOARD_LAYOUT", "PATH", "LANG"] {
            assert!(
                !is_secret_name(name),
                "{name} should not look like a secret"
            );
        }
    }

    #[test]
    fn only_the_whole_value_form_counts_as_a_reference() {
        assert!(is_env_reference("${GITHUB_TOKEN}"));
        assert!(!is_env_reference("$GITHUB_TOKEN"));
        assert!(!is_env_reference("Bearer ${TOKEN}"));
        assert!(!is_env_reference("${}"));
        assert!(!is_env_reference("literal"));
    }
}
