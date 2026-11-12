use super::*;

/// A rootless task's turn-private shell directory, removed on drop.
pub(crate) struct TurnTemp(PathBuf);

impl Drop for TurnTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many consecutive rounds may repeat the exact same tool call before the
/// turn is stopped. Ten is enough to catch a loop while leaving room for
/// deliberate retries.
pub(crate) const MAX_IDENTICAL_ROUNDS: u32 = 10;

/// What the user's client sends when it continues a paused turn. The marker
/// stays first so a log reader (and the model) can tell it from a real message;
/// the rest is the instruction, because the transcript above it carries the task
/// but not the "don't redo what is done" part.
pub const CONTINUE_PROMPT: &str = "[continue] 按你上面的计划接着做；已完成的部分不要重做，\
也不要重复调用同样的工具。如果已经做完，直接给出结论。";

/// The instruction a paused turn's wrap-up sends: stop working, write the note.
/// The frontends all send this text, so the wording lives next to
/// [`CONTINUE_PROMPT`] instead of being retyped per UI (ADR-0060 §2.5).
pub const WRAP_UP_PROMPT: &str = "[wrap-up] 请收尾：不要再调用工具，用不超过 15 行总结这次做了什么、\
没做什么、还剩什么需要人工处理。";

/// Fraction of a cost budget the loop may spend before it stops. The budget is
/// a wall, but a call that costs $0.40 cannot be un-placed once it is sent, so
/// the brake fires slightly early (ADR-0060 §2.2).
pub(crate) const COST_BRAKE_RATIO: f64 = 0.95;

/// Fraction of a budget at which the "close to the limit" hint fires.
pub(crate) const BUDGET_HINT_RATIO: f64 = 0.75;

/// The instruction for the hand-off call: one tool-less response that says where
/// the turn stopped, so the next turn (or the user) can pick it up.
pub(crate) const HAND_OFF_PROMPT: &str = "You are about to stop: you have no budget left for \
     another step. Do not call tools. In at most 12 lines, write the hand-off for whoever \
     continues this task: 已完成 (what is done), 未完成 (what is left), 建议下一步 (the next \
     concrete action). Plain text, no preamble.";

/// Guards against a model that keeps asking for the same call.
///
/// Compares *consecutive rounds* by name+arguments: a retry with different
/// arguments is progress, the same call ten times is not.
pub(crate) struct ProgressGuard {
    signature: Option<String>,
    repeats: u32,
}

impl ProgressGuard {
    pub(crate) fn new() -> Self {
        Self {
            signature: None,
            repeats: 0,
        }
    }

    /// Returns the number of identical rounds if the limit was reached.
    pub(crate) fn observe(&mut self, signature: &str) -> Option<u32> {
        if self.signature.as_deref() == Some(signature) {
            self.repeats += 1;
            if self.repeats + 1 >= MAX_IDENTICAL_ROUNDS {
                return Some(self.repeats + 1);
            }
        } else {
            self.signature = Some(signature.to_string());
            self.repeats = 0;
        }
        None
    }
}

