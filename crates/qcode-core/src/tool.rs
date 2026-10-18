//! Tool contract and registry.
//!
//! The engine only knows this trait. Concrete tools live in `qcode-tools`, so
//! the loop stays independent of the filesystem and of process spawning.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::event::{ToolMetadata, ToolStatus};
use crate::provider::ToolSpec;

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace: PathBuf,
}

/// How dangerous a call is, derived from the tool and its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalClass {
    Read,
    WriteInside,
    WriteOutside,
    Execute,
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
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Invalid(String),
    #[error("path escapes the workspace: {0}")]
    PathEscape(String),
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
