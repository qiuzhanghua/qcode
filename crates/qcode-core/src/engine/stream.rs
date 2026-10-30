use super::*;

pub(crate) struct ToolCallBlock {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

pub(crate) struct Collected {
    pub(crate) blocks: Vec<ContentBlock>,
    pub(crate) usage: Usage,
    pub(crate) stop_reason: StopReason,
    pub(crate) cancelled: bool,
}

pub(crate) async fn collect_stream(
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

pub(crate) enum AttemptFailure {
    /// The stream failed; `started` records whether any token was emitted.
    Provider { error: ProviderError, started: bool },
    /// Not retryable at this layer (e.g. a truncated stream).
    Fatal(EngineError),
}

pub(crate) fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::RateLimit { .. } | ProviderError::Server(_) | ProviderError::Network(_)
    )
}

/// Exponential backoff with jitter. `Retry-After` wins when the server sent it
/// (tests use zero to stay fast).
pub(crate) fn retry_delay(attempt: u32, error: &ProviderError) -> Duration {
    if let ProviderError::RateLimit {
        retry_after_secs: Some(seconds),
        ..
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

pub(crate) async fn collect_once(
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
