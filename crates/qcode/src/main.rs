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
use qcode_tools::builtin_registry_with;
use qcode_tools::sandbox::Sandbox;
use qcode_tools::skills::{SkillSet, SkillSource, SkillTool};
use qcode_tui::SkillView;
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
    /// Approval policy file ([[rules]]). Replaces the config's approval rules.
    #[arg(long, global = true, value_name = "PATH")]
    approval_policy: Option<PathBuf>,
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
    /// Inspect discovered Agent Skills.
    Skills(SkillsArgs),
    /// Diagnose configuration, credentials and data directory.
    Doctor,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Prompt text. Read from stdin when omitted.
    prompt: Option<String>,
    /// Activate a discovered skill by prepending its instructions to the prompt.
    #[arg(long, value_name = "NAME")]
    skill: Option<String>,
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

#[derive(Debug, Args)]
struct SkillsArgs {
    #[command(subcommand)]
    command: SkillsCommand,
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    /// List discovered skills and the roots that were checked.
    List,
}

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();

    let Cli {
        data_dir,
        config,
        approval_policy,
        command,
    } = Cli::parse();

    let result = match command {
        Some(Command::Run(args)) => cmd_run(args, data_dir, config, approval_policy).await,
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Skills(args)) => cmd_skills(args, config),
        Some(Command::Doctor) => cmd_doctor(data_dir, config).await,
        None => cmd_tui(data_dir, config, approval_policy).await,
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
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let RunArgs {
        prompt,
        skill,
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
    // Resolve the policy before creating a session: a bad policy file should
    // not leave an empty session behind.
    let policy = build_policy(&loaded, approval_policy.as_deref())?;

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
    let sandbox =
        qcode_tools::sandbox::build(loaded.sandbox.mode, &workspace, loaded.sandbox.network)?;
    tracing::info!(sandbox = sandbox.name(), "sandbox configured");
    let agents_md = agents::load(&workspace)?;
    if agents_md.is_some() {
        tracing::info!("loaded AGENTS.md from the workspace root");
    }
    let skills = load_skills(&loaded.skills, &workspace);
    let skills_prompt = skills.prompt_section();
    let prompt = match skill {
        Some(name) => activated_prompt(&skills, &name, prompt)?,
        None => prompt,
    };
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

    let tools = build_tools(&loaded.mcp, &workspace, &data_dir, &skills, sandbox).await;
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
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI needs a terminal; use `qc run \"...\"` for headless mode".into());
    }

    let loaded = load_config(config_path)?;
    let resolved = config::resolve(&loaded, &config::Overrides::default(), &|key| {
        std::env::var(key).ok()
    })?;
    let pricing = config::pricing_for(&loaded, &resolved.model, None)?;
    let policy = build_policy(&loaded, approval_policy.as_deref())?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        "starting TUI"
    );

    let data_dir = resolve_data_dir(data_dir);
    let store = SessionStore::new(data_dir.clone());
    let workspace = std::env::current_dir()?.canonicalize()?;
    let sandbox =
        qcode_tools::sandbox::build(loaded.sandbox.mode, &workspace, loaded.sandbox.network)?;
    tracing::info!(sandbox = sandbox.name(), "sandbox configured");
    let agents_md = agents::load(&workspace)?;
    let skills = load_skills(&loaded.skills, &workspace);
    let skills_prompt = skills.prompt_section();
    let skill_views = skill_views(&skills);
    let provider = build_provider(&resolved)?;
    let tools = build_tools(&loaded.mcp, &workspace, &data_dir, &skills, sandbox.clone()).await;

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
        skills_prompt,
        skills: skill_views,
        sandbox: sandbox.name().to_string(),
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
    sandbox: Arc<dyn Sandbox>,
) -> ToolRegistry {
    let mut tools = builtin_registry_with(sandbox);
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

/// Discover skills unless the config disables them. A missing `HOME` only
/// loses the user-level root; configured and project roots still work.
fn load_skills(config: &config::SkillsConfig, workspace: &Path) -> Arc<SkillSet> {
    if !config.enabled {
        return Arc::new(SkillSet::default());
    }
    Arc::new(SkillSet::discover(&skill_roots(config, workspace)))
}

/// All discovery roots in increasing precedence: global, user, configured,
/// project. The `.agents/skills` paths are what `npx skills` manages.
fn skill_roots(config: &config::SkillsConfig, workspace: &Path) -> Vec<(SkillSource, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push((SkillSource::Global, home.join(".agents/skills")));
        roots.push((SkillSource::Global, home.join(".config/agents/skills")));
        roots.push((SkillSource::User, home.join(".config/qcode/skills")));
    }
    roots.extend(config_roots(config, workspace));
    roots.push((SkillSource::Project, workspace.join(".agents/skills")));
    roots.push((SkillSource::Project, workspace.join(".qcode/skills")));
    roots
}

