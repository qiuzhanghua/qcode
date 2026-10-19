//! One MCP tool, adapted to the engine's `Tool` contract.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::ToolMetadata;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents,
    Tool as McpToolDefinition,
};
use rmcp::service::RunningService;
use serde_json::{Map, Value};

/// Same ceiling as the built-in tools: the model sees a head/tail preview.
const OUTPUT_LIMIT: usize = 32 * 1024;

pub struct McpTool {
    /// Public name (`mcp__<server>__<tool>`), shown to the model.
    name: String,
    server: String,
    definition: McpToolDefinition,
    service: Arc<RunningService<RoleClient, ()>>,
    timeout: Duration,
}

impl McpTool {
    pub(crate) fn new(
        name: String,
        server: String,
        definition: McpToolDefinition,
        service: Arc<RunningService<RoleClient, ()>>,
        timeout: Duration,
    ) -> Self {
        Self {
            name,
            server,
            definition,
            service,
            timeout,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self) -> ToolSpec {
        let description = self
            .definition
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "MCP tool '{}' exposed by server '{}'",
                    self.definition.name, self.server
                )
            });
        ToolSpec {
            name: self.name.clone(),
            description,
            parameters: Value::Object((*self.definition.input_schema).clone()),
        }
    }

    /// MCP tools can do anything the server does. Even a tool that claims to be
    /// read-only is external code, so the engine treats every call as `ask`.
    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::External
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let arguments = match args {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => {
                return Err(ToolError::Invalid(
                    "MCP tool arguments must be a JSON object".to_string(),
                ));
            }
        };

        let params =
            CallToolRequestParams::new(self.definition.name.clone()).with_arguments(arguments);

        let outcome = tokio::select! {
            _ = ctx.cancel.cancelled() => None,
            result = tokio::time::timeout(self.timeout, self.service.call_tool(params)) => Some(result),
        };

        let Some(result) = outcome else {
            return Ok(ToolOutput::cancelled(format!(
                "MCP tool '{}' cancelled",
                self.name
            )));
        };

        let result = match result {
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "MCP tool '{}' timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                )));
            }
            Ok(Err(error)) => {
                return Ok(ToolOutput::error(format!(
                    "MCP tool '{}' failed: {error}",
                    self.name
                )));
            }
            Ok(Ok(result)) => result,
        };

        Ok(render_result(&self.server, result))
    }
}

/// Flatten MCP content blocks into the text the model sees.
fn render_result(server: &str, result: CallToolResult) -> ToolOutput {
    let CallToolResult {
        content,
        structured_content,
        is_error,
        ..
    } = result;

    let mut parts: Vec<String> = content.into_iter().map(render_block).collect();
    parts.retain(|part| !part.is_empty());
    let mut body = parts.join("\n");

    if body.is_empty()
        && let Some(value) = structured_content
    {
        body = serde_json::to_string(&value).unwrap_or_else(|_| "(invalid JSON)".to_string());
    }

    let body = if body.is_empty() {
        "(no output)".to_string()
    } else {
        body
    };
    // The tool call already carries the server name; the marker repeats it in
    // the tool message so the model can attribute the text after compaction.
    let full = format!("[mcp:{server}]\n{body}");
    let bytes = full.len() as u64;
    let (output, truncated) = truncate(&full, OUTPUT_LIMIT);
    let metadata = ToolMetadata {
        truncated,
        bytes,
        ..ToolMetadata::default()
    };

    if is_error == Some(true) {
        ToolOutput::error(output).with_metadata(metadata)
    } else {
        ToolOutput::ok(output).with_metadata(metadata)
    }
}

