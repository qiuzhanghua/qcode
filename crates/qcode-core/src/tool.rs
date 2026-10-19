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

use crate::event::{ToolMetadata, ToolStatus};
use crate::provider::ToolSpec;

/// Everything a tool is allowed to know about its surroundings.
///
/// `cancel` is the run-wide cancellation token. Tools that can block for a long
/// time (notably `run_shell`) must observe it and stop what they are doing;
/// the engine deliberately does not drop the `execute` future, because dropping
/// it would leave the cleanup path half-run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
    pub cancel: CancellationToken,
}

impl ToolContext {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
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
        }
    }

    pub fn with_metadata(mut self, metadata: ToolMetadata) -> Self {
        self.metadata = metadata;
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
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Invalid(String),
    #[error("path escapes the workspace: {0}")]
    PathEscape(String),
    #[error("network error: {0}")]
    Network(String),
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
