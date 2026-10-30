#[derive(Debug, Parser)]
#[command(name = "qc", version, about = "qcode — terminal coding agent")]
pub(crate) struct Cli {
    /// Data directory. Defaults to the XDG data home.
    #[arg(long, global = true)]
    pub(crate) data_dir: Option<PathBuf>,
    /// Config file. Defaults to ~/.config/qcode/config.toml.
    #[arg(long, global = true)]
    pub(crate) config: Option<PathBuf>,
    /// Approval policy file ([[rules]]). Replaces the config's approval rules.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) approval_policy: Option<PathBuf>,
    /// Without a subcommand, qc starts the TUI.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

use crate::update;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
    SandboxProbe(crate::sandbox_probe::ProbeArgs),
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
pub(crate) struct SandboxArgs {
    #[command(subcommand)]
    pub(crate) command: SandboxCommand,
}

#[cfg(windows)]
#[derive(Debug, Subcommand)]
pub(crate) enum SandboxCommand {
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
pub(crate) struct SandboxRunArgs {
    /// Everything after `--` is the command to run.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub(crate) command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
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
pub(crate) enum KeyCommand {
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
pub(crate) struct McpArgs {
    #[command(subcommand)]
    pub(crate) command: McpCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum McpCommand {
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
pub(crate) struct ResumeArgs {
    /// Session id to resume.
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Prompt text. Read from stdin when omitted.
    pub(crate) prompt: Option<String>,
    /// Activate a discovered skill by prepending its instructions to the prompt.
    #[arg(long, value_name = "NAME")]
    pub(crate) skill: Option<String>,
    /// Emit the event stream as JSONL on stdout.
    #[arg(long)]
    pub(crate) json: bool,
    /// Include provider_raw in --json output (hidden by default).
    #[arg(long)]
    pub(crate) include_raw: bool,
    /// Provider name: mock, openai, deepseek, ollama, or a custom entry.
    #[arg(long)]
    pub(crate) provider: Option<String>,
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Override the provider base URL.
    #[arg(long)]
    pub(crate) base_url: Option<String>,
    /// Do not ask the server for a usage chunk.
    #[arg(long)]
    pub(crate) no_stream_usage: bool,
    /// Maximum model calls for this turn. Defaults to [agent] max_turns
    /// (8 unless the config says otherwise).
    #[arg(long)]
    pub(crate) max_turns: Option<u32>,
    /// Auto-approve anything the policy marks as ask.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Workspace root for tools. Defaults to the current directory.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Resume an existing session instead of creating a new one.
    #[arg(long, value_name = "ID")]
    pub(crate) resume: Option<String>,
    /// Stop once the estimated cost exceeds this many USD. Requires a
    /// [pricing."<model>"] entry in the config file.
    #[arg(long, value_name = "USD")]
    pub(crate) max_cost: Option<f64>,
    /// Attach an image (PNG/JPEG/WebP/GIF) to this message. Repeatable; needs a
    /// provider that accepts images.
    #[arg(long = "image", value_name = "PATH")]
    pub(crate) image: Vec<PathBuf>,
    /// Whole-turn wall-clock limit in seconds. On expiry the turn is cancelled
    /// (same path as Ctrl-C, including process-group cleanup) and qc exits 124.
    #[arg(long, value_name = "SECS")]
    pub(crate) timeout: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    #[command(subcommand)]
    pub(crate) command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionsCommand {
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
pub(crate) struct SkillsArgs {
    #[command(subcommand)]
    pub(crate) command: SkillsCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SkillsCommand {
    /// List discovered skills and the roots that were checked.
    List,
}
