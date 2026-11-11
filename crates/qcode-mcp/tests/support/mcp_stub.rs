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
    // Optional capabilities come from the environment so a test can prove the
    // client passes `[[mcp.servers]] env` through to the server process.
    let caps = std::env::var("MCP_STUB_CAPS").unwrap_or_else(|_| "tools".to_string());
    let has = |capability: &str| caps.split(',').any(|name| name.trim() == capability);

    // ADR-0067 §5 fixtures. Both are opt-in so every existing test keeps the
    // old "list once" behaviour.
    //
    // * `MCP_STUB_TTL_MS` puts `ttlMs` on `tools/list`, which is what makes the
    //   client's table expire (or, at 0, never be trusted).
    // * `MCP_STUB_MUTATE` swaps one tool for another after the first
    //   `tools/list` and announces it with `notifications/tools/list_changed`,
    //   which is the only way to exercise the notification path over stdio.
    let ttl_ms: Option<u64> = std::env::var("MCP_STUB_TTL_MS")
        .ok()
        .and_then(|value| value.parse().ok());
    let mutate = std::env::var_os("MCP_STUB_MUTATE").is_some();
    // ADR-0068 §3 fixture: a server that describes itself in the handshake.
    // `MCP_STUB_INSTRUCTIONS_BYTES=n` sends `n` bytes of prose, which is how a
    // test reaches the 8 KiB budget without embedding a huge string.
    let instructions = match std::env::var("MCP_STUB_INSTRUCTIONS_BYTES") {
        Ok(bytes) => {
            let bytes: usize = bytes.parse().expect("MCP_STUB_INSTRUCTIONS_BYTES");
            Some(format!("{}{}", INSTRUCTIONS_PREFIX, "x".repeat(bytes)))
        }
        Err(_) => std::env::var("MCP_STUB_INSTRUCTIONS").ok(),
    };
    // Which generation of the table the next `tools/list` reports.
    let mut generation = 1u32;
    let mut listed = 0u32;

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
            "initialize" => {
                let mut capabilities = json!({"tools": {}});
                if has("resources") {
                    capabilities["resources"] = json!({});
                }
                if has("prompts") {
                    capabilities["prompts"] = json!({});
                }
                let mut result = json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": capabilities,
                    "serverInfo": {"name": "mcp-stub", "version": "0.0.1"}
                });
                if let Some(instructions) = &instructions {
                    result["instructions"] = json!(instructions);
                }
                respond(&mut stdout, id, result)
            }
            "notifications/initialized" => {}
            "tools/list" => {
                listed += 1;
                // Only the fixture modes get the extra tool, so the plain
                // stub keeps the seven-tool table older tests assert on.
                let reported = (mutate || ttl_ms.is_some()).then_some(generation);
                let mut result = json!({"tools": tools(reported)});
                if let Some(ttl_ms) = ttl_ms {
                    result["ttlMs"] = json!(ttl_ms);
                    result["cacheScope"] = json!("public");
                    result["resultType"] = json!("complete");
                }
                // After the first listing, move the table and say so. The
                // notification arrives while no turn is running, so a client
                // that refetches immediately and one that waits for the next
                // turn boundary both "work" — only the event count tells them
                // apart.
                respond(&mut stdout, id, result);
                // In fixture mode the table moves on every read, so a re-read is
                // observable as a different tool name. Without this the client
                // could refetch every turn and still see an unchanged set.
                if mutate || ttl_ms.is_some() {
                    generation += 1;
                }
                if mutate && listed == 1 {
                    write_message(
                        &mut stdout,
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                    );
                }
            }
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
            "resources/list" if has("resources") => respond(
                &mut stdout,
                id,
                json!({
                    "resources": [
                        {
                            "uri": "file:///notes.md",
                            "name": "notes",
                            "mimeType": "text/markdown"
                        }
                    ]
                }),
            ),
            "resources/read" if has("resources") => {
                let uri = message
                    .pointer("/params/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "contents": [
                            {"uri": uri, "mimeType": "text/markdown", "text": "# notes\nfrom the stub"}
                        ]
                    }),
                );
            }
            "prompts/list" if has("prompts") => respond(
                &mut stdout,
                id,
                json!({"prompts": [{"name": "greet", "description": "Say hello"}]}),
            ),
            "prompts/get" if has("prompts") => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                respond(
                    &mut stdout,
                    id,
                    json!({
                        "description": "greeting",
                        "messages": [
                            {"role": "user", "content": {"type": "text", "text": format!("hello from {name}")}}
                        ]
                    }),
                );
            }
            _ => {
                if id.is_some() {
                    error(&mut stdout, id, -32601, "method not found");
                }
            }
        }
    }
}

/// The table as of `generation`.
///
/// `None` is the historical seven-tool table, so every test written before
/// ADR-0067 keeps seeing the same list. `Some(n)` adds one generation-stamped
/// tool, which lets a refresh be observed in both directions when the server
/// moves from generation 1 to 2 (one added, one removed).
/// Keeps the byte count in the test's control while the text stays readable.
const INSTRUCTIONS_PREFIX: &str = "Use `echo` before `big`. ";

fn tools(generation: Option<u32>) -> Value {
    let extra = generation.map(|generation| {
        json!([{
            "name": format!("echo_v{generation}"),
            "description": "Exists only in this generation of the table",
            "inputSchema": {"type": "object"}
        }])
    });
    let mut list = json!([
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
    ]);
    if let (Some(extra), Some(items)) = (extra, list.as_array_mut()) {
        items.extend(extra.as_array().cloned().unwrap_or_default());
    }
    list
}

fn call(stdout: &mut impl Write, id: Option<Value>, name: &str, arguments: Value) {
    // The generation-stamped fixture tools behave like `echo`, so a test can
    // call one and prove which generation is live.
    if name.starts_with("echo_v") {
        respond(
            stdout,
            id,
            json!({"content": [{"type": "text", "text": name.to_string()}]}),
        );
        return;
    }
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
