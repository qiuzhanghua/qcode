use super::*;

/// A rootless task's turn-private shell directory, removed on drop.
pub(crate) struct TurnTemp(PathBuf);

impl Drop for TurnTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many consecutive rounds may repeat the exact same tool call before the
/// turn is stopped. Three is enough to catch a loop while leaving room for a
/// deliberate retry (`cargo test` twice).
pub(crate) const MAX_IDENTICAL_ROUNDS: u32 = 3;

/// Guards against a model that keeps asking for the same call.
///
/// Compares *consecutive rounds* by name+arguments: a retry with different
/// arguments is progress, the same call three times is not.
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
    if !options.attachments.is_empty() && !ctx.provider.capabilities().images {
        return Err(EngineError::ImagesUnsupported);
    }

    let mut writer = SessionWriter::resume(ctx.store, options.session_id)?;
    writer.begin_turn(&options.input, &mut on_event)?;

    let mut events = ctx.store.load(options.session_id)?;
    let mut messages = history::messages_from_events(&events);
    // Images ride on the last user message. The event log keeps the text only,
    // so a resumed turn replays without them (ADR-0035).
    if !options.attachments.is_empty()
        && let Some(message) = messages.iter_mut().rev().find(|m| m.role == Role::User)
    {
        for attachment in &options.attachments {
            message.blocks.push(ContentBlock::Image {
                mime_type: attachment.mime_type.clone(),
                data_base64: attachment.data_base64.clone(),
            });
        }
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
    if let Some(temp) = &turn_temp {
        tool_context = tool_context.with_temp_dir(temp.0.clone());
    }
    if let Some(network) = &options.network {
        tool_context = tool_context.with_network(network.clone());
    }

    let mut turns = 0;
    let mut progress = ProgressGuard::new();
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
                options.inbox.as_deref(),
                &mut on_event,
            );
        }

        turns += 1;
        if turns > options.max_turns {
            let message = format!(
                "exceeded max turns ({}): raise [agent] max_turns or pass --max-turns, \
                 or send another message to keep going",
                options.max_turns
            );
            writer.append(
                EventKind::Error(ErrorEvent {
                    kind: "max_turns".to_string(),
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
                                options.inbox.as_deref(),
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
                writer.append(
                    EventKind::UserMessage(UserMessage {
                        starts_segment: true,
                        queued_id: Some(message.id.clone()),
                        text: message.text.clone(),
                        attachments: Vec::new(),
                    }),
                    &mut on_event,
                )?;
                let mut blocks = vec![ContentBlock::Text { text: message.text }];
                blocks.extend(message.attachments.into_iter().map(|attachment| {
                    ContentBlock::Image {
                        mime_type: attachment.mime_type,
                        data_base64: attachment.data_base64,
                    }
                }));
                messages.push(Message {
                    role: Role::User,
                    blocks,
                    tool_call_id: None,
                });
            }
        }

        let mut system = system_prompt(
            options.agents_md.as_deref(),
            options.skills.as_deref(),
            todos.as_deref().unwrap_or(&[]),
        );
        system.push_str(&roots_prompt(&task.roots));
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
                options.inbox.as_deref(),
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
                options.inbox.as_deref(),
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
                }),
                &mut on_event,
            )?;
            return Err(EngineError::NoProgress { tool, rounds });
        }

        // Capture the pre-image of everything these writers are about to touch,
        // before they touch it. A snapshot failure must not fail the turn: the
        // write still has to happen.
        if let Some(snapshots) = ctx.snapshots {
            for call in &calls {
                // A shell command can write anywhere, so guessing paths from the
                // command line would lose files. When enabled, copy the tree.
                if call.name == "run_shell" && snapshots.shell_enabled() {
                    match snapshots.snapshot_tree(options.session_id, turns, &options.cwd) {
                        Ok(report) if report.capped => tracing::warn!(
                            files = report.files,
                            bytes = report.bytes,
                            "workspace snapshot hit its size cap; rollback may be partial"
                        ),
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