/// One round's calls as a single comparable string.
pub(crate) fn call_signature(calls: &[ToolCallBlock]) -> String {
    let mut signature = String::new();
    for call in calls {
        signature.push_str(&call.name);
        signature.push(' ');
        signature.push_str(&call.arguments.to_string());
        signature.push('\n');
    }
    signature
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

    // The window number and the paused turn it continues are the engine's to
    // decide, from the log: a frontend that sends a stale or forged value must
    // not be able to claim a fresh budget (ADR-0060 §2.11).
    let mut events = ctx.store.load(options.session_id)?;
    let window_no = budget_window(&events);
    let continuation = options.continuation.clone().map(|mut continuation| {
        continuation.window = window_no + 1;
        continuation.turn = events.last().map(|event| event.turn).unwrap_or(0);
        continuation
    });

    // Bytes go to the store first, so the log entry below can name them and a
    // resumed turn can read them back (ADR-0061 §3).
    let attachment_store = qcode_attachments::AttachmentStore::open(ctx.store.root())?;
    let attachments = super::attachments::store_sources(&attachment_store, &options.attachments)?;

    let mut writer = SessionWriter::resume(ctx.store, options.session_id)?;
    writer.begin_turn(
        &options.input,
        continuation,
        &attachments,
        options.scheduled.clone(),
        &mut on_event,
    )?;

    // ADR-0067 §5: the MCP tool table is re-read here, at the turn boundary, and
    // whatever comes back is used for the whole turn. Two reasons it is a
    // snapshot rather than a live view:
    //
    // * a table that moved between the request the model saw and the calls that
    //   request produced would let it name a tool it was shown and have the call
    //   arrive after the tool is gone;
    // * `ttlMs = 0` is a server saying "assume this is stale" — acting on that
    //   per round would mean a round trip before every model call.
    //
    // Sources that are still fresh answer from their own cache, so a session
    // with a `ttlMs = 5000` server makes one extra call per turn at most.
    let (tools, changed) = ctx.tools.refreshed().await;
    for diff in changed {
        writer.append(
            EventKind::McpToolsRefreshed(crate::event::McpToolsRefreshed {
                server: diff.server.clone(),
                added: diff.added.clone(),
                removed: diff.removed.clone(),
            }),
            &mut on_event,
        )?;
    }
    // Read before the shadowing below: the snapshot deliberately carries no
    // sources (so a refresh cannot recurse), which means it also carries no
    // `instructions`. Those come from the handshake and never change, so reading
    // them once per turn from the live registry is both correct and cheap.
    let mcp_prompts = ctx.tools.server_prompts();
    // Shadowing keeps the rest of the function reading the same way; the
    // snapshot lives as long as this borrow scope, which is the whole turn.
    let ctx = TurnContext {
        tools: &tools,
        ..ctx
    };
    events = ctx.store.load(options.session_id)?;
    let mut messages = history::messages_from_events_with(
        &events,
        |image| attachment_store.read_image(image).ok(),
        |file| super::attachments::file_handle_text(&attachment_store, file),
    );
    // A model without image input still gets a usable turn: the picture becomes
    // a line of text naming what was attached (ADR-0061 §8).
    if !ctx.provider.capabilities().images {
        super::attachments::omit_images(&mut messages);
    } else {
        // Even a vision model has a request budget: past a few pictures, the
        // oldest ones become placeholders and the newest still gets through.
        super::attachments::offload_images(
            &mut messages,
            super::attachments::ImageBudget::default(),
        );
    }
    // The todo list is session state rebuilt from the log, so a resumed or
    // forked session starts with the list the model last saw.
    let mut todos = todo::current(&events);
    let task = ctx.store.task_from_events(options.session_id, &events)?;
    let turn_no = events.last().map(|event| event.turn).unwrap_or(0);
    let turn_temp = if task.roots.is_empty() {
        let path = ctx
            .store
            .root()
            .join("tasks")
            .join(options.session_id)
            .join("tmp")
            .join(format!("turn-{turn_no}"));
        std::fs::create_dir_all(&path)?;
        Some(TurnTemp(path))
    } else {
        None
    };
    let mut compaction_disabled = false;
    let mut tool_context =
        ToolContext::for_roots(task.roots.clone()).with_cancel(options.cancel.clone());
    tool_context = tool_context.with_session_id(options.session_id);
    if let Some(temp) = &turn_temp {
        tool_context = tool_context.with_temp_dir(temp.0.clone());
    }
    if let Some(network) = &options.network {
        tool_context = tool_context.with_network(network.clone());
    }
    if ctx.scheduler.is_some() {
        tool_context = tool_context.with_scheduler(ctx.scheduler.clone());
    }
    if ctx.questioner.is_some() {
        tool_context = tool_context.with_questioner(ctx.questioner.clone());
    }

    let mut turns = 0;
    let mut progress = ProgressGuard::new();
    let mut total_usage = Usage::default();
    let mut total_cost = 0.0;
    let mut hint_sent = false;
    let mut last_text = String::new();
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
                options.inbox.as_deref(),
                None,
                &mut on_event,
            );
        }

        turns += 1;
        if turns > options.max_turns {
            let pause = PauseInfo {
                reason: PauseReason::MaxTurns,
                limit: PauseLimit::Turns(options.max_turns),
                used: PauseUsage {
                    turns: options.max_turns,
                    cost_usd: total_cost,
                },
                window: window_no,
                summary: None,
            };
            let (pause, usage, cost) =
                hand_off(ctx, &options, &messages, &mut writer, pause, &mut on_event).await;
            total_usage.merge(&usage);
            total_cost += cost;
            return finish(
                &mut writer,
                TurnStatus::Paused,
                total_usage,
                total_cost,
                last_text,
                StopReason::EndTurn,
                options.session_id,
                turns,
                options.inbox.as_deref(),
                Some(pause),
                &mut on_event,
            );
        }

        // The hint fires once, on the first round that crosses 75%, and never on
        // the round that pauses anyway.
        if !hint_sent
            && let Some(hint) = budget_hint(
                turns,
                options.max_turns,
                total_cost,
                options.max_cost_usd,
                window_no,
            )
        {
            writer.append(EventKind::BudgetHint(hint), &mut on_event)?;
            hint_sent = true;
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
                                options.inbox.as_deref(),
                                None,
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

        // Steering input lands at the next step boundary: claim every pending
        // message and append it as a user message before the next request, so
        // the running turn answers it next instead of closing. The message is
        // persisted so the transcript and a later replay see it; attached
        // images ride only on this request's blocks (like the initial input).
        if let Some(inbox) = &options.inbox {
            for message in inbox.claim_next_step() {
                // The refs go into the log too, so a resumed turn still sees
                // the picture the user sent mid-turn.
                let refs =
                    super::attachments::store_sources(&attachment_store, &message.attachments)?;
                writer.append(
                    EventKind::UserMessage(UserMessage {
                        starts_segment: true,
                        queued_id: Some(message.id.clone()),
                        text: message.text.clone(),
                        attachments: refs.clone(),
                        // Steering is the user talking, not a continuation.
                        continuation: None,
                        question_reply: message.question_reply.clone(),
                        scheduled: None,
                    }),
                    &mut on_event,
                )?;
                let mut blocks = history::blocks_for_message(
                    &message.text,
                    &refs,
                    |image| attachment_store.read_image(image).ok(),
                    |file| super::attachments::file_handle_text(&attachment_store, file),
                );
                if !ctx.provider.capabilities().images {
                    super::attachments::omit_images_in(&mut blocks);
                }
                messages.push(Message {
                    role: Role::User,
                    blocks,
                    tool_call_id: None,
                });
            }
        }

        // ADR-0068 §3: third-party `instructions` from the connected servers.
        // Read from the registry rather than from `RunOptions` so every
        // frontend and every sub-agent gets them without extra wiring.
        let mut system = system_prompt(
            options.agents_md.as_deref(),
            options.skills.as_deref(),
            &mcp_prompts,
            todos.as_deref().unwrap_or(&[]),
        );
        system.push_str(&roots_prompt(&task.roots));
        // Measured before the request takes ownership of the string.
        let system_tokens = compact::estimate_text_tokens(&system);
        let request = CompletionRequest {
            model: options.model.to_string(),
            system,
            messages: messages.clone(),
            tools: ctx.tools.specs(),
            max_output_tokens: None,
        };

        // Transient UI signal only: the estimate is recomputed on every call,
        // after compaction, and never enters the event log.
        if let Some(window) = options.context_window
            && let Some(sink) = &options.on_delta
        {
            // The system prompt counts too. ADR-0068 后果 flags this
            // explicitly: `estimate_tokens` only walks `messages`, so without
            // this the MCP `instructions` sections are a silent budget cost the
            // status bar cannot show.
            sink.emit(Delta::Context {
                tokens: compact::estimate_tokens(&messages) + system_tokens,
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
        let step_text = blocks_text(&collected.blocks);
        if !step_text.trim().is_empty() {
            last_text = step_text;
        }
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
                options.inbox.as_deref(),
                None,
                &mut on_event,
            );
        }

        if let Some(limit) = options.max_cost_usd
            && total_cost > limit * COST_BRAKE_RATIO
        {
            let pause = PauseInfo {
                reason: PauseReason::MaxCost,
                limit: PauseLimit::Usd(limit),
                used: PauseUsage {
                    turns,
                    cost_usd: total_cost,
                },
                window: window_no,
                summary: None,
            };
            // A spent cost budget buys no more calls, so the hand-off is built
            // from what the log already knows instead of asking the model.
            let pause = with_local_summary(ctx, &options, &events, pause);
            return finish(
                &mut writer,
                TurnStatus::Paused,
                total_usage,
                total_cost,
                last_text,
                StopReason::EndTurn,
                options.session_id,
                turns,
                options.inbox.as_deref(),
                Some(pause),
                &mut on_event,
            );
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
                options.inbox.as_deref(),
                None,
                &mut on_event,
            );
        }

        // The model repeating itself is not progress; stop before the turn
        // budget is spent on identical calls.
        if let Some(rounds) = progress.observe(&call_signature(&calls)) {
            let tool = calls
                .first()
                .map(|call| call.name.clone())
                .unwrap_or_default();
            let message = format!(
                "no progress: `{tool}` was called with identical arguments {rounds} rounds \
                 in a row; stopping. Change the approach, raise [agent] max_turns, or \
                 send a different instruction"
            );
            writer.append(
                EventKind::Error(ErrorEvent {
                    kind: "no_progress".to_string(),
                    message,
                    retryable: false,
                }),
                &mut on_event,
            )?;
            writer.append(
                EventKind::TurnEnded(TurnEnded {
                    status: TurnStatus::Failed,
                    usage: total_usage.clone(),
                    cost_usd: total_cost,
                    pause: None,
                }),
                &mut on_event,
            )?;
            return Err(EngineError::NoProgress { tool, rounds });
        }

        // Capture the pre-image of everything these writers are about to touch,
        // before they touch it. A snapshot failure must not fail the turn: the
        // write still has to happen.
        if let Some(snapshots) = ctx.snapshots {
            // The cap is charged per turn, so one turn's several shell commands
            // report it once instead of once per call.
            let mut capped_reported = false;
            for call in &calls {
                // A shell command can write anywhere, so guessing paths from the
                // command line would lose files. When enabled, copy the tree.
                if call.name == "run_shell" && snapshots.shell_enabled() {
                    match snapshots.snapshot_tree(options.session_id, turns, &options.cwd) {
                        // The cap is per turn, so report it once per turn. A turn
                        // that was already full adds nothing: saying "hit its cap"
                        // there reads like a new truncation and repeats forever on
                        // turns recorded before toolchain caches were skipped.
                        Ok(report) if report.capped => {
                            if !capped_reported {
                                capped_reported = true;
                                if report.added_bytes > 0 {
                                    tracing::warn!(
                                        files = report.files,
                                        bytes = report.bytes,
                                        "workspace snapshot hit its size cap; rollback may be \
                                         partial"
                                    );
                                } else {
                                    tracing::debug!(
                                        files = report.files,
                                        bytes = report.bytes,
                                        "workspace snapshot for this turn is already at its size \
                                         cap; nothing added"
                                    );
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(%error, "cannot snapshot the workspace before run_shell")
                        }
                    }
                }
                let paths = crate::snapshot::write_hints(&call.name, &call.arguments);
                if paths.is_empty() {
                    continue;
                }
                if let Err(error) =
                    snapshots.record(options.session_id, turns, &options.cwd, &paths)
                {
                    tracing::warn!(
                        %error,
                        tool = %call.name,
                        "cannot record a checkpoint pre-image"
                    );
                }
            }
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
                options.inbox.as_deref(),
                None,
                &mut on_event,
            );
        }
    }
}