fn config_roots(config: &config::SkillsConfig, workspace: &Path) -> Vec<(SkillSource, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
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

fn expand_root(path: &Path, workspace: &Path, home: Option<&Path>) -> PathBuf {
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

fn cmd_skills(
    args: SkillsArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        SkillsCommand::List => {}
    }

    let loaded = load_config(config_path)?;
    if !loaded.skills.enabled {
        println!("skills are disabled ([skills] enabled = false)");
        return Ok(ExitCode::SUCCESS);
    }

    let workspace = std::env::current_dir()?.canonicalize()?;
    let roots = skill_roots(&loaded.skills, &workspace);
    let skills = SkillSet::discover(&roots);

    if skills.is_empty() {
        println!("no skills discovered; checked:");
        for (source, path) in &roots {
            println!("  [{}] {}", source.label(), path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    for skill in skills.iter() {
        println!(
            "{}  [{}]  files={}",
            skill.name,
            skill.source.label(),
            skill.files.len()
        );
        println!("  {}", skill.dir.display());
        println!("  {}", skill.description.replace('\n', " "));
    }
    println!("\n{} skill(s)", skills.len());
    Ok(ExitCode::SUCCESS)
}

fn activated_prompt(
    skills: &SkillSet,
    name: &str,
    task: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(skill) = skills.get(name) else {
        let available: Vec<&str> = skills
            .iter()
            .take(20)
            .map(|skill| skill.name.as_str())
            .collect();
        return Err(format!(
            "unknown skill '{name}'; available: {}",
            available.join(", ")
        )
        .into());
    };
    let task = if task.trim().is_empty() {
        "Follow the skill instructions above now.".to_string()
    } else {
        task
    };
    Ok(format!(
        "[skill:{}]\n{}\n\n{task}",
        skill.name,
        skill.body.trim()
    ))
}

fn skill_views(skills: &SkillSet) -> Vec<SkillView> {
    skills
        .iter()
        .map(|skill| SkillView {
            name: skill.name.clone(),
            source: skill.source.label().to_string(),
            description: skill.description.clone(),
            dir: skill.dir.display().to_string(),
            body: skill.body.clone(),
            files: skill.files.clone(),
            files_truncated: skill.files_truncated,
        })
        .collect()
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

/// Resolve the approval policy for this invocation.
///
/// `--approval-policy` is an explicit, auditable decision, so its rules replace
/// the config's `[[approval.rules]]` instead of extending them. Without the
/// flag the config rules (or the built-in defaults) apply unchanged.
fn build_policy(
    loaded: &config::Config,
    explicit: Option<&Path>,
) -> Result<Policy, Box<dyn std::error::Error>> {
    let rules = match explicit {
        Some(path) => {
            if !path.exists() {
                return Err(format!("approval policy not found: {}", path.display()).into());
            }
            tracing::info!(path = %path.display(), "loaded approval policy file");
            config::load_policy(path)?
        }
        None => loaded.approval.rules.clone(),
    };
    Ok(Policy::new(rules))
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
