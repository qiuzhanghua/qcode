mod agents;
mod doctor;
mod logging;
#[cfg(windows)]
mod sandbox_probe;
mod subagent;
mod tui;
mod update;

use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use qcode_core::approval::{Approver, AutoApprove, AutoDeny, InteractiveApprover};
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::{Decision, Event, EventKind, SessionStarted, ToolStatus};
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::Tool;
use qcode_kernel::config;
use qcode_mcp::McpConfig;
use qcode_tools::skills::SkillSet;
use qcode_tools::subagent::SubagentTool;
use qcode_tui::SkillView;
use serde_json::Value;
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
    /// Resume a stored session in the TUI.
    Resume(ResumeArgs),
    /// Inspect persisted sessions.
    Sessions(SessionsArgs),
    /// Inspect discovered Agent Skills.
    Skills(SkillsArgs),
    /// Show or edit the config file.
    Config(ConfigArgs),
    /// Manage configured MCP servers.
    Mcp(McpArgs),
    /// Diagnose configuration, credentials and data directory.
    Doctor,
    /// Check for a newer release, or install it.
    Update(update::UpdateArgs),
    /// Serve the Agent Client Protocol over stdio (for editors).
    Acp,
    /// Open the desktop window (GPUI Kit; needs a gui build).
    Gui,
    /// Probe the Windows ACL sandbox (experiment, hidden).
    #[cfg(windows)]
    #[command(hide = true)]
    SandboxProbe(sandbox_probe::ProbeArgs),
    /// Run one command under the Windows ACL sandbox (internal).
    #[cfg(windows)]
    #[command(hide = true)]
    SandboxRun(SandboxRunArgs),
    /// Inspect or repair the Windows ACL sandbox grants.
    #[cfg(windows)]
    Sandbox(SandboxArgs),
    /// Print the process token and temp bases this build sees (internal).
    #[cfg(windows)]
    #[command(hide = true, name = "whoami")]
    Whoami,
}

#[cfg(windows)]
#[derive(Debug, Args)]
struct SandboxArgs {
    #[command(subcommand)]
    command: SandboxCommand,
}

#[cfg(windows)]
#[derive(Debug, Subcommand)]
enum SandboxCommand {
    /// Give the sandbox write access to a directory tree.
    Grant {
        /// Directory to grant. Defaults to the current directory.
        path: Option<PathBuf>,
    },
    /// Remove a grant from a directory tree.
    Revoke {
        /// Directory to revoke. Defaults to the current directory.
        path: Option<PathBuf>,
    },
}

