//! Anthropic Messages API provider (streaming).
//!
//! Anthropic's wire format differs from OpenAI's chat-completions:
//!
//! - `system` is a top-level field, not a message;
//! - images use `source: {type: "base64", media_type, data}`;
//! - tool results are user messages with `tool_result` blocks;
//! - streaming uses `message_start` / `content_block_*` / `message_delta` /
//!   `message_stop` events, and tool arguments arrive as `input_json_delta`.
//!
//! The HTTP shell is the only part that needs a socket; request conversion and
//! the SSE mapper are pure and covered by recorded fixtures.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use qcode_core::event::{ContentBlock, Usage};
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Message, Provider, ProviderError, Role,
    StopReason, StreamEvent,
};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::sse::{SseDecoder, SseEvent};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;

#[derive(Clone)]
pub struct AnthropicConfig {
    /// Display name, e.g. `anthropic` or `claude`.
    pub name: String,
    /// Base URL, e.g. `https://api.anthropic.com` or a compatible gateway.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Session egress proxy; `None` uses a direct connection (tests/offline).
    pub proxy_url: Option<String>,
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("proxy_url", &self.proxy_url)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    config: AnthropicConfig,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(proxy) = &config.proxy_url {
            let proxy = reqwest::Proxy::all(proxy)
                .map_err(|err| ProviderError::Network(err.to_string()))?;
            builder = builder.proxy(proxy).no_proxy();
        }
        let client = builder
            .build()
            .map_err(|err| ProviderError::Network(err.to_string()))?;
        Ok(Self { client, config })
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: true,
            streaming_tool_args: true,
            reasoning: true,
            prompt_cache: true,
            images: true,
            max_context: None,
            max_output: None,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let body = build_body(&request)?;
        let mut builder = self
            .client
            .post(self.endpoint())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .json(&body);
        if let Some(key) = &self.config.api_key {
            builder = builder.header("x-api-key", key);
        }

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = builder.send() => {
                result.map_err(|err| ProviderError::Network(err.to_string()))?
            }
        };

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let text = response.text().await.unwrap_or_default();
            return Err(map_status(status.as_u16(), &text, retry_after));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut decoder = SseDecoder::new();
            let mut state = StreamState::default();
            let mut bytes = response.bytes_stream();
            let mut ended = false;

            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = tx.send(Err(ProviderError::Cancelled)).await;
                        return;
                    }
                    next = bytes.next() => next,
                };

                let Some(chunk) = chunk else {
                    break;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        let _ = tx.send(Err(ProviderError::Network(err.to_string()))).await;
                        return;
                    }
                };

                for event in decoder.feed(&chunk) {
                    match process_event(&event, &mut state) {
                        Ok(events) => {
                            for event in events {
                                if matches!(event, StreamEvent::Ended { .. }) {
                                    ended = true;
                                }
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err)).await;
                            return;
                        }
                    }
                    if ended {
                        return;
                    }
                }
            }

            // EOF without `message_stop`: close the stream so the engine can
            // finish. A truncated tool call is reported as a protocol error.
            if !ended {
                match finish(&mut state) {
                    Ok(events) => {
                        for event in events {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err)).await;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

#[derive(Debug, Default)]
struct StreamState {
    usage: Usage,
    stop_reason: Option<String>,
    /// Content-block index -> accumulated tool use.
    tools: BTreeMap<u64, ToolAccum>,
    usage_sent: bool,
}

#[derive(Debug, Default)]
struct ToolAccum {
    id: String,
    name: String,
    arguments: String,
}

fn build_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": convert_messages(&request.messages)?,
        "stream": true,
    });
    if !request.system.is_empty() {
        body["system"] = json!(request.system);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(convert_tool).collect());
        body["tool_choice"] = json!({ "type": "auto" });
    }
    Ok(body)
}

/// Convert normalized messages into Anthropic's alternating-role shape.
///
/// Consecutive tool results are one user message with multiple `tool_result`
/// blocks; Anthropic rejects two user messages in a row.
fn convert_messages(messages: &[Message]) -> Result<Vec<Value>, ProviderError> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let (role, blocks) = convert_message(message)?;
        if let Some(last) = out.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut)
        {
            existing.extend(blocks);
            continue;
        }
        out.push(json!({ "role": role, "content": blocks }));
    }
    Ok(out)
}

