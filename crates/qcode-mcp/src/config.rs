//! `[[mcp.servers]]` configuration.
//!
//! The type lives here (not in the CLI) so the client and the config loader
//! share one definition of a server. Validation is a pure function: it runs
//! when the config is loaded, before any process is spawned.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub use crate::grant::CredentialPreset;

fn default_true() -> bool {
    true
}

fn default_timeout_secs() -> u64 {
    120
}

/// Whether an MCP server runs under the OS sandbox (ADR-0066).
///
/// `confine` derives a grant per server: the runtime directory is readable and
/// executable, writes go to the workspace plus a synthetic HOME. `off` starts
/// the server unconfined and is always reported by `qc doctor`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpSandboxMode {
    #[default]
    Confine,
    Off,
}

impl McpSandboxMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Confine => "confine",
            Self::Off => "off",
        }
    }

    pub fn is_confined(self) -> bool {
        matches!(self, Self::Confine)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// Stdio transport: the program to start. Mutually exclusive with `url`.
    ///
    /// `Option` rather than `String` because a remote server has no program to
    /// start (ADR-0067 §1). Configs written before that ADR only set
    /// `command`, so they keep parsing unchanged.
    #[serde(default)]
    pub command: Option<String>,
    /// Streamable HTTP transport: the endpoint. Mutually exclusive with
    /// `command`; `validate()` requires exactly one of the two.
    #[serde(default)]
    pub url: Option<String>,
    /// Name of the environment variable holding the bearer token — never the
    /// token itself (ADR-0067 §6, ADR-0056).
    #[serde(default)]
    pub bearer_token_env_var: Option<String>,
    /// Extra request headers for the HTTP transport. Sensitive names are
    /// rejected by `validate()`: those go in `bearer_token_env_var`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the server process, on top of the whitelist (ADR-0068).
    /// A value of exactly `${NAME}` is read from qcode's environment, so a
    /// token can be passed through without being written to the config file.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-request timeout for the handshake, `tools/list` and `tools/call`.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Read + execute roots on top of the derived ones (ADR-0066 §2).
    #[serde(default)]
    pub read_paths: Vec<PathBuf>,
    /// Writable roots on top of the workspace and the synthetic HOME.
    #[serde(default)]
    pub writable_paths: Vec<PathBuf>,
    /// Credential subtrees under the real `$HOME` this server may read.
    #[serde(default)]
    pub credentials: Vec<CredentialPreset>,
    /// Overrides `[mcp] sandbox` for this one server.
    #[serde(default)]
    pub sandbox: Option<McpSandboxMode>,
}

