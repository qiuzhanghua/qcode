//! ADR-0067 §2, end to end: a remote MCP server's traffic goes through the
//! session's egress proxy, so it is allowlisted and written to
//! `network.jsonl` like every other outbound request.
//!
//! The test does not assert "the request worked". It asserts the request is
//! **in the audit log**, because that is the difference between honouring the
//! egress policy and quietly bypassing it: with a default `reqwest::Client`
//! the tools would still arrive, and the only symptom would be a shorter audit
//! file.
//!
//! `allow_private` is on deliberately. Without it the proxy rejects loopback
//! as a private address before the allowlist is even consulted, so the test
//! could pass without ever exercising the audit path it claims to check.

use std::path::Path;

use qcode_core::network::SharedNetwork;
use qcode_kernel::ToolWiring;
use qcode_mcp::McpConfig;
use qcode_net::{Egress, EgressOptions};
use qcode_tools::sandbox::{SandboxMode, SandboxNetwork};

/// Start the Streamable HTTP probe server and return it plus its port.
///
/// `std::process` rather than a tokio task: the probe is a separate binary
/// (a real server, not a mock in-process one), which is the point — it means
/// the client's HTTP goes out over a socket.
struct Probe {
    child: std::process::Child,
    port: u16,
}

impl Probe {
    fn start() -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_mcp-http-probe"))
            .arg("--modern")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn probe");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read port");
        let port = line
            .trim()
            .strip_prefix("listening ")
            .expect("port line")
            .parse()
            .expect("port");
        Self { child, port }
    }

    fn host_port(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn url(&self) -> String {
        format!("http://{}/mcp", self.host_port())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::too_many_arguments)]
async fn tools_for(
    config_path: &Path,
    workspace: &Path,
    data_dir: &Path,
    network: Option<SharedNetwork>,
) -> Vec<String> {
    let loaded =
        qcode_kernel::load_config(Some(config_path.to_path_buf()), workspace).expect("load config");
    let sandbox =
        qcode_tools::sandbox::build(
            SandboxMode::None,
            workspace,
            SandboxNetwork::Deny,
            qcode_tools::sandbox::SandboxPaths::default(),
        )
            .expect("sandbox");
    let skills = std::sync::Arc::new(qcode_tools::skills::SkillSet::default());
    let shell_env = std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::new(
        loaded.shell.clone(),
    ));
    let tools = qcode_kernel::build_tools(
        &loaded.mcp,
        &loaded.search,
        workspace,
        data_dir,
        &skills,
        sandbox,
        ToolWiring {
            network_env: network.as_ref().map(|network| network.env()),
            network,
            shell_env,
        },
    )
    .await;
    // `specs()` is what the model sees, so the assertion is on the same list
    // the registry actually hands out. Only the MCP ones matter here: the
    // built-ins are registered no matter what the transport does.
    tools
        .specs()
        .iter()
        .map(|spec| spec.name.clone())
        .filter(|name| name.starts_with("mcp__"))
        .collect::<Vec<_>>()
}

/// The audit file's lines as JSON, so assertions can talk about fields instead
/// of substrings.
fn records(audit: &str) -> Vec<serde_json::Value> {
    audit
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn write_config(dir: &Path, url: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            r#"
            [network]
            enabled = true
            allow_private = true
            unknown = "deny"
            audit = true

            [[mcp.servers]]
            name = "probe"
            url = "{url}"
            "#
        ),
    )
    .expect("write config");
    path
}

#[tokio::test]
async fn a_remote_server_is_reached_through_the_egress_proxy_and_audited() {
    let probe = Probe::start();
    let dir = tempfile::tempdir().expect("dir");
    let workspace = dir.path().join("ws");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&workspace).expect("ws");
    std::fs::create_dir_all(&data).expect("data");
    let config = write_config(dir.path(), &probe.url());

    // Exactly what `assemble` does: the proxy exists once per session and is
    // both the child-process environment and the client handed to MCP.
    let egress = Egress::start(EgressOptions {
        allow: vec![probe.host_port()],
        allow_private: true,
        audit_path: Some(data.join("logs").join("network.jsonl")),
        ..EgressOptions::default()
    })
    .await
    .expect("egress");
    std::fs::create_dir_all(data.join("logs")).expect("logs");
    let network: SharedNetwork = std::sync::Arc::new(egress);

    let names = tools_for(&config, &workspace, &data, Some(network)).await;
    assert_eq!(
        names,
        vec!["mcp__probe__ping".to_string()],
        "the remote server produced no tools"
    );

    let audit = std::fs::read_to_string(data.join("logs").join("network.jsonl"))
        .expect("no audit log: the request did not go through the proxy");
    // One JSON line per proxied connection. The strongest statement available
    // is "the proxy saw this exact host and port": with a default reqwest
    // client the request would still succeed, and this line would be absent.
    let endpoint = records(&audit)
        .into_iter()
        .find(|record| record["host"] == "127.0.0.1" && record["port"] == probe.port as i64)
        .unwrap_or_else(|| panic!("the audit log does not mention the endpoint:\n{audit}"));
    assert_eq!(endpoint["result"], "allowed", "{endpoint}");
    assert_eq!(endpoint["reason"], "http", "{endpoint}");
}

