// Test-only Streamable HTTP MCP server, used by `qcode-mcp`'s
// `tests/http_transport.rs` and by `qcode-kernel`'s egress test.
//
// Plain `//` rather than `//!`: the kernel's copy is an `include!` of this
// file, and inner doc comments cannot follow the `include!` item.
//
// Hand-rolled HTTP/1.1 over a TcpListener rather than a framework: the test
// has to assert on the *raw* requests (which method went out, which headers,
// which `_meta` version), and a framework would hide exactly that. No extra
// dependency, no Python, no uvicorn.
//
// Modes:
//
// * `--modern` (default): answers `server/discover` with
//   `supportedVersions = ["2026-07-28"]`, so the client stays on the
//   no-initialize lifecycle and must never send `initialize`.
// * `--legacy`: answers `server/discover` with `-32601` method not found,
//   which is what a 2025-06-18 server does (wordcraft's shape), so the
//   client must fall back to `initialize`.
// * `--silent`: never answers `server/discover` — exercises the discover
//   timeout path.
//
// Every request is appended to `--audit <path>` as one JSON line, so a test
// can assert what actually crossed the wire.
//
// Prints `listening <port>` on stdout once bound, which is how the test
// learns the port after asking for port 0.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Modern,
    Legacy,
    Silent,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = Mode::Modern;
    let mut audit: Option<PathBuf> = None;
    let mut ttl_ms: u64 = 30_000;
    let mut instructions: Option<String> = None;
    // ADR-0068 §4: which optional capabilities to declare in the handshake.
    // Empty by default, so the zero-probe assertion has something to assert on.
    let mut caps: Vec<String> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--modern" => mode = Mode::Modern,
            "--legacy" => mode = Mode::Legacy,
            "--silent" => mode = Mode::Silent,
            "--ttl-ms" => {
                index += 1;
                ttl_ms = args[index].parse().expect("--ttl-ms takes a number");
            }
            "--capabilities" => {
                index += 1;
                caps = args[index]
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect();
            }
            "--instructions" => {
                index += 1;
                instructions = Some(args[index].clone());
            }
            "--require-token" => {
                index += 1;
                tokens.push(args[index].clone());
            }
            "--audit" => {
                index += 1;
                audit = Some(PathBuf::from(&args[index]));
            }
            other => panic!("unknown argument {other}"),
        }
        index += 1;
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let audit = Arc::new(Mutex::new(audit));
    println!("listening {port}");
    std::io::stdout().flush().expect("flush");

    let state = Arc::new(State {
        mode,
        ttl_ms,
        instructions,
        capabilities: capabilities(&caps),
        tokens,
        audit,
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        // One thread per connection; the test makes a handful of requests.
        std::thread::spawn(move || {
            let _ = handle(stream, &state);
        });
    }
}

/// The `capabilities` object the handshake reports, from `--capabilities`.
///
/// `tools` is always there (every mode serves `tools/list`); the rest is opt-in
/// so a mode that declares nothing is the default.
fn capabilities(names: &[String]) -> Value {
    let mut caps = json!({"tools": {}});
    for name in names {
        caps[name.as_str()] = json!({});
    }
    caps
}

struct State {
    mode: Mode,
    ttl_ms: u64,
    instructions: Option<String>,
    capabilities: Value,
    tokens: Vec<String>,
    audit: Arc<Mutex<Option<PathBuf>>>,
}