pub(crate) fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Budget window for this turn: 1 unless the turn's user message is a
/// continuation, in which case it is that continuation's window.
fn budget_window(events: &[Event]) -> u32 {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => Some(
                message
                    .continuation
                    .as_ref()
                    .map(|continuation| continuation.window.max(1))
                    .unwrap_or(1),
            ),
            _ => None,
        })
        .unwrap_or(1)
}

/// The one-off 75% notice, or `None` while the turn is still comfortable.
///
/// Either budget can trigger it: a turn on round 48 of 64 is worth warning
/// about even if it has spent almost nothing, and a turn that has spent 75% of
/// its money is worth warning about however many rounds it has left. A budget
/// too small to have a comfortable quarter never warns at all (the pause card is
/// two rounds away and says more).
fn budget_hint(
    turns: u32,
    max_turns: u32,
    cost_usd: f64,
    max_cost_usd: Option<f64>,
    window: u32,
) -> Option<BudgetHint> {
    let near_turns = turns as f64 >= max_turns as f64 * BUDGET_HINT_RATIO;
    let near_cost =
        max_cost_usd.is_some_and(|limit| limit > 0.0 && cost_usd >= limit * BUDGET_HINT_RATIO);
    if !near_turns && !near_cost {
        return None;
    }
    // A hint on the last round is noise: the pause card is about to say the
    // same thing with more detail.
    if turns >= max_turns || max_cost_usd.is_some_and(|limit| cost_usd >= limit * COST_BRAKE_RATIO)
    {
        return None;
    }
    Some(BudgetHint {
        turns,
        max_turns,
        cost_usd,
        max_cost_usd,
        window,
    })
}

