//! Test-only stdio MCP server used by `tests/mcp_stdio.rs`.
//!
//! Implements just enough of the protocol to exercise the qcode client:
//! `initialize`, `tools/list` and `tools/call`. Each scenario is a tool so one
//! server process can serve every test.

use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");

        match method {
            "initialize" => respond(
                &mut stdout,
                id,
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mcp-stub", "version": "0.0.1"}
                }),
            ),
            "notifications/initialized" => {}
            "tools/list" => respond(&mut stdout, id, json!({"tools": tools()})),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(Value::Null);
                call(&mut stdout, id, name, arguments);
            }
            _ => {
                if id.is_some() {
                    error(&mut stdout, id, -32601, "method not found");
                }
            }
        }
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "echo",
            "description": "Echo the message argument",
            "inputSchema": {
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"]
            }
        },
        {"name": "fail", "description": "Return an error result", "inputSchema": {"type": "object"}},
        {"name": "big", "description": "Return a large text result", "inputSchema": {"type": "object"}},
        {"name": "hang", "description": "Never reply", "inputSchema": {"type": "object"}},
        {"name": "crash", "description": "Exit the server", "inputSchema": {"type": "object"}},
        {"name": "badresult", "description": "Send a malformed result", "inputSchema": {"type": "object"}},
        {"name": "image", "description": "Return an image block", "inputSchema": {"type": "object"}}
    ])
}

fn call(stdout: &mut impl Write, id: Option<Value>, name: &str, arguments: Value) {
    match name {
        "echo" => {
            let text = arguments
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| arguments.to_string());
            respond(
                stdout,
                id,
                json!({"content": [{"type": "text", "text": text}]}),
            );
        }
        "fail" => respond(
            stdout,
            id,
            json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        ),
        "big" => respond(
            stdout,
            id,
            json!({"content": [{"type": "text", "text": "x".repeat(40_000)}]}),
        ),
        "image" => respond(
            stdout,
            id,
            json!({"content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}),
        ),
        "hang" => {
            // Keep the process alive without answering; the client must time
            // out or cancel instead of hanging forever.
            std::thread::sleep(Duration::from_secs(600));
        }
        "crash" => std::process::exit(1),
        "badresult" => {
            let id = id.unwrap_or(Value::Null);
            let _ = writeln!(
                stdout,
                "{}",
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": "not-an-array"}})
            );
            let _ = stdout.flush();
        }
        _ => {
            if id.is_some() {
                error(stdout, id, -32602, "unknown tool");
            }
        }
    }
}

fn respond(stdout: &mut impl Write, id: Option<Value>, result: Value) {
    // Requests carry an id; notifications do not get a response.
    if let Some(id) = id {
        write_message(
            stdout,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        );
    }
}

fn error(stdout: &mut impl Write, id: Option<Value>, code: i64, message: &str) {
    if let Some(id) = id {
        write_message(
            stdout,
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        );
    }
}

fn write_message(stdout: &mut impl Write, message: Value) {
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}
