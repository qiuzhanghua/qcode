use crate::approval::{ApprovalRequest, Approver};
use crate::compact;
use crate::cost::Pricing;
use crate::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, BudgetHint, Compaction, ContentBlock,
    Continuation, Decision, ErrorEvent, Event, EventKind, PauseInfo, PauseLimit, PauseReason,
    PauseUsage, Scope, ToolCall, ToolResult, ToolStatus, TurnEnded, TurnStarted, TurnStatus, Usage,
    UserMessage,
};
use crate::history;
use crate::inbox::Inbox;
use crate::policy::{Action, Policy};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, Role, StopReason, StreamEvent,
};
use crate::store::{SessionLock, SessionStore, StoreError};
use crate::todo::{self, TodoItem};
use crate::tool::{ApprovalClass, Tool, ToolContext, ToolOutput, ToolRegistry};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

pub mod attachments;
mod stream;
mod tools;
mod turn;
mod writer;

pub(crate) use stream::*;
pub(crate) use tools::*;
pub use turn::{CONTINUE_PROMPT, WRAP_UP_PROMPT, run_turn};

#[cfg(test)]
pub(crate) use turn::{ProgressGuard, call_signature};
pub use writer::SessionWriter;

/// A streamed delta. Deltas are transient: they drive the UI while the model
/// is talking and are never written to the event log. The completed blocks are
/// persisted later as one `assistant_message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    /// Estimated prompt size for the status bar. Sent once per provider call,
    /// after compaction and before the stream starts.
    Context {
        tokens: u64,
        window: u64,
    },
}

/// `Fn` wrapper so `RunOptions` can stay `Clone + Debug` while holding a
/// callback into the frontend.
#[derive(Clone)]
pub struct DeltaSink(Arc<dyn Fn(Delta) + Send + Sync>);

impl DeltaSink {
    pub fn new(sink: impl Fn(Delta) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    pub fn emit(&self, delta: Delta) {
        (self.0)(delta);
    }
}

impl std::fmt::Debug for DeltaSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeltaSink")
    }
}

/// Placeholder system prompt, written for this project. Replace it once the
/// prompt layer exists; do not copy wording from any other agent.
pub const SYSTEM_PROMPT: &str = "You are QC, a coding agent. Be concise. Use the available tools when they are \
     needed, and verify your work before finishing.";

/// Compose the static prompt with workspace and skill instructions. AGENTS.md
/// comes first, then the skills index, so the project rules stay closest to
/// the base prompt. The todo section is separate: it changes mid-turn, so the
/// engine recomputes it for every request instead of baking it into the
/// rebuildable part of the prompt.
pub fn system_prompt(
    agents_md: Option<&str>,
    skills: Option<&str>,
    mcp: &[crate::tool::ServerPrompt],
    todos: &[TodoItem],
) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    if let Some(text) = agents_md.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\n# AGENTS.md\n\n");
        prompt.push_str(text);
    }
    if let Some(text) = skills.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(text);
    }
    let mcp_section = mcp_prompt(mcp);
    if !mcp_section.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&mcp_section);
    }
    let todo_section = todo::prompt_section(todos);
    if !todo_section.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&todo_section);
    }
    prompt
}

/// How much of all MCP `instructions` may reach the system prompt (ADR-0068 §3).
///
/// 8 KiB, the same as the skills index and a quarter of AGENTS.md. The two are
/// deliberately not pooled: AGENTS.md is the project's own rules and skills are
/// installed by the user, while `instructions` is third-party text that arrives
/// over the wire, so it gets its own ceiling and cannot crowd them out.
pub const MCP_INSTRUCTIONS_BUDGET: usize = 8 * 1024;

