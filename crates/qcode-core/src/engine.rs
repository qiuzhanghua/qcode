//! The turn loop: model call -> tool calls -> approvals -> tool results -> repeat.
//!
//! The engine owns the state machine, the event log and the policy boundary.
//! Providers only stream; tools only execute; approvers only answer.

use std::path::PathBuf;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::approval::{Answer, ApprovalRequest, Approver};
use crate::cost::Pricing;
use crate::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, ContentBlock, Decision, ErrorEvent,
    Event, EventKind, Scope, ToolCall, ToolResult, ToolStatus, TurnEnded, TurnStarted, TurnStatus,
    Usage, UserMessage,
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
pub const SYSTEM_PROMPT: &str = "You are qcode, a terminal coding agent. Be concise. Use the available tools when they are \
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
    #[error("estimated cost ${spent:.6} exceeded --max-cost ${limit:.6}")]
    MaxCost { spent: f64, limit: f64 },
    #[error("--max-cost requires pricing for the model")]
    MissingPricing,
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
    pub input: String,
    pub workspace: PathBuf,
    pub max_turns: u32,
    /// Cooperative cancellation. The CLI cancels it on Ctrl-C, a future TUI on
    /// Esc. Cancellation is not an error: the turn ends with `cancelled`.
    pub cancel: CancellationToken,
    /// Per-model token rates. `None` means cost is unknown and stays at zero.
    pub pricing: Option<Pricing>,
    /// Stop before the next provider call once the estimate passes this USD
    /// limit. Requires `pricing`; the CLI validates the pair before running.
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: String,
    pub turns: u32,
    pub text: String,
    pub stop_reason: StopReason,
    pub cancelled: bool,
    /// Estimated USD for the whole turn, zero when no pricing is configured.
    pub cost_usd: f64,
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
    if options.max_cost_usd.is_some() && options.pricing.is_none() {
        return Err(EngineError::MissingPricing);
    }

    let mut writer = SessionWriter::resume(ctx.store, options.session_id)?;
    writer.begin_turn(&options.input, &mut on_event)?;

    let events = ctx.store.load(options.session_id)?;
    let mut messages = history::messages_from_events(&events);
    let tool_context =
        ToolContext::new(options.workspace.clone()).with_cancel(options.cancel.clone());

    let mut turns = 0;
    let mut total_usage = Usage::default();
    let mut total_cost = 0.0;
    loop {
        if options.cancel.is_cancelled() {
            return finish(
                &mut writer,
                TurnStatus::Cancelled,
                total_usage,
                total_cost,
                String::new(),
                StopReason::EndTurn,
                options.session_id,
                turns,
                &mut on_event,
            );
        }

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
                    cost_usd: total_cost,
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

        let collected = collect_stream(ctx.provider, request, &options.cancel).await?;
        total_usage.merge(&collected.usage);
        if let Some(pricing) = &options.pricing {
            total_cost += pricing.cost_usd(&collected.usage);
        }

        // Partial output survives cancellation: the model may already have
        // said something useful before Ctrl-C.
        if !collected.blocks.is_empty() {
            writer.append(
                EventKind::AssistantMessage(AssistantMessage {
                    blocks: collected.blocks.clone(),
                    provider_raw: json!({ "provider": ctx.provider.name() }),
                    usage: collected.usage.clone(),
                }),
                &mut on_event,
            )?;
            messages.push(Message {
                role: Role::Assistant,
                blocks: collected.blocks.clone(),
                tool_call_id: None,
            });
        }

        if collected.cancelled {
            return finish(
                &mut writer,
                TurnStatus::Cancelled,
                total_usage,
                total_cost,
                blocks_text(&collected.blocks),
                StopReason::EndTurn,
                options.session_id,
                turns,
                &mut on_event,
            );
        }

        if let Some(limit) = options.max_cost_usd
            && total_cost > limit
        {
            writer.append(
                EventKind::Error(ErrorEvent {
                    kind: "max_cost".to_string(),
                    message: format!(
                        "estimated cost ${total_cost:.6} exceeded --max-cost ${limit:.6}"
                    ),
                    retryable: false,
                }),
                &mut on_event,
            )?;
            writer.append(
                EventKind::TurnEnded(TurnEnded {
                    status: TurnStatus::Failed,
                    usage: total_usage.clone(),
                    cost_usd: total_cost,
                }),
                &mut on_event,
            )?;
            return Err(EngineError::MaxCost {
                spent: total_cost,
                limit,
            });
        }

        let calls: Vec<ToolCallBlock> = collected
            .blocks
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
            let text = blocks_text(&collected.blocks);
            return finish(
                &mut writer,
                TurnStatus::Completed,
                total_usage,
                total_cost,
                text,
                collected.stop_reason,
                options.session_id,
                turns,
                &mut on_event,
            );
        }

        let mut halted = false;
        for call in calls {
            if halted || tool_context.cancel.is_cancelled() {
                // Keep the one-call-one-result invariant even when the run is
                // going away; replay and resume depend on it.
                halted = true;
                append_result(
                    &mut writer,
                    &call.call_id,
                    &ToolOutput::cancelled("cancelled by user"),
                    &mut on_event,
                )?;
                continue;
            }

            let output =
                execute_call(&ctx, &tool_context, &mut writer, &call, &mut on_event).await?;
            if output.status == ToolStatus::Cancelled {
                halted = true;
                continue;
            }
            messages.push(Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::Text {
                    text: output.output,
                }],
                tool_call_id: Some(call.call_id),
            });
        }

        if halted {
            return finish(
                &mut writer,
                TurnStatus::Cancelled,
                total_usage,
                total_cost,
                String::new(),
                StopReason::EndTurn,
                options.session_id,
                turns,
                &mut on_event,
            );
        }
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn finish<F>(
    writer: &mut SessionWriter<'_>,
    status: TurnStatus,
    usage: Usage,
    cost_usd: f64,
    text: String,
    stop_reason: StopReason,
    session_id: &str,
    turns: u32,
    on_event: &mut F,
) -> Result<RunOutcome, EngineError>
where
    F: FnMut(&Event),
{
    writer.append(
        EventKind::TurnEnded(TurnEnded {
            status,
            usage,
            cost_usd,
        }),
        on_event,
    )?;
    Ok(RunOutcome {
        session_id: session_id.to_string(),
        turns,
        text,
        stop_reason,
        cancelled: status == TurnStatus::Cancelled,
        cost_usd,
    })
}