fn convert_message(message: &Message) -> Result<(&'static str, Vec<Value>), ProviderError> {
    match message.role {
        Role::User => {
            let mut blocks = Vec::new();
            for block in &message.blocks {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    ContentBlock::Image {
                        mime_type,
                        data_base64,
                    } => blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": data_base64,
                        },
                    })),
                    // A user message carrying tool calls is not produced by the
                    // engine; ignore it instead of inventing a role.
                    ContentBlock::ToolCall { .. } | ContentBlock::Reasoning { .. } => {}
                }
            }
            Ok(("user", blocks))
        }
        Role::Assistant => {
            let mut blocks = Vec::new();
            for block in &message.blocks {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    ContentBlock::ToolCall {
                        call_id,
                        name,
                        arguments,
                    } => blocks.push(json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": arguments,
                    })),
                    // Anthropic thinking blocks need their signature; the
                    // normalized history does not carry it, so do not replay a
                    // half-formed thinking block.
                    ContentBlock::Reasoning { .. } | ContentBlock::Image { .. } => {}
                }
            }
            Ok(("assistant", blocks))
        }
        Role::Tool => {
            let call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                ProviderError::InvalidRequest("a tool message must carry tool_call_id".to_string())
            })?;
            let text = message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            Ok((
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": text,
                })],
            ))
        }
    }
}

fn convert_tool(tool: &qcode_core::provider::ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

fn process_event(
    event: &SseEvent,
    state: &mut StreamState,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let data = event.data.trim();
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|err| ProviderError::Protocol(format!("invalid chunk: {err}")))?;
    let kind = event
        .event
        .as_deref()
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");

    match kind {
        "message_start" => {
            if let Some(usage) = value.pointer("/message/usage") {
                state.usage = merge_usage(&state.usage, usage);
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let Some(block) = value.get("content_block") else {
                return Ok(Vec::new());
            };
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        return Ok(vec![StreamEvent::TextDelta(text.to_string())]);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    state.tools.insert(
                        index,
                        ToolAccum {
                            id,
                            name,
                            arguments: String::new(),
                        },
                    );
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        return Ok(vec![StreamEvent::ReasoningDelta(text.to_string())]);
                    }
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let Some(delta) = value.get("delta") else {
                return Ok(Vec::new());
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        return Ok(vec![StreamEvent::TextDelta(text.to_string())]);
                    }
                }
                Some("thinking_delta") => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        return Ok(vec![StreamEvent::ReasoningDelta(text.to_string())]);
                    }
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(tool) = state.tools.get_mut(&index) {
                        tool.arguments.push_str(partial);
                        return Ok(vec![StreamEvent::ToolCallDelta {
                            call_id: tool.id.clone(),
                            name: if tool.name.is_empty() {
                                None
                            } else {
                                Some(tool.name.clone())
                            },
                            arguments_delta: partial.to_string(),
                        }]);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(tool) = state.tools.remove(&index) {
                return Ok(vec![emit_tool(tool)?]);
            }
        }
        "message_delta" => {
            if let Some(usage) = value.get("usage") {
                state.usage = merge_usage(&state.usage, usage);
            }
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(reason.to_string());
            }
            if !state.usage_sent {
                state.usage_sent = true;
                return Ok(vec![StreamEvent::Usage(state.usage.clone())]);
            }
        }
        "message_stop" => {
            let mut out = Vec::new();
            for (_, tool) in std::mem::take(&mut state.tools) {
                out.push(emit_tool(tool)?);
            }
            if !state.usage_sent {
                out.push(StreamEvent::Usage(state.usage.clone()));
            }
            out.push(StreamEvent::Ended {
                stop_reason: stop_reason(state.stop_reason.as_deref()),
            });
            return Ok(out);
        }
        "error" => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("stream error");
            return Err(map_stream_error(
                value.pointer("/error/type").and_then(Value::as_str),
                message,
            ));
        }
        // `ping` and unknown events are ignored for forward compatibility.
        _ => {}
    }

    Ok(Vec::new())
}

fn finish(state: &mut StreamState) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut out = Vec::new();
    for (_, tool) in std::mem::take(&mut state.tools) {
        out.push(emit_tool(tool)?);
    }
    if !state.usage_sent {
        out.push(StreamEvent::Usage(state.usage.clone()));
    }
    out.push(StreamEvent::Ended {
        stop_reason: stop_reason(state.stop_reason.as_deref()),
    });
    Ok(out)
}

fn emit_tool(tool: ToolAccum) -> Result<StreamEvent, ProviderError> {
    let arguments: Value = if tool.arguments.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&tool.arguments).map_err(|err| {
            ProviderError::Protocol(format!("tool arguments are not valid JSON: {err}"))
        })?
    };
    Ok(StreamEvent::ToolCallDone {
        call_id: tool.id,
        name: tool.name,
        arguments,
    })
}

fn merge_usage(current: &Usage, value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .or(current.input_tokens),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .or(current.output_tokens),
        cache_read_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .or(current.cache_read_tokens),
        cache_write_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .or(current.cache_write_tokens),
    }
}

fn stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

fn map_stream_error(kind: Option<&str>, message: &str) -> ProviderError {
    match kind {
        Some("authentication_error") | Some("permission_error") => {
            ProviderError::Auth(message.to_string())
        }
        Some("rate_limit_error") => ProviderError::RateLimit {
            retry_after_secs: None,
            detail: message.to_string(),
        },
        Some("invalid_request_error") => ProviderError::InvalidRequest(message.to_string()),
        _ => ProviderError::Server(message.to_string()),
    }
}

