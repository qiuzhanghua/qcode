mod agents;
mod config;
mod doctor;
mod logging;
mod tui;

use std::io::IsTerminal;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use qcode_core::approval::{Approver, AutoApprove, AutoDeny, InteractiveApprover};
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{Decision, Event, EventKind, SessionStarted, ToolStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::{Provider, StopReason, StreamEvent};
use qcode_core::store::SessionStore;
use qcode_core::tool::{Tool, ToolRegistry};
use qcode_mcp::McpConfig;
use qcode_providers::{MockProvider, OpenAiCompatConfig, OpenAiCompatProvider};
use qcode_tools::builtin_registry;
use qcode_tools::skills::{SkillSet, SkillSource, SkillTool};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "qc", version, about = "qcode — terminal coding agent")]
struct Cli {
    /// Data directory. Defaults to the XDG data home.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Config file. Defaults to ~/.config/qcode/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Without a subcommand, qc starts the TUI.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a single headless turn.
    Run(RunArgs),
    /// Inspect persisted sessions.
    Sessions(SessionsArgs),
    /// Diagnose configuration, credentials and data directory.
    Doctor,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Prompt text. Read from stdin when omitted.
    prompt: Option<String>,
    /// Emit the event stream as JSONL on stdout.
    #[arg(long)]
    json: bool,
    /// Provider name: mock, openai, deepseek, ollama, or a custom entry.
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Override the provider base URL.
    #[arg(long)]
    base_url: Option<String>,
    /// Do not ask the server for a usage chunk.
    #[arg(long)]
    no_stream_usage: bool,
    #[arg(long, default_value_t = 8)]
    max_turns: u32,
    /// Auto-approve anything the policy marks as ask.
    #[arg(long)]
    yes: bool,
    /// Workspace root for tools. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Stop once the estimated cost exceeds this many USD. Requires a
    /// [pricing."<model>"] entry in the config file.
    #[arg(long, value_name = "USD")]
    max_cost: Option<f64>,
    /// Whole-turn wall-clock limit in seconds. On expiry the turn is cancelled
    /// (same path as Ctrl-C, including process-group cleanup) and qc exits 124.
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,
}

