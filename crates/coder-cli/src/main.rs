use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use coder_core::engine::{RunOptions, run_turn};
use coder_core::event::SessionStarted;
use coder_core::store::SessionStore;
use coder_providers::MockProvider;

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a single headless turn.
    Run(RunArgs),
    /// Inspect persisted sessions.
    Sessions(SessionsArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Prompt text. Read from stdin when omitted.
    prompt: Option<String>,
    /// Emit the event stream as JSONL on stdout.
    #[arg(long)]
    json: bool,
    /// Provider to use (only `mock` in M0).
    #[arg(long, default_value = "mock")]
    provider: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value_t = 8)]
    max_turns: u32,
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
    let Cli { data_dir, command } = Cli::parse();
    let result = match command {
        Command::Run(args) => cmd_run(args, data_dir).await,
        Command::Sessions(args) => cmd_sessions(args, data_dir),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_run(
    args: RunArgs,
    data_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let prompt = match args.prompt {
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

    let store = SessionStore::new(resolve_data_dir(data_dir));
    let cwd = std::env::current_dir()?.display().to_string();
    let model = args.model.unwrap_or_else(|| "mock-1".to_string());
    let json = args.json;

    let (session_id, first_event) = store.create(&SessionStarted {
        model: model.clone(),
        provider: args.provider.clone(),
        cwd,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })?;
    if json {
        println!("{}", serde_json::to_string(&first_event)?);
    }

    let provider = match args.provider.as_str() {
        "mock" => MockProvider::text(
            "Hello from the mock provider. The event log and replay path are working.",
        ),
        other => return Err(format!("unknown provider: {other}").into()),
    };

    let outcome = run_turn(
        &store,
        &provider,
        RunOptions {
            session_id: &session_id,
            model: &model,
            input: &prompt,
            max_turns: args.max_turns,
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
    .await?;

    if !json {
        println!("session: {}", outcome.session_id);
        println!();
        println!("{}", outcome.text);
    }

    Ok(())
}

fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = SessionStore::new(resolve_data_dir(data_dir));

    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(());
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

    Ok(())
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
