//! End-to-end tests against `tests/support/mcp_stub.rs`, a real child process
//! speaking newline-delimited JSON-RPC over stdio.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qcode_core::event::ToolStatus;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext};
use qcode_mcp::{McpServerConfig, connect_all};
use serde_json::json;
use tempfile::TempDir;

const STUB: &str = env!("CARGO_BIN_EXE_mcp-stub");

fn config(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: STUB.to_string(),
        args: Vec::new(),
        enabled: true,
        timeout_secs: 10,
    }
}

async fn connect(config: McpServerConfig) -> (TempDir, Vec<Arc<dyn Tool>>) {
    let dir = tempfile::tempdir().unwrap();
    let tools = connect_all(&[config], dir.path(), &dir.path().join("logs")).await;
    (dir, tools)
}

fn find<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("missing tool {name}; have {:?}", names(tools)))
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name()).collect()
}

#[tokio::test]
async fn lists_tools_with_server_prefix_and_schema() {
    let (dir, tools) = connect(config("stub")).await;
    assert_eq!(tools.len(), 7, "got {:?}", names(&tools));
    assert!(names(&tools).contains(&"mcp__stub__echo"));

    let echo = find(&tools, "mcp__stub__echo");
    let spec = echo.spec();
    assert_eq!(spec.name, "mcp__stub__echo");
    assert!(spec.description.contains("Echo the message"));
    assert_eq!(spec.parameters["type"], "object");

    // MCP tools are external code: the default is ask, never allow.
    let context = ToolContext::new(dir.path());
    assert_eq!(echo.approval(&context, &json!({})), ApprovalClass::External);
}

#[tokio::test]
async fn calls_a_tool_and_renders_text() {
    let (dir, tools) = connect(config("stub")).await;
    let echo = find(&tools, "mcp__stub__echo").clone();
    let context = ToolContext::new(dir.path());

    let output = echo
        .execute(&context, json!({"message": "hello"}))
        .await
        .unwrap();
    assert_eq!(output.status, ToolStatus::Ok);
    assert_eq!(output.output, "[mcp:stub]\nhello");
}

#[tokio::test]
async fn tool_error_flag_becomes_error_status() {
    let (dir, tools) = connect(config("stub")).await;
    let fail = find(&tools, "mcp__stub__fail").clone();
    let context = ToolContext::new(dir.path());

    let output = fail.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Error);
    assert!(output.output.contains("boom"));
}

#[tokio::test]
async fn large_output_is_truncated() {
    let (dir, tools) = connect(config("stub")).await;
    let big = find(&tools, "mcp__stub__big").clone();
    let context = ToolContext::new(dir.path());

    let output = big.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Ok);
    assert!(output.metadata.truncated);
    assert!(output.output.contains("truncated"));
    assert!(output.output.len() < 33 * 1024);
}

#[tokio::test]
async fn server_crash_becomes_a_tool_error() {
    let (dir, tools) = connect(config("stub")).await;
    let crash = find(&tools, "mcp__stub__crash").clone();
    let context = ToolContext::new(dir.path());

    let output = crash.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Error);
    assert!(output.output.contains("failed"));
}

#[tokio::test]
async fn request_timeout_becomes_a_tool_error() {
    let mut config = config("stub");
    config.timeout_secs = 1;
    let (dir, tools) = connect(config).await;
    let hang = find(&tools, "mcp__stub__hang").clone();
    let context = ToolContext::new(dir.path());

    let started = Instant::now();
    let output = hang.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Error);
    assert!(output.output.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancellation_becomes_cancelled_without_responses() {
    let mut config = config("stub");
    config.timeout_secs = 30;
    let (dir, tools) = connect(config).await;
    let hang = find(&tools, "mcp__stub__hang").clone();
    let cancel = tokio_util::sync::CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        canceller.cancel();
    });
    let context = ToolContext::new(dir.path()).with_cancel(cancel);

    let output = hang.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Cancelled);
}

#[tokio::test]
async fn malformed_result_is_an_error_not_a_hang() {
    let mut config = config("stub");
    config.timeout_secs = 2;
    let (dir, tools) = connect(config).await;
    let bad = find(&tools, "mcp__stub__badresult").clone();
    let context = ToolContext::new(dir.path());

    let started = Instant::now();
    let output = bad.execute(&context, json!({})).await.unwrap();
    assert_eq!(output.status, ToolStatus::Error);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn disabled_and_missing_servers_degrade_to_no_tools() {
    let dir = tempfile::tempdir().unwrap();

    let mut disabled = config("off");
    disabled.enabled = false;
    let missing = McpServerConfig {
        name: "missing".to_string(),
        command: dir.path().join("does-not-exist").display().to_string(),
        args: Vec::new(),
        enabled: true,
        timeout_secs: 2,
    };

    let tools = connect_all(&[disabled, missing], dir.path(), &dir.path().join("logs")).await;
    assert!(tools.is_empty(), "got {:?}", names(&tools));
    assert!(Path::new(STUB).exists());
}