struct ToolCallBlock {
    call_id: String,
    name: String,
    arguments: Value,
}

struct Collected {
    blocks: Vec<ContentBlock>,
    usage: Usage,
    stop_reason: StopReason,
    cancelled: bool,
}

async fn collect_stream(
    provider: &dyn Provider,
    request: CompletionRequest,
    cancel: &CancellationToken,
) -> Result<Collected, EngineError> {
    let mut stream = match provider.stream(request, cancel.clone()).await {
        Ok(stream) => stream,
        // Providers check the token while opening the connection (DNS, TLS,
        // waiting for response headers), so cancellation can surface here
        // before a stream ever exists.
        Err(ProviderError::Cancelled) if cancel.is_cancelled() => {
            return Ok(Collected {
                blocks: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                cancelled: true,
            });
        }
        Err(err) => return Err(err.into()),
    };

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_blocks = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::EndTurn;
    let mut ended = false;
    let mut cancelled = false;

    loop {
        // `biased` makes cancellation win over a stream item that is ready in
        // the same poll, so Ctrl-C is never swallowed by a busy stream.
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancelled = true;
                break;
            }
            item = stream.next() => item,
        };

        let Some(item) = item else {
            break;
        };

        match item {
            Ok(StreamEvent::TextDelta(delta)) => text.push_str(&delta),
            Ok(StreamEvent::ReasoningDelta(delta)) => reasoning.push_str(&delta),
            Ok(StreamEvent::ToolCallDone {
                call_id,
                name,
                arguments,
            }) => tool_blocks.push(ContentBlock::ToolCall {
                call_id,
                name,
                arguments,
            }),
            Ok(StreamEvent::Usage(reported)) => usage = reported,
            Ok(StreamEvent::Ended {
                stop_reason: reason,
            }) => {
                stop_reason = reason;
                ended = true;
                break;
            }
            Ok(StreamEvent::Started | StreamEvent::ToolCallDelta { .. }) => {}
            // A provider that notices the token before we do reports Cancelled
            // instead of ending the stream; that is a cancellation, not a bug.
            Err(ProviderError::Cancelled) => {
                cancelled = true;
                break;
            }
            Err(err) => return Err(err.into()),
        }
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

    if cancelled {
        // Never surface half-streamed tool calls: their arguments may be
        // truncated, and executing them after a cancel is exactly wrong.
        return Ok(Collected {
            blocks,
            usage,
            stop_reason: StopReason::EndTurn,
            cancelled: true,
        });
    }

    if !ended {
        return Err(EngineError::IncompleteStream);
    }

    blocks.extend(tool_blocks);
    Ok(Collected {
        blocks,
        usage,
        stop_reason,
        cancelled: false,
    })
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

    if tool_context.cancel.is_cancelled() {
        let output = ToolOutput::cancelled("cancelled by user");
        append_result(writer, &call.call_id, &output, on_event)?;
        return Ok(output);
    }

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
    let mut aborted = false;

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
        let answer = ctx
            .approver
            .approve(&request, tool_context.cancel.clone())
            .await;
        allowed = answer == Answer::Allow;
        // A denial that raced with cancellation is not a user decision.
        aborted = tool_context.cancel.is_cancelled();
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

    if aborted {
        // The approver was still waiting when the run was cancelled. Log the
        // tool result so the call/result invariant holds, then unwind.
        let output = ToolOutput::cancelled("cancelled by user");
        append_result(writer, &call.call_id, &output, on_event)?;
        return Ok(output);
    }

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