/// Ask the model for the hand-off summary, then return the pause with it.
///
/// The call is tool-less and one-shot: if it fails or the user cancels, the
/// pause still happens and the locally derived summary stands in. Running out
/// of budget must never turn into an error (ADR-0060 §2.2).
async fn hand_off<F>(
    ctx: TurnContext<'_>,
    options: &RunOptions<'_>,
    messages: &[Message],
    writer: &mut SessionWriter<'_>,
    pause: PauseInfo,
    on_event: &mut F,
) -> (PauseInfo, Usage, f64)
where
    F: FnMut(&Event),
{
    let mut messages = messages.to_vec();
    messages.push(Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: HAND_OFF_PROMPT.to_string(),
        }],
        tool_call_id: None,
    });
    let request = CompletionRequest {
        model: options.model.to_string(),
        system: SYSTEM_PROMPT.to_string(),
        messages,
        tools: Vec::new(),
        max_output_tokens: Some(1024),
    };
    match collect_stream(ctx.provider, request, &options.cancel, None).await {
        Ok(collected) => {
            let usage = collected.usage.clone();
            let cost = options
                .pricing
                .as_ref()
                .map(|pricing| pricing.cost_usd(&usage))
                .unwrap_or(0.0);
            let text = blocks_text(&collected.blocks);
            let text = text.trim();
            // The summary is what the next turn reads; persist it as an
            // assistant message so a replay shows the same hand-off.
            if !text.is_empty() {
                let stored = writer.append(
                    EventKind::AssistantMessage(AssistantMessage {
                        blocks: vec![ContentBlock::Text {
                            text: text.to_string(),
                        }],
                        provider_raw: serde_json::Value::Null,
                        usage: usage.clone(),
                    }),
                    on_event,
                );
                if stored.is_err() {
                    tracing::warn!("cannot persist the hand-off summary");
                }
            }
            let pause = PauseInfo {
                summary: (!text.is_empty()).then(|| text.to_string()),
                ..pause
            };
            (pause, usage, cost)
        }
        Err(error) => {
            tracing::warn!(%error, "hand-off summary failed; pausing without it");
            (pause, Usage::default(), 0.0)
        }
    }
}

