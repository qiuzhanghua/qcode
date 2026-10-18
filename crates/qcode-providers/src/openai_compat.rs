//! OpenAI-compatible chat-completions provider (streaming).
//!
//! Covers OpenAI, DeepSeek, Ollama, vLLM and anything else that speaks the same
//! wire format. The SSE decoder and the chunk mapper are pure and covered by
//! recorded fixtures; only the HTTP shell needs a socket.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use qcode_core::event::{ContentBlock, Usage};
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Message, Provider, ProviderError, Role,
    StopReason, StreamEvent, ToolSpec,
};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::sse::{SseDecoder, SseEvent};

#[derive(Clone)]
pub struct OpenAiCompatConfig {
    /// Display name, e.g. `openai` / `deepseek` / `ollama`.
    pub name: String,
    /// Base URL including the version segment, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Ask for a final usage chunk via `stream_options.include_usage`.
    pub stream_usage: bool,
}

impl std::fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("stream_usage", &self.stream_usage)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    config: OpenAiCompatConfig,
}

impl OpenAiCompatProvider {
    pub fn new(config: OpenAiCompatConfig) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| ProviderError::Network(err.to_string()))?;
        Ok(Self { client, config })
    }

    pub fn config(&self) -> &OpenAiCompatConfig {
        &self.config
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
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
            images: false,
            max_context: None,
            max_output: None,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let mut include_usage = self.config.stream_usage;
        let response = loop {
            let body = build_body(&request, include_usage)?;
            let mut builder = self.client.post(self.endpoint()).json(&body);
            if let Some(key) = &self.config.api_key {
                builder = builder.bearer_auth(key);
            }
            builder = builder.header("accept", "text/event-stream");

            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = builder.send() => {
                    result.map_err(|err| ProviderError::Network(err.to_string()))?
                }
            };

            let status = response.status();
            if status.is_success() {
                break response;
            }

            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let text = response.text().await.unwrap_or_default();

            // Some OpenAI-compatible servers reject `stream_options`; retry
            // once without it instead of failing the whole turn.
            if status.as_u16() == 400 && include_usage && text.contains("stream_options") {
                include_usage = false;
                continue;
            }

            return Err(map_status(status.as_u16(), &text, retry_after));
        };

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut decoder = SseDecoder::new();
            let mut state = ChunkState::default();
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

            // EOF without `[DONE]`: close the stream so the engine can finish.
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
struct ChunkState {
    tool_calls: BTreeMap<u32, ToolCallAccum>,
    finish_reason: Option<String>,
}

#[derive(Debug, Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

fn build_body(request: &CompletionRequest, include_usage: bool) -> Result<Value, ProviderError> {
    let mut messages = vec![json!({
        "role": "system",
        "content": request.system,
    })];

    for message in &request.messages {
        messages.push(convert_message(message)?);
    }

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
    });

    if include_usage {
        body["stream_options"] = json!({ "include_usage": true });
    }

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(convert_tool).collect());
        body["tool_choice"] = json!("auto");
    }
    if let Some(max) = request.max_output_tokens {
        body["max_tokens"] = json!(max);
    }

    Ok(body)
}

fn convert_message(message: &Message) -> Result<Value, ProviderError> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in &message.blocks {
        match block {
            ContentBlock::Text { text: content } => text.push_str(content),
            // Reasoning is replayed as part of `provider_raw` in M1; the
            // normalized request path only carries text for now.
            ContentBlock::Reasoning { .. } => {}
            ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() },
            })),
        }
    }

    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut value = json!({ "role": role, "content": text });
    if let Some(call_id) = &message.tool_call_id {
        value["tool_call_id"] = json!(call_id);
    }
    if !tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(tool_calls);
    }
    Ok(value)
}

fn convert_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