#[cfg(windows)]
#[derive(Debug, Args)]
struct SandboxRunArgs {
    /// Everything after `--` is the command to run.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the effective (merged) configuration.
    Show,
    /// Open the user config in $EDITOR.
    Edit,
    /// Store an API key in the platform key store (ADR-0039).
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Read the key from stdin and store it, then print the config line.
    Set {
        /// Config provider name, e.g. deepseek.
        #[arg(long, default_value = "deepseek")]
        provider: String,
        /// Key store service name.
        #[arg(long, default_value = "qcode")]
        service: String,
        /// Account name inside the service.
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Args)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// List configured servers.
    List,
    /// Append a server to the user config.
    Add {
        name: String,
        #[arg(long)]
        command: String,
        /// Repeatable: one argument for the server command.
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Add the server but leave it disabled.
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// Remove a server by name.
    Remove { name: String },
}

#[derive(Debug, Args)]
struct ResumeArgs {
    /// Session id to resume.
    id: String,
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
    /// Include provider_raw in --json output (hidden by default).
    #[arg(long)]
    include_raw: bool,
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
    /// Maximum model calls for this turn. Defaults to [agent] max_turns
    /// (8 unless the config says otherwise).
    #[arg(long)]
    max_turns: Option<u32>,
    /// Auto-approve anything the policy marks as ask.
    #[arg(long)]
    yes: bool,
    /// Workspace root for tools. Defaults to the current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Resume an existing session instead of creating a new one.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Stop once the estimated cost exceeds this many USD. Requires a
    /// [pricing."<model>"] entry in the config file.
    #[arg(long, value_name = "USD")]
    max_cost: Option<f64>,
    /// Attach an image (PNG/JPEG/WebP/GIF) to this message. Repeatable; needs a
    /// provider that accepts images.
    #[arg(long = "image", value_name = "PATH")]
    image: Vec<PathBuf>,
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
    /// Copy a conversation prefix into a new session.
    Fork {
        id: String,
        /// Fork after this event seq (default: the whole session).
        #[arg(long, value_name = "SEQ")]
        at: Option<u64>,
    },
    /// Remove sessions whose last event is older than N days.
    Gc {
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,
        /// Actually delete; without it, only print candidates.
        #[arg(long)]
        yes: bool,
    },
    /// List the checkpoints (file pre-images) a session recorded.
    Checkpoints { id: String },
    /// Put the workspace back to the state before a turn.
    Rollback {
        id: String,
        /// Turn to roll back to: files are restored to their state before it.
        #[arg(long, value_name = "TURN")]
        to: u32,
        /// Restore without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
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
        Some(Command::Resume(args)) => {
            cmd_tui(data_dir, config, approval_policy, Some(args.id)).await
        }
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Skills(args)) => cmd_skills(args, config),
        Some(Command::Config(args)) => cmd_config(args, config),
        Some(Command::Mcp(args)) => cmd_mcp(args, config),
        Some(Command::Doctor) => cmd_doctor(data_dir, config).await,
        Some(Command::Update(args)) => cmd_update(args, data_dir, config).await,
        Some(Command::Acp) => cmd_acp(data_dir, config, approval_policy).await,
        Some(Command::Gui) => cmd_gui(data_dir, config, approval_policy).await,
        #[cfg(windows)]
        Some(Command::SandboxProbe(args)) => sandbox_probe::run(&args),
        #[cfg(windows)]
        Some(Command::SandboxRun(args)) => Ok(ExitCode::from(
            qcode_tools::windows_acl::run_helper(&args.args) as u8,
        )),
        #[cfg(windows)]
        Some(Command::Sandbox(args)) => cmd_sandbox(args),
        #[cfg(windows)]
        Some(Command::Whoami) => {
            for line in qcode_tools::windows_acl::diagnose() {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        None => cmd_tui(data_dir, config, approval_policy, None).await,
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn cmd_sandbox(args: SandboxArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = match &args.command {
        SandboxCommand::Grant { path } | SandboxCommand::Revoke { path } => {
            path.clone().unwrap_or_else(|| PathBuf::from("."))
        }
    };
    match args.command {
        SandboxCommand::Grant { .. } => {
            let root =
                qcode_tools::windows_acl::grant_path(&path).map_err(std::io::Error::other)?;
            println!("granted windows-acl write access to {}", root.display());
        }
        SandboxCommand::Revoke { .. } => {
            let (root, removed) =
                qcode_tools::windows_acl::revoke_path(&path).map_err(std::io::Error::other)?;
            println!("revoked {removed} ACE(s) under {}", root.display());
        }
    }
    Ok(ExitCode::SUCCESS)
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
        include_raw,
        provider,
        model,
        base_url,
        no_stream_usage,
        max_turns,
        image,
        yes,
        cwd,
        resume,
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

    let workspace = match cwd {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides {
            provider,
            model,
            base_url,
            stream_usage: no_stream_usage.then_some(false),
        },
    )
    .await?;
    let loaded = &kernel.config;
    // The flag wins, then the config, then the built-in default: a skill run
    // needs far more turns than a single question does.
    let max_turns = max_turns.unwrap_or(loaded.agent.max_turns);
    let max_cost = max_cost.or(loaded.agent.max_cost_usd);
    let resolved = &kernel.resolved;
    let pricing = config::pricing_for(loaded, &resolved.model, max_cost)?;

    let attachments = read_images(&image)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        base_url = ?resolved.base_url,
        "provider resolved"
    );

    let agents_md = agents::load(&workspace)?;
    if agents_md.is_some() {
        tracing::info!("loaded AGENTS.md from the workspace root");
    }
    let skills = kernel.skills.clone();
    let skills_prompt = skills.prompt_section();
    let prompt = match skill {
        Some(name) => activated_prompt(&skills, &name, prompt)?,
        None => prompt,
    };
    let cwd = workspace.display().to_string();
    let data_dir = kernel.store.root().to_path_buf();
    let policy = kernel.policy;
    let store = kernel.store;
    let mut tools = kernel.tools;
    let network = kernel.network;
    let provider: Arc<dyn Provider> = kernel.provider.into();

    let (session_id, first_event) = match resume {
        Some(id) => {
            // Validate before the engine writes anything; a typo should not
            // create a new session file.
            store.load(&id)?;
            tracing::info!(session = %id, "session resumed");
            (id, None)
        }
        None => {
            let (id, event) = store.create(&SessionStarted {
                model: resolved.model.clone(),
                provider: resolved.name.clone(),
                cwd,
                version: env!("CARGO_PKG_VERSION").to_string(),
            })?;
            tracing::info!(session = %id, "session created");
            (id, Some(event))
        }
    };
    if json && let Some(event) = &first_event {
        print_event(event, include_raw);
    }

    let approver: Arc<dyn Approver> = if yes {
        Arc::new(AutoApprove)
    } else if std::io::stdin().is_terminal() {
        Arc::new(InteractiveApprover)
    } else {
        Arc::new(AutoDeny)
    };

    // Pre-images for rollback: every write the agent makes records what was
    // there first.
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(&data_dir)
        .with_shell(loaded.agent.snapshot_shell);

    // The child registry excludes `subagent`, so nesting stops at depth one.
    let runner = subagent::build(
        &store,
        provider.clone(),
        tools.clone(),
        &policy,
        approver.clone(),
        &workspace,
        &resolved.model,
        Some(loaded.context.window_tokens),
        pricing,
        network.clone(),
        Some(snapshots.clone()),
        None,
    );
    tools.extend([Arc::new(SubagentTool::new(runner)) as Arc<dyn Tool>]);

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
            snapshots: Some(&snapshots),
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
            network: network.clone(),
            attachments,
        },
        |event| {
            if json {
                print_event(event, include_raw);
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

    // A stale, cached version check only ever prints: `qc run` must not fail
    // because GitHub is unreachable, and the JSONL stream stays on stdout.
    if loaded.update.checks_on_startup()
        && let Some(notice) = update::notice(&loaded.update, &data_dir).await
    {
        eprintln!("[update] {notice}");
    }

    Ok(ExitCode::SUCCESS)
}

async fn cmd_update(
    args: update::UpdateArgs,
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let data_dir = qcode_kernel::resolve_data_dir(data_dir);
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let loaded = qcode_kernel::load_config(config_path, &workspace)?;
    update::run(args, &loaded.update, &data_dir).await?;
    Ok(ExitCode::SUCCESS)
}

async fn cmd_acp(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    let pricing = config::pricing_for(
        &kernel.config,
        &kernel.resolved.model,
        kernel.config.agent.max_cost_usd,
    )?;
    let agents_md = agents::load(&workspace)?;
    let skills_prompt = kernel.skills.prompt_section();

    tracing::info!(
        provider = %kernel.resolved.name,
        model = %kernel.resolved.model,
        "acp server starting"
    );
    let deps = qcode_acp::Deps {
        store: kernel.store,
        provider: kernel.provider.into(),
        tools: kernel.tools,
        policy: kernel.policy,
        workspace,
        model: kernel.resolved.model.clone(),
        pricing,
        max_turns: kernel.config.agent.max_turns,
        snapshot_shell: kernel.config.agent.snapshot_shell,
        max_cost_usd: kernel.config.agent.max_cost_usd,
        context_window: Some(kernel.config.context.window_tokens),
        agents_md,
        skills: skills_prompt,
        network: kernel.network,
    };
    qcode_acp::stdio::run(Arc::new(deps)).await?;
    Ok(ExitCode::SUCCESS)
}

/// Open the desktop window (ADR-0037).
///
/// The window itself lives in the `qcode-gui` binary for now; this subcommand
/// assembles the kernel first so the GUI can be handed the same pieces as the
/// other frontends.
/// The lean build keeps the subcommand so the error explains what to download
/// instead of letting clap call it an unknown command.
#[cfg(not(feature = "gui"))]
async fn cmd_gui(
    _data_dir: Option<PathBuf>,
    _config_path: Option<PathBuf>,
    _approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    eprintln!("this build has no GUI support; download the desktop build (qcode-desktop)");
    Ok(ExitCode::from(2))
}
#[cfg(feature = "gui")]
async fn cmd_gui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let setup_path = config::edit_path(config_path.clone())?;
    let needs_setup = !setup_path.exists() || config::load(&setup_path).is_err();
    if needs_setup {
        qcode_gui::run_setup(setup_path.clone());
        if config::load(&setup_path).is_err() {
            return Err("config was not created; run `qc gui` again to configure".into());
        }
        // GPUI cannot restart its application loop in-process. Re-exec with the
        // same argv so the just-written config is assembled cleanly.
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
            .status()?;
        return Ok(ExitCode::from(status.code().unwrap_or(1) as u8));
    }
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    println!(
        "gui: provider={} model={} sandbox={} tools={} skills={}",
        kernel.resolved.name,
        kernel.resolved.model,
        kernel.sandbox,
        kernel.tools.len(),
        kernel.skills.len()
    );
    // Seed the projection with what the kernel resolved, then hand the window
    // the same App the TUI would render.
    let mut app = qcode_tui::App::new(kernel.resolved.name.clone(), kernel.resolved.model.clone());
    app.sandbox = kernel.sandbox.clone();
    app.context_window = Some(kernel.config.context.window_tokens);
    app.push_notice(format!(
        "kernel: sandbox={} tools={} skills={}",
        kernel.sandbox,
        kernel.tools.len(),
        kernel.skills.len()
    ));
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(kernel.store.root())
        .with_shell(kernel.config.agent.snapshot_shell);
    let engine = qcode_gui::Engine {
        store: kernel.store,
        provider: kernel.provider.into(),
        provider_name: kernel.resolved.name.clone(),
        tools: kernel.tools,
        policy: kernel.policy,
        workspace: workspace.clone(),
        model: kernel.resolved.model.clone(),
        pricing: config::pricing_for(
            &kernel.config,
            &kernel.resolved.model,
            kernel.config.agent.max_cost_usd,
        )?,
        max_turns: kernel.config.agent.max_turns,
        max_cost_usd: kernel.config.agent.max_cost_usd,
        context_window: Some(kernel.config.context.window_tokens),
        network: kernel.network,
        snapshots: Some(snapshots),
    };
    let backend = qcode_gui::LocalBackend::new(engine);
    qcode_gui::run(app, Box::new(backend));
    Ok(ExitCode::SUCCESS)
}
async fn cmd_tui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
    resume: Option<String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI needs a terminal; use `qc run \"...\"` for headless mode".into());
    }