#[derive(Debug, Args)]
struct SessionsArgs {
    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List stored sessions, newest first.
    List,
    /// Replay one session as JSON.
    Show { id: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();

    let Cli {
        data_dir,
        config,
        command,
    } = Cli::parse();

    let result = match command {
        Some(Command::Run(args)) => cmd_run(args, data_dir, config).await,
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Doctor) => cmd_doctor(data_dir, config).await,
        None => cmd_tui(data_dir, config).await,
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_run(
    args: RunArgs,
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let RunArgs {
        prompt,
        json,
        provider,
        model,
        base_url,
        no_stream_usage,
        max_turns,
        yes,
        cwd,
        max_cost,
        timeout,
    } = args;

    let prompt = match prompt {
        Some(prompt) => prompt,
        None => {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        return Err("prompt is empty".into());
    }

    let loaded = load_config(config_path)?;
    let resolved = config::resolve(
        &loaded,
        &config::Overrides {
            provider,
            model,
            base_url,
            stream_usage: no_stream_usage.then_some(false),
        },
        &|key| std::env::var(key).ok(),
    )?;

    let pricing = config::pricing_for(&loaded, &resolved.model, max_cost)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        base_url = ?resolved.base_url,
        "provider resolved"
    );

    let data_dir = resolve_data_dir(data_dir);
    let store = SessionStore::new(data_dir.clone());
    let workspace = match cwd {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
    let agents_md = agents::load(&workspace)?;
    if agents_md.is_some() {
        tracing::info!("loaded AGENTS.md from the workspace root");
    }
    let skills = load_skills(&loaded.skills, &workspace);
    let skills_prompt = skills.prompt_section();
    let cwd = workspace.display().to_string();

    let (session_id, first_event) = store.create(&SessionStarted {
        model: resolved.model.clone(),
        provider: resolved.name.clone(),
        cwd,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })?;
    tracing::info!(session = %session_id, "session created");
    if json {
        println!("{}", serde_json::to_string(&first_event)?);
    }

    let provider = build_provider(&resolved)?;

    let tools = build_tools(&loaded.mcp, &workspace, &data_dir, &skills).await;
    let policy = Policy::new(loaded.approval.rules.clone());
    let approver: Box<dyn Approver> = if yes {
        Box::new(AutoApprove)
    } else if std::io::stdin().is_terminal() {
        Box::new(InteractiveApprover)
    } else {
        Box::new(AutoDeny)
    };

    if timeout == Some(0) {
        return Err("--timeout must be at least 1 second".into());
    }

    let cancel = CancellationToken::new();
    install_ctrl_c(cancel.clone());
    let timed_out = Arc::new(AtomicBool::new(false));
    if let Some(seconds) = timeout {
        install_timeout(
            cancel.clone(),
            Duration::from_secs(seconds),
            timed_out.clone(),
        );
    }

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: provider.as_ref(),
            tools: &tools,
            policy: &policy,
            approver: approver.as_ref(),
        },
        RunOptions {
            session_id: &session_id,
            model: &resolved.model,
            input: prompt,
            workspace: workspace.clone(),
            max_turns,
            cancel: cancel.clone(),
            pricing,
            max_cost_usd: max_cost,
            on_delta: None,
            context_window: Some(loaded.context.window_tokens),
            agents_md,
            skills: skills_prompt,
        },
        |event| {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(event).expect("event should serialize")
                );
            } else {
                progress(event);
            }
        },
    )
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    if outcome.cancelled {
        if !json && !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        if timed_out.load(Ordering::SeqCst) {
            eprintln!(
                "[timeout] turn exceeded {}s; cancelled",
                timeout.unwrap_or_default()
            );
            return Ok(ExitCode::from(124));
        }
        eprintln!("[cancelled] turn interrupted; the event log is intact");
        return Ok(ExitCode::from(130));
    }

    if !json {
        println!("session: {}", outcome.session_id);
        println!();
        println!("{}", outcome.text);
        if outcome.cost_usd > 0.0 {
            println!();
            println!("cost: ${:.6} (estimated)", outcome.cost_usd);
        }
    }

    Ok(ExitCode::SUCCESS)
}

async fn cmd_tui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI needs a terminal; use `qc run \"...\"` for headless mode".into());
    }

    let loaded = load_config(config_path)?;
    let resolved = config::resolve(&loaded, &config::Overrides::default(), &|key| {
        std::env::var(key).ok()
    })?;
    let pricing = config::pricing_for(&loaded, &resolved.model, None)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        "starting TUI"
    );

    let data_dir = resolve_data_dir(data_dir);
    let store = SessionStore::new(data_dir.clone());
    let workspace = std::env::current_dir()?.canonicalize()?;
    let agents_md = agents::load(&workspace)?;
    let skills = load_skills(&loaded.skills, &workspace);
    let skills_prompt = skills.prompt_section();
    let provider = build_provider(&resolved)?;
    let tools = build_tools(&loaded.mcp, &workspace, &data_dir, &skills).await;
    let policy = Policy::new(loaded.approval.rules.clone());

    tui::run(tui::TuiOptions {
        store: &store,
        provider: provider.as_ref(),
        tools: &tools,
        policy: &policy,
        workspace,
        provider_name: &resolved.name,
        model: &resolved.model,
        pricing,
        max_cost_usd: None,
        max_turns: 8,
        context_window: Some(loaded.context.window_tokens),
        agents_md,
        skills: skills_prompt,
    })
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    Ok(ExitCode::SUCCESS)
}

fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = SessionStore::new(resolve_data_dir(data_dir));

    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(ExitCode::SUCCESS);
            }
            for session in sessions {
                println!(
                    "{}  turns={}  model={}  cwd={}",
                    session.id, session.turns, session.started.model, session.started.cwd
                );
            }
        }
        SessionsCommand::Show { id } => {
            for event in store.load(&id)? {
                println!("{}", serde_json::to_string_pretty(&event)?);
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

async fn cmd_doctor(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let healthy = doctor::run(data_dir, config_path)?;
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Built-in tools plus whatever the enabled MCP servers advertise and the
/// `skill` tool when skills were discovered. A server that fails to start only
/// removes its own tools; `connect_all` logs why.
async fn build_tools(
    mcp: &McpConfig,
    workspace: &Path,
    data_dir: &Path,
    skills: &Arc<SkillSet>,
) -> ToolRegistry {
    let mut tools = builtin_registry();
    let mcp_tools = qcode_mcp::connect_all(&mcp.servers, workspace, &data_dir.join("logs")).await;
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

/// Discover user and project skills unless the config disables them. A missing
/// `HOME` only loses the user-level root; the project root still works.
fn load_skills(config: &config::SkillsConfig, workspace: &Path) -> Arc<SkillSet> {
    if !config.enabled {
        return Arc::new(SkillSet::default());
    }

    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push((
            SkillSource::User,
            PathBuf::from(home).join(".config/qcode/skills"),
        ));
    }
    roots.push((SkillSource::Project, workspace.join(".qcode/skills")));

    Arc::new(SkillSet::discover(&roots))
}

fn build_provider(
    resolved: &config::Resolved,
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
        _ => Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
            name: resolved.name.clone(),
            base_url: resolved
                .base_url
                .clone()
                .expect("non-mock provider always has a base URL"),
            api_key: resolved.api_key.clone(),
            stream_usage: resolved.stream_usage,
        })?),
    })
}

fn load_config(explicit: Option<PathBuf>) -> Result<config::Config, Box<dyn std::error::Error>> {
    let explicit = explicit.or_else(|| std::env::var("QCODE_CONFIG").ok().map(PathBuf::from));

    if let Some(path) = explicit {
        if !path.exists() {
            return Err(format!("config file not found: {}", path.display()).into());
        }
        return Ok(config::load(&path)?);
    }

    let path = config::default_path()?;
    if path.exists() {
        Ok(config::load(&path)?)
    } else {
        Ok(config::Config::default())
    }
}

fn resolve_data_dir(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Ok(path) = std::env::var("QCODE_DATA_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(path).join("qcode");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("qcode")
}

/// Ctrl-C asks the engine to stop; a second Ctrl-C wins immediately.
///
/// The first press cancels the token, which makes the engine abort the stream
/// and lets `run_shell` kill its process group before the tool result is
/// written. Pressing again is the escape hatch for a wedged process.
fn install_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut presses = 0u32;
        while tokio::signal::ctrl_c().await.is_ok() {
            presses += 1;
            if presses == 1 {
                eprintln!(
                    "\n[cancel] stopping the current turn… (Ctrl-C again to quit immediately)"
                );
                cancel.cancel();
            } else {
                std::process::exit(130);
            }
        }
    });
}

/// Whole-turn deadline. It cancels through the same token as Ctrl-C, so a
/// running shell still gets its process group killed before the turn ends.
fn install_timeout(cancel: CancellationToken, timeout: Duration, fired: Arc<AtomicBool>) {
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        fired.store(true, Ordering::SeqCst);
        eprintln!(
            "\n[timeout] turn exceeded {}s; stopping…",
            timeout.as_secs()
        );
        cancel.cancel();
    });
}

/// Human-mode progress on stderr. Without this a 10-minute test run looks like
/// a hang, because the engine only reports the final text on stdout.
fn progress(event: &Event) {
    match &event.kind {
        EventKind::ToolCall(call) => {
            let summary = call
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .or_else(|| call.arguments.get("path").and_then(|value| value.as_str()))
                .unwrap_or_default();
            eprintln!("\n[tool] {} {}", call.name, truncate_chars(summary, 120));
        }
        EventKind::ToolResult(result) => {
            let status = match result.status {
                ToolStatus::Ok => "ok",
                ToolStatus::Error => "error",
                ToolStatus::Cancelled => "cancelled",
                ToolStatus::Interrupted => "interrupted",
            };
            eprintln!("[tool] {status} ({} bytes)", result.metadata.bytes);
        }
        EventKind::ApprovalDecided(decided) => {
            let decision = if decided.decision == Decision::Allow {
                "allowed"
            } else {
                "denied"
            };
            eprintln!("[approval] {decision}");
        }
        EventKind::TurnEnded(ended) => {
            eprintln!("[turn] {:?}", ended.status);
        }
        _ => {}
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}
