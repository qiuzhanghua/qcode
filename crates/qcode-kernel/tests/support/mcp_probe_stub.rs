//! Test-only stdio MCP server for the sandbox grant tests (ADR-0066).
//!
//! Two modes, one implementation of the probing:
//!
//! * default: speak just enough MCP for `connect_all` (`initialize`,
//!   `tools/list`, `tools/call`);
//! * `--probe <read>... --write <path>...`: run the same probe and print the
//!   report, so a test can exercise the sandbox without the MCP layer.
//!
//! Every probe reports `ok`, `denied` or `missing` per path, which is exactly
//! the question the grant is supposed to answer.

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(position) = args.iter().position(|arg| arg == "--probe") {
        let report = probe(&args[position + 1..]);
        println!("{report}");
        return;
    }
    serve();
}

/// `<kind> <path> = <result>` per line, for the argv mode and for the tool.
fn probe(args: &[String]) -> String {
    let mut read: Vec<&str> = Vec::new();
    let mut write: Vec<&str> = Vec::new();
    let mut into_read = true;
    for arg in args {
        match arg.as_str() {
            "--read" => into_read = true,
            "--write" => into_read = false,
            path if into_read => read.push(path),
            path => write.push(path),
        }
    }
    report(&read, &write)
}

fn report(read: &[&str], write: &[&str]) -> String {
    let mut lines = Vec::new();
    for path in read {
        lines.push(format!("read {path} = {}", attempt_read(Path::new(path))));
    }
    for path in write {
        let result = std::fs::write(path, b"probe");
        lines.push(format!("write {path} = {}", classify(&result)));
    }
    lines.join("\n")
}

fn attempt_read(path: &Path) -> &'static str {
    // Reading a directory exercises the same Landlock path-beneath rule as
    // reading a file, and works even when the leaf does not exist.
    if path.is_dir() {
        classify(&std::fs::read_dir(path).map(|_| ()))
    } else {
        classify(&std::fs::read(path).map(|_| ()))
    }
}

fn classify(result: &std::io::Result<()>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => "denied",
        Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
        Err(_) => "error",
    }
}

fn serve() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
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
                    "serverInfo": {"name": "mcp-probe-stub", "version": "0.0.1"}
                }),
            ),
            "notifications/initialized" => {}
            "tools/list" => respond(
                &mut stdout,
                id,
                json!({"tools": [
                    {
                        "name": "probe",
                        "description": "Try to read and write the given paths",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "read": {"type": "array", "items": {"type": "string"}},
                                "write": {"type": "array", "items": {"type": "string"}}
                            }
                        }
                    },
                    {
                        "name": "env",
                        "description": "List the environment variable names",
                        "inputSchema": {"type": "object"}
                    },
                    {
                        "name": "home",
                        "description": "Report HOME and the working directory",
                        "inputSchema": {"type": "object"}
                    }
                ]}),
            ),
            "tools/call" => {
                let name = message
                    .get("params")
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = message
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let strings = |key: &str| -> Vec<String> {
                    arguments
                        .get(key)
                        .and_then(Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let text = match name {
                    "probe" => {
                        let read = strings("read");
                        let write = strings("write");
                        let read: Vec<&str> = read.iter().map(String::as_str).collect();
                        let write: Vec<&str> = write.iter().map(String::as_str).collect();
                        report(&read, &write)
                    }
                    "env" => {
                        let mut keys: Vec<String> = std::env::vars().map(|(key, _)| key).collect();
                        keys.sort();
                        keys.join(" ")
                    }
                    "home" => format!(
                        "HOME={} cwd={}",
                        std::env::var("HOME").unwrap_or_default(),
                        std::env::current_dir()
                            .map(|path| path.display().to_string())
                            .unwrap_or_default()
                    ),
                    _ => String::new(),
                };
                respond(
                    &mut stdout,
                    id,
                    json!({"content": [{"type": "text", "text": text}]}),
                );
            }
            _ => {
                if id.is_some() {
                    respond(&mut stdout, id, json!({}));
                }
            }
        }
    }
}

fn respond(stdout: &mut impl Write, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    let _ = writeln!(
        stdout,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    );
    let _ = stdout.flush();
}