fn map_status(status: u16, body: &str, retry_after: Option<u64>) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(body.to_string()),
        429 => ProviderError::RateLimit {
            retry_after_secs: retry_after,
            detail: body.to_string(),
        },
        400 if body.contains("context")
            || body.contains("too many tokens")
            || body.contains("maximum") =>
        {
            ProviderError::ContextOverflow
        }
        400 | 422 => ProviderError::InvalidRequest(body.to_string()),
        500..=599 => ProviderError::Server(body.to_string()),
        _ => ProviderError::Protocol(format!("unexpected status {status}: {body}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(sse: &str) -> Vec<StreamEvent> {
        let mut decoder = SseDecoder::new();
        let mut state = StreamState::default();
        let mut out = Vec::new();
        for event in decoder.feed(sse.as_bytes()) {
            out.extend(process_event(&event, &mut state).expect("chunk should parse"));
        }
        out
    }

    #[test]
    fn text_stream() {
        let events = collect(include_str!("../tests/fixtures/anthropic_text.sse"));
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Ended {
                stop_reason: StopReason::EndTurn
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Usage(Usage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                ..
            })
        )));
    }

    #[test]
    fn tool_call_stream() {
        let events = collect(include_str!("../tests/fixtures/anthropic_tool_call.sse"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallDelta { .. })),
            "tool argument deltas must be surfaced"
        );
        let (name, arguments) = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallDone {
                    name, arguments, ..
                } => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .expect("a completed tool call");
        assert_eq!(name, "read_file");
        assert_eq!(arguments["path"], "src/main.rs");
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Ended {
                stop_reason: StopReason::ToolUse
            })
        ));
    }

    #[test]
    fn image_and_system_are_converted_to_anthropic_blocks() {
        let request = CompletionRequest {
            model: "claude-test".to_string(),
            system: "be concise".to_string(),
            messages: vec![Message {
                role: Role::User,
                blocks: vec![
                    ContentBlock::Text {
                        text: "look".to_string(),
                    },
                    ContentBlock::Image {
                        mime_type: "image/png".to_string(),
                        data_base64: "aGk=".to_string(),
                    },
                ],
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_output_tokens: None,
        };
        let body = build_body(&request).expect("body");
        assert_eq!(body["system"], "be concise");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
    }

    #[test]
    fn tool_results_are_user_messages_with_tool_result_blocks() {
        let request = CompletionRequest {
            model: "claude-test".to_string(),
            system: String::new(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolCall {
                        call_id: "toolu_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: json!({"path": "a"}),
                    }],
                    tool_call_id: None,
                },
                Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::Text {
                        text: "contents".to_string(),
                    }],
                    tool_call_id: Some("toolu_1".to_string()),
                },
            ],
            tools: Vec::new(),
            max_output_tokens: Some(100),
        };
        let body = build_body(&request).expect("body");
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(body["max_tokens"], 100);
    }

    #[test]
    fn parallel_tool_results_share_one_user_message() {
        let request = CompletionRequest {
            model: "claude-test".to_string(),
            system: String::new(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    blocks: vec![
                        ContentBlock::ToolCall {
                            call_id: "toolu_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: json!({"path": "a"}),
                        },
                        ContentBlock::ToolCall {
                            call_id: "toolu_2".to_string(),
                            name: "read_file".to_string(),
                            arguments: json!({"path": "b"}),
                        },
                    ],
                    tool_call_id: None,
                },
                Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::Text {
                        text: "a".to_string(),
                    }],
                    tool_call_id: Some("toolu_1".to_string()),
                },
                Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::Text {
                        text: "b".to_string(),
                    }],
                    tool_call_id: Some("toolu_2".to_string()),
                },
            ],
            tools: Vec::new(),
            max_output_tokens: None,
        };
        let body = build_body(&request).expect("body");
        assert_eq!(body["messages"].as_array().expect("messages").len(), 2);
        assert_eq!(body["messages"][1]["role"], "user");
        let results = body["messages"][1]["content"]
            .as_array()
            .expect("tool results");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn endpoint_does_not_double_v1() {
        let config = AnthropicConfig {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1/".to_string(),
            api_key: None,
            proxy_url: None,
        };
        let provider = AnthropicProvider::new(config).expect("provider");
        assert_eq!(provider.endpoint(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn debug_redacts_api_key() {
        let config = AnthropicConfig {
            name: "test".to_string(),
            base_url: "https://example.com".to_string(),
            api_key: Some("sk-ant-secret".to_string()),
            proxy_url: None,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-ant-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(map_status(401, "", None), ProviderError::Auth(_)));
        assert!(matches!(
            map_status(429, "", Some(3)),
            ProviderError::RateLimit {
                retry_after_secs: Some(3),
                ..
            }
        ));
        assert!(matches!(
            map_status(400, "too many tokens", None),
            ProviderError::ContextOverflow
        ));
        assert!(matches!(
            map_status(529, "", None),
            ProviderError::Server(_)
        ));
    }
}