fn handle(mut stream: TcpStream, state: &State) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let Some((method, headers, body)) = read_request(&mut reader)? else {
            return Ok(());
        };
        record(state, &method, &headers, &body);

        // A GET is the client opening the server-to-client SSE stream. This
        // stub has nothing to send, so 405 is the spec-blessed answer and
        // keeps rmcp from waiting on a stream that will never produce events.
        if method == "GET" {
            write_response(&mut stream, 405, None, "")?;
            continue;
        }
        if method == "DELETE" {
            write_response(&mut stream, 200, None, "")?;
            continue;
        }

        if let Some(required) = state.tokens.first() {
            let presented = header(&headers, "authorization").unwrap_or_default();
            if presented != format!("Bearer {required}") {
                write_response(
                    &mut stream,
                    401,
                    Some(("WWW-Authenticate", "Bearer realm=\"probe\"".to_string())),
                    r#"{"error":"unauthorized"}"#,
                )?;
                continue;
            }
        }

        let Ok(message) = serde_json::from_str::<Value>(&body) else {
            write_response(&mut stream, 400, None, r#"{"error":"bad json"}"#)?;
            continue;
        };
        let id = message.get("id").cloned();
        let rpc = message.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            // A notification: 202 with no body, per the transport spec.
            write_response(&mut stream, 202, None, "")?;
            continue;
        }
        let payload = match (rpc, state.mode) {
            ("server/discover", Mode::Silent) => continue,
            ("server/discover", Mode::Legacy) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }),
            ("server/discover", Mode::Modern) => {
                let mut result = json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": state.capabilities,
                    "ttlMs": state.ttl_ms,
                    "cacheScope": "public",
                });
                // ADR-0068 §2: `instructions` is the server's self-description.
                // Offered here so that work has a fixture waiting for it.
                if let Some(instructions) = &state.instructions {
                    result["instructions"] = json!(instructions);
                }
                ok(id, result)
            }
            ("initialize", _) => ok(
                id,
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": state.capabilities,
                    "serverInfo": {"name": "http-probe", "version": "0.0.1"},
                }),
            ),
            ("tools/list", _) => {
                let mut result = json!({"tools": [tool_definition()]});
                if state.mode == Mode::Modern {
                    result["ttlMs"] = json!(state.ttl_ms);
                    result["cacheScope"] = json!("public");
                    result["resultType"] = json!("complete");
                }
                ok(id, result)
            }
            ("tools/call", _) => ok(
                id,
                json!({"content": [{"type": "text", "text": "probe says hi"}]}),
            ),
            // No resources or prompts: the client's capability probe gets an
            // error and registers no extra tools, which is the path most
            // servers take.
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }),
        };
        write_response(&mut stream, 200, None, &payload.to_string())?;
    }
}

fn tool_definition() -> Value {
    json!({
        "name": "ping",
        "description": "Say hi from the HTTP probe",
        "inputSchema": {"type": "object"}
    })
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// One parsed request: method, lowercased header pairs, raw body.
type Request = (String, Vec<(String, String)>, String);

/// Split the request line, the headers we care about and the body.
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let method = line.split_whitespace().next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length: usize = header(&headers, "content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        std::io::Read::read_exact(reader, &mut body)?;
    }
    Ok(Some((
        method,
        headers,
        String::from_utf8_lossy(&body).into_owned(),
    )))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    extra: Option<(&str, String)>,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some((name, value)) = extra {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    // No session id: `2026-07-28` has no sessions, and `allow_stateless = true`
    // lets the client start without one.
    response.push_str("\r\n");
    response.push_str(body);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// One JSON line per request: method, headers, and the raw body. Tests read
/// this file instead of inferring from behaviour, so a request that was never
/// sent cannot be mistaken for one that was.
fn record(state: &State, method: &str, headers: &[(String, String)], body: &str) {
    let Some(path) = state.audit.lock().expect("audit lock").clone() else {
        return;
    };
    let message: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let line = json!({
        "method": method,
        "authorization": header(headers, "authorization"),
        "accept": header(headers, "accept"),
        "user_agent": header(headers, "user-agent"),
        "custom": header(headers, "x-probe"),
        "rpc": message.get("method"),
        "protocol_meta": message
            .get("params")
            .and_then(|params| params.get("_meta"))
            .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion")),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open audit");
    writeln!(file, "{line}").expect("write audit");
}
