//! The turn loop: model call -> tool calls -> approvals -> tool results -> repeat.
//!
//! The engine owns the state machine, the event log and the policy boundary.
//! Providers only stream; tools only execute; approvers only answer.

use std::path::PathBuf;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::approval::{Answer, ApprovalRequest, Approver};
use crate::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, ContentBlock, Decision, ErrorEvent,
    Event, EventKind, Scope, ToolCall, ToolResult, TurnEnded, TurnStarted, TurnStatus, Usage,
    UserMessage,
};
use crate::history;
use crate::policy::{Action, Policy};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, Role, StopReason, StreamEvent,
};
use crate::store::{SessionStore, StoreError};
use crate::tool::{ToolContext, ToolOutput, ToolRegistry};

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
    #[error("exceeded max turns ({0})")]
    MaxTurns(u32),
}

pub struct TurnContext<'a> {
    pub store: &'a SessionStore,
    pub provider: &'a dyn Provider,
    pub tools: &'a ToolRegistry,
    pub policy: &'a Policy,
    pub approver: &'a dyn Approver,
}

#[derive(Debug, Clone)]
pub struct RunOptions<'a> {
    pub session_id: &'a str,
    pub model: &'a str,
    pub input: &'a str,
    pub workspace: PathBuf,
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
    ctx: TurnContext<'_>,
    options: RunOptions<'_>,
    mut on_event: F,
) -> Result<RunOutcome, EngineError>
where
    F: FnMut(&Event),
{
    let mut writer = SessionWriter::resume(ctx.store, options.session_id)?;
    writer.begin_turn(options.input, &mut on_event)?;

    let events = ctx.store.load(options.session_id)?;
    let mut messages = history::messages_from_events(&events);
    let tool_context = ToolContext {
        workspace: options.workspace.clone(),
    };

    let mut turns = 0;
    let mut total_usage = Usage::default();
    loop {
        turns += 1;
        if turns > options.max_turns {
            writer.append(
                EventKind::Error(ErrorEvent {
                    kind: "max_turns".to_string(),
                    message: format!("exceeded max turns ({})", options.max_turns),
                    retryable: false,
                }),
                &mut on_event,
            )?;
            writer.append(
                EventKind::TurnEnded(TurnEnded {
                    status: TurnStatus::Failed,
                    usage: total_usage.clone(),
                    cost_usd: 0.0,
                }),
                &mut on_event,
            )?;
            return Err(EngineError::MaxTurns(options.max_turns));
        }

        let request = CompletionRequest {
            model: options.model.to_string(),
            system: SYSTEM_PROMPT.to_string(),
            messages: messages.clone(),
            tools: ctx.tools.specs(),
            max_output_tokens: None,
        };

        let (blocks, usage, stop_reason) = collect_stream(ctx.provider, request).await?;
        total_usage.merge(&usage);

        writer.append(
            EventKind::AssistantMessage(AssistantMessage {
                blocks: blocks.clone(),
                provider_raw: json!({ "provider": ctx.provider.name() }),
                usage: usage.clone(),
            }),
            &mut on_event,
        )?;
        messages.push(Message {
            role: Role::Assistant,
            blocks: blocks.clone(),
            tool_call_id: None,
        });

        let calls: Vec<ToolCallBlock> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => Some(ToolCallBlock {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect();

        if calls.is_empty() {
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            writer.append(
                EventKind::TurnEnded(TurnEnded {
                    status: TurnStatus::Completed,
                    usage: total_usage,
                    cost_usd: 0.0,
                }),
                &mut on_event,
            )?;
            return Ok(RunOutcome {
                session_id: options.session_id.to_string(),
                turns,
                text,
                stop_reason,
            });
        }

        for call in calls {
            let output =
                execute_call(&ctx, &tool_context, &mut writer, &call, &mut on_event).await?;
            messages.push(Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::Text {
                    text: output.output,
                }],
                tool_call_id: Some(call.call_id),
            });
        }
    }
}

struct ToolCallBlock {
    call_id: String,
    name: String,
    arguments: Value,
}

async fn collect_stream(
    provider: &dyn Provider,
    request: CompletionRequest,
) -> Result<(Vec<ContentBlock>, Usage, StopReason), EngineError> {
    let cancel = CancellationToken::new();
    let mut stream = provider.stream(request, cancel).await?;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_blocks = Vec::new();
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

    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            text: reasoning,
            signature: None,
        });
    }
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    blocks.extend(tool_blocks);

    Ok((blocks, usage, stop_reason))
}

async fn execute_call<F>(
    ctx: &TurnContext<'_>,
    tool_context: &ToolContext,
    writer: &mut SessionWriter<'_>,
    call: &ToolCallBlock,
    on_event: &mut F,
) -> Result<ToolOutput, EngineError>
where
    F: FnMut(&Event),
{
    writer.append(
        EventKind::ToolCall(ToolCall {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }),
        on_event,
    )?;

    let Some(tool) = ctx.tools.get(&call.name) else {
        let output = ToolOutput::error(format!("unknown tool: {}", call.name));
        append_result(writer, &call.call_id, &output, on_event)?;
        return Ok(output);
    };

    let class = tool.approval(tool_context, &call.arguments);
    let decision = ctx.policy.evaluate(&call.name, class, &call.arguments);
    let reason = decision.reason.clone();
    let rule_id = decision.rule_id.clone();

    let mut allowed = decision.action == Action::Allow;

    if decision.action == Action::Ask {
        let request = ApprovalRequest {
            call_id: call.call_id.clone(),
            tool: call.name.clone(),
            summary: summarize_call(&call.name, &call.arguments),
            reason: reason.clone(),
            rule_id: rule_id.clone(),
        };
        writer.append(
            EventKind::ApprovalRequested(ApprovalRequested {
                call_id: call.call_id.clone(),
                rule_id: rule_id.clone(),
                reason: reason.clone(),
            }),
            on_event,
        )?;
        allowed = ctx.approver.approve(&request).await == Answer::Allow;
    }

    let scope = if rule_id.is_some() {
        Scope::User
    } else {
        Scope::Session
    };

    writer.append(
        EventKind::ApprovalDecided(ApprovalDecided {
            call_id: call.call_id.clone(),
            decision: if allowed {
                Decision::Allow
            } else {
                Decision::Deny
            },
            rule_id,
            scope,
        }),
        on_event,
    )?;

    let output = if allowed {
        match tool.execute(tool_context, call.arguments.clone()).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(err.to_string()),
        }
    } else {
        ToolOutput::error(format!("denied: {reason}"))
    };

    append_result(writer, &call.call_id, &output, on_event)?;
    Ok(output)
}

fn append_result<F>(
    writer: &mut SessionWriter<'_>,
    call_id: &str,
    output: &ToolOutput,
    on_event: &mut F,
) -> Result<(), StoreError>
where
    F: FnMut(&Event),
{
    writer.append(
        EventKind::ToolResult(ToolResult {
            call_id: call_id.to_string(),
            status: output.status,
            output: output.output.clone(),
            metadata: output.metadata.clone(),
        }),
        on_event,
    )?;
    Ok(())
}

fn summarize_call(name: &str, args: &Value) -> String {
    let raw = match name {
        "run_shell" => args
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        "read_file" | "write_file" | "edit_file" | "list_dir" => args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => args.to_string(),
    };

    if raw.chars().count() > 160 {
        format!("{}…", raw.chars().take(160).collect::<String>())
    } else {
        raw
    }
}
