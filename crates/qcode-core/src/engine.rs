//! The turn loop: model call -> tool calls -> approvals -> tool results -> repeat.
//!
//! The engine owns the state machine, the event log and the policy boundary.
//! Providers only stream; tools only execute; approvers only answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::approval::{ApprovalRequest, Approver};
use crate::compact;
use crate::cost::Pricing;
use crate::event::{
    ApprovalDecided, ApprovalRequested, AssistantMessage, Compaction, ContentBlock, Decision,
    ErrorEvent, Event, EventKind, Scope, ToolCall, ToolResult, ToolStatus, TurnEnded, TurnStarted,
    TurnStatus, Usage, UserMessage,
};
use crate::history;
use crate::policy::{Action, Policy};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, Role, StopReason, StreamEvent,
};
use crate::store::{SessionLock, SessionStore, StoreError};
use crate::todo::{self, TodoItem};
use crate::tool::{ApprovalClass, Tool, ToolContext, ToolOutput, ToolRegistry};

/// A streamed delta. Deltas are transient: they drive the UI while the model
/// is talking and are never written to the event log. The completed blocks are
/// persisted later as one `assistant_message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    /// Estimated prompt size for the status bar. Sent once per provider call,
    /// after compaction and before the stream starts.
    Context {
        tokens: u64,
        window: u64,
    },
}

/// `Fn` wrapper so `RunOptions` can stay `Clone + Debug` while holding a
/// callback into the frontend.
#[derive(Clone)]
pub struct DeltaSink(Arc<dyn Fn(Delta) + Send + Sync>);

impl DeltaSink {
    pub fn new(sink: impl Fn(Delta) + Send + Sync + 'static) -> Self {
        Self(Arc::new(sink))
    }

    pub fn emit(&self, delta: Delta) {
        (self.0)(delta);
    }
}

impl std::fmt::Debug for DeltaSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeltaSink")
    }
}

/// Placeholder system prompt, written for this project. Replace it once the
/// prompt layer exists; do not copy wording from any other agent.
pub const SYSTEM_PROMPT: &str = "You are qcode, a terminal coding agent. Be concise. Use the available tools when they are \
     needed, and verify your work before finishing.";

/// Compose the static prompt with workspace and skill instructions. AGENTS.md
/// comes first, then the skills index, so the project rules stay closest to
/// the base prompt. The todo section is separate: it changes mid-turn, so the
/// engine recomputes it for every request instead of baking it into the
/// rebuildable part of the prompt.
pub fn system_prompt(agents_md: Option<&str>, skills: Option<&str>, todos: &[TodoItem]) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    if let Some(text) = agents_md.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\n# AGENTS.md\n\n");
        prompt.push_str(text);
    }
    if let Some(text) = skills.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(text);
    }
    let todo_section = todo::prompt_section(todos);
    if !todo_section.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&todo_section);
    }
    prompt
}

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
    /// Optional live stream for the frontend. Deltas are not persisted.
    pub on_delta: Option<DeltaSink>,
    /// Context window in tokens; `None` disables compaction. When the estimated
    /// context reaches 78% of it, older turns are summarized into a
    /// `compaction` event.
    pub context_window: Option<u64>,
    /// Contents of the workspace `AGENTS.md`, injected into the system prompt.
    pub agents_md: Option<String>,
    /// Rendered `# Skills` index. The `skill` tool loads bodies on demand;
    /// only names and descriptions live in the system prompt.
    pub skills: Option<String>,
    /// Egress proxy shared with tools that make network calls.
    pub network: Option<crate::network::SharedNetwork>,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub session_id: String,
    pub turns: u32,
    pub text: String,
    pub stop_reason: StopReason,
    pub cancelled: bool,
    /// Total tokens for the turn, including nested sub-agent work.
    pub usage: Usage,
    /// Estimated USD for the whole turn, zero when no pricing is configured.
    pub cost_usd: f64,
}

/// Appends events for one session while keeping `seq` and `turn` consistent.
pub struct SessionWriter<'a> {
    store: &'a SessionStore,
    session_id: String,
    seq: u64,
    turn: u32,
    /// Held for the lifetime of the turn; two writers cannot share a session.
    _lock: SessionLock,
}