/// How a server is reached (ADR-0067 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A child process over the standard MCP framing.
    Stdio,
    /// The Streamable HTTP endpoint named by `url`.
    StreamableHttp,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable-http",
        }
    }

    pub fn is_remote(self) -> bool {
        matches!(self, Self::StreamableHttp)
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl McpServerConfig {
    /// Which transport this server uses, or `None` when the config is
    /// ambiguous (`command` and `url` both set, or neither).
    ///
    /// `validate()` rejects both of those before anything is started, so
    /// `None` here means the caller skipped validation. Returning it instead of
    /// picking a side keeps that impossible state from silently becoming a
    /// stdio server with a URL-shaped config.
    pub fn transport(&self) -> Option<Transport> {
        match (self.command.is_some(), self.url.is_some()) {
            (true, false) => Some(Transport::Stdio),
            (false, true) => Some(Transport::StreamableHttp),
            _ => None,
        }
    }

    pub fn sandbox_mode(&self, default: McpSandboxMode) -> McpSandboxMode {
        self.sandbox.unwrap_or(default)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    /// Public tool name: `mcp__<server>__<tool>`. The prefix keeps MCP tools
    /// from colliding with built-ins or with each other.
    pub fn tool_name(&self, tool: &str) -> String {
        format!("mcp__{}__{}", self.name, tool)
    }

    /// Per-server state directory under `<data_dir>/mcp`.
    pub fn state_dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("mcp").join(&self.name)
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: None,
            url: None,
            bearer_token_env_var: None,
            headers: BTreeMap::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            timeout_secs: default_timeout_secs(),
            read_paths: Vec::new(),
            writable_paths: Vec::new(),
            credentials: Vec::new(),
            sandbox: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// Default sandbox for every server; a per-server override wins.
    #[serde(default)]
    pub sandbox: McpSandboxMode,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpConfigError {
    #[error("MCP server name must not be empty")]
    EmptyName,
    #[error("MCP server name '{0}' may only contain ASCII letters, digits, '-' and '_'")]
    InvalidName(String),
    #[error("duplicate MCP server name '{0}'")]
    DuplicateName(String),
    #[error("MCP server '{0}' has an empty command")]
    EmptyCommand(String),
    #[error(
        "MCP server '{0}' sets neither `command` (stdio) nor `url` (streamable-http); \
         one of the two is required"
    )]
    NoTransport(String),
    #[error(
        "MCP server '{0}' sets both `command` and `url`; they are mutually exclusive — \
         drop one, or use two servers if you want both"
    )]
    AmbiguousTransport(String),
    #[error("MCP server '{0}' has an empty url")]
    EmptyUrl(String),
    #[error("MCP server '{server}' url {url:?} is not an absolute http(s) URL: {reason}")]
    InvalidUrl {
        server: String,
        url: String,
        reason: String,
    },
    #[error(
        "MCP server '{server}' sets `{field}`, which only applies to the {owner} transport; \
         remove it"
    )]
    WrongTransportField {
        server: String,
        field: &'static str,
        owner: &'static str,
    },
    #[error(
        "MCP server '{server}' header {name:?} looks like a credential; put the token in \
         `bearer_token_env_var` instead (the config file is not a secret store)"
    )]
    SensitiveHeader { server: String, name: String },
    #[error("MCP server '{server}' header {name:?} contains a line break")]
    HeaderInjection { server: String, name: String },
    #[error("MCP server '{0}' must set timeout_secs to at least 1")]
    ZeroTimeout(String),
    #[error("MCP server '{server}' {field} entry {path} must be an absolute path")]
    RelativePath {
        server: String,
        field: &'static str,
        path: String,
    },
}