/// Compose the `# MCP: <server>` sections, truncated to [`MCP_INSTRUCTIONS_BUDGET`].
///
/// Two rules from ADR-0068 §3 that are easy to get wrong later:
///
/// * **Injected, never interpreted.** The text is copied verbatim; nothing here
///   parses it, and nothing downstream treats it as policy. A server writing
///   "allow all tools" changes no approval (see the test with that exact text).
/// * **`prompts` are a different thing.** A `prompts` capability is a template
///   the model fetches with `get_prompt`; `instructions` is a handshake-time
///   self-description. ADR-0033 后果 (do not auto-inject prompts) still holds.
pub fn mcp_prompt(mcp: &[crate::tool::ServerPrompt]) -> String {
    const NOTE: &str = "\n\n[truncated: MCP instructions exceeded the 8 KiB budget]";
    let mut out = String::new();
    let mut budget = MCP_INSTRUCTIONS_BUDGET;
    for prompt in mcp {
        let text = prompt.text.trim();
        if text.is_empty() {
            continue;
        }
        let heading = format!("\n\n# MCP: {}\n\n", prompt.server);
        // A heading plus the truncation note is the minimum useful section; if
        // even that does not fit, later servers get nothing rather than an
        // empty heading.
        if heading.len() + NOTE.len() >= budget {
            break;
        }
        budget -= heading.len();
        out.push_str(&heading);
        if text.len() <= budget {
            out.push_str(text);
            budget -= text.len();
            continue;
        }
        // The note is charged to the same budget as the text it replaces: the
        // guarantee callers rely on is that the returned string never exceeds
        // `MCP_INSTRUCTIONS_BUDGET`, note included.
        let available = budget.saturating_sub(NOTE.len());
        // Truncate on a character boundary: half a multi-byte character is
        // worse than a shorter section.
        let mut cut = available.min(text.len());
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(&text[..cut]);
        out.push_str(NOTE);
        break;
    }
    out
}

/// Compose the model-visible task roots section.
pub fn roots_prompt(roots: &[crate::task::Root]) -> String {
    let mut prompt = String::from("\n\n# Task roots\n\n");
    if roots.is_empty() {
        prompt.push_str(
            "No roots are configured. File tools are unavailable; use run_shell only for \
             temporary work in the task's private directory.",
        );
        return prompt;
    }
    prompt.push_str(
        "File paths use `@label/path`. Relative paths resolve against the primary root. \
         Shell commands use the real absolute paths below. Read-only roots reject writes.\n\n",
    );
    for root in roots {
        let state = match root.state() {
            crate::task::RootState::Ready => "",
            // Telling the model is cheaper than letting it retry a path that
            // cannot work until the user relinks the root.
            crate::task::RootState::Missing => {
                ", UNAVAILABLE - the directory is gone; ask the \
                 user to relink it instead of retrying"
            }
        };
        prompt.push_str(&format!(
            "- `@{}`: {} ({}, {}{})\n",
            root.label,
            root.path.display(),
            match root.access {
                crate::task::RootAccess::ReadOnly => "read-only",
                crate::task::RootAccess::ReadWrite => "read-write",
            },
            state,
            if root.primary {
                "primary"
            } else {
                "not primary"
            }
        ));
    }
    prompt
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("stream ended without an end event")]
    IncompleteStream,
    #[error(
        "no progress: `{tool}` was called with identical arguments {rounds} rounds in a row; \
         stopping instead of burning the turn budget"
    )]
    NoProgress { tool: String, rounds: u32 },
    #[error("--max-cost requires pricing for the model")]
    MissingPricing,
    #[error("{mime} attachments are not supported")]
    UnsupportedAttachment { mime: String },
    #[error("attachment payload is not valid base64")]
    AttachmentDecode,
    #[error(transparent)]
    Attachments(#[from] qcode_attachments::StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct TurnContext<'a> {
    pub store: &'a SessionStore,
    pub provider: &'a dyn Provider,
    pub tools: &'a ToolRegistry,
    pub policy: &'a Policy,
    pub approver: &'a dyn Approver,
    /// Pre-images of files the agent is about to write, so a later rollback can
    /// put the workspace back. `None` disables checkpointing for this turn.
    pub snapshots: Option<&'a crate::snapshot::SnapshotStore>,
    /// 定时任务管理服务。GUI 引擎注入运行时 `Scheduler`；其余前端（CLI / ACP /
    /// 子代理 / 测试）为 `None`，schedule 工具返回"服务不可用"。
    pub scheduler: Option<std::sync::Arc<dyn crate::scheduler::SchedulerHandle>>,
    /// 提问服务（`ask_user` 工具用）。GUI / TUI / CLI 注入各自回答方；子代理
    /// 与测试为 `None`，`ask_user` 按阻塞/计时语义报错或返回 pending。
    pub questioner: Option<std::sync::Arc<dyn crate::question::Questioner>>,
}