    let workspace = std::env::current_dir()?.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    let loaded = &kernel.config;
    let resolved = &kernel.resolved;
    let pricing = config::pricing_for(loaded, &resolved.model, None)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        "starting TUI"
    );

    let agents_md = agents::load(&workspace)?;
    let skills = kernel.skills.clone();
    let skills_prompt = skills.prompt_section();
    let skill_views = skill_views(&skills);
    let policy = kernel.policy;
    let store = kernel.store;
    let tools = kernel.tools;
    let network = kernel.network;
    let provider: Arc<dyn Provider> = kernel.provider.into();

    tui::run(tui::TuiOptions {
        store: &store,
        provider,
        tools: &tools,
        policy: &policy,
        workspace,
        provider_name: &resolved.name,
        model: &resolved.model,
        pricing,
        max_cost_usd: loaded.agent.max_cost_usd,
        max_turns: loaded.agent.max_turns,
        snapshot_shell: loaded.agent.snapshot_shell,
        context_window: Some(loaded.context.window_tokens),
        agents_md,
        skills_prompt,
        skills: skill_views,
        sandbox: kernel.sandbox.clone(),
        resume,
        network,
    })
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    Ok(ExitCode::SUCCESS)
}

/// JSONL on stdout. `provider_raw` is diagnostic noise for most consumers, so
/// it is hidden unless `--include-raw` is passed (the log on disk keeps it).
fn print_event(event: &Event, include_raw: bool) {
    if include_raw {
        println!(
            "{}",
            serde_json::to_string(event).expect("event should serialize")
        );
        return;
    }
    let mut value = serde_json::to_value(event).expect("event should serialize");
    if let Some(data) = value.get_mut("data").and_then(Value::as_object_mut) {
        data.remove("provider_raw");
    }
    println!(
        "{}",
        serde_json::to_string(&value).expect("event should serialize")
    );
}

fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = SessionStore::new(qcode_kernel::resolve_data_dir(data_dir));

    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(ExitCode::SUCCESS);
            }
            for session in sessions {
                println!(
                    "{}  turns={}  cost=${:.4}  model={}  cwd={}",
                    session.id,
                    session.turns,
                    session.cost_usd,
                    session.started.model,
                    session.started.cwd
                );
            }
        }
        SessionsCommand::Show { id } => {
            for event in store.load(&id)? {
                println!("{}", serde_json::to_string_pretty(&event)?);
            }
        }
        SessionsCommand::Fork { id, at } => {
            let forked = store.fork(&id, at)?;
            println!("{forked}");
        }
        SessionsCommand::Gc {
            older_than_days,
            yes,
        } => {
            let ids = store.gc(older_than_days, yes)?;
            if ids.is_empty() {
                println!("nothing to clean");
            }
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let mut freed = 0u64;
            for id in &ids {
                let size = snapshots.size(id).unwrap_or(0);
                freed += size;
                println!(
                    "{} {id} (snapshots: {:.1} MB)",
                    if yes { "removed" } else { "would remove" },
                    size as f64 / (1024.0 * 1024.0)
                );
            }
            if yes {
                for id in &ids {
                    if let Err(error) = snapshots.remove_session(id) {
                        println!("cannot remove snapshots for {id}: {error}");
                    }
                }
                // Snapshot trees whose session is gone (deleted by hand, or by an
                // older qc that did not clean up) are orphans.
                let keep: Vec<String> = store.list()?.into_iter().map(|s| s.id).collect();
                match snapshots.sweep(&keep) {
                    Ok(removed) if !removed.is_empty() => {
                        println!("removed {} orphan snapshot tree(s)", removed.len());
                    }
                    Ok(_) => {}
                    Err(error) => println!("cannot sweep snapshots: {error}"),
                }
                println!("freed {:.1} MB", freed as f64 / (1024.0 * 1024.0));
            } else {
                println!("dry run; pass --yes to delete");
            }
        }
        SessionsCommand::Checkpoints { id } => {
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            if turns.is_empty() {
                println!("no checkpoints for {id}");
                return Ok(ExitCode::SUCCESS);
            }
            for turn in turns {
                let files = snapshots.files(&id, turn)?;
                println!("turn {turn}: {} file(s)", files.len());
                for file in files {
                    println!("  {file}");
                }
            }
        }
        SessionsCommand::Rollback { id, to, yes } => {
            // The workspace being restored is the one the session recorded, not
            // the current directory: rolling back from another repo would
            // otherwise write files into the wrong tree.
            let session = store
                .list()?
                .into_iter()
                .find(|session| session.id == id)
                .ok_or_else(|| format!("no such session: {id}"))?;
            let workspace = PathBuf::from(&session.started.cwd);
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            let affected: Vec<u32> = turns.into_iter().filter(|turn| *turn >= to).collect();
            if affected.is_empty() {
                println!("nothing recorded at or after turn {to}; nothing to do");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "would restore {} turn(s) into {}",
                affected.len(),
                workspace.display()
            );
            if !yes {
                println!("dry run; pass --yes to restore");
                return Ok(ExitCode::SUCCESS);
            }
            let report = snapshots.restore(&id, to, &workspace)?;
            println!(
                "restored {} file(s), removed {} created file(s) (turns {:?})",
                report.restored, report.deleted, report.turns
            );
            for refused in &report.refused {
                println!("skipped {refused}");
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Store an API key in the platform key store and tell the user what to paste.
///
/// The key arrives on stdin so it never shows up in the process list, and the
/// printed snippet uses `api_key_command` (ADR-0039) instead of a plaintext
/// environment variable.
fn store_key(
    provider: &str,
    service: &str,
    account: Option<&str>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use std::io::Read;

    let account = account.unwrap_or(provider);
    let mut key = String::new();
    std::io::stdin().read_to_string(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        return Err("no key on stdin; try: echo <key> | qc config key set".into());
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-w",
                key,
            ])
            .status()?;
        if !status.success() {
            return Err("`security add-generic-password` failed".into());
        }
        println!(
            "stored in the keychain; add to your config:\n\
             api_key_command = [\"security\", \"find-generic-password\", \"-w\", \"-s\", \"{service}\", \"-a\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write as _;
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store", "--label", service, "service", service, "user", account,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(key.as_bytes())?;
        if !child.wait()?.success() {
            return Err("`secret-tool store` failed".into());
        }
        println!(
            "stored in libsecret; add to your config:\n\
             api_key_command = [\"secret-tool\", \"lookup\", \"service\", \"{service}\", \"user\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "windows")]
    {
        // DPAPI keeps it user-scoped and encrypted; HKCU would be plaintext.
        let data_dir = qcode_kernel::resolve_data_dir(None);
        std::fs::create_dir_all(&data_dir)?;
        let path = data_dir.join(format!("{service}-{account}.cred"));
        let script = format!(
            "$s = [Console]::In.ReadLine(); \
             $s | ConvertTo-SecureString -AsPlainText -Force | ConvertFrom-SecureString | Set-Content -NoNewline '{}'",
            path.display()
        );
        use std::io::Write as _;
        // `pwsh` is what most machines running qc will have on PATH; powershell
        // is the fallback for a clean Windows install.
        let shell = if std::process::Command::new("pwsh")
            .arg("-NoProfile")
            .arg("-Command")
            .arg("exit 0")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            "pwsh"
        } else {
            "powershell"
        };
        let mut child = std::process::Command::new(shell)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(format!("{key}\n").as_bytes())?;
        if !child.wait()?.success() {
            return Err("storing the key with DPAPI failed".into());
        }
        println!(
            "stored (DPAPI, user-scoped) in {}; add to your config:\n\
             api_key_command = [\"{shell}\", \"-NoProfile\", \"-Command\", \"$s = Get-Content '{}' | ConvertTo-SecureString; [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($s))\"]",
            path.display(),
            path.display()
        );
    }

    let _ = provider;
    Ok(ExitCode::SUCCESS)
}
fn cmd_config(
    args: ConfigArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        ConfigCommand::Show => {
            let workspace = std::env::current_dir()?.canonicalize()?;
            let loaded = qcode_kernel::load_config(config_path, &workspace)?;
            print_config(&loaded);
        }
        ConfigCommand::Key { command } => match command {
            KeyCommand::Set {
                provider,
                service,
                account,
            } => return store_key(&provider, &service, account.as_deref()),
        },
        ConfigCommand::Edit => {
            let path = config::edit_path(config_path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() {
                std::fs::write(&path, "# qcode config\nversion = 1\n")?;
            }
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            let mut parts = editor.split_whitespace();
            let program = parts.next().unwrap_or("vi");
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(&path)
                .status()?;
            if !status.success() {
                return Err(format!("editor exited with {status}").into());
            }
            match config::load(&path) {
                Ok(_) => println!("config ok: {}", path.display()),
                Err(err) => eprintln!("warning: {err}"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_mcp(
    args: McpArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    match args.command {
        McpCommand::List => {
            let loaded = qcode_kernel::load_config(config_path, &workspace)?;
            print_mcp(&loaded.mcp);
        }
        McpCommand::Add {
            name,
            command,
            args,
            disabled,
            timeout_secs,
        } => {
            let path = config::edit_path(config_path)?;
            config::mcp_add(&path, &name, &command, &args, !disabled, timeout_secs)?;
            println!("added MCP server '{name}' to {}", path.display());
        }
        McpCommand::Remove { name } => {
            let path = config::edit_path(config_path)?;
            config::mcp_remove(&path, &name)?;
            println!("removed MCP server '{name}' from {}", path.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_config(config: &config::Config) {
    println!(
        "version = {} (this build supports {})",
        config.version,
        config::CONFIG_VERSION
    );
    println!("default_provider = {:?}", config.default_provider);
    println!("providers ({}):", config.providers.len());
    for (name, provider) in &config.providers {
        println!(
            "  {name}: base_url={:?} model={:?} api_key_env={:?} stream_usage={}",
            provider.base_url, provider.default_model, provider.api_key_env, provider.stream_usage
        );
    }
    println!(
        "network.enabled = {}, allow = {:?}, allow_private = {}, unknown = {:?}, audit = {}",
        config.network.enabled,
        config.network.allow,
        config.network.allow_private,
        config.network.unknown,
        config.network.audit
    );
    println!("context.window_tokens = {}", config.context.window_tokens);
    println!("agent.max_turns = {}", config.agent.max_turns);
    println!("agent.max_cost_usd = {:?}", config.agent.max_cost_usd);
    println!("agent.snapshot_shell = {}", config.agent.snapshot_shell);
    println!(
        "search.enabled = {}, backend = {:?}, api_key_env = {:?}, max_results = {}, base_url = {:?}",
        config.search.enabled,
        config.search.backend,
        config.search.key_env_name(),
        config.search.max_results,
        config.search.base_url
    );
    println!(
        "update.enabled = {}, check = {}, check_days = {}, repo = {}, base_url = {:?}",
        config.update.enabled,
        config.update.check,
        config.update.check_days,
        config.update.repo,
        config.update.base_url
    );
    println!(
        "sandbox.mode = {}, network = {}, extra_writable = {:?}",
        config.sandbox.mode.label(),
        config.sandbox.network,
        config.sandbox.extra_writable
    );
    println!(
        "skills.enabled = {}, roots = {:?}",
        config.skills.enabled, config.skills.roots
    );
    println!("approval rules ({}):", config.approval.rules.len());
    for rule in &config.approval.rules {
        println!(
            "  {} action={:?} tool={:?} prefix={:?} glob={:?} scope={:?}",
            rule.id, rule.action, rule.tool, rule.command_prefix, rule.path_glob, rule.scope
        );
    }
    print_mcp(&config.mcp);
}

fn print_mcp(config: &McpConfig) {
    println!("mcp servers ({}):", config.servers.len());
    for server in &config.servers {
        println!(
            "  {} enabled={} timeout={}s command={:?} args={:?}",
            server.name, server.enabled, server.timeout_secs, server.command, server.args
        );
    }
}

async fn cmd_doctor(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let healthy = doctor::run(data_dir, config_path).await?;
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Read `--image` files into base64 attachments.
///
/// Validating here keeps the engine's error path about capabilities only; a
/// missing or oversized file is a user error, not a model error.
fn read_images(
    paths: &[PathBuf],
) -> Result<Vec<qcode_core::engine::Attachment>, Box<dyn std::error::Error>> {
    const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
    let mut attachments = Vec::new();
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!("{} is larger than 8 MB", path.display()).into());
        }
        let mime = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            Some("gif") => "image/gif",
            _ => {
                return Err(format!(
                    "{}: only PNG, JPEG, WebP and GIF are supported",
                    path.display()
                )
                .into());
            }
        };
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        use base64::Engine as _;
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        attachments.push(qcode_core::engine::Attachment {
            mime_type: mime.to_string(),
            data_base64,
        });
    }
    Ok(attachments)
}

/// Built-in tools plus whatever the enabled MCP servers advertise and the
/// `skill` tool when skills were discovered. A server that fails to start only
/// removes its own tools; `connect_all` logs why.
fn cmd_skills(
    args: SkillsArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        SkillsCommand::List => {}
    }

    let workspace = std::env::current_dir()?.canonicalize()?;
    let loaded = qcode_kernel::load_config(config_path, &workspace)?;
    if !loaded.skills.enabled {
        println!("skills are disabled ([skills] enabled = false)");
        return Ok(ExitCode::SUCCESS);
    }

    let roots = qcode_kernel::skill_roots(&loaded.skills, &workspace);
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
