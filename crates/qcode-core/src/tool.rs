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

/// One server's tool table changed, as reported by a [`ToolSource`].
///
/// Names are the *bare* server-side names; the source applies whatever
/// namespacing the public registry expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerToolsChanged {
    pub server: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl ServerToolsChanged {
    /// Whether anything actually moved. A refresh that fetched the same list is
    /// not a change and must not produce an event.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// A server's own description of itself, for the system prompt (ADR-0068 §3).
///
/// Carried through the tool source rather than through `RunOptions` on purpose:
/// the text belongs to the connected server, so every frontend (CLI, TUI, GUI,
/// ACP) and every sub-agent gets it without threading a new field through each
/// one. A `RunOptions` field would have to be filled in at 28 call sites, and
/// the one that gets forgotten is silently wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPrompt {
    /// Server name, used as the section heading.
    pub server: String,
    /// The server's `instructions` string, as received.
    pub text: String,
}

/// The result of asking a source for its current tools.
#[derive(Default)]
pub struct ToolRefresh {
    pub tools: Vec<Arc<dyn Tool>>,
    /// Only the servers whose table actually changed.
    pub changed: Vec<ServerToolsChanged>,
}

/// A tool set that may change while the session runs (MCP, ADR-0067 §5).
///
/// Refreshing is deliberately not automatic. The engine asks once, at the start
/// of a turn, and uses the answer for the whole turn: a table that changed
/// between the request the model saw and the calls that request produced would
/// let the model name a tool it was shown and have it be gone by the time the
/// call arrives. `ttlMs = 0` therefore means "before every turn", not "before
/// every round".
///
/// Implementations decide for themselves whether a refresh costs a round trip:
/// `refresh` is allowed to return the cached tools unchanged.
#[async_trait]
pub trait ToolSource: Send + Sync {
    async fn refresh(&self) -> ToolRefresh;

    /// Self-descriptions to inject into the system prompt.
    ///
    /// Provided so a source that has none does not have to say so. The text is
    /// injected, never interpreted: it cannot change an approval decision
    /// (ADR-0068 §3.2).
    fn prompts(&self) -> Vec<ServerPrompt> {
        Vec::new()
    }
}

/// Tools this registry no longer has, and why.
///
/// Only used to turn "unknown tool" into something a model can act on: the
/// difference between "you invented a name" and "that server dropped the tool
/// while you were running" is the whole of ADR-0067 §5's error requirement.
#[derive(Debug, Clone, Default)]
pub struct RetiredTools {
    entries: BTreeMap<String, String>,
}

impl RetiredTools {
    pub fn insert(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        self.entries.insert(name.into(), reason.into());
    }

    pub fn reason(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(String::as_str)
    }

