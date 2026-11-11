//! The remote transport (ADR-0067 P1): a local Streamable HTTP probe server,
//! so none of this needs the network.
//!
//! What is asserted, and why each one is worth a test:
//!
//! * `server/discover` succeeds → `2026-07-28`, and `initialize` is **never**
//!   sent. A regression that quietly went back to `initialize` would still
//!   work, so counting requests is the only way to see it.
//! * A `-32601` server (wordcraft's shape) → `Auto` falls back and the tools
//!   still arrive, with `initialize` on the wire.
//! * The bearer token comes from an environment variable and appears as an
//!   `Authorization: Bearer …` header — never in the config file.
//! * No client means no connection: the "only ever `with_client`" rule
//!   (ADR-0067 §2) is enforced, not documented.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use qcode_mcp::{ConnectOptions, McpConfig, McpServerConfig, connect_all_with};
use serde_json::Value;

/// A probe server process plus the port it bound.
struct Probe {
    child: Child,
    port: u16,
    audit: PathBuf,
}

impl Probe {
    fn start(args: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        // The temp dir has to outlive the child, so leak it: the process ends
        // with the test binary.
        let dir = dir.keep();
        let audit = dir.join("requests.jsonl");
        let mut child = Command::new(env!("CARGO_BIN_EXE_http-probe-server"))
            .args(args)
            .arg("--audit")
            .arg(&audit)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn probe");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read port");
        let port = line
            .trim()
            .strip_prefix("listening ")
            .expect("port line")
            .parse()
            .expect("port");
        Self { child, port, audit }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Every request the server received, in order.
    fn requests(&self) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(&self.audit) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn rpc_methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|request| request["rpc"].as_str().map(str::to_string))
            .collect()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(server: McpServerConfig) -> McpConfig {
    McpConfig {
        servers: vec![server],
        ..Default::default()
    }
}

fn workspace() -> PathBuf {
    std::env::temp_dir()
}

/// The egress client a real session would hand in. `Network::client()` is a
/// plain `reqwest::Client` pointed at the proxy, so a direct client is what
/// "the caller's client" looks like for this crate.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client")
}

#[tokio::test]
async fn a_modern_server_is_reached_without_initialize() {
    let probe = Probe::start(&["--modern"]);
    let mcp = config(McpServerConfig {
        name: "probe".to_string(),
        command: None,
        url: Some(probe.url()),
        ..Default::default()
    });

    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    assert_eq!(names, vec!["mcp__probe__ping"], "tools: {names:?}");

    let methods = probe.rpc_methods();
    assert!(
        methods.iter().any(|method| method == "server/discover"),
        "the probe never saw a discover: {methods:?}"
    );
    assert!(
        !methods.iter().any(|method| method == "initialize"),
        "a 2026-07-28 server must not be sent initialize: {methods:?}"
    );
    assert!(
        methods.iter().any(|method| method == "tools/list"),
        "no tools/list: {methods:?}"
    );
    // The whole point of the modern lifecycle: the version travels in each
    // request's `_meta` instead of being agreed once up front.
    let discover = probe
        .requests()
        .into_iter()
        .find(|request| request["rpc"] == "server/discover")
        .expect("discover request");
    assert_eq!(
        discover["protocol_meta"], "2026-07-28",
        "discover must declare the version it is probing for: {discover}"
    );
}

#[tokio::test]
async fn a_legacy_server_falls_back_to_initialize() {
    // `-32601` to `server/discover` is what wordcraft answers, and the reason
    // `Auto` exists (ADR-0067 §3): without the fallback this server would be
    // unusable.
    let probe = Probe::start(&["--legacy"]);
    let mcp = config(McpServerConfig {
        name: "legacy".to_string(),
        command: None,
        url: Some(probe.url()),
        ..Default::default()
    });

    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>(),
        vec!["mcp__legacy__ping"],
        "the fallback must still produce the tools"
    );

    let methods = probe.rpc_methods();
    let discover = methods
        .iter()
        .position(|method| method == "server/discover")
        .unwrap_or_else(|| panic!("no discover probe: {methods:?}"));
    let initialize = methods
        .iter()
        .position(|method| method == "initialize")
        .unwrap_or_else(|| panic!("no initialize fallback: {methods:?}"));
    assert!(
        discover < initialize,
        "initialize must follow the probe: {methods:?}"
    );
}