impl<'a> SessionWriter<'a> {
    pub fn resume(store: &'a SessionStore, session_id: &str) -> Result<Self, StoreError> {
        let lock = store.lock(session_id)?;
        // A previous run may have died between `tool_call` and `tool_result`.
        store.repair_interrupted(session_id)?;
        let events = store.load(session_id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0);
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        Ok(Self {
            store,
            session_id: session_id.to_string(),
            seq,
            turn,
            _lock: lock,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
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

    let mut events = ctx.store.load(options.session_id)?;
    let mut messages = history::messages_from_events(&events);
    // The todo list is session state rebuilt from the log, so a resumed or
    // forked session starts with the list the model last saw.
    let mut todos = todo::current(&events);
    let mut compaction_disabled = false;
    let mut tool_context =
        ToolContext::new(options.workspace.clone()).with_cancel(options.cancel.clone());
    if let Some(network) = &options.network {
        tool_context = tool_context.with_network(network.clone());
    }

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

        if let Some(window) = options.context_window
            && !compaction_disabled
            && compact::estimate_tokens(&messages) >= compact::threshold(window)
        {
            events = ctx.store.load(options.session_id)?;
            if let Some(boundary) = compact::keep_from(&events, compact::KEEP_TURNS) {
                let transcript = compact::summary_input(&events, boundary);
                let summary_request = CompletionRequest {
                    model: options.model.to_string(),
                    system: compact::SUMMARY_SYSTEM.to_string(),
                    messages: vec![Message {
                        role: Role::User,
                        blocks: vec![ContentBlock::Text { text: transcript }],
                        tool_call_id: None,
                    }],
                    tools: Vec::new(),
                    max_output_tokens: Some(1024),
                };

                match collect_stream(ctx.provider, summary_request, &options.cancel, None).await {
                    Ok(summarized) => {
                        total_usage.merge(&summarized.usage);
                        if let Some(pricing) = &options.pricing {
                            total_cost += pricing.cost_usd(&summarized.usage);
                        }
                        if summarized.cancelled {
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

                        let summary = blocks_text(&summarized.blocks).trim().to_string();
                        if summary.is_empty() {
                            compaction_disabled = true;
                            writer.append(
                                EventKind::Error(ErrorEvent {
                                    kind: "compaction_empty".to_string(),
                                    message: "summarizer returned no text; keeping full context"
                                        .to_string(),
                                    retryable: true,
                                }),
                                &mut on_event,
                            )?;
                        } else {
                            let kept: Vec<Event> = events
                                .iter()
                                .filter(|event| event.seq >= boundary)
                                .cloned()
                                .collect();
                            let mut kept_messages = vec![Message {
                                role: Role::User,
                                blocks: vec![ContentBlock::Text {
                                    text: history::summary_text(&summary),
                                }],
                                tool_call_id: None,
                            }];
                            kept_messages.extend(history::messages_from_events(&kept));
                            let tokens_after = compact::estimate_tokens(&kept_messages);
                            let replaced_from =
                                compact::first_message_seq_before(&events, boundary).unwrap_or(1);

                            writer.append(
                                EventKind::Compaction(Compaction {
                                    trigger: compact::TRIGGER.to_string(),
                                    summary,
                                    replaced_from: replaced_from as u32,
                                    replaced_to: (boundary - 1) as u32,
                                    tokens_before: compact::estimate_tokens(&messages),
                                    tokens_after,
                                }),
                                &mut on_event,
                            )?;

                            messages = kept_messages;
                        }
                    }
                    Err(err) => {
                        compaction_disabled = true;
                        writer.append(
                            EventKind::Error(ErrorEvent {
                                kind: "compaction_failed".to_string(),
                                message: err.to_string(),
                                retryable: true,
                            }),
                            &mut on_event,
                        )?;
                    }
                }
            }
        }

        let request = CompletionRequest {
            model: options.model.to_string(),
            system: system_prompt(
                options.agents_md.as_deref(),
                options.skills.as_deref(),
                todos.as_deref().unwrap_or(&[]),
            ),
            messages: messages.clone(),
            tools: ctx.tools.specs(),
            max_output_tokens: None,
        };

        // Transient UI signal only: the estimate is recomputed on every call,
        // after compaction, and never enters the event log.
        if let Some(window) = options.context_window
            && let Some(sink) = &options.on_delta
        {
            sink.emit(Delta::Context {
                tokens: compact::estimate_tokens(&messages),
                window,
            });
        }

        let collected = collect_stream(
            ctx.provider,
            request,
            &options.cancel,
            options.on_delta.as_ref(),
        )
        .await?;
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

        if execute_calls(
            &ctx,
            &tool_context,
            &mut writer,
            calls,
            &mut messages,
            &mut todos,
            &mut total_usage,
            &mut total_cost,
            &mut on_event,
        )
        .await?
        {
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
            usage: usage.clone(),
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
        usage,
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
    on_delta: Option<&DeltaSink>,
) -> Result<Collected, EngineError> {
    // SPEC §10: retry only before the first token, at most three attempts.
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0;

    loop {
        attempt += 1;
        match collect_once(provider, request.clone(), cancel, on_delta).await {
            Ok(collected) => return Ok(collected),
            Err(AttemptFailure::Fatal(error)) => return Err(error),
            Err(AttemptFailure::Provider { error, started }) => {
                if started || attempt >= MAX_ATTEMPTS || !is_retryable(&error) {
                    return Err(error.into());
                }

                let delay = retry_delay(attempt, &error);
                tracing::warn!(
                    attempt,
                    error = %error,
                    delay_ms = delay.as_millis(),
                    "provider failed before the first token; retrying"
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Ok(Collected {
                            blocks: Vec::new(),
                            usage: Usage::default(),
                            stop_reason: StopReason::EndTurn,
                            cancelled: true,
                        });
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

enum AttemptFailure {
    /// The stream failed; `started` records whether any token was emitted.
    Provider { error: ProviderError, started: bool },
    /// Not retryable at this layer (e.g. a truncated stream).
    Fatal(EngineError),
}

fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::RateLimit { .. } | ProviderError::Server(_) | ProviderError::Network(_)
    )
}

/// Exponential backoff with jitter. `Retry-After` wins when the server sent it
/// (tests use zero to stay fast).
fn retry_delay(attempt: u32, error: &ProviderError) -> Duration {
    if let ProviderError::RateLimit {
        retry_after_secs: Some(seconds),
    } = error
    {
        return Duration::from_secs(*seconds);
    }

    let base = 250u64.saturating_mul(1 << attempt.saturating_sub(1).min(4));
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_millis()))
        .unwrap_or(0)
        % (base / 2 + 1);
    Duration::from_millis(base + jitter)
}

async fn collect_once(
    provider: &dyn Provider,
    request: CompletionRequest,
    cancel: &CancellationToken,
    on_delta: Option<&DeltaSink>,
) -> Result<Collected, AttemptFailure> {
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
        Err(error) => {
            return Err(AttemptFailure::Provider {
                error,
                started: false,
            });
        }
    };

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_blocks = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::EndTurn;
    let mut ended = false;
    let mut cancelled = false;
    let mut started = false;

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
            Ok(StreamEvent::TextDelta(delta)) => {
                started = true;
                if let Some(sink) = on_delta {
                    sink.emit(Delta::Text(delta.clone()));
                }
                text.push_str(&delta);
            }
            Ok(StreamEvent::ReasoningDelta(delta)) => {
                started = true;
                if let Some(sink) = on_delta {
                    sink.emit(Delta::Reasoning(delta.clone()));
                }
                reasoning.push_str(&delta);
            }
            Ok(StreamEvent::ToolCallDelta { .. }) => {
                started = true;
            }
            Ok(StreamEvent::ToolCallDone {
                call_id,
                name,
                arguments,
            }) => {
                started = true;
                tool_blocks.push(ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                });
            }
            Ok(StreamEvent::Usage(reported)) => usage = reported,
            Ok(StreamEvent::Ended {
                stop_reason: reason,
            }) => {
                stop_reason = reason;
                ended = true;
                break;
            }
            Ok(StreamEvent::Started) => {}
            // A provider that notices the token before we do reports Cancelled
            // instead of ending the stream; that is a cancellation, not a bug.
            Err(ProviderError::Cancelled) => {
                cancelled = true;
                break;
            }
            Err(error) => {
                return Err(AttemptFailure::Provider { error, started });
            }
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
        return Err(AttemptFailure::Fatal(EngineError::IncompleteStream));
    }

    blocks.extend(tool_blocks);
    Ok(Collected {
        blocks,
        usage,
        stop_reason,
        cancelled: false,
    })
}

/// Adjacent read-only calls in one provider batch run together. Writes, shell
/// and MCP calls keep their original position and run alone, so a read never
/// jumps over a write that could change what it sees.
const MAX_PARALLEL_READS: usize = 4;

struct PlannedCall {
    call: ToolCallBlock,
    class: Option<ApprovalClass>,
    tool: Option<Arc<dyn Tool>>,
    /// A result that was decided before execution: unknown tool, denied, or
    /// cancelled while waiting for approval.
    terminal: Option<ToolOutput>,
}

impl PlannedCall {
    fn terminal(call: ToolCallBlock, class: Option<ApprovalClass>, output: ToolOutput) -> Self {
        Self {
            call,
            class,
            tool: None,
            terminal: Some(output),
        }
    }
}

/// Plan, execute and log one batch of tool calls.
///
/// Planning appends `tool_call` and approval events in provider order. Only
/// then are tools executed, so every call has exactly one result and the
/// results are written in that same order even when reads run concurrently.
/// Returns `true` when the turn has to end as cancelled.
#[allow(clippy::too_many_arguments)]
async fn execute_calls<F>(
    ctx: &TurnContext<'_>,
    tool_context: &ToolContext,
    writer: &mut SessionWriter<'_>,
    calls: Vec<ToolCallBlock>,
    messages: &mut Vec<Message>,
    todos: &mut Option<Vec<TodoItem>>,
    total_usage: &mut Usage,
    total_cost: &mut f64,
    on_event: &mut F,
) -> Result<bool, EngineError>
where
    F: FnMut(&Event),
{
    let mut planned = Vec::with_capacity(calls.len());
    for call in calls {
        planned.push(plan_call(ctx, tool_context, writer, call, on_event).await?);
    }

    let mut outputs: Vec<Option<ToolOutput>> = planned.iter().map(|_| None).collect();
    let mut halted = false;
    let mut index = 0;
    // SPEC §4.9: parallel reads only when the provider advertises them.
    let parallel_reads = ctx.provider.capabilities().parallel_tools;

    while index < planned.len() {
        if halted || tool_context.cancel.is_cancelled() {
            // Keep the one-call-one-result invariant even when the run is
            // going away; replay and resume depend on it.
            outputs[index] = Some(ToolOutput::cancelled("cancelled by user"));
            halted = true;
            index += 1;
            continue;
        }

        if let Some(output) = planned[index].terminal.take() {
            if output.status == ToolStatus::Cancelled {
                halted = true;
            }
            outputs[index] = Some(output);
            index += 1;
            continue;
        }

        if parallel_reads && planned[index].class == Some(ApprovalClass::Read) {
            let start = index;
            while index < planned.len()
                && planned[index].terminal.is_none()
                && planned[index].class == Some(ApprovalClass::Read)
            {
                index += 1;
            }

            let results: Vec<(usize, ToolOutput)> =
                futures_util::stream::iter((start..index).map(|slot| {
                    let tool = planned[slot]
                        .tool
                        .as_ref()
                        .expect("read call has a tool")
                        .clone();
                    let arguments = planned[slot].call.arguments.clone();
                    async move {
                        let output = match tool.execute(tool_context, arguments).await {
                            Ok(output) => output,
                            Err(err) => ToolOutput::error(err.to_string()),
                        };
                        (slot, output)
                    }
                }))
                .buffer_unordered(MAX_PARALLEL_READS)
                .collect()
                .await;

            for (slot, output) in results {
                if output.status == ToolStatus::Cancelled {
                    halted = true;
                }
                outputs[slot] = Some(output);
            }
            if tool_context.cancel.is_cancelled() {
                halted = true;
            }
            continue;
        }

        let tool = planned[index]
            .tool
            .as_ref()
            .expect("planned call has a tool")
            .clone();
        let output = match tool
            .execute(tool_context, planned[index].call.arguments.clone())
            .await
        {
            Ok(output) => output,
            Err(err) => ToolOutput::error(err.to_string()),
        };
        if output.status == ToolStatus::Cancelled || tool_context.cancel.is_cancelled() {
            halted = true;
        }
        outputs[index] = Some(output);
        index += 1;
    }

    let mut finished = false;
    for (index, planned) in planned.into_iter().enumerate() {
        let output = outputs[index].take().expect("every call has a result");
        // A tool may have done work on the engine's behalf (a sub-agent).
        // Fold its usage/cost into the parent turn before the output moves.
        total_usage.merge(&output.usage);
        *total_cost += output.cost_usd;
        // `todo_write` is session state: keep the in-memory copy in sync so the
        // next request's system prompt sees the list. The tool result remains
        // the audit trail and the replay source (`todo::current`).
        if output.status == ToolStatus::Ok
            && planned.call.name == "todo_write"
            && let Ok(items) = todo::from_arguments(&planned.call.arguments)
        {
            *todos = (!items.is_empty()).then_some(items);
        }
        append_result(writer, &planned.call.call_id, &output, on_event)?;
        if output.status == ToolStatus::Cancelled {
            finished = true;
        }
        if finished {
            continue;
        }
        messages.push(Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::Text {
                text: output.output,
            }],
            tool_call_id: Some(planned.call.call_id),
        });
    }

    Ok(finished)
}

async fn plan_call<F>(
    ctx: &TurnContext<'_>,
    tool_context: &ToolContext,
    writer: &mut SessionWriter<'_>,
    call: ToolCallBlock,
    on_event: &mut F,
) -> Result<PlannedCall, EngineError>
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
        return Ok(PlannedCall::terminal(
            call,
            None,
            ToolOutput::cancelled("cancelled by user"),
        ));
    }

    let Some(tool) = ctx.tools.get(&call.name) else {
        let message = format!("unknown tool: {}", call.name);
        return Ok(PlannedCall::terminal(
            call,
            None,
            ToolOutput::error(message),
        ));
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
            argv: call
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    call.arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .map(|path| format!("path {path}"))
                }),
            cwd: (call.name == "run_shell").then(|| {
                let cwd = call.arguments.get("cwd").and_then(Value::as_str);
                match cwd.filter(|cwd| !cwd.is_empty()) {
                    Some(cwd) if Path::new(cwd).is_absolute() => cwd.to_string(),
                    Some(cwd) => tool_context.workspace.join(cwd).display().to_string(),
                    None => tool_context.workspace.display().to_string(),
                }
            }),
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
        allowed = answer.is_allow();
        // A denial that raced with cancellation is not a user decision.
        aborted = tool_context.cancel.is_cancelled();
    }

    // A matched rule carries its provenance; defaults are session-scoped.
    let scope = decision.scope.unwrap_or(Scope::Session);

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
        // The approver was still waiting when the run was cancelled.
        return Ok(PlannedCall::terminal(
            call,
            Some(class),
            ToolOutput::cancelled("cancelled by user"),
        ));
    }

    if !allowed {
        return Ok(PlannedCall::terminal(
            call,
            Some(class),
            ToolOutput::error(format!("denied: {reason}")),
        ));
    }

    Ok(PlannedCall {
        call,
        class: Some(class),
        tool: Some(tool),
        terminal: None,
    })
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
    let mut metadata = output.metadata.clone();
    // Large outputs stay out of the transcript but remain readable on disk.
    if metadata.truncated
        && let Some(full) = &output.full_output
    {
        match writer
            .store
            .write_output(writer.session_id(), call_id, full)
        {
            Ok(path) => metadata.full_output_path = Some(path.display().to_string()),
            Err(err) => tracing::warn!(%err, call_id, "failed to persist full tool output"),
        }
    }

    writer.append(
        EventKind::ToolResult(ToolResult {
            call_id: call_id.to_string(),
            status: output.status,
            output: output.output.clone(),
            metadata,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_md_and_skills_are_appended_to_the_system_prompt() {
        assert_eq!(system_prompt(None, None, &[]), SYSTEM_PROMPT);
        assert_eq!(system_prompt(Some("   "), None, &[]), SYSTEM_PROMPT);

        let with_rules = system_prompt(Some("  use tabs  "), None, &[]);
        assert!(with_rules.starts_with(SYSTEM_PROMPT));
        assert!(with_rules.contains("# AGENTS.md"));
        assert!(with_rules.ends_with("use tabs"));

        let with_skills = system_prompt(Some("use tabs"), Some("# Skills\n\n- `x` (user): y"), &[]);
        assert!(with_skills.contains("# AGENTS.md"));
        assert!(with_skills.ends_with("- `x` (user): y"));
    }

    #[test]
    fn todo_section_is_appended_after_skills() {
        let todos = vec![TodoItem {
            content: "write tests".to_string(),
            status: crate::todo::TodoStatus::InProgress,
        }];
        let prompt = system_prompt(
            Some("use tabs"),
            Some("# Skills\n\n- `x` (user): y"),
            &todos,
        );
        assert!(prompt.contains("# Skills"));
        assert!(prompt.contains("# Todo"));
        assert!(prompt.contains("[~] write tests"));
        assert!(prompt.ends_with("[~] write tests"));
    }
}
