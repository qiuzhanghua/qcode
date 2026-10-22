//! `web_search`: query a search backend and return titles, URLs and snippets.
//!
//! Network access is outside the workspace boundary, so the default approval is
//! ask (the same class as `web_fetch`). When a `Network` egress proxy is
//! configured (ADR-0023) the request goes through it: the backend's API host is
//! added to the session allowlist after approval, and redirects to other hosts
//! fail closed. Result URLs are returned as text only; reading a result is a
//! separate `web_fetch` call with its own approval, so a search cannot silently
//! pull arbitrary pages.
//!
//! Backends: Brave and Tavily read an API key from the environment variable
//! named in `[search] api_key_env`; SearXNG is keyless and needs a base URL.

use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::ToolMetadata;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(15);
/// Hard ceiling for `max_results`, matching the tool schema.
pub const MAX_RESULTS: u32 = 20;
const MAX_QUERY_CHARS: usize = 500;
const MAX_TITLE: usize = 200;
const MAX_SNIPPET: usize = 400;
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_ERROR_BODY: usize = 400;

/// A search provider. Config: `[search] backend = "brave" | "tavily" | "searxng"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    #[default]
    Brave,
    Tavily,
    /// A self-hosted or public SearXNG instance with its JSON API enabled.
    Searxng,
}

impl SearchBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Tavily => "tavily",
            Self::Searxng => "searxng",
        }
    }

    /// The conventional environment variable, used when `api_key_env` is empty.
    /// `None` means the backend is keyless.
    pub fn default_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::Brave => Some("BRAVE_API_KEY"),
            Self::Tavily => Some("TAVILY_API_KEY"),
            Self::Searxng => None,
        }
    }

    fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Brave => Some("https://api.search.brave.com"),
            Self::Tavily => Some("https://api.tavily.com"),
            Self::Searxng => None,
        }
    }
}

/// Resolved settings handed to the tool by the CLI.
#[derive(Debug, Clone)]
pub struct SearchSettings {
    pub backend: SearchBackend,
    pub api_key: Option<String>,
    /// API origin override, e.g. a self-hosted SearXNG instance.
    pub base_url: Option<String>,
    pub max_results: u32,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            backend: SearchBackend::default(),
            api_key: None,
            base_url: None,
            max_results: 5,
        }
    }
}

impl SearchSettings {
    /// True when the backend has everything it needs to answer a query.
    pub fn is_usable(&self) -> bool {
        let base_url = self
            .base_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty());
        match self.backend {
            SearchBackend::Searxng => base_url,
            _ => self
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
        }
    }

    fn endpoint(&self) -> Result<String, ToolError> {
        let base = self
            .base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .or_else(|| self.backend.default_base_url())
            .ok_or_else(|| {
                ToolError::Invalid(format!(
                    "the {} backend needs [search] base_url",
                    self.backend.name()
                ))
            })?;
        let base = base.trim_end_matches('/');
        Ok(match self.backend {
            SearchBackend::Brave => format!("{base}/res/v1/web/search"),
            SearchBackend::Tavily | SearchBackend::Searxng => format!("{base}/search"),
        })
    }

    fn require_key(&self) -> Result<&str, ToolError> {
        self.api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| ToolError::Invalid(self.unconfigured_message()))
    }

    fn unconfigured_message(&self) -> String {
        match self.backend {
            SearchBackend::Searxng => "web_search is not configured: set [search] base_url to a \
                                       SearXNG instance with the JSON API enabled"
                .to_string(),
            backend => format!(
                "web_search is not configured for the {} backend: export {} (or set \
                 [search] api_key_env to another variable)",
                backend.name(),
                backend.default_api_key_env().unwrap_or("an API key")
            ),
        }
    }
}

pub struct WebSearch {
    settings: SearchSettings,
}

impl WebSearch {
    pub fn new(settings: SearchSettings) -> Self {
        Self { settings }
    }