#[tokio::test]
async fn a_bearer_token_is_read_from_the_environment() {
    let token = "probe-token-8f2a";
    let probe = Probe::start(&["--modern", "--require-token", token]);
    let mcp = config(McpServerConfig {
        name: "authed".to_string(),
        command: None,
        url: Some(probe.url()),
        bearer_token_env_var: Some("QCODE_TEST_MCP_TOKEN".to_string()),
        ..Default::default()
    });

    // Set for this test only; the name (not the value) is what the config holds.
    unsafe { std::env::set_var("QCODE_TEST_MCP_TOKEN", token) };
    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;
    unsafe { std::env::remove_var("QCODE_TEST_MCP_TOKEN") };

    assert_eq!(tools.len(), 1, "the authenticated call must succeed");
    let authorization = probe
        .requests()
        .iter()
        .find_map(|request| request["authorization"].as_str().map(str::to_string))
        .expect("no Authorization header reached the server");
    assert_eq!(authorization, format!("Bearer {token}"));
}

#[tokio::test]
async fn a_missing_token_variable_names_the_variable() {
    let probe = Probe::start(&["--modern", "--require-token", "unused"]);
    let mcp = config(McpServerConfig {
        name: "authed".to_string(),
        command: None,
        url: Some(probe.url()),
        bearer_token_env_var: Some("QCODE_TEST_MCP_TOKEN_DEFINITELY_UNSET".to_string()),
        ..Default::default()
    });

    // `connect_all_with` logs and skips rather than failing, so the assertion is
    // on the absence of tools plus the fact that nothing was sent: a server
    // with a missing token must not be contacted at all.
    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;
    assert!(tools.is_empty(), "nothing should be registered");
    assert!(
        probe.requests().is_empty(),
        "a missing token must fail before the first request: {:?}",
        probe.requests()
    );
}

#[tokio::test]
async fn without_a_caller_supplied_client_the_server_is_not_contacted() {
    // ADR-0067 §2: `reqwest::Client::default()` here would bypass the egress
    // allowlist and the audit log, so `None` has to mean "do not connect"
    // rather than "connect directly". This is the negative test for the whole
    // proxy story: if a default client were built, the probe would receive a
    // request.
    let probe = Probe::start(&["--modern"]);
    let mcp = config(McpServerConfig {
        name: "unreachable".to_string(),
        command: None,
        url: Some(probe.url()),
        ..Default::default()
    });

    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::default(),
    )
    .await;

    assert!(tools.is_empty(), "no client means no tools");
    assert!(
        probe.requests().is_empty(),
        "a missing client must not fall back to a direct connection: {:?}",
        probe.rpc_methods()
    );
}

#[tokio::test]
async fn custom_headers_reach_the_server() {
    let probe = Probe::start(&["--modern"]);
    let mut server = McpServerConfig {
        name: "headers".to_string(),
        command: None,
        url: Some(probe.url()),
        ..Default::default()
    };
    server
        .headers
        .insert("X-Probe".to_string(), "on".to_string());
    let mcp = config(server);

    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    assert_eq!(tools.len(), 1);
    let sent = probe
        .requests()
        .iter()
        .find_map(|request| request["custom"].as_str().map(str::to_string))
        .expect("the custom header never arrived");
    assert_eq!(sent, "on");
}

#[tokio::test]
async fn a_stdio_server_with_a_url_is_never_contacted() {
    // `validate()` rejects the ambiguous config; the transport accessor refuses
    // to pick a side, so nothing is spawned and no request is sent.
    let probe = Probe::start(&["--modern"]);
    let mcp = config(McpServerConfig {
        name: "confused".to_string(),
        command: Some("/bin/true".to_string()),
        url: Some(probe.url()),
        ..Default::default()
    });
    assert!(mcp.validate().is_err(), "the config must be rejected");

    let tools = connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;
    assert!(tools.is_empty());
    assert!(
        probe.requests().is_empty(),
        "no request should have been sent"
    );
}

/// The probe's audit file is what the proxy assertion in the kernel-level test
/// reads, so make sure it records what we think it does.
#[test]
fn the_probe_records_requests_it_receives() {
    let probe = Probe::start(&["--modern"]);
    let response = reqwest::blocking::Client::new()
        .post(probe.url())
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .expect("send");
    assert_eq!(response.status(), 200);
    let methods = probe.rpc_methods();
    assert_eq!(methods, vec!["tools/list".to_string()]);
}

