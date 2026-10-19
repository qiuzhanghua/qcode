//! Scripted provider for tests and for the offline smoke path.
//!
//! Each `stream` call pops one scripted response, which lets the engine tests
//! drive a full tool loop (tool call -> tool result -> final text).

use std::collections::VecDeque;
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

#[derive(Debug, Clone)]
pub struct MockProvider {
    scripts: ScriptQueue,
    capabilities: Capabilities,
    /// Every request the engine sent, in order. Tests use this to assert what
    /// actually reached the provider (system prompt, tools, messages).
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
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

    fn scripted_results(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            capabilities: Capabilities::default(),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
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
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        tokio::spawn(async move {
            for event in events {
                if cancel.is_cancelled() {
                    return;
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
