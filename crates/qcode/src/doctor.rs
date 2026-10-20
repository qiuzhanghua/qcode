//! `qc doctor`: answer "why doesn't it run?" in one command.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use futures_util::StreamExt;
use qcode_core::event::ContentBlock;
use qcode_core::provider::{CompletionRequest, Message, ProviderError, Role};
use qcode_tools::sandbox::{self, SandboxMode};
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};

pub async fn run(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
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

impl Doctor {
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
                self.ok(
                    "config schema",
                    format!(
                        "version {} (this build supports {})",
                        config.version,
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
                    self.ok("credentials", "API key present");
                } else {
                    self.ok("credentials", "keyless provider");
                }
            }
            Err(err) => self.fail("provider", err.to_string()),
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
        if config.sandbox.mode == SandboxMode::None {
            self.warn(
                "sandbox",
                "none; approval and workspace path checks are the boundary",
            );
            return;
        }

        let workspace = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        match sandbox::build(
            config.sandbox.mode,
            &workspace,
            config.sandbox.network,
            &config.sandbox.extra_writable,
        ) {
            Ok(sandbox) => self.ok("sandbox", sandbox.describe()),
            Err(err) => self.fail("sandbox", err.to_string()),
        }
    }
}