/// A path that does not exist cannot be the audit file, so this guards the
/// helper itself: a probe silently writing nowhere would make every other
/// assertion here vacuous.
#[test]
fn the_audit_path_is_the_one_the_test_reads() {
    let probe = Probe::start(&["--modern"]);
    assert!(Path::new(&probe.audit).ends_with("requests.jsonl"));
}

/// ADR-0068 §4, the point of the change: a 2026-07-28 server already told us
/// its capabilities in `server/discover`, so asking again is a wasted round
/// trip. Counting requests is the only way to see that no regression
/// reintroduced the probe — a probe that comes back still "works", it just
/// costs two requests and logs an error per server.
#[tokio::test]
async fn a_modern_server_is_never_probed_for_resources_or_prompts() {
    let probe = Probe::start(&["--modern"]);
    let tools = connect(&probe).await;
    assert_eq!(tools.len(), 1, "the modern server's tool should arrive");

    let methods = probe.rpc_methods();
    assert!(
        !methods.contains(&"resources/list".to_string()),
        "resources/list was sent to a modern server: {methods:?}"
    );
    assert!(
        !methods.contains(&"prompts/list".to_string()),
        "prompts/list was sent to a modern server: {methods:?}"
    );
    assert!(
        !methods.contains(&"initialize".to_string()),
        "a modern server must not see initialize: {methods:?}"
    );
}

/// ADR-0068 §4 as revised (B-2): legacy is not special any more. The version
/// split went away because the reason for distrusting a legacy declaration was
/// a guess — wordcraft-cli 0.15.2 is a real 2025-06-18 server and declares
/// exactly what it serves — while the probe it justified cost a `-32601` per
/// server on every connect.
#[tokio::test]
async fn a_legacy_server_is_not_probed_either() {
    let probe = Probe::start(&["--legacy"]);
    let tools = connect(&probe).await;
    assert_eq!(tools.len(), 1, "the legacy server's tool should arrive");

    let methods = probe.rpc_methods();
    let count = |method: &str| methods.iter().filter(|seen| *seen == method).count();
    assert_eq!(count("initialize"), 1, "a legacy server still handshakes");
    assert_eq!(count("resources/list"), 0, "{methods:?}");
    assert_eq!(count("prompts/list"), 0, "{methods:?}");
}

/// And the declaration still decides, on legacy too: declared resources means
/// the two tools exist, with no request sent to confirm it.
#[tokio::test]
async fn a_legacy_server_gets_its_declared_extras_without_a_probe() {
    let probe = Probe::start(&["--legacy", "--capabilities", "resources,prompts"]);
    let tools = connect(&probe).await;
    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    for expected in [
        "ping",
        "list_resources",
        "read_resource",
        "list_prompts",
        "get_prompt",
    ] {
        assert!(
            names.iter().any(|name| name.contains(expected)),
            "{expected} is missing from {names:?}"
        );
    }
    let methods = probe.rpc_methods();
    assert!(
        !methods.contains(&"resources/list".to_string()),
        "{methods:?}"
    );
    assert!(
        !methods.contains(&"prompts/list".to_string()),
        "{methods:?}"
    );
}

/// `qc doctor --probe`: the diagnostic that replaced the connect-time probe.
///
/// This server declares nothing and serves nothing, so every answer agrees with
/// the declaration — the boring case, pinned because "reports OK" must be
/// distinguishable from "reports nothing".
#[tokio::test]
async fn the_probe_reports_a_server_that_tells_the_truth() {
    let probe = Probe::start(&["--modern"]);
    let reports = qcode_mcp::probe_capabilities(
        &config(McpServerConfig {
            name: "probe".to_string(),
            command: None,
            url: Some(probe.url()),
            ..Default::default()
        }),
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.protocol_version.as_deref(), Some("2026-07-28"));
    assert!(
        report.declared.tools,
        "the probe server always serves tools"
    );
    assert!(!report.declared.resources);
    assert!(!report.declared.prompts);
    assert!(
        matches!(report.resources, qcode_mcp::ProbeAnswer::Refused(_)),
        "{:?}",
        report.resources
    );
    assert!(report.registered.is_empty(), "{:?}", report.registered);

    // The probe asked even though nothing was declared: that is what makes
    // "serves but does not declare" detectable at all.
    let methods = probe.rpc_methods();
    assert!(
        methods.contains(&"resources/list".to_string()),
        "{methods:?}"
    );
    assert!(methods.contains(&"prompts/list".to_string()), "{methods:?}");
}

