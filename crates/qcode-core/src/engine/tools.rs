use super::*;

/// Adjacent read-only calls in one provider batch run together. Writes, shell
/// and MCP calls keep their original position and run alone, so a read never
/// jumps over a write that could change what it sees.
pub(crate) const MAX_PARALLEL_READS: usize = 4;

pub(crate) struct PlannedCall {
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
pub(crate) async fn execute_calls<F>(
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

pub(crate) async fn plan_call<F>(
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
                let Some(base) = tool_context.shell_cwd() else {
                    return String::from("(no root; nowhere to run)");
                };
                let cwd = call.arguments.get("cwd").and_then(Value::as_str);
                match cwd.filter(|cwd| !cwd.is_empty()) {
                    Some(cwd) if Path::new(cwd).is_absolute() => cwd.to_string(),
                    Some(cwd) => base.join(cwd).display().to_string(),
                    None => base.display().to_string(),
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

pub(crate) fn append_result<F>(
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

pub(crate) fn summarize_call(name: &str, args: &Value) -> String {
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
