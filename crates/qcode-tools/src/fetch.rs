//! `web_fetch`: fetch one http(s) URL as text.
//!
//! Network access is outside the workspace boundary, so the default approval is
//! ask. There is no credential injection: the request carries no API keys and
//! redirects are limited. Bodies are capped at 100KiB; only textual content is
//! returned. SSRF protection is not attempted here (there is no reliable
//! host-level egress boundary in the engine); the approval prompt is the
//! control, and the model sees the URL it is about to fetch.

use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::ToolMetadata;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_BYTES: usize = 100 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 5;

pub struct WebFetch;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description: "Fetch a single http(s) URL and return its text body. Use for public \
                          documentation and APIs. Only textual content is returned, the body is \
                          capped at 100KiB, and redirects are limited."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http:// or https:// URL"}
                },
                "required": ["url"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Network
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::Invalid(error.to_string()))?;
        let url = validate_url(&args.url)?;

        let client = if let Some(network) = &ctx.network {
            // The tool approval already ran; add the host to the session
            // allowlist so the proxy accepts it. Redirects to other hosts are
            // still checked by the proxy and fail closed.
            if let Some(host) = url.host_str() {
                network.allow_host(host);
            }
            network.client()
        } else {
            Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .user_agent(concat!("qcode/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| ToolError::Network(error.to_string()))?
        };

        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::cancelled("web_fetch cancelled")),
            response = client.get(url.clone()).send() => response,
        }
        .map_err(|error| ToolError::Network(error.to_string()))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(content_type) = &content_type
            && !is_textual(content_type)
        {
            return Err(ToolError::Invalid(format!(
                "refusing non-text content type '{content_type}' (only text, JSON and XML are supported)"
            )));
        }

        let mut body = Vec::new();
        let mut truncated = false;
        let mut response = response;
        loop {
            let chunk = tokio::select! {
                _ = ctx.cancel.cancelled() => return Ok(ToolOutput::cancelled("web_fetch cancelled")),
                chunk = response.chunk() => chunk,
            }
            .map_err(|error| ToolError::Network(error.to_string()))?;
            let Some(chunk) = chunk else {
                break;
            };
            if body.len() + chunk.len() > MAX_BYTES {
                body.extend_from_slice(&chunk[..MAX_BYTES - body.len()]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_BYTES {
                truncated = true;
                break;
            }
        }

        if body.iter().take(8192).any(|byte| *byte == 0) {
            return Err(ToolError::Invalid(
                "response looks binary; refusing to return it as text".to_string(),
            ));
        }

        let text = String::from_utf8_lossy(&body);
        let final_url = response.url().to_string();
        let mut output = format!(
            "url: {final_url}\nstatus: {status}\ncontent-type: {}\n\n{text}",
            content_type.as_deref().unwrap_or("unknown")
        );
        let bytes = output.len() as u64;
        let full_len = output.len();
        if full_len > MAX_BYTES {
            // A lossy conversion can grow the body slightly; keep the ceiling.
            let cut = crate::truncate(&output, MAX_BYTES).0;
            output = cut;
            truncated = true;
        }

        let metadata = ToolMetadata {
            truncated,
            bytes,
            ..ToolMetadata::default()
        };
        let result = ToolOutput::ok(output).with_metadata(metadata.clone());
        Ok(if status.is_success() {
            result
        } else {
            ToolOutput::error(result.output).with_metadata(metadata)
        })
    }
}

fn validate_url(raw: &str) -> Result<Url, ToolError> {
    let url = Url::parse(raw.trim())
        .map_err(|error| ToolError::Invalid(format!("invalid URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Invalid(format!(
            "only http:// and https:// URLs are supported, got '{}'",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::Invalid(
            "credentials in URLs are not supported".to_string(),
        ));
    }
    Ok(url)
}

fn is_textual(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-ndjson"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn approval_defaults_to_network() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            WebFetch.approval(&ToolContext::new(dir.path()), &json!({})),
            ApprovalClass::Network
        );
    }

    async fn serve_once(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{address}/")
    }

    async fn run(workspace: &std::path::Path, url: &str) -> Result<ToolOutput, ToolError> {
        WebFetch
            .execute(&ToolContext::new(workspace), json!({"url": url}))
            .await
    }

    #[tokio::test]
    async fn fetches_text() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello"
                .to_vec(),
        )
        .await;

        let output = run(dir.path(), &url).await.unwrap();
        assert!(output.output.contains("status: 200"));
        assert!(output.output.contains("hello"));
    }

    #[tokio::test]
    async fn reports_http_errors_with_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\nmissing"
                .to_vec(),
        )
        .await;

        let output = run(dir.path(), &url).await.unwrap();
        assert_eq!(output.status, qcode_core::event::ToolStatus::Error);
        assert!(output.output.contains("missing"));
    }

    #[tokio::test]
    async fn rejects_binary_content_types_and_schemes() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\n\r\nabc".to_vec(),
        )
        .await;

        assert!(run(dir.path(), &url).await.is_err());
        assert!(run(dir.path(), "file:///etc/passwd").await.is_err());
        assert!(run(dir.path(), "ftp://example.com/x").await.is_err());
        assert!(
            run(dir.path(), "http://user:pass@example.com/")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn truncates_large_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let body = "x".repeat(MAX_BYTES + 1000);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        let url = serve_once(response).await;

        let output = run(dir.path(), &url).await.unwrap();
        assert!(output.metadata.truncated);
        assert!(output.output.len() <= MAX_BYTES + 200);
    }
}