/// Fill in a hand-off from the log when no call can be made (the cost budget is
/// already spent). Never empty, so the card always says something true.
fn with_local_summary(
    ctx: TurnContext<'_>,
    options: &RunOptions<'_>,
    _events: &[Event],
    pause: PauseInfo,
) -> PauseInfo {
    let events = ctx.store.load(options.session_id).unwrap_or_default();
    let report = crate::progress::report(&events, Some(pause.used.turns.max(1)));
    let report = if report.is_empty() {
        crate::progress::report(&events, None)
    } else {
        report
    };
    let mut summary = String::new();
    match report.files_summary().as_deref() {
        Some(files) => summary.push_str(files),
        None => summary.push_str("本次没有改动文件"),
    }
    if report.tool_calls > 0 {
        summary.push_str(&format!("\n调用了 {} 次工具", report.tool_calls));
    }
    if !report.failures.is_empty() {
        summary.push_str(&format!("\n{} 次工具调用失败", report.failures.len()));
    }
    if !report.last_text.is_empty() {
        summary.push_str(&format!("\n最后的说明：{}", report.last_text));
    }
    PauseInfo {
        summary: Some(summary),
        ..pause
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish<F>(
    writer: &mut SessionWriter<'_>,
    status: TurnStatus,
    usage: Usage,
    cost_usd: f64,
    text: String,
    stop_reason: StopReason,
    session_id: &str,
    turns: u32,
    inbox: Option<&Inbox>,
    pause: Option<PauseInfo>,
    on_event: &mut F,
) -> Result<RunOutcome, EngineError>
where
    F: FnMut(&Event),
{
    // Steering that arrived after the last step boundary never got claimed.
    // Promote it to the follow-up queue so it becomes its own turn instead of
    // riding along in the next run's first request.
    if let Some(inbox) = inbox {
        inbox.promote_pending_steering();
    }

    writer.append(
        EventKind::TurnEnded(TurnEnded {
            status,
            usage: usage.clone(),
            cost_usd,
            pause: pause.clone(),
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
        pause,
    })
}