impl McpConfig {
    pub fn validate(&self) -> Result<(), McpConfigError> {
        let mut seen = BTreeSet::new();
        for server in &self.servers {
            if server.name.is_empty() {
                return Err(McpConfigError::EmptyName);
            }
            if !server
                .name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            {
                return Err(McpConfigError::InvalidName(server.name.clone()));
            }
            if !seen.insert(server.name.as_str()) {
                return Err(McpConfigError::DuplicateName(server.name.clone()));
            }
            if !server.enabled {
                continue;
            }
            check_transport(server)?;
            if server.timeout_secs == 0 {
                return Err(McpConfigError::ZeroTimeout(server.name.clone()));
            }
            for (field, paths) in [
                ("read_paths", &server.read_paths),
                ("writable_paths", &server.writable_paths),
            ] {
                if let Some(path) = paths.iter().find(|path| !path.is_absolute()) {
                    return Err(McpConfigError::RelativePath {
                        server: server.name.clone(),
                        field,
                        path: path.display().to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Exactly one of `command` / `url`, and then only the fields that transport
/// understands (ADR-0067 §1, §6, §7).
///
/// The field checks are not pedantry: every one of these is a setting that
/// would otherwise be accepted and silently ignored, which is the failure mode
/// ADR-0068 §2 exists to remove.
fn check_transport(server: &McpServerConfig) -> Result<(), McpConfigError> {
    let transport = match server.transport() {
        Some(transport) => transport,
        None if server.command.is_none() && server.url.is_none() => {
            return Err(McpConfigError::NoTransport(server.name.clone()));
        }
        None => return Err(McpConfigError::AmbiguousTransport(server.name.clone())),
    };

    let wrong = |field: &'static str, owner: &'static str| McpConfigError::WrongTransportField {
        server: server.name.clone(),
        field,
        owner,
    };

    match transport {
        Transport::Stdio => {
            let command = server.command.as_deref().unwrap_or_default();
            if command.trim().is_empty() {
                return Err(McpConfigError::EmptyCommand(server.name.clone()));
            }
            if server.bearer_token_env_var.is_some() {
                return Err(wrong("bearer_token_env_var", "streamable-http"));
            }
            if !server.headers.is_empty() {
                return Err(wrong("headers", "streamable-http"));
            }
        }
        Transport::StreamableHttp => {
            let url = server.url.as_deref().unwrap_or_default();
            if url.trim().is_empty() {
                return Err(McpConfigError::EmptyUrl(server.name.clone()));
            }
            match reqwest::Url::parse(url) {
                Ok(parsed) if matches!(parsed.scheme(), "http" | "https") && parsed.has_host() => {}
                Ok(parsed) => {
                    return Err(McpConfigError::InvalidUrl {
                        server: server.name.clone(),
                        url: url.to_string(),
                        reason: format!("scheme is {:?}, need http or https", parsed.scheme()),
                    });
                }
                Err(error) => {
                    return Err(McpConfigError::InvalidUrl {
                        server: server.name.clone(),
                        url: url.to_string(),
                        reason: error.to_string(),
                    });
                }
            }
            // No child process, so every stdio-shaped knob is a no-op. `sandbox`
            // is in the list because ADR-0067 §7 names it: accepting
            // `sandbox = "off"` here would let the user believe they turned
            // something off.
            if !server.args.is_empty() {
                return Err(wrong("args", "stdio"));
            }
            if !server.env.is_empty() {
                return Err(wrong("env", "stdio"));
            }
            if !server.read_paths.is_empty() {
                return Err(wrong("read_paths", "stdio"));
            }
            if !server.writable_paths.is_empty() {
                return Err(wrong("writable_paths", "stdio"));
            }
            if !server.credentials.is_empty() {
                return Err(wrong("credentials", "stdio"));
            }
            if server.sandbox.is_some() {
                return Err(wrong("sandbox", "stdio"));
            }
            for (name, value) in &server.headers {
                if is_credential_header(name) {
                    return Err(McpConfigError::SensitiveHeader {
                        server: server.name.clone(),
                        name: name.clone(),
                    });
                }
                if value.contains(['\r', '\n']) {
                    return Err(McpConfigError::HeaderInjection {
                        server: server.name.clone(),
                        name: name.clone(),
                    });
                }
            }
            if let Some(var) = &server.bearer_token_env_var
                && var.trim().is_empty()
            {
                return Err(wrong("bearer_token_env_var", "streamable-http"));
            }
        }
    }
    Ok(())
}

/// Header names that carry a credential (ADR-0067 §6).
///
/// Substring, not exact match: `Proxy-Authorization`, `X-Api-Key` and
/// `X-Auth-Token` are the shapes people actually write. A false positive here
/// is a config error with a message naming the alternative, not a security
/// hole, so erring wide is the right direction.
fn is_credential_header(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "authorization",
        "cookie",
        "token",
        "key",
        "secret",
        "password",
    ];
    let name = name.to_ascii_lowercase();
    WORDS.iter().any(|word| name.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config mutation, named because `Vec<(&str, Box<dyn Fn(&mut …)>)>` is
    /// unreadable inline and the closure types differ per case.
    type Mutate = Box<dyn Fn(&mut McpServerConfig)>;

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some("true".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn accepts_unique_names() {
        let config = McpConfig {
            servers: vec![server("filesystem"), server("github")],
            ..Default::default()
        };
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn rejects_bad_names() {
        assert_eq!(
            McpConfig {
                servers: vec![server("")],
                ..Default::default()
            }
            .validate(),
            Err(McpConfigError::EmptyName)
        );
        assert_eq!(
            McpConfig {
                servers: vec![server("file system")],
                ..Default::default()
            }
            .validate(),
            Err(McpConfigError::InvalidName("file system".to_string()))
        );
    }

    #[test]
    fn rejects_duplicates() {
        assert_eq!(
            McpConfig {
                servers: vec![server("fs"), server("fs")],
                ..Default::default()
            }
            .validate(),
            Err(McpConfigError::DuplicateName("fs".to_string()))
        );
    }

    #[test]
    fn disabled_servers_skip_command_checks() {
        let mut disabled = server("fs");
        disabled.enabled = false;
        disabled.command = Some(" ".to_string());
        assert_eq!(
            McpConfig {
                servers: vec![disabled],
                ..Default::default()
            }
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn tool_name_is_namespaced() {
        assert_eq!(
            server("filesystem").tool_name("read_file"),
            "mcp__filesystem__read_file"
        );
    }

    #[test]
    fn read_paths_must_be_absolute() {
        let mut config = server("fs");
        config.read_paths = vec![PathBuf::from("relative/dir")];
        assert_eq!(
            McpConfig {
                servers: vec![config],
                ..Default::default()
            }
            .validate(),
            Err(McpConfigError::RelativePath {
                server: "fs".to_string(),
                field: "read_paths",
                path: "relative/dir".to_string(),
            })
        );
    }

    #[test]
    fn sandbox_defaults_to_confine_and_can_be_overridden() {
        let parsed: McpConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.sandbox, McpSandboxMode::Confine);

        let parsed: McpConfig = serde_json::from_str(
            r#"{
                "sandbox": "off",
                "servers": [
                    { "name": "a", "command": "true" },
                    {
                        "name": "b",
                        "command": "true",
                        "sandbox": "confine",
                        "credentials": ["github", "aws"],
                        "read_paths": ["/opt/b"]
                    }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.sandbox, McpSandboxMode::Off);
        // The per-server override wins over `[mcp] sandbox`.
        assert_eq!(
            parsed.servers[0].sandbox_mode(parsed.sandbox),
            McpSandboxMode::Off
        );
        assert_eq!(
            parsed.servers[1].sandbox_mode(parsed.sandbox),
            McpSandboxMode::Confine
        );
        assert_eq!(
            parsed.servers[1].credentials,
            vec![CredentialPreset::Github, CredentialPreset::Aws]
        );
        assert_eq!(parsed.servers[1].read_paths, vec![PathBuf::from("/opt/b")]);
    }

    fn remote(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: None,
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    fn validate(servers: Vec<McpServerConfig>) -> Result<(), McpConfigError> {
        McpConfig {
            servers,
            ..Default::default()
        }
        .validate()
    }

    #[test]
    fn a_stdio_server_is_unaffected_by_the_transport_split() {
        // The whole point of making `command` an `Option` (ADR-0067 §1): every
        // config written before it keeps parsing and validating.
        let parsed: McpConfig = serde_json::from_str(
            r#"{"servers": [{"name": "fs", "command": "npx", "args": ["-y", "srv"]}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.servers[0].command.as_deref(), Some("npx"));
        assert_eq!(parsed.servers[0].url, None);
        assert_eq!(
            parsed.servers[0].transport(),
            Some(Transport::Stdio),
            "an old one-field config must still be stdio"
        );
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn a_remote_server_needs_only_a_url() {
        let parsed: McpConfig = serde_json::from_str(
            r#"{"servers": [{"name": "c7", "url": "https://mcp.context7.com/mcp"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.servers[0].command, None);
        assert_eq!(
            parsed.servers[0].transport(),
            Some(Transport::StreamableHttp)
        );
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn exactly_one_of_command_and_url_is_required() {
        assert_eq!(
            validate(vec![McpServerConfig {
                name: "nothing".to_string(),
                ..Default::default()
            }]),
            Err(McpConfigError::NoTransport("nothing".to_string()))
        );
        let mut both = server("both");
        both.url = Some("https://example.test/mcp".to_string());
        assert_eq!(
            validate(vec![both]),
            Err(McpConfigError::AmbiguousTransport("both".to_string()))
        );
    }

    #[test]
    fn the_url_must_be_an_absolute_http_url() {
        for (url, reason) in [
            ("ftp://example.test/mcp", "scheme"),
            ("/mcp", "relative"),
            ("", "empty"),
        ] {
            let mut config = remote("x", url);
            if url.is_empty() {
                config.url = Some(String::new());
                assert_eq!(
                    validate(vec![config]),
                    Err(McpConfigError::EmptyUrl("x".to_string()))
                );
                continue;
            }
            match validate(vec![config]) {
                Err(McpConfigError::InvalidUrl { reason: got, .. }) => {
                    assert!(!got.is_empty(), "{url}: {reason}")
                }
                other => panic!("{url}: expected InvalidUrl, got {other:?}"),
            }
        }
    }

    #[test]
    fn stdio_shaped_settings_are_rejected_on_a_remote_server() {
        // Each of these would otherwise be accepted and silently do nothing,
        // which is the failure mode ADR-0066/0068 §2 exists to remove.
        let cases: Vec<(&'static str, Mutate)> = vec![
            ("args", Box::new(|s| s.args.push("--x".into()))),
            (
                "env",
                Box::new(|s| {
                    s.env.insert("K".into(), "v".into());
                }),
            ),
            (
                "read_paths",
                Box::new(|s| s.read_paths.push(PathBuf::from("/opt/x"))),
            ),
            (
                "writable_paths",
                Box::new(|s| s.writable_paths.push(PathBuf::from("/opt/w"))),
            ),
            (
                "credentials",
                Box::new(|s| s.credentials.push(CredentialPreset::Github)),
            ),
            (
                "sandbox",
                Box::new(|s| s.sandbox = Some(McpSandboxMode::Off)),
            ),
        ];
        for (field, set) in cases {
            let mut config = remote("r", "https://example.test/mcp");
            set(&mut config);
            match validate(vec![config]) {
                Err(McpConfigError::WrongTransportField { field: got, .. }) => {
                    assert_eq!(got, field)
                }
                other => panic!("{field}: expected WrongTransportField, got {other:?}"),
            }
        }
    }

    #[test]
    fn http_shaped_settings_are_rejected_on_a_stdio_server() {
        for (field, set) in [
            (
                "headers",
                Box::new(|s: &mut McpServerConfig| {
                    s.headers.insert("X-Trace".into(), "1".into());
                }) as Mutate,
            ),
            (
                "bearer_token_env_var",
                Box::new(|s: &mut McpServerConfig| {
                    s.bearer_token_env_var = Some("TOKEN".into());
                }),
            ),
        ] {
            let mut config = server("local");
            set(&mut config);
            match validate(vec![config]) {
                Err(McpConfigError::WrongTransportField { field: got, .. }) => {
                    assert_eq!(got, field)
                }
                other => panic!("{field}: expected WrongTransportField, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_credential_shaped_header_is_rejected() {
        // ADR-0067 §6: the config file is not a secret store, and the header
        // table is exactly the place that would make it one.
        for name in [
            "Authorization",
            "Proxy-Authorization",
            "Cookie",
            "X-Api-Key",
            "X-Auth-Token",
            "X-Client-Secret",
        ] {
            let mut config = remote("r", "https://example.test/mcp");
            config.headers.insert(name.to_string(), "value".to_string());
            match validate(vec![config]) {
                Err(McpConfigError::SensitiveHeader { name: got, .. }) => {
                    assert_eq!(got, name)
                }
                other => panic!("{name}: expected SensitiveHeader, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_plain_header_is_allowed() {
        let mut config = remote("r", "https://example.test/mcp");
        config
            .headers
            .insert("Accept".to_string(), "application/json".to_string());
        config
            .headers
            .insert("X-Trace-Id".to_string(), "abc".to_string());
        config.bearer_token_env_var = Some("MY_TOKEN".to_string());
        assert_eq!(validate(vec![config]), Ok(()));
    }

    #[test]
    fn a_header_value_with_a_line_break_is_rejected() {
        let mut config = remote("r", "https://example.test/mcp");
        config
            .headers
            .insert("X-Trace".to_string(), "a\r\nX-Injected: 1".to_string());
        assert_eq!(
            validate(vec![config]),
            Err(McpConfigError::HeaderInjection {
                server: "r".to_string(),
                name: "X-Trace".to_string(),
            })
        );
    }

    #[test]
    fn transport_is_none_when_the_config_is_ambiguous() {
        // `validate()` rejects this; the accessor must not pick a side, because
        // a stdio server holding a URL would try to exec the URL.
        let mut both = server("both");
        both.url = Some("https://example.test/mcp".to_string());
        assert_eq!(both.transport(), None);
        assert_eq!(McpServerConfig::default().transport(), None);
    }

    #[test]
    fn unknown_sandbox_value_is_a_parse_error() {
        let parsed = serde_json::from_str::<McpConfig>(r#"{"sandbox": "maybe"}"#);
        assert!(parsed.is_err(), "{parsed:?}");
    }
}
