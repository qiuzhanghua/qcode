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
    // ADR-0108 §2: one line per process start, so a test can count launches.
    // Connecting twice is invisible from the outside — the session still works,
    // the servers just run twice — so the launch itself has to be recorded.
    if let Ok(log) = std::env::var("MCP_START_LOG") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(file, "start");
        }
    }
    // Optional capabilities come from the environment, like `mcp-stub` in
    // `qcode-mcp`, so a test can make this server declare the skills extension.
    let caps = std::env::var("MCP_STUB_CAPS").unwrap_or_else(|_| "tools".to_string());
    // ADR-0108 §2: a listing's freshness hint, and a catalogue that moves after
    // the first page, so a test can watch a TTL-driven refresh land.
    let skill_ttl: Option<u64> = std::env::var("MCP_STUB_SKILL_TTL_MS")
        .ok()
        .and_then(|value| value.parse().ok());
    let rotate = std::env::var("MCP_STUB_SKILL_ROTATE").is_ok_and(|value| value == "1");
    // ADR-0108 §4: a listing may be partial. This mode serves no listed skills
    // but still answers `skills/get`, the shape a URI-only skill has.
    let list_empty = std::env::var("MCP_STUB_SKILL_LIST_EMPTY").is_ok_and(|value| value == "1");
    let mut skill_listings = 0u32;
    let has = |capability: &str| caps.split(',').any(|name| name.trim() == capability);
    let mut capabilities = json!({"tools": {}});
    if has("skills") || has("directory-read") {
        let mut skills = json!({});
        if has("directory-read") {
            skills["directoryRead"] = json!(true);
        }
        capabilities["extensions"] = json!({"io.modelcontextprotocol/skills": skills});
    }
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
                    "capabilities": capabilities,
                    "serverInfo": {"name": "mcp-probe-stub", "version": "0.0.1"}
                }),
            ),
            "notifications/initialized" => {}
            // ADR-0108: one skill, served with real digests so a test can drive
            // the whole path — list, index, fetch, verify.
            "skills/get" => {
                let uri = message
                    .get("params")
                    .and_then(|params| params.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match uri {
                    SKILL_URI => respond(&mut stdout, id, json!({"skill": probe_skill_entry()})),
                    LATE_URI => respond(&mut stdout, id, json!({"skill": late_skill_entry()})),
                    other => {
                        respond_error(&mut stdout, id, -32602, format!("no such skill: {other}"))
                    }
                }
            }
            "skills/list" => {
                skill_listings += 1;
                let mut skills = if list_empty {
                    Vec::new()
                } else {
                    vec![probe_skill_entry()]
                };
                if rotate && skill_listings > 1 {
                    skills.push(late_skill_entry());
                }
                let mut result = json!({"skills": skills});
                if let Some(ttl) = skill_ttl {
                    result["ttlMs"] = json!(ttl);
                }
                respond(&mut stdout, id, result)
            }
            "resources/read" => {
                let uri = message
                    .get("params")
                    .and_then(|params| params.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match uri {
                    SKILL_URI => respond(
                        &mut stdout,
                        id,
                        json!({"contents": [{"uri": SKILL_URI, "text": SKILL_MD}]}),
                    ),
                    LATE_URI => respond(
                        &mut stdout,
                        id,
                        json!({"contents": [{"uri": LATE_URI, "text": LATE_MD}]}),
                    ),
                    GUIDE_URI => respond(
                        &mut stdout,
                        id,
                        json!({"contents": [{"uri": GUIDE_URI, "text": GUIDE}]}),
                    ),
                    other => respond_error(
                        &mut stdout,
                        id,
                        -32602,
                        format!("no such resource: {other}"),
                    ),
                }
            }
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

fn respond_error(stdout: &mut impl Write, id: Option<Value>, code: i64, message: String) {
    let Some(id) = id else { return };
    let _ = writeln!(
        stdout,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
    );
    let _ = stdout.flush();
}

fn probe_skill_entry() -> serde_json::Value {
    json!({
        "uri": SKILL_URI,
        "frontmatter": {
            "name": "probe-skill",
            "description": "A skill the probe stub serves.",
            "license": "MIT"
        },
        "resources": [
            {"uri": SKILL_URI, "digest": SKILL_DIGEST, "size": SKILL_MD.len()},
            {"uri": GUIDE_URI, "digest": GUIDE_DIGEST, "size": GUIDE.len()}
        ]
    })
}

fn late_skill_entry() -> serde_json::Value {
    json!({
        "uri": LATE_URI,
        "frontmatter": {
            "name": "late-skill",
            "description": "A skill that appeared after the first listing."
        },
        "resources": [
            {"uri": LATE_URI, "digest": LATE_DIGEST, "size": LATE_MD.len()}
        ]
    })
}

const SKILL_URI: &str = "skill://probe-skill/SKILL.md";
const LATE_URI: &str = "skill://late-skill/SKILL.md";
const LATE_MD: &str = "---\nname: late-skill\ndescription: A skill that appeared after the first listing.\n---\n\nLate body.\n";
const LATE_DIGEST: &str = "sha256:f517c4407b729bced4793744a4d1d02088b00aa8fcf2c7e62418be241b72426c";
const GUIDE_URI: &str = "skill://probe-skill/references/GUIDE.md";
// `license` is here so a test can see that a frontmatter field other than
// `name`/`description` survives both the wire and the full-field comparison.
const SKILL_MD: &str = "---\nname: probe-skill\ndescription: A skill the probe stub serves.\nlicense: MIT\n---\n\nProbe body.\n";
const GUIDE: &str = "guide body\n";

// The digests of the two constants above, computed once so this test binary
// needs no hashing dependency: if either string changes, these must be updated
// and the test will say so (the digest check in `qcode-tools` refuses the body).
const SKILL_DIGEST: &str =
    "sha256:d716973bf301ef0dda297ff7dbd2d1586fba16e9220789a0aa78a433fc5f5cfd";
const GUIDE_DIGEST: &str =
    "sha256:0d1e5a71e81ff1119e35713c5294c37a63090c03b6e42ecb8a491558e9b23a6a";