fn render_block(block: ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text,
        ContentBlock::Image(image) => format!("[image omitted: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio omitted: {}]", audio.mime_type),
        ContentBlock::Resource(resource) => match resource.resource {
            ResourceContents::TextResourceContents { uri, text, .. } => {
                format!("[resource: {uri}]\n{text}")
            }
            ResourceContents::BlobResourceContents { uri, mime_type, .. } => match mime_type {
                Some(mime_type) => format!("[binary resource: {uri} ({mime_type})]"),
                None => format!("[binary resource: {uri}]"),
            },
            _ => "[unsupported resource]".to_string(),
        },
        ContentBlock::ResourceLink(link) => format!("[resource link: {}]", link.uri),
        _ => "[unsupported content block]".to_string(),
    }
}

/// Head+tail truncation on UTF-8 boundaries. Mirrors `qcode-tools::truncate`;
/// the two crates stay independent on purpose.
fn truncate(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }

    let head = limit * 3 / 4;
    let tail = limit - head;
    let head_end = floor_char_boundary(text, head);
    let tail_start = ceil_char_boundary(text, text.len() - tail);
    let omitted = text.len() - head_end - (text.len() - tail_start);

    (
        format!(
            "{}\n… [truncated {omitted} bytes] …\n{}",
            &text[..head_end],
            &text[tail_start..]
        ),
        true,
    )
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{EmbeddedResource, Resource, TextContent};

    fn result_with(content: Vec<ContentBlock>) -> CallToolResult {
        CallToolResult::success(content)
    }

    #[test]
    fn prefixes_server_and_joins_text_blocks() {
        let result = result_with(vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ]);
        let output = render_result("fs", result);
        assert_eq!(output.status, qcode_core::event::ToolStatus::Ok);
        assert_eq!(output.output, "[mcp:fs]\nfirst\nsecond");
    }

    #[test]
    fn error_flag_becomes_error_status() {
        let mut result = result_with(vec![ContentBlock::text("boom")]);
        result.is_error = Some(true);
        let output = render_result("fs", result);
        assert_eq!(output.status, qcode_core::event::ToolStatus::Error);
        assert!(output.output.contains("boom"));
    }

    #[test]
    fn non_text_blocks_are_described_not_inlined() {
        let image = ContentBlock::image("aGk=", "image/png");
        let link = ContentBlock::resource_link(Resource::new("file:///tmp/x", "x"));
        let resource = ContentBlock::Resource(EmbeddedResource::new(ResourceContents::text(
            "hello",
            "file:///tmp/a.txt",
        )));
        let output = render_result("fs", result_with(vec![image, link, resource]));
        assert!(output.output.contains("[image omitted: image/png]"));
        assert!(output.output.contains("[resource link: file:///tmp/x]"));
        assert!(
            output
                .output
                .contains("[resource: file:///tmp/a.txt]\nhello")
        );
    }

    #[test]
    fn structured_content_is_the_fallback_body() {
        let mut result = result_with(Vec::new());
        result.structured_content = Some(serde_json::json!({"answer": 42}));
        let output = render_result("calc", result);
        assert_eq!(output.output, "[mcp:calc]\n{\"answer\":42}");
    }

    #[test]
    fn empty_result_gets_a_placeholder() {
        let output = render_result("fs", result_with(Vec::new()));
        assert_eq!(output.output, "[mcp:fs]\n(no output)");
    }

    #[test]
    fn large_output_is_truncated() {
        let big = "x".repeat(OUTPUT_LIMIT + 1000);
        let output = render_result("fs", result_with(vec![ContentBlock::text(big)]));
        assert!(output.metadata.truncated);
        assert!(output.output.contains("truncated"));
        assert!(output.output.len() <= OUTPUT_LIMIT + 200);
        assert_eq!(
            output.metadata.bytes,
            (OUTPUT_LIMIT + 1000 + "[mcp:fs]\n".len()) as u64
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let big = "界".repeat(OUTPUT_LIMIT);
        let (text, truncated) = truncate(&big, OUTPUT_LIMIT);
        assert!(truncated);
        assert!(text.is_char_boundary(text.len()));
    }
}