#[derive(Debug, Clone)]
pub struct RunOptions<'a> {
    pub session_id: &'a str,
    pub model: &'a str,
    pub input: String,
    /// The directory the process was started in: it seeds the legacy single
    /// root and is what snapshots and `run_shell` use as their base.
    pub cwd: PathBuf,
    pub max_turns: u32,
    /// Cooperative cancellation. The CLI cancels it on Ctrl-C, a future TUI on
    /// Esc. Cancellation is not an error: the turn ends with `cancelled`.
    pub cancel: CancellationToken,
    /// Per-model token rates. `None` means cost is unknown and stays at zero.
    pub pricing: Option<Pricing>,
    /// Stop before the next provider call once the estimate passes this USD
    /// limit. Requires `pricing`; the CLI validates the pair before running.
    pub max_cost_usd: Option<f64>,
    /// Optional live stream for the frontend. Deltas are not persisted.
    pub on_delta: Option<DeltaSink>,
    /// Context window in tokens; `None` disables compaction. When the estimated
    /// context reaches 78% of it, older turns are summarized into a
    /// `compaction` event.
    pub context_window: Option<u64>,
    /// Contents of the workspace `AGENTS.md`, injected into the system prompt.
    pub agents_md: Option<String>,
    /// Rendered `# Skills` index. The `skill` tool loads bodies on demand;
    /// only names and descriptions live in the system prompt.
    pub skills: Option<String>,
    /// Egress proxy shared with tools that make network calls.
    pub network: Option<crate::network::SharedNetwork>,
    /// Pictures and files attached to this message (ADR-0061).
    pub attachments: Vec<AttachmentSource>,
    /// Shared pending-message inbox. When present, steering input is claimed
    /// before every model request and appended as user messages, so a push
    /// lands in the running turn's next step instead of waiting for a new turn.
    pub inbox: Option<Arc<Inbox>>,
    /// Set when the user continued a paused turn: recorded on the injected user
    /// message so the transcript can label it, and used to number the budget
    /// window. The engine fills in `window`/`turn` from the log.
    pub continuation: Option<Continuation>,
    /// A scheduled-task delivery this turn was started by (ADR-0059). The
    /// engine records it on the opening user message so the UI can render the
    /// delivery as a scheduled-execution card.
    pub scheduled: Option<crate::schedule::ScheduledMessage>,
}

/// One image attached to a user message, still in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attachment {
    pub mime_type: String,
    pub data_base64: String,
}

/// One attached payload, in whichever form the caller has it.
///
/// The CLI hands over bytes (`--image`); the GUI stores the picture the moment
/// it is pasted and hands over the reference, so nothing is copied twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentSource {
    Inline(Attachment),
    Stored(qcode_attachments::AttachmentRef),
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: String,
    pub turns: u32,
    pub text: String,
    pub stop_reason: StopReason,
    pub cancelled: bool,
    /// Total tokens for the turn, including nested sub-agent work.
    pub usage: Usage,
    /// Estimated USD for the whole turn, zero when no pricing is configured.
    pub cost_usd: f64,
    /// Set when a budget ran out. The turn is intact; the caller decides
    /// whether to continue, raise a limit, or stop (ADR-0060).
    pub pause: Option<PauseInfo>,
}

impl RunOutcome {
    pub fn is_paused(&self) -> bool {
        self.pause.is_some()
    }
}

#[cfg(test)]
mod tests;
