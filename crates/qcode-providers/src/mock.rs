//! Scripted provider for tests and for the offline smoke path.
//!
//! Each `stream` call pops one scripted response, which lets the engine tests
//! drive a full tool loop (tool call -> tool result -> final text).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::Usage;
use qcode_core::provider::{
    Capabilities, CompletionRequest, EventStream, Provider, ProviderError, StopReason, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

type Script = Vec<Result<StreamEvent, ProviderError>>;
type ScriptQueue = Arc<Mutex<VecDeque<Script>>>;

/// Handle that lets a held-open stream end.
///
/// Returned by [`MockProvider::scripted_holding_first`]. A semaphore rather than
/// a `Notify` because a permit is remembered: releasing before the stream even
/// reaches its wait still works, so the test cannot deadlock by being ahead.
#[derive(Debug, Clone)]
pub struct MockGate(Arc<tokio::sync::Semaphore>);

impl MockGate {
    /// Let the stream waiting inside [`MockProvider`] finish.
    pub fn release(&self) {
        self.0.add_permits(1);
    }
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    scripts: ScriptQueue,
    capabilities: Capabilities,
    /// Every request the engine sent, in order. Tests use this to assert what
    /// actually reached the provider (system prompt, tools, messages).
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    /// Streams handed out so far; only the first one is held by `hold_first`.
    calls: Arc<AtomicUsize>,
    /// When set, the first stream stops before its final `Ended` event until the
    /// paired [`MockGate`] is released.
    hold_first: Option<Arc<tokio::sync::Semaphore>>,
}

impl MockProvider {
    /// Emit `text` in small deltas, then usage and a clean end.
    pub fn text(text: impl Into<String>) -> Self {
        Self::scripted(vec![text_script(&text.into())])
    }

    /// One entry per provider call; each entry is a complete stream.
    pub fn scripted(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self::scripted_results(
            responses
                .into_iter()
                .map(|response| response.into_iter().map(Ok).collect())
                .collect(),
        )
    }

    pub fn with_events(events: Vec<Result<StreamEvent, ProviderError>>) -> Self {
        Self::scripted_results(vec![events])
    }

    /// One entry per provider call, but the **first** stream does not end until
    /// the returned [`MockGate`] is released.
    ///
    /// A test asserting "turn B started while turn A was still running" needs A
    /// to still be running at that moment. A longer script only makes that
    /// *likely*: on a loaded runner the test's own setup can outlast it and the
    /// assertion then fails without anything being wrong. Holding A open turns
    /// the overlap into something the test controls rather than a race it hopes
    /// to win.
    pub fn scripted_holding_first(responses: Vec<Vec<StreamEvent>>) -> (Self, MockGate) {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut provider = Self::scripted(responses);
        provider.hold_first = Some(gate.clone());
        (provider, MockGate(gate))
    }

    fn scripted_results(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            // The mock stands in for a fully capable provider; tests that need
            // a reduced feature set call `with_capabilities`.
            capabilities: Capabilities {
                tools: true,
                parallel_tools: true,
                streaming_tool_args: true,
                ..Capabilities::default()
            },
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            hold_first: None,
        }
    }

    /// Override the advertised capabilities (e.g. disable parallel tools).
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Requests seen so far, oldest first.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("mock requests lock").clone()
    }

    fn next_script(&self) -> Script {
        self.scripts
            .lock()
            .expect("mock script lock")
            .pop_front()
            .unwrap_or_else(|| {
                vec![Ok(StreamEvent::Ended {
                    stop_reason: StopReason::EndTurn,
                })]
            })
    }
}

fn text_script(text: &str) -> Vec<StreamEvent> {
    let chars: Vec<char> = text.chars().collect();
    let mut events = vec![StreamEvent::Started];
    for chunk in chars.chunks(8) {
        events.push(StreamEvent::TextDelta(chunk.iter().collect()));
    }
    events.push(StreamEvent::Usage(Usage {
        input_tokens: Some(12),
        output_tokens: Some((chars.len() as u64).div_ceil(4)),
        ..Usage::default()
    }));
    events.push(StreamEvent::Ended {
        stop_reason: StopReason::EndTurn,
    });
    events
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        self.requests
            .lock()
            .expect("mock requests lock")
            .push(request);
        let events = self.next_script();
        // Only the first stream can be held; later calls run to completion so
        // the second turn in a test is never blocked by the same gate.
        let hold = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.hold_first.clone()
        } else {
            None
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            for event in events {
                if cancel.is_cancelled() {
                    return;
                }
                // Hold before the final `Ended`: everything the UI needs to see
                // the turn as running has already been sent, and `TurnEnded`
                // cannot be emitted until the test says so.
                if let Some(gate) = &hold
                    && matches!(event, Ok(StreamEvent::Ended { .. }))
                {
                    tokio::select! {
                        _ = gate.acquire() => {}
                        _ = cancel.cancelled() => return,
                    }
                }
                if tx.send(event).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}
