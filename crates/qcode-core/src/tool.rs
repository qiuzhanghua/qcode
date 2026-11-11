//! Tool contract and registry.
//!
//! The engine only knows this trait. Concrete tools live in `qcode-tools`, so
//! the loop stays independent of the filesystem and of process spawning.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::{ToolDiff, ToolMetadata, ToolStatus, Usage};
use crate::network::Network;
use crate::provider::ToolSpec;
use crate::scheduler::SchedulerHandle;
use crate::task::{Root, RootAccess};

/// Everything a tool is allowed to know about its surroundings.
///
/// `cancel` is the run-wide cancellation token. Tools that can block for a long
/// time (notably `run_shell`) must observe it and stop what they are doing;
/// the engine deliberately does not drop the `execute` future, because dropping
/// it would leave the cleanup path half-run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Authorized task roots. `workspace` is only the legacy primary root: new
    /// code should set roots explicitly with [`Self::with_roots`].
    pub roots: Vec<Root>,
    /// Turn-private directory for a rootless task's shell.
    pub temp_dir: Option<PathBuf>,
    pub cancel: CancellationToken,
    /// Egress proxy for tools that make network calls.
    pub network: Option<Arc<dyn Network>>,
    /// 定时任务管理服务（GUI 引擎注入；`None` = 该环境没有定时任务引擎，
    /// schedule 工具返回错误而不是静默失败）。
    pub scheduler: Option<Arc<dyn SchedulerHandle>>,
    /// 提问服务（`ask_user` 工具用）。`None` = 该环境无人可回答，工具按
    /// 阻塞/计时语义返回错误或 pending。
    pub questioner: Option<Arc<dyn crate::question::Questioner>>,
    /// The provider's `tool_call_id` of the call being executed. Set by the
    /// engine per call; empty in tests and for legacy contexts. `ask_user`
    /// keys its question lifecycle on it.
    pub call_id: String,
    /// The session this call belongs to. Set by the engine per turn; empty in
    /// tests. `schedule_create` uses it to bind a task to the conversation it
    /// was created in.
    pub session_id: String,
}

impl ToolContext {
    /// Legacy single-root context: the directory becomes a root labelled
    /// [`crate::task::LEGACY_ROOT_LABEL`]. Task-aware code uses
    /// [`Self::with_roots`] instead.
    /// A context with one root built from a directory.
    ///
    /// This is the legacy shape and what most tests want; production builds a
    /// context from a task with [`Self::for_roots`].
    pub fn for_test(primary_root: impl Into<PathBuf>) -> Self {
        let primary_root = primary_root.into();
        let roots = vec![Root {
            id: crate::task::LEGACY_ROOT_LABEL.to_string(),
            label: crate::task::LEGACY_ROOT_LABEL.to_string(),
            path: primary_root.clone(),
            access: RootAccess::ReadWrite,
            primary: true,
        }];
        Self {
            roots,
            temp_dir: None,
            cancel: CancellationToken::new(),
            network: None,
            scheduler: None,
            questioner: None,
            call_id: String::new(),
            session_id: String::new(),
        }
    }

    /// Replace the default single root with the task's authorized roots.
    pub fn with_roots(mut self, roots: Vec<Root>) -> Self {
        self.roots = roots;
        self
    }

    /// Set the turn-private directory used by a rootless task's shell.
    pub fn with_temp_dir(mut self, temp: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(temp.into());
        self
    }

    /// Where `run_shell` starts when the call has no explicit `cwd`.
    ///
    /// The primary root wins, then any root, then the turn's private temp. A
    /// rootless task therefore runs in its temp directory, never in whatever
    /// directory qc happened to be started from.
    pub fn shell_cwd(&self) -> Option<PathBuf> {
        self.roots
            .iter()
            .find(|root| root.primary)
            .or_else(|| self.roots.first())
            .map(|root| root.path.clone())
            .or_else(|| self.temp_dir.clone())
    }
    /// Build a context from a task's authorized roots and nothing else.
    pub fn for_roots(roots: Vec<Root>) -> Self {
        Self {
            roots,
            temp_dir: None,
            cancel: CancellationToken::new(),
            network: None,
            scheduler: None,
            questioner: None,
            call_id: String::new(),
            session_id: String::new(),
        }
    }
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_network(mut self, network: Arc<dyn Network>) -> Self {
        self.network = Some(network);
        self
    }

    /// 注入定时任务管理服务（`None` 可显式清除）。引擎在 turn 开始时设置它。
    pub fn with_scheduler(mut self, scheduler: Option<Arc<dyn SchedulerHandle>>) -> Self {
        self.scheduler = scheduler;
        self
    }

    /// 注入提问服务（`None` 可显式清除）。`ask_user` 工具经此向用户提问。
    pub fn with_questioner(
        mut self,
        questioner: Option<Arc<dyn crate::question::Questioner>>,
    ) -> Self {
        self.questioner = questioner;
        self
    }

