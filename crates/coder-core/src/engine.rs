//! Single-turn engine for M0.
//!
//! Deliberately small: append the turn events, drive one provider stream, store
//! the assistant message, close the turn. The tool loop lands in M1, which is
//! why tool calls currently fail loudly instead of being silently dropped.

use futures_util::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::event::{
    AssistantMessage, ContentBlock, ErrorEvent, Event, EventKind, TurnEnded, TurnStarted,
    TurnStatus, Usage, UserMessage,
};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, Role, StopReason, StreamEvent,
};
use crate::store::{SessionStore, StoreError};

/// Placeholder system prompt, written for this project. Replace it once the
/// prompt layer exists; do not copy wording from any other agent.
pub const SYSTEM_PROMPT: &str = "You are coder, a terminal coding agent. Be concise. Use the available tools when they are \
     needed, and verify your work before finishing.";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("stream ended without an end event")]
    IncompleteStream,
    #[error("tool calls are not implemented yet")]
    ToolCallsUnsupported,
}

#[derive(Debug, Clone)]
pub struct RunOptions<'a> {
    pub session_id: &'a str,
    pub model: &'a str,
    pub input: &'a str,
    #[allow(dead_code)]
    pub max_turns: u32,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: String,
    pub turns: u32,
    pub text: String,
    pub stop_reason: StopReason,
}

/// Appends events for one session while keeping `seq` and `turn` consistent.
pub struct SessionWriter<'a> {
    store: &'a SessionStore,
    session_id: String,
    seq: u64,
    turn: u32,
}

impl<'a> SessionWriter<'a> {
    pub fn resume(store: &'a SessionStore, session_id: &str) -> Result<Self, StoreError> {
        let events = store.load(session_id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0);
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        Ok(Self {
            store,
            session_id: session_id.to_string(),
            seq,
            turn,
        })
    }

    pub fn append(
        &mut self,
        kind: EventKind,
        sink: &mut dyn FnMut(&Event),
    ) -> Result<Event, StoreError> {
        let seq = self.seq + 1;
        let event = Event::new(seq, &self.session_id, self.turn, kind);
        self.store.append(&event)?;
        self.seq = seq;
        sink(&event);
        Ok(event)
    }

    pub fn begin_turn(
        &mut self,
        input: &str,
        sink: &mut dyn FnMut(&Event),
    ) -> Result<(), StoreError> {
        self.turn += 1;
        self.append(
            EventKind::TurnStarted(TurnStarted {
                input: input.to_string(),
            }),
            sink,
        )?;
        self.append(
            EventKind::UserMessage(UserMessage {
                text: input.to_string(),
                attachments: Vec::new(),
            }),
            sink,
        )?;
        Ok(())
    }

    pub fn turn(&self) -> u32 {
        self.turn
    }
}

pub async fn run_turn<F>(
    store: &SessionStore,
    provider: &dyn Provider,
    options: RunOptions<'_>,
    mut on_event: F,
) -> Result<RunOutcome, EngineError>
where
    F: FnMut(&Event),
{
    let mut writer = SessionWriter::resume(store, options.session_id)?;
    writer.begin_turn(options.input, &mut on_event)?;

    let request = CompletionRequest {
        model: options.model.to_string(),
        system: SYSTEM_PROMPT.to_string(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: options.input.to_string(),
            }],
        }],
        tools: Vec::new(),
        max_output_tokens: None,
    };

    let cancel = CancellationToken::new();
    let mut stream = provider.stream(request, cancel).await?;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::EndTurn;
    let mut ended = false;

    while let Some(item) = stream.next().await {
        match item? {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
            StreamEvent::ToolCallDone {
                call_id,
                name,
                arguments,
            } => tool_blocks.push(ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
            }),
            StreamEvent::Usage(reported) => usage = reported,
            StreamEvent::Ended {
                stop_reason: reason,
            } => {
                stop_reason = reason;
                ended = true;
                break;
            }
            StreamEvent::Started | StreamEvent::ToolCallDelta { .. } => {}
        }
    }

    if !ended {
        return Err(EngineError::IncompleteStream);
    }

    let has_tool_calls = !tool_blocks.is_empty();
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            text: reasoning,
            signature: None,
        });
    }
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text: text.clone() });
    }
    blocks.extend(tool_blocks);

    writer.append(
        EventKind::AssistantMessage(AssistantMessage {
            blocks,
            provider_raw: json!({ "provider": provider.name() }),
            usage: usage.clone(),
        }),
        &mut on_event,
    )?;

    if has_tool_calls || stop_reason == StopReason::ToolUse {
        writer.append(
            EventKind::Error(ErrorEvent {
                kind: "unsupported".to_string(),
                message: "tool calls are not implemented yet".to_string(),
                retryable: false,
            }),
            &mut on_event,
        )?;
        writer.append(
            EventKind::TurnEnded(TurnEnded {
                status: TurnStatus::Failed,
                usage,
                cost_usd: 0.0,
            }),
            &mut on_event,
        )?;
        return Err(EngineError::ToolCallsUnsupported);
    }

    writer.append(
        EventKind::TurnEnded(TurnEnded {
            status: TurnStatus::Completed,
            usage,
            cost_usd: 0.0,
        }),
        &mut on_event,
    )?;

    Ok(RunOutcome {
        session_id: options.session_id.to_string(),
        turns: writer.turn(),
        text,
        stop_reason,
    })
}
