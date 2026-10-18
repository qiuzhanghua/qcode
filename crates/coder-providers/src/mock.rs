//! Scripted provider for tests and for the M0 headless smoke path.

use std::time::Duration;

use async_trait::async_trait;
use coder_core::event::Usage;
use coder_core::provider::{
    Capabilities, CompletionRequest, EventStream, Provider, ProviderError, StopReason, StreamEvent,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct MockProvider {
    events: Vec<Result<StreamEvent, ProviderError>>,
    capabilities: Capabilities,
}

impl MockProvider {
    /// Emit `text` in small deltas, then usage and a clean end.
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        let chars: Vec<char> = text.chars().collect();
        let mut events = vec![Ok(StreamEvent::Started)];
        for chunk in chars.chunks(8) {
            events.push(Ok(StreamEvent::TextDelta(chunk.iter().collect())));
        }
        events.push(Ok(StreamEvent::Usage(Usage {
            input_tokens: Some(12),
            output_tokens: Some((chars.len() as u64).div_ceil(4)),
            ..Usage::default()
        })));
        events.push(Ok(StreamEvent::Ended {
            stop_reason: StopReason::EndTurn,
        }));
        Self {
            events,
            capabilities: Capabilities::default(),
        }
    }

    pub fn with_events(events: Vec<Result<StreamEvent, ProviderError>>) -> Self {
        Self {
            events,
            capabilities: Capabilities::default(),
        }
    }
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
        _request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let events = self.events.clone();
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
