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
    /// Show where the log file is, and its most recent lines (ADR-0077).
    Logs(LogsArgs),
    /// Show or edit the config file.
    Config(ConfigArgs),
    /// Manage configured MCP servers.
    Mcp(McpArgs),
    /// Diagnose configuration, credentials and data directory.
    Doctor(DoctorArgs),
    /// Show the shell environment `run_shell` would see (ADR-0064).
    ShellEnv(ShellEnvArgs),
    /// Check for a newer release, or install it.
    Update(update::UpdateArgs),
    /// Serve the Agent Client Protocol over stdio (for editors).
    Acp,
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

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Connect to every MCP server and compare what it declares in the
    /// handshake with what it actually answers (ADR-0068 §4).
    ///
    /// Off by default: a session must not pay one probe per server, and doctor
    /// is where that question gets asked on purpose. This starts each stdio
    /// server, and for remote ones the session's egress proxy — the same
    /// machinery a real run uses, so the network side is exercised too.
    #[arg(long)]
    pub(crate) probe: bool,
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
pub(crate) struct ShellEnvArgs {
    /// Print variable values too. Refused when stdout is not a terminal.
    #[arg(long)]
    pub(crate) full: bool,
    /// Machine-readable output: one JSON object, values included only with --full.
    #[arg(long)]
    pub(crate) json: bool,
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
    /// Rewrite a v1 file into the schema-2 shape (ADR-0065): fold
    /// `[providers.*].default_model` into `[default-model]`, collapse model
    /// tables that only carry an id, and switch `version` to `schema`.
    Migrate,
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
// `Add` carries a dozen strings and paths while `List`/`Remove` carry none, so
// the variants differ by ~200 bytes. Boxing would mean a `Box<…>` field that
// clap's derive cannot flatten into, for an enum that exists for the duration
// of one argument parse. The allow is the honest trade.
#[allow(clippy::large_enum_variant)]
pub(crate) enum McpCommand {
    /// List configured servers.
    List,
    /// Append a server to the user config.
    Add {
        name: String,
        /// Which transport, and the fields that belong to it (ADR-0067 §1).
        #[command(flatten)]
        transport: McpTransportArgs,
        /// Repeatable: one argument for the server command.
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Add the server but leave it disabled.
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Extra read+execute root, beyond the ones derived from `command`
        /// (ADR-0066). Repeatable.
        #[arg(long = "read-path", value_name = "PATH")]
        read_paths: Vec<PathBuf>,
        /// Extra writable root, beyond the workspace and the server's private
        /// HOME. Repeatable.
        #[arg(long = "write-path", value_name = "PATH")]
        writable_paths: Vec<PathBuf>,
        /// Credential subtree under `$HOME` the server may read: `github`,
        /// `aws`, `ssh`, `npm`, `netrc` or `docker`. Repeatable.
        #[arg(long = "credential", value_name = "PRESET")]
        credentials: Vec<String>,
        /// `confine` (default) or `off`; `off` runs this server without an OS
        /// sandbox and makes `qc doctor` mark it.
        #[arg(long, value_name = "MODE")]
        sandbox: Option<String>,
    },
    /// Remove a server by name.
    Remove { name: String },
}

/// The transport half of `qc mcp add`: exactly one of `--command` and `--url`.
///
/// A separate struct because clap wants options in a struct or a variant, and
/// because the pair is what `McpServerConfig::transport()` mirrors — keeping
/// them together here means the CLI cannot drift from the config shape.
///
/// The group is required and single-valued. Expressing "exactly one" as an
/// `ArgGroup` rather than two separate `conflicts_with`/`required_unless_present`
/// constraints is what makes clap report `--command|--url` as one choice
/// instead of naming whichever field it happened to check first.
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("transport").required(true).multiple(false).args(["command", "url"])
))]
pub(crate) struct McpTransportArgs {
    /// Stdio transport: the program to start.
    #[arg(long)]
    pub(crate) command: Option<String>,
    /// Streamable HTTP transport: the endpoint URL (ADR-0067).
    #[arg(long, value_name = "URL")]
    pub(crate) url: Option<String>,
    /// Name of the environment variable holding the bearer token. The token
    /// itself is never written to the config file.
    #[arg(long, value_name = "VAR", requires = "url")]
    pub(crate) bearer_token_env: Option<String>,
    /// Extra request header, `Name: value`. Repeatable. Sensitive names
    /// (authorization, cookie, token, key, secret, password) are refused: use
    /// `--bearer-token-env`.
    #[arg(long = "header", value_name = "NAME:VALUE", requires = "url")]
    pub(crate) headers: Vec<String>,
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

    /// Attach a file of any type. The bytes go into the attachment store and
    /// the model reads a handle, not the contents. Repeatable.
    #[arg(long = "file", value_name = "PATH")]
    pub(crate) file: Vec<PathBuf>,
    /// Whole-turn wall-clock limit in seconds. On expiry the turn is cancelled
    /// (same path as Ctrl-C, including process-group cleanup) and qc exits 124.
    #[arg(long, value_name = "SECS")]
    pub(crate) timeout: Option<u64>,
    /// Continue a paused turn (`--resume <id>`) without retyping the task. With
    /// `--raise-turns`/`--raise-cost`, the new limits apply to this window only.
    #[arg(long = "continue")]
    pub(crate) continue_: bool,
    /// Turns for this continuation window (implies --continue).
    #[arg(long, value_name = "N")]
    pub(crate) raise_turns: Option<u32>,
    /// Cost ceiling for this continuation window (implies --continue).
    #[arg(long, value_name = "USD")]
    pub(crate) raise_cost: Option<f64>,
    /// Also write the raised limit to config.toml, so the next session keeps it.
    /// Needs --raise-turns or --raise-cost.
    #[arg(long)]
    pub(crate) raise_persist: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    #[command(subcommand)]
    pub(crate) command: SessionsCommand,
}

#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// Print the path and the file listing, but no log lines.
    #[arg(long, conflicts_with = "follow")]
    pub(crate) path_only: bool,
    /// How many trailing lines to show.
    #[arg(long, short = 'n', default_value_t = 40, value_name = "N")]
    pub(crate) lines: usize,
    /// Keep printing new lines as they are written (Ctrl-C to stop).
    #[arg(long, short = 'f')]
    pub(crate) follow: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionsCommand {
    /// List stored sessions, newest first.
    List {
        /// Also print the title the sidebar shows for each session.
        ///
        /// Reading a title means scanning the session log (the last rename
        /// wins), so this is opt-in: a plain listing stays index-only.
        #[arg(long)]
        titles: bool,
    },
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
    /// Remove one or more sessions by id.
    ///
    /// `gc` can only drop by age, which does not help when a batch of runs
    /// polluted the store and the real work is interleaved with it.
    Delete {
        /// Session ids to remove. With `--except`, the ids to keep.
        #[arg(value_name = "ID", num_args = 1.., required_unless_present = "keep_title")]
        ids: Vec<String>,
        /// Treat `ids` as the sessions to keep and delete every other one.
        ///
        /// Listing what to keep is the shorter and safer form when a few real
        /// sessions are buried under a pile of test runs.
        #[arg(long)]
        except: bool,
        /// Keep the sessions shown under these titles in the sidebar.
        ///
        /// The sidebar shows names, not ids, so this is what a user can copy.
        /// A title that matches no session, or more than one, is an error.
        #[arg(long, value_name = "TITLE", num_args = 1..)]
        keep_title: Vec<String>,
        /// Actually delete; without it, only print what would go.
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
