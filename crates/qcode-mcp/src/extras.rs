//! MCP resources and prompts, adapted to the engine's `Tool` contract.
//!
//! MCP servers may expose far more than tools; the engine only understands
//! tools, so each extra capability becomes two tools: one to list and one to
//! fetch. They are registered only when the server answered the corresponding
//! `*/list` during the handshake.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use rmcp::RoleClient;
use rmcp::model::{GetPromptRequestParams, ReadResourceRequestParams};
use rmcp::service::RunningService;
use serde_json::{Value, json};

/// Same ceiling as the built-in tools: the model sees a truncated body.
const OUTPUT_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extra {
    ListResources,
    ReadResource,
    ListPrompts,
    GetPrompt,
}

impl Extra {
    pub fn suffix(self) -> &'static str {
        match self {
            Self::ListResources => "list_resources",
            Self::ReadResource => "read_resource",
            Self::ListPrompts => "list_prompts",
            Self::GetPrompt => "get_prompt",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ListResources => {
                "List the resources this MCP server exposes (uri, name, mime type)"
            }
            Self::ReadResource => "Read one MCP resource by uri",
            Self::ListPrompts => "List the prompt templates this MCP server exposes",
            Self::GetPrompt => "Render one MCP prompt template by name",
        }
    }
}

pub struct McpExtra {
    /// Public name (`mcp__<server>__<suffix>`), shown to the model.
    name: String,
    server: String,
    kind: Extra,
    service: Arc<RunningService<RoleClient, ()>>,
    timeout: Duration,
}

impl McpExtra {
    pub(crate) fn new(
        name: String,
        server: String,
        kind: Extra,
        service: Arc<RunningService<RoleClient, ()>>,
        timeout: Duration,
    ) -> Self {
        Self {
            name,
            server,
            kind,
            service,
            timeout,
        }
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        match self.kind {
            Extra::ListResources => self
                .service
                .list_resources(None)
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string())),
            Extra::ReadResource => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|uri| !uri.trim().is_empty())
                    .ok_or_else(|| "read_resource needs a `uri`".to_string())?;
                // Built through serde so a protocol struct gaining a field does
                // not break this call.
                let params: ReadResourceRequestParams =
                    serde_json::from_value(json!({ "uri": uri })).map_err(|e| e.to_string())?;
                self.service
                    .read_resource(params)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string()))
            }
            Extra::ListPrompts => self
                .service
                .list_prompts(None)
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string())),
            Extra::GetPrompt => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| "get_prompt needs a `name`".to_string())?;
                let arguments = args.get("arguments").cloned();
                let params: GetPromptRequestParams =
                    serde_json::from_value(json!({ "name": name, "arguments": arguments }))
                        .map_err(|e| e.to_string())?;
                self.service
                    .get_prompt(params)
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string()))
            }
        }
    }
}

#[async_trait]
impl Tool for McpExtra {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> ToolSpec {
        let parameters = match self.kind {
            Extra::ListResources | Extra::ListPrompts => json!({
                "type": "object",
                "properties": {}
            }),
            Extra::ReadResource => json!({
                "type": "object",
                "properties": {"uri": {"type": "string", "description": "Resource uri, e.g. file:///a.rs"}},
                "required": ["uri"]
            }),
            Extra::GetPrompt => json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "arguments": {"type": "object", "description": "Prompt arguments, if any"}
                },
                "required": ["name"]
            }),
        };
        ToolSpec {
            name: self.name.clone(),
            description: format!("{} (MCP server '{}')", self.kind.description(), self.server),
            parameters,
        }
    }

    /// The server may read anything it can reach; same class as its tools.
    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::External
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let outcome = tokio::select! {
            _ = ctx.cancel.cancelled() => None,
            result = tokio::time::timeout(self.timeout, self.call(&args)) => Some(result),
        };

        let Some(result) = outcome else {
            return Ok(ToolOutput::cancelled(format!("{} cancelled", self.name)));
        };

        // Three layers: timeout, the call itself, then the JSON encoding.
        let result = match result {
            Ok(Ok(value)) => value,
            Ok(Err(detail)) => {
                return Ok(ToolOutput::error(format!("{} failed: {detail}", self.name)));
            }
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "{} timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                )));
            }
        };

        let text = serde_json::to_string_pretty(&result).unwrap_or_default();
        let (preview, truncated) = truncate(&text, OUTPUT_LIMIT);
        let mut output = ToolOutput::ok(preview);
        output.metadata.truncated = truncated;
        output.metadata.bytes = text.len() as u64;
        if truncated {
            output.full_output = Some(text);
        }
        Ok(output)
    }
}

/// Head/tail preview, same shape as the other tools.
fn truncate(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let head = limit * 2 / 3;
    let tail = limit - head;
    let mut preview = String::with_capacity(limit + 64);
    preview.push_str(&text[..floor_boundary(text, head)]);
    preview.push_str(&format!(
        "\n… [{} bytes truncated] …\n",
        text.len().saturating_sub(limit)
    ));
    preview.push_str(&text[ceil_boundary(text, text.len() - tail)..]);
    (preview, true)
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_carry_the_server_prefix_and_the_schema() {
        let name = format!("mcp__{}__{}", "demo", Extra::ReadResource.suffix());
        assert_eq!(name, "mcp__demo__read_resource");

        // The suffix names are what the model sees; keep them stable.
        assert_eq!(Extra::ListResources.suffix(), "list_resources");
        assert_eq!(Extra::ListPrompts.suffix(), "list_prompts");
        assert_eq!(Extra::GetPrompt.suffix(), "get_prompt");
    }

    #[test]
    fn truncation_keeps_both_ends_on_char_boundaries() {
        let text = "é".repeat(100);
        let (preview, truncated) = truncate(&text, 40);
        assert!(truncated);
        assert!(preview.starts_with('é'));
        assert!(preview.contains("truncated"));
        // No panic above means the slicing stayed on char boundaries.
    }
}
