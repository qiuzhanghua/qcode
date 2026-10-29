//! `qc acp` over real stdio: spawn the binary, speak JSON-RPC, read what an
//! editor would see.
//!
//! These tests are the seam between the crate tests (which use a fake client)
//! and a real editor: they prove the stdio transport, the argument parsing and
//! the engine wiring work in one process.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl Session {
    async fn start(provider: &str) -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            // The sandbox is pinned off: this suite speaks the ACP protocol
            // and must not depend on the host being able to apply a kernel
            // sandbox (a nested container cannot).
            format!(
                "version = 1\ndefault_provider = \"{provider}\"\n\n[sandbox]\nmode = \"none\"\n"
            ),
        )
        .expect("write config");
        let mut child = Command::new(env!("CARGO_BIN_EXE_qc"))
            .arg("acp")
            .arg("--config")
            .arg(&config)
            .arg("--data-dir")
            .arg(dir.path().join("data"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn qc acp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let session = Session {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        };
        (session, dir)
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).expect("json");
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.expect("write");
        self.stdin.flush().await.expect("flush");
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
            .await
            .expect("a line within the timeout")
            .expect("stdio read")
            .expect("a line before EOF");
        serde_json::from_str(&line).expect("json-rpc line")
    }

    /// Read until the response with `id`, collecting the notifications seen.
    async fn response(&mut self, id: i64, seen: &mut Vec<Value>) -> Value {
        loop {
            let message = self.next().await;
            if message.get("method").is_some() {
                seen.push(message);
                continue;
            }
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> (Value, Vec<Value>) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let mut seen = Vec::new();
        let response = self.response(id, &mut seen).await;
        (response, seen)
    }

    async fn close(mut self) {
        drop(self.stdin);
        let status = tokio::time::timeout(TIMEOUT, self.child.wait())
            .await
            .expect("the server exits when stdin closes")
            .expect("wait");
        assert!(status.success(), "server exited with {status}");
    }
}

fn text_update(update: &Value, kind: &str) -> Option<String> {
    (update["sessionUpdate"] == kind).then(|| {
        update["content"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    })
}

#[tokio::test]
async fn initialize_a_session_and_a_prompt_over_stdio() {
    let (mut session, dir) = Session::start("mock").await;

    let (response, _) = session
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentInfo"]["name"], "qcode");
    assert_eq!(result["agentCapabilities"]["loadSession"], true);

    let (response, _) = session
        .request(
            2,
            "session/new",
            json!({"cwd": dir.path().canonicalize().expect("cwd"), "mcpServers": []}),
        )
        .await;
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let (response, seen) = session
        .request(
            3,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "hello"}]
            }),
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    let chunks: Vec<String> = seen
        .iter()
        .filter_map(|message| {
            (message["method"] == "session/update")
                .then(|| text_update(&message["params"]["update"], "agent_message_chunk"))
                .flatten()
        })
        .collect();
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.contains("Hello from the mock provider")),
        "{chunks:?}"
    );

    session.close().await;
}

#[tokio::test]
async fn a_tool_call_asks_permission_over_stdio_and_a_reject_stops_it() {
    let (mut session, dir) = Session::start("mock-approval").await;

    session
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let (response, _) = session
        .request(
            2,
            "session/new",
            json!({"cwd": dir.path().canonicalize().expect("cwd"), "mcpServers": []}),
        )
        .await;
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    session
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "run the demo"}]
            }
        }))
        .await;

    // The server asks first; answer with a rejection, as a user would.
    let mut asked = None;
    let mut seen = Vec::new();
    let prompt_response = loop {
        let message = session.next().await;
        if message["method"] == "session/request_permission" {
            asked = Some(message.clone());
            let id = message["id"].clone();
            let options = message["params"]["options"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(
                options
                    .iter()
                    .any(|option| option["optionId"] == "reject-once"),
                "{message}"
            );
            session
                .send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}}
                }))
                .await;
            continue;
        }
        if message.get("method").is_some() {
            seen.push(message);
            continue;
        }
        if message.get("id").and_then(Value::as_i64) == Some(3) {
            break message;
        }
    };

    assert!(asked.is_some(), "the tool was never put up for approval");
    let permission = asked.expect("permission request");
    assert_eq!(permission["params"]["toolCall"]["toolCallId"], "approval-1");
    assert_eq!(prompt_response["result"]["stopReason"], "end_turn");

    // The rejected tool must be reported as not completed.
    let statuses: Vec<String> = seen
        .iter()
        .filter(|message| message["method"] == "session/update")
        .filter(|message| message["params"]["update"]["sessionUpdate"] == "tool_call_update")
        .map(|message| {
            message["params"]["update"]["status"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(
        statuses.iter().any(|status| status != "completed"),
        "{statuses:?}"
    );

    session.close().await;
}