    /// 标记当前正在执行的调用 id。引擎在每次 execute 前设置。
    pub fn with_call_id(mut self, call_id: &str) -> Self {
        self.call_id = call_id.to_string();
        self
    }

    /// 标记当前调用所属的会话 id。引擎在 turn 开始时设置。
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = session_id.to_string();
        self
    }
}

/// How dangerous a call is, derived from the tool and its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalClass {
    Read,
    WriteInside,
    WriteOutside,
    Execute,
    /// An MCP tool: its side effects are unknown to the engine, so it is never
    /// treated as a read. The default policy asks before every call.
    External,
    /// A multi-file write (`apply_patch`). Default ask even though the targets
    /// are inside the workspace.
    BulkWrite,
    /// A network read (`web_fetch`). Default ask; the network is outside the
    /// workspace boundary.
    Network,
    /// The model asking the user (`ask_user`). The default policy always allows
    /// it: the question *is* the interaction, gating it again would only add a
    /// prompt nobody asked for.
    Question,
    /// A shell command at the session's write scope (`run_shell` without a
    /// `sandbox_permissions` argument). Default allow: the sandbox — not a
    /// per-command prompt — is the boundary (SANDBOX-MODE.md).
    Shell,
    /// A shell command asking to widen its write scope. Default ask: this is
    /// the only point where a command prompts for permission.
    Escalation,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub status: ToolStatus,
    pub output: String,
    pub metadata: ToolMetadata,
    /// Tokens consumed by work the tool performed on the engine's behalf (a
    /// sub-agent). The engine merges this into the parent turn's usage.
    pub usage: Usage,
    /// Estimated USD for that work, using the parent's pricing.
    pub cost_usd: f64,
    /// Untruncated output when `metadata.truncated` is set; the engine writes it
    /// to `sessions/<id>/outputs/<call_id>.txt` and records the path.
    pub full_output: Option<String>,
    /// What the call changed on disk, for the inline diff view. Only the
    /// writing tools fill this in; everything else leaves it `None`.
    pub diff: Option<ToolDiff>,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            metadata: ToolMetadata {
                bytes: output.len() as u64,
                ..ToolMetadata::default()
            },
            status: ToolStatus::Ok,
            output,
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: None,
            diff: None,
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            metadata: ToolMetadata {
                bytes: output.len() as u64,
                ..ToolMetadata::default()
            },
            status: ToolStatus::Error,
            output,
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: None,
            diff: None,
        }
    }

    /// Attach the untruncated text. Ignored when `truncated` is false.
    pub fn with_full_output(mut self, full: impl Into<String>, truncated: bool) -> Self {
        if truncated {
            self.full_output = Some(full.into());
        }
        self
    }

    pub fn with_metadata(mut self, metadata: ToolMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Attach the change this call made, so the transcript can show it.
    pub fn with_diff(mut self, diff: Option<ToolDiff>) -> Self {
        self.diff = diff;
        self
    }

    /// A call that was stopped because the run was cancelled. Partial output is
    /// kept: a cancelled `cargo test` still tells the model what it had done.
    pub fn cancelled(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            metadata: ToolMetadata {
                bytes: output.len() as u64,
                ..ToolMetadata::default()
            },
            status: ToolStatus::Cancelled,
            output,
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: None,
            diff: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Invalid(String),
    #[error("path escapes the authorized roots: {0}")]
    PathEscape(String),
    #[error("task has no roots; file tools are unavailable")]
    NoRoots,
    #[error("root `{0}` is read-only")]
    ReadOnly(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("sandbox error: {0}")]
    Sandbox(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn spec(&self) -> ToolSpec;

    /// Never performs I/O that changes state; used to classify the call before
    /// the policy engine and the approver see it.
    fn approval(&self, ctx: &ToolContext, args: &Value) -> ApprovalClass;

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut map = BTreeMap::new();
        for tool in tools {
            map.insert(tool.name().to_string(), tool);
        }
        Self { tools: map }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Add tools, typically MCP tools discovered at startup. Later tools win on
    /// a name collision; MCP names are prefixed with `mcp__`, so they cannot
    /// shadow a built-in tool.
    pub fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for tool in tools {
            self.tools.insert(tool.name().to_string(), tool);
        }
    }

    /// 一个不含给定工具的新注册表（原注册表不变）。
    ///
    /// 用于子代理 / 无头服务：定时任务管理工具只对带运行时驱动的会话开放，
    /// 子代理上下文拿不到 `SchedulerHandle`，直接从工具清单里去掉更干净。
    pub fn without(&self, names: &[&str]) -> Self {
        let mut map = BTreeMap::new();
        for (name, tool) in &self.tools {
            if !names.contains(&name.as_str()) {
                map.insert(name.clone(), tool.clone());
            }
        }
        Self { tools: map }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