    fn request(
        &self,
        client: &Client,
        query: &str,
        limit: u32,
    ) -> Result<reqwest::RequestBuilder, ToolError> {
        let endpoint = self.settings.endpoint()?;
        Ok(match self.settings.backend {
            SearchBackend::Brave => client
                .get(build_url(
                    &endpoint,
                    &[("q", query), ("count", &limit.to_string())],
                )?)
                .header("accept", "application/json")
                .header("x-subscription-token", self.settings.require_key()?),
            SearchBackend::Tavily => client
                .post(endpoint)
                .header("accept", "application/json")
                .bearer_auth(self.settings.require_key()?)
                .json(&json!({
                    "query": query,
                    "max_results": limit,
                    "search_depth": "basic",
                })),
            SearchBackend::Searxng => client
                .get(build_url(
                    &endpoint,
                    &[("q", query), ("format", "json"), ("categories", "general")],
                )?)
                .header("accept", "application/json"),
        })
    }
}

/// Joins `endpoint` with percent-encoded `params`. `reqwest`'s `query` method
/// needs its `query` feature; encoding here keeps the dependency surface small.
fn build_url(endpoint: &str, params: &[(&str, &str)]) -> Result<reqwest::Url, ToolError> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|error| ToolError::Invalid(format!("invalid search endpoint: {error}")))?;
    let pairs: Vec<String> = params
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect();
    url.set_query(Some(&pairs.join("&")));
    Ok(url)
}

/// Percent-encodes every byte outside the URL unreserved set.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    max_results: Option<u32>,
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description: "Search the web and return result titles, URLs and snippets. Use it to \
                          discover pages; call web_fetch (a separate approval) to read one. \
                          Snippets are trimmed and the output is capped."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query, at most 500 characters"
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_RESULTS,
                        "description": "Number of results (default from config)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Network
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::Invalid(error.to_string()))?;
        let query = args.query.trim().to_string();
        if query.is_empty() {
            return Err(ToolError::Invalid("query must not be empty".to_string()));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(ToolError::Invalid(format!(
                "query must be at most {MAX_QUERY_CHARS} characters"
            )));
        }
        // Resolve configuration before touching the network so a missing key is
        // a clear error rather than a 401 from the backend.
        if !self.settings.is_usable() {
            return Err(ToolError::Invalid(self.settings.unconfigured_message()));
        }
        let endpoint = self.settings.endpoint()?;

        let limit = args
            .max_results
            .unwrap_or(self.settings.max_results)
            .clamp(1, MAX_RESULTS);

        let host = reqwest::Url::parse(&endpoint)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string));
        let client = match &ctx.network {
            Some(network) => {
                // The Network approval already ran; allow the backend host for
                // this session. Redirects elsewhere are still refused.
                if let Some(host) = &host {
                    network.allow_host(host);
                }
                network.client()
            }
            None => Client::builder()
                .timeout(TIMEOUT)
                .user_agent(concat!("qcode/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| ToolError::Network(error.to_string()))?,
        };

        let request = self
            .request(&client, &query, limit)?
            .build()
            .map_err(|error| ToolError::Network(error.to_string()))?;
        let response = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::cancelled("web_search cancelled")),
            response = tokio::time::timeout(TIMEOUT, client.execute(request)) => response,
        };

        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(ToolError::Network(error.to_string())),
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "web_search timed out after {}s",
                    TIMEOUT.as_secs()
                )));
            }
        };

        let status = response.status();
        let body = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::cancelled("web_search cancelled")),
            body = response.text() => body.map_err(|error| ToolError::Network(error.to_string()))?,
        };

        if !status.is_success() {
            let detail = truncate_chars(&clean_text(&body), MAX_ERROR_BODY);
            let hint = match status.as_u16() {
                401 | 403 => " (check the API key in the configured environment variable)",
                429 => " (the backend rate-limited this request; retry later)",
                _ => "",
            };
            return Ok(ToolOutput::error(format!(
                "web_search failed: HTTP {status}{hint}\n{detail}"
            )));
        }

        let value: Value = serde_json::from_str(&body).map_err(|error| {
            ToolError::Invalid(format!("search backend returned invalid JSON: {error}"))
        })?;
        let results = parse_results(self.settings.backend, &value);
        Ok(render(&query, self.settings.backend, &results))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn parse_results(backend: SearchBackend, value: &Value) -> Vec<SearchResult> {
    let items = match backend {
        SearchBackend::Brave => value.pointer("/web/results"),
        SearchBackend::Tavily | SearchBackend::Searxng => value.get("results"),
    };
    let Some(items) = items.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(clean_text)
                .unwrap_or_default();
            let snippet = item
                .get("description")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
                .map(clean_text)
                .unwrap_or_default();
            Some(SearchResult {
                title: truncate_chars(&title, MAX_TITLE),
                url,
                snippet: truncate_chars(&snippet, MAX_SNIPPET),
            })
        })
        .collect()
}

