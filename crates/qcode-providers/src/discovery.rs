//! Endpoint model discovery (dsh `discovery.ts` semantics).
//!
//! Probing an OpenAI-compatible endpoint means `GET {baseURL}/models` with
//! Bearer auth. The result is *candidate metadata only*: nothing here is
//! persisted, the caller decides what to do with the ids, and the config file
//! stays the only fact source for what a route serves.

use std::collections::BTreeMap;

/// The discovery URL for one base URL: trailing `/` trimmed, `/models` appended.
pub fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Parse an OpenAI-compatible `/models` reply: `data[].id`, sorted.
pub fn parse_model_ids(body: &str, url: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("model listing at {url} is not JSON: {error}"))?;
    let Some(data) = value.get("data").and_then(serde_json::Value::as_array) else {
        return Err(format!("model listing at {url} has no data array"));
    };
    let mut ids: Vec<String> = data
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// Build the discovery request. Caller-supplied headers are applied first; the
/// mandatory headers (`accept`, `authorization`) are applied afterwards with
/// replace semantics so reserved names can never be overridden.
fn build_request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> Result<reqwest::Request, String> {
    let url = models_url(base_url);
    let mut builder = client.get(&url);
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::try_from(name.as_str()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let mut mandatory = reqwest::header::HeaderMap::new();
    mandatory.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    if let Some(key) = api_key {
        // A key that cannot become a header value (pathological config) sends
        // no Authorization rather than panicking.
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
            mandatory.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    builder = builder.headers(mandatory);
    builder
        .build()
        .map_err(|error| format!("cannot build discovery request for {url}: {error}"))
}

/// Probe an OpenAI-compatible endpoint for its model list.
///
/// URL is `base_url` trimmed of a trailing `/`, plus `/models`; the request
/// carries `accept: application/json` and `authorization: Bearer <key>` when a
/// key is given, plus the caller-supplied `headers` (mandatory headers win).
/// Results are candidate metadata only — nothing is persisted here.
pub async fn discover_models(
    base_url: &str,
    api_key: Option<&str>,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| format!("cannot build HTTP client: {error}"))?;
    let url = models_url(base_url);
    let request = build_request(&client, base_url, api_key, headers)?;
    let response = client
        .execute(request)
        .await
        .map_err(|error| format!("GET {url} failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("GET {url} returned {status}: {text}"));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("cannot read the model listing body: {error}"))?;
    parse_model_ids(&body, &url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joins_models_without_double_slash() {
        assert_eq!(
            models_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url("https://ark.cn-beijing.volces.com/api/v3/"),
            "https://ark.cn-beijing.volces.com/api/v3/models"
        );
        assert_eq!(
            models_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/models"
        );
    }

    #[test]
    fn parse_returns_sorted_ids_and_rejects_bad_shapes() {
        let ids = parse_model_ids(
            r#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"}]}"#,
            "https://example.com/v1/models",
        )
        .expect("listing");
        assert_eq!(ids, vec!["a", "b", "b"]);
        assert!(parse_model_ids("not json", "https://example.com/v1/models").is_err());
        assert!(parse_model_ids(r#"{"data":{}}"#, "https://example.com/v1/models").is_err());
        assert!(parse_model_ids(r#"{"nope":[]}"#, "https://example.com/v1/models").is_err());
    }

    #[tokio::test]
    async fn mandatory_headers_win_over_caller_headers() {
        let client = reqwest::Client::new();
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_string(), "Basic nope".to_string());
        headers.insert("x-gateway".to_string(), "v1".to_string());
        let request = build_request(&client, "https://example.com/v1/", Some("sk-123"), &headers)
            .expect("request");
        assert_eq!(request.url().as_str(), "https://example.com/v1/models");
        assert_eq!(
            request.headers().get("authorization").expect("auth header"),
            "Bearer sk-123"
        );
        assert_eq!(
            request.headers().get("accept").expect("accept header"),
            "application/json"
        );
        assert_eq!(
            request.headers().get("x-gateway").expect("caller header"),
            "v1"
        );
    }

    #[tokio::test]
    async fn no_key_means_no_authorization_header() {
        let client = reqwest::Client::new();
        let request = build_request(&client, "https://example.com/v1", None, &BTreeMap::new())
            .expect("request");
        assert!(request.headers().get("authorization").is_none());
        assert_eq!(
            request.headers().get("accept").expect("accept header"),
            "application/json"
        );
    }
}
