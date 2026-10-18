//! HTTP shell test: no network, a local TCP listener serves a recorded SSE body.

use futures_util::StreamExt;
use qcode_core::event::ContentBlock;
use qcode_core::provider::{
    CompletionRequest, Message, Provider, ProviderError, Role, StreamEvent,
};
use qcode_providers::{OpenAiCompatConfig, OpenAiCompatProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

async fn serve_once(response: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local listener");
    let address = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buffer = [0u8; 8192];
        let _ = socket.read(&mut buffer).await;
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        let _ = socket.shutdown().await;
    });

    format!("http://{address}/v1")
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".to_string(),
        system: "be brief".to_string(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
            tool_call_id: None,
        }],
        tools: Vec::new(),
        max_output_tokens: None,
    }
}

fn provider(base_url: String) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        name: "test".to_string(),
        base_url,
        api_key: Some("test-key".to_string()),
        stream_usage: true,
    })
    .expect("provider")
}

#[tokio::test]
async fn streams_text_over_http() {
    let body = include_str!("fixtures/openai_text.sse");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
    );
    let base_url = serve_once(response).await;
    let provider = provider(base_url);

    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("start stream");

    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let StreamEvent::TextDelta(delta) = item.expect("stream item") {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "Hello world");
}

#[tokio::test]
async fn maps_401_to_auth_error() {
    let response =
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string();
    let base_url = serve_once(response).await;
    let provider = provider(base_url);

    let error = match provider.stream(request(), CancellationToken::new()).await {
        Ok(_) => panic!("401 must not produce a stream"),
        Err(error) => error,
    };
    assert!(matches!(error, ProviderError::Auth(_)));
}