#[tokio::test]
async fn without_an_egress_proxy_the_remote_server_is_not_contacted() {
    // `[network] enabled = false` means there is no `Network`, which means no
    // client, which means "do not connect" rather than "connect directly"
    // (ADR-0067 §2). The tool list has to be empty: a tool here would prove a
    // bypass.
    let probe = Probe::start();
    let dir = tempfile::tempdir().expect("dir");
    let workspace = dir.path().join("ws");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&workspace).expect("ws");
    std::fs::create_dir_all(&data).expect("data");
    let config = write_config(dir.path(), &probe.url());

    let names = tools_for(&config, &workspace, &data, None).await;
    assert!(
        names.is_empty(),
        "a remote server connected without the egress proxy: {names:?}"
    );
    // Nothing was sent, so there is no audit file — and, more to the point, no
    // request. The probe server is still up, so an empty list really means
    // "did not connect", not "connected and got nothing".
    assert!(
        !data.join("logs").join("network.jsonl").exists(),
        "nothing should have been sent"
    );
}

/// A config the user is likely to write first: the URL is not in
/// `[network] allow`, so the proxy answers 403 and the tools never arrive.
/// This is ADR-0067 后果's "首次配一个远程 server 要手写 `[network] allow`",
/// kept as a test so it stays true.
#[tokio::test]
async fn a_host_outside_the_allowlist_is_refused() {
    let probe = Probe::start();
    let dir = tempfile::tempdir().expect("dir");
    let workspace = dir.path().join("ws");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&workspace).expect("ws");
    std::fs::create_dir_all(data.join("logs")).expect("logs");
    let config = write_config(dir.path(), &probe.url());

    let egress = Egress::start(EgressOptions {
        // Deliberately not the probe's host:port.
        allow: vec!["example.com".to_string()],
        allow_private: true,
        audit_path: Some(data.join("logs").join("network.jsonl")),
        ..EgressOptions::default()
    })
    .await
    .expect("egress");
    let network: SharedNetwork = std::sync::Arc::new(egress);

    let names = tools_for(&config, &workspace, &data, Some(network)).await;
    assert!(
        names.is_empty(),
        "the allowlist was not enforced: {names:?}"
    );
    let audit =
        std::fs::read_to_string(data.join("logs").join("network.jsonl")).expect("no audit log");
    let denied = records(&audit)
        .into_iter()
        .find(|record| record["port"] == probe.port as i64)
        .unwrap_or_else(|| panic!("the attempt was not logged at all:\n{audit}"));
    assert_eq!(denied["result"], "denied", "{denied}");
}

/// The server-side probe and this test must agree on what `[mcp]` config means;
/// `McpConfig::validate` is what rejects a URL with no scheme, so make sure a
/// minimal remote config is accepted at all.
#[test]
fn the_remote_config_parses_and_validates() {
    let parsed: McpConfig = toml::from_str(
        r#"
        [[servers]]
        name = "probe"
        url = "https://mcp.example.test/mcp"
        "#,
    )
    .expect("parse");
    assert_eq!(parsed.validate(), Ok(()));
    assert_eq!(
        parsed.servers[0].transport(),
        Some(qcode_mcp::Transport::StreamableHttp)
    );
    // The old shape, unchanged.
    let legacy: McpConfig = toml::from_str(
        r#"
        [[servers]]
        name = "local"
        command = "npx"
        args = ["-y", "srv"]
        "#,
    )
    .expect("parse");
    assert_eq!(legacy.validate(), Ok(()));
    assert_eq!(
        legacy.servers[0].transport(),
        Some(qcode_mcp::Transport::Stdio)
    );
}
