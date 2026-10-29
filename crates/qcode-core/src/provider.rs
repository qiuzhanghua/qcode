//! Provider port.
//!
//! `qcode-core` owns this trait; concrete providers live in `qcode-providers`.
//! The port is deliberately small: capabilities in, a stream of deltas out.
//! Approval, retries, budgets and persistence are the caller's job, not the
//! provider's.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::{ContentBlock, Usage};

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub parallel_tools: bool,
    pub streaming_tool_args: bool,
    pub reasoning: bool,
    pub prompt_cache: bool,
    pub images: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    /// Set for `Role::Tool`: the tool call this message is answering.
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Started,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
    },
    ToolCallDone {
        call_id: String,
        name: String,
        arguments: Value,
    },
    Usage(Usage),
    Ended {
        stop_reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited: {detail}")]
    RateLimit {
        #[allow(dead_code)]
        retry_after_secs: Option<u64>,
        /// 服务端返回的原始内容（如「5 小时额度已用尽」），保留给界面展示
        /// 完整原因，光靠 "rate limited" 不说明问题。
        detail: String,
    },
    #[error("context window exceeded")]
    ContextOverflow,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("cancelled")]
    Cancelled,
    #[error("protocol error: {0}")]
    Protocol(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    async fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError>;
}