fn process_event(
    event: &SseEvent,
    state: &mut ChunkState,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let data = event.data.trim();
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data == "[DONE]" {
        let mut out = Vec::new();
        emit_tool_calls(state, &mut out)?;
        out.push(StreamEvent::Ended {
            stop_reason: stop_reason(state.finish_reason.as_deref()),
        });
        return Ok(out);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| ProviderError::Protocol(format!("invalid chunk: {err}")))?;

    if let Some(error) = value.get("error") {
        return Err(ProviderError::Server(error.to_string()));
    }

    let mut out = Vec::new();

    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        out.push(StreamEvent::Usage(parse_usage(usage)));
    }

    let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
        return Ok(out);
    };
    let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

    if let Some(text) = delta
        .get("content")
        .and_then(|value| value.as_str())
        .filter(|text| !text.is_empty())
    {
        out.push(StreamEvent::TextDelta(text.to_string()));
    }

    if let Some(text) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(|value| value.as_str())
        .filter(|text| !text.is_empty())
    {
        out.push(StreamEvent::ReasoningDelta(text.to_string()));
    }

    if let Some(calls) = delta.get("tool_calls").and_then(|value| value.as_array()) {
        for call in calls {
            let index = call
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32;
            let id = call
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();

            let entry = state.tool_calls.entry(index).or_default();
            if !id.is_empty() {
                entry.id = id;
            }
            if !name.is_empty() {
                entry.name = name.clone();
            }
            entry.arguments.push_str(&arguments);

            out.push(StreamEvent::ToolCallDelta {
                call_id: entry.id.clone(),
                name: if name.is_empty() { None } else { Some(name) },
                arguments_delta: arguments,
            });
        }
    }

    if let Some(reason) = choice.get("finish_reason").and_then(|value| value.as_str()) {
        state.finish_reason = Some(reason.to_string());
    }

    Ok(out)
}

fn finish(state: &mut ChunkState) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut out = Vec::new();
    emit_tool_calls(state, &mut out)?;
    out.push(StreamEvent::Ended {
        stop_reason: stop_reason(state.finish_reason.as_deref()),
    });
    Ok(out)
}

fn emit_tool_calls(
    state: &mut ChunkState,
    out: &mut Vec<StreamEvent>,
) -> Result<(), ProviderError> {
    for (_, call) in std::mem::take(&mut state.tool_calls) {
        let arguments: Value = serde_json::from_str(&call.arguments).map_err(|err| {
            ProviderError::Protocol(format!("tool arguments are not valid JSON: {err}"))
        })?;
        out.push(StreamEvent::ToolCallDone {
            call_id: call.id,
            name: call.name,
            arguments,
        });
    }
    Ok(())
}

fn stop_reason(finish_reason: Option<&str>) -> StopReason {
    match finish_reason {
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value.get("prompt_tokens").and_then(|v| v.as_u64()),
        output_tokens: value.get("completion_tokens").and_then(|v| v.as_u64()),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        cache_write_tokens: None,
    }
}

fn map_status(status: u16, body: &str, retry_after: Option<u64>) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth(body.to_string()),
        429 => ProviderError::RateLimit {
            retry_after_secs: retry_after,
        },
        400 if body.contains("context_length")
            || body.contains("context window")
            || body.contains("maximum context") =>
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
        let mut state = ChunkState::default();
        let mut out = Vec::new();
        for event in decoder.feed(sse.as_bytes()) {
            out.extend(process_event(&event, &mut state).expect("chunk should parse"));
        }
        out
    }

    #[test]
    fn text_stream() {
        let events = collect(include_str!("../tests/fixtures/openai_text.sse"));
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
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..
            })
        )));
    }

    #[test]
    fn tool_call_stream() {
        let events = collect(include_str!("../tests/fixtures/openai_tool_call.sse"));
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
    fn status_mapping() {
        assert!(matches!(map_status(401, "", None), ProviderError::Auth(_)));
        assert!(matches!(
            map_status(429, "", Some(3)),
            ProviderError::RateLimit {
                retry_after_secs: Some(3)
            }
        ));
        assert!(matches!(
            map_status(400, "maximum context length exceeded", None),
            ProviderError::ContextOverflow
        ));
        assert!(matches!(
            map_status(503, "", None),
            ProviderError::Server(_)
        ));
    }

    #[test]
    fn debug_redacts_api_key() {
        let config = OpenAiCompatConfig {
            name: "test".to_string(),
            base_url: "https://example.com/v1".to_string(),
            api_key: Some("sk-secret-value".to_string()),
            stream_usage: true,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-secret-value"));
        assert!(debug.contains("<redacted>"));
    }
}