    pub fn remove(&mut self, name: &str) {
        self.entries.remove(name);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Sources whose contents the engine re-reads at the start of every turn.
    /// A snapshot returned by [`Self::refreshed`] carries none of these, so a
    /// refresh cannot recurse.
    sources: Vec<Arc<dyn ToolSource>>,
    /// Tools the registry used to hold, so a call to one explains itself.
    retired: RetiredTools,
    /// Names removed by [`Self::without`], kept so a refresh cannot put them
    /// back. Without this the filter would only hold until the first turn's
    /// call to [`Self::refreshed`], which re-reads the sources and re-inserts
    /// whatever they serve — and the registry returned by `without` is exactly
    /// the one a sub-agent and the ACP service run their turns with.
    excluded: Vec<String>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut map = BTreeMap::new();
        for tool in tools {
            map.insert(tool.name().to_string(), tool);
        }
        Self {
            tools: map,
            sources: Vec::new(),
            retired: RetiredTools::default(),
            excluded: Vec::new(),
        }
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
    ///
    /// `sources` 保留下来（丢掉它们会让子代理与 ACP 的 MCP 工具表从启动起就冻结，
    /// ADR-0067 §5 的每轮刷新形同虚设），因此被排除的名字必须记在 `excluded` 里，
    /// 由 [`Self::refreshed`] 重新套用——否则第一轮刷新就会把它们插回来，
    /// 「不含给定工具」这个承诺只成立到第一个 turn。
    pub fn without(&self, names: &[&str]) -> Self {
        let mut map = BTreeMap::new();
        for (name, tool) in &self.tools {
            if !names.contains(&name.as_str()) {
                map.insert(name.clone(), tool.clone());
            }
        }
        let mut excluded = self.excluded.clone();
        for name in names {
            if !excluded.iter().any(|seen| seen == name) {
                excluded.push((*name).to_string());
            }
        }
        Self {
            tools: map,
            sources: self.sources.clone(),
            retired: self.retired.clone(),
            excluded,
        }
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

    /// Register a source that is re-read at the start of every turn.
    pub fn add_source(&mut self, source: Arc<dyn ToolSource>) {
        self.sources.push(source);
    }

    pub fn has_sources(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Every source's self-description, in registration order.
    ///
    /// Deduplicated by server name because one server may be reachable through
    /// more than one source in a future with per-transport sources; two
    /// identical sections would just be tokens spent twice.
    pub fn server_prompts(&self) -> Vec<ServerPrompt> {
        let mut seen = std::collections::BTreeSet::new();
        let mut prompts = Vec::new();
        for source in &self.sources {
            for prompt in source.prompts() {
                if !seen.insert(prompt.server.clone()) {
                    continue;
                }
                prompts.push(prompt);
            }
        }
        prompts
    }

    /// Why `name` is no longer available, if it used to be.
    pub fn retired_reason(&self, name: &str) -> Option<&str> {
        self.retired.reason(name)
    }

    /// Tool present in this registry but not in `next`, as a retired entry.
    fn retire(missing: impl IntoIterator<Item = String>, reason: &str) -> RetiredTools {
        let mut retired = RetiredTools::default();
        for name in missing {
            retired.insert(name, reason.to_string());
        }
        retired
    }

    /// Ask every source for its current tools and return a frozen snapshot.
    ///
    /// The returned registry has no sources, so the engine can use it for the
    /// whole turn without anything swapping underneath it. `changed` lists only
    /// servers whose table actually moved; a refresh that fetched the same list
    /// produces nothing and therefore writes no event.
    pub async fn refreshed(&self) -> (Self, Vec<ServerToolsChanged>) {
        if self.sources.is_empty() {
            return (self.clone(), Vec::new());
        }
        let mut snapshot = ToolRegistry {
            tools: self.tools.clone(),
            sources: Vec::new(),
            retired: self.retired.clone(),
            excluded: self.excluded.clone(),
        };
        let mut changed = Vec::new();
        for source in &self.sources {
            let refresh = source.refresh().await;
            // Whatever the source itself carries is authoritative for its own
            // names; the merge is a replace, not an append, so a tool the
            // server dropped disappears here.
            snapshot.extend(refresh.tools);
            for diff in refresh.changed {
                if diff.is_empty() {
                    continue;
                }
                // Removal has to be explicit: `extend` is an insert, so a tool
                // the source stopped reporting would otherwise linger and the
                // model would keep seeing a name that is gone.
                for name in &diff.removed {
                    snapshot.tools.remove(name);
                    snapshot.retired.insert(
                        name.clone(),
                        format!(
                            "tool '{name}' no longer exists on MCP server '{}'; it was dropped \
                             from the tool list since the previous turn",
                            diff.server
                        ),
                    );
                }
                for name in &diff.added {
                    snapshot.retired.remove(name);
                }
                changed.push(diff);
            }
        }
        // Re-apply `without`'s filter: a source may serve a name the caller
        // excluded, and the snapshot is what the whole turn runs with.
        for name in &self.excluded {
            snapshot.tools.remove(name);
        }
        (snapshot, changed)
    }

    /// Drop names from this registry, recording why. Used by tests and by any
    /// caller that wants the retired explanation without a live source.
    pub fn retire_missing(&mut self, missing: impl IntoIterator<Item = String>, reason: &str) {
        self.retired = Self::retire(missing, reason);
    }
}
