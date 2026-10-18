mod config;
mod doctor;
mod logging;

use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use coder_core::approval::{Approver, AutoApprove, AutoDeny, InteractiveApprover};
use coder_core::engine::{RunOptions, TurnContext, run_turn};
use coder_core::event::SessionStarted;
use coder_core::policy::Policy;
use coder_core::provider::Provider;
use coder_core::store::SessionStore;
use coder_providers::{MockProvider, OpenAiCompatConfig, OpenAiCompatProvider};
use coder_tools::builtin_registry;

#[derive(Debug, Parser)]
#[command(
    name = "coder",
    version,
    about = "Terminal coding agent (working title)"
)]
struct Cli {
    /// Data directory. Defaults to the XDG data home.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Config file. Defaults to ~/.config/coder/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
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
        Command::Run(args) => cmd_run(args, data_dir, config).await,
        Command::Sessions(args) => cmd_sessions(args, data_dir),
        Command::Doctor => cmd_doctor(data_dir, config).await,
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

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        base_url = ?resolved.base_url,
        "provider resolved"
    );

    let store = SessionStore::new(resolve_data_dir(data_dir));
    let workspace = match cwd {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
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

    let provider: Box<dyn Provider> = if resolved.is_mock {
        Box::new(MockProvider::text(
            "Hello from the mock provider. The event log and replay path are working.",
        ))
    } else {
        Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
            name: resolved.name.clone(),
            base_url: resolved
                .base_url
                .clone()
                .expect("non-mock provider always has a base URL"),
            api_key: resolved.api_key.clone(),
            stream_usage: resolved.stream_usage,
        })?)
    };

    let tools = builtin_registry();
    let policy = Policy::new(loaded.approval.rules.clone());
    let approver: Box<dyn Approver> = if yes {
        Box::new(AutoApprove)
    } else if std::io::stdin().is_terminal() {
        Box::new(InteractiveApprover)
    } else {
        Box::new(AutoDeny)
    };

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
            input: &prompt,
            workspace: workspace.clone(),
            max_turns,
        },
        |event| {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(event).expect("event should serialize")
                );
            }
        },
    )
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    if !json {
        println!("session: {}", outcome.session_id);
        println!();
        println!("{}", outcome.text);
    }

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

fn load_config(explicit: Option<PathBuf>) -> Result<config::Config, Box<dyn std::error::Error>> {
    let explicit = explicit.or_else(|| std::env::var("CODER_CONFIG").ok().map(PathBuf::from));

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
    if let Ok(path) = std::env::var("CODER_DATA_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(path).join("coder");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("coder")
}