fn render(query: &str, backend: SearchBackend, results: &[SearchResult]) -> ToolOutput {
    let mut output = format!(
        "query: {query}\nbackend: {}\nresults: {}\n",
        backend.name(),
        results.len()
    );
    for (index, result) in results.iter().enumerate() {
        output.push_str(&format!("\n{}. {}\n", index + 1, result.title));
        output.push_str(&format!("   {}\n", result.url));
        if !result.snippet.is_empty() {
            output.push_str(&format!("   {}\n", result.snippet));
        }
    }

    let bytes = output.len() as u64;
    let truncated = output.len() > MAX_OUTPUT;
    let (output, _) = crate::truncate(&output, MAX_OUTPUT);
    let metadata = ToolMetadata {
        truncated,
        bytes,
        ..ToolMetadata::default()
    };
    ToolOutput::ok(output).with_metadata(metadata)
}

/// Strip tags, decode the common entities, and collapse whitespace. Search
/// snippets are HTML fragments; the model does not need the markup.
fn clean_text(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => text.push(ch),
        }
    }
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qcode_core::event::ToolStatus;
    use qcode_core::network::SharedNetwork;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn settings(
        backend: SearchBackend,
        key: Option<&str>,
        base_url: Option<String>,
    ) -> SearchSettings {
        SearchSettings {
            backend,
            api_key: key.map(str::to_string),
            base_url,
            max_results: 5,
        }
    }

    /// Serves one request, records it, and replies with `body`.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&buffer).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn approval_defaults_to_network() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebSearch::new(SearchSettings::default());
        assert_eq!(
            tool.approval(&ToolContext::for_test(dir.path()), &json!({})),
            ApprovalClass::Network
        );
    }

    #[test]
    fn settings_usability_depends_on_the_backend() {
        assert!(!settings(SearchBackend::Brave, None, None).is_usable());
        assert!(settings(SearchBackend::Brave, Some("k"), None).is_usable());
        assert!(!settings(SearchBackend::Tavily, Some("  "), None).is_usable());
        assert!(!settings(SearchBackend::Searxng, None, None).is_usable());
        assert!(
            settings(
                SearchBackend::Searxng,
                None,
                Some("http://localhost:8888".into())
            )
            .is_usable()
        );
    }

    #[test]
    fn parses_brave_results() {
        let value = json!({
            "web": {"results": [
                {"title": "Rust <b>lang</b>", "url": "https://www.rust-lang.org/", "description": "A   language &amp; toolchain"},
                {"title": "Docs", "url": "https://doc.rust-lang.org/", "description": ""}
            ]}
        });
        let results = parse_results(SearchBackend::Brave, &value);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust lang");
        assert_eq!(results[0].snippet, "A language & toolchain");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn parses_tavily_and_searxng_results() {
        let tavily =
            json!({"results": [{"title": "T", "url": "https://t.example/", "content": "C"}]});
        let results = parse_results(SearchBackend::Tavily, &tavily);
        assert_eq!(results[0].snippet, "C");

        let searxng =
            json!({"results": [{"title": "S", "url": "https://s.example/", "content": "body"}]});
        let results = parse_results(SearchBackend::Searxng, &searxng);
        assert_eq!(results[0].url, "https://s.example/");
        assert_eq!(results[0].snippet, "body");
    }

    #[tokio::test]
    async fn missing_keys_and_empty_results_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = WebSearch::new(SearchSettings::default());
        tool.settings.api_key = None;
        let error = tool
            .execute(&ToolContext::for_test(dir.path()), json!({"query": "rust"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("BRAVE_API_KEY"), "{error}");

        let output = render("rust", SearchBackend::Brave, &[]);
        assert!(output.output.contains("results: 0"), "{}", output.output);
    }

    #[test]
    fn long_outputs_are_truncated() {
        let results: Vec<_> = (0..20)
            .map(|index| SearchResult {
                title: format!("title {index}"),
                url: format!("https://example.com/{index}"),
                snippet: "x".repeat(3_000),
            })
            .collect();
        let output = render("q", SearchBackend::Brave, &results);
        assert!(output.output.len() <= MAX_OUTPUT + 100);
        assert!(output.metadata.truncated);
    }

    #[tokio::test]
    async fn brave_requests_the_endpoint_with_count_and_token() {
        let body = r#"{"web":{"results":[{"title":"Rust","url":"https://rust-lang.org/","description":"lang"}]}}"#;
        let (url, handle) = serve_once("200 OK", body).await;
        let tool = WebSearch::new(settings(SearchBackend::Brave, Some("secret"), Some(url)));
        let dir = tempfile::tempdir().unwrap();
        let output = tool
            .execute(
                &ToolContext::for_test(dir.path()),
                json!({"query": "rust lang", "max_results": 3}),
            )
            .await
            .unwrap();
        assert!(output.output.contains("results: 1"), "{}", output.output);
        assert!(
            output.output.contains("https://rust-lang.org/"),
            "{}",
            output.output
        );

        let request = handle.await.unwrap();
        assert!(request.starts_with("GET /res/v1/web/search?"), "{request}");
        assert!(request.contains("q=rust"), "{request}");
        assert!(request.contains("count=3"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-subscription-token: secret"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn tavily_posts_json_with_a_bearer_token() {
        let body = r#"{"results":[{"title":"T","url":"https://t.example/","content":"C"}]}"#;
        let (url, handle) = serve_once("200 OK", body).await;
        let tool = WebSearch::new(settings(SearchBackend::Tavily, Some("tvly-1"), Some(url)));
        let dir = tempfile::tempdir().unwrap();
        let output = tool
            .execute(&ToolContext::for_test(dir.path()), json!({"query": "rust"}))
            .await
            .unwrap();
        assert!(output.output.contains("https://t.example/"));

        let request = handle.await.unwrap();
        assert!(request.starts_with("POST /search "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer tvly-1"),
            "{request}"
        );
        assert!(request.contains("\"query\":\"rust\""), "{request}");
        assert!(request.contains("\"max_results\":5"), "{request}");
    }

    #[tokio::test]
    async fn http_errors_are_returned_as_tool_errors() {
        let (url, handle) = serve_once("401 Unauthorized", r#"{"error":"bad key"}"#).await;
        let tool = WebSearch::new(settings(SearchBackend::Brave, Some("wrong"), Some(url)));
        let dir = tempfile::tempdir().unwrap();
        let output = tool
            .execute(&ToolContext::for_test(dir.path()), json!({"query": "rust"}))
            .await
            .unwrap();
        assert_eq!(output.status, ToolStatus::Error);
        assert!(output.output.contains("HTTP 401"), "{}", output.output);
        let _ = handle.await.unwrap();
    }

    #[tokio::test]
    async fn requests_go_through_the_egress_proxy() {
        // A loopback backend is reachable either way; only the proxy's audit
        // line proves the tool used `ctx.network`'s client rather than a direct
        // one.
        let body =
            r#"{"results":[{"title":"Rust","url":"https://rust-lang.org/","content":"lang"}]}"#;
        let (url, handle) = serve_once("200 OK", body).await;
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("network.jsonl");
        let egress = qcode_net::Egress::start(qcode_net::EgressOptions {
            allow: vec!["127.0.0.1".to_string()],
            allow_private: true,
            audit_path: Some(audit.clone()),
            ..qcode_net::EgressOptions::default()
        })
        .await
        .unwrap();
        let network = std::sync::Arc::new(egress) as SharedNetwork;
        let tool = WebSearch::new(settings(SearchBackend::Searxng, None, Some(url.clone())));
        let output = tool
            .execute(
                &ToolContext::for_test(dir.path()).with_network(network),
                json!({"query": "rust"}),
            )
            .await
            .unwrap();
        assert!(
            output.output.contains("https://rust-lang.org/"),
            "{}",
            output.output
        );

        let _ = handle.await.unwrap();
        let log = std::fs::read_to_string(&audit).unwrap();
        assert!(log.contains("\"result\":\"allowed\""), "{log}");
        assert!(log.contains("\"reason\":\"http\""), "{log}");
    }
}