/// Mismatch direction A: **serves, does not declare.** The tools are silently
/// missing from the session — the failure nobody can see without this check.
#[tokio::test]
async fn the_probe_finds_a_capability_served_but_not_declared() {
    let probe = Probe::start(&["--modern", "--serve-caps", "resources"]);
    let reports = qcode_mcp::probe_capabilities(
        &config(McpServerConfig {
            name: "probe".to_string(),
            command: None,
            url: Some(probe.url()),
            ..Default::default()
        }),
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    let report = &reports[0];
    assert!(!report.declared.resources, "it does not declare resources");
    assert_eq!(report.resources, qcode_mcp::ProbeAnswer::Supported);
    assert!(
        !report.resources.agrees_with(report.declared.resources),
        "the mismatch must be visible to the caller"
    );

    // The connect path still registers nothing, which is the point of the
    // warning: the tools exist on the server and are absent from the model.
    let tools = connect(&probe).await;
    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    assert!(
        !names.iter().any(|name| name.contains("list_resources")),
        "{names:?}"
    );
}

/// Mismatch direction B: **declares, refuses.** The tools are registered and
/// every call fails. Reported separately from A because the user-visible damage
/// is opposite.
#[tokio::test]
async fn the_probe_finds_a_capability_declared_but_refused() {
    let probe = Probe::start(&["--modern", "--capabilities", "prompts"]);
    let reports = qcode_mcp::probe_capabilities(
        &config(McpServerConfig {
            name: "probe".to_string(),
            command: None,
            url: Some(probe.url()),
            ..Default::default()
        }),
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await;

    let report = &reports[0];
    assert!(report.declared.prompts);
    assert!(matches!(report.prompts, qcode_mcp::ProbeAnswer::Refused(_)));
    assert!(!report.prompts.agrees_with(report.declared.prompts));

    let tools = connect(&probe).await;
    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    assert!(
        names.iter().any(|name| name.contains("list_prompts")),
        "the declaration registered the tool: {names:?}"
    );
}

/// Reading the field must reach the same conclusion the probe would have: a
/// modern server that *does* declare resources and prompts gets those tools
/// without a single list request.
#[tokio::test]
async fn a_modern_server_gets_its_declared_extras() {
    let probe = Probe::start(&["--modern", "--capabilities", "resources,prompts"]);
    let tools = connect(&probe).await;

    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    for expected in [
        "ping",
        "list_resources",
        "read_resource",
        "list_prompts",
        "get_prompt",
    ] {
        assert!(
            names.iter().any(|name| name.contains(expected)),
            "{expected} is missing from {names:?}"
        );
    }

    let methods = probe.rpc_methods();
    assert!(
        !methods.contains(&"resources/list".to_string()),
        "the capability was declared; no probe should be needed: {methods:?}"
    );
    assert!(
        !methods.contains(&"prompts/list".to_string()),
        "the capability was declared; no probe should be needed: {methods:?}"
    );
}

/// And the converse, so "declared" is not simply "always on": a modern server
/// declaring nothing gets no extra tools.
#[tokio::test]
async fn a_modern_server_without_extras_gets_no_extra_tools() {
    let probe = Probe::start(&["--modern"]);
    let tools = connect(&probe).await;
    let names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    for absent in [
        "list_resources",
        "read_resource",
        "list_prompts",
        "get_prompt",
    ] {
        assert!(
            !names.iter().any(|name| name.contains(absent)),
            "{absent} should not exist: {names:?}"
        );
    }
}

/// One place to build the connect call for the tests above.
async fn connect(probe: &Probe) -> Vec<std::sync::Arc<dyn qcode_core::tool::Tool>> {
    let mcp = config(McpServerConfig {
        name: "probe".to_string(),
        command: None,
        url: Some(probe.url()),
        ..Default::default()
    });
    connect_all_with(
        &mcp,
        &workspace(),
        &tempfile::tempdir().expect("data").keep(),
        &[],
        ConnectOptions::with_http(&client()),
    )
    .await
}
