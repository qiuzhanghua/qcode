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
    pub command: String,
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

impl McpServerConfig {
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
            command: String::new(),
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
            if server.command.trim().is_empty() {
                return Err(McpConfigError::EmptyCommand(server.name.clone()));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "true".to_string(),
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
        disabled.command = " ".to_string();
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

    #[test]
    fn unknown_sandbox_value_is_a_parse_error() {
        let parsed = serde_json::from_str::<McpConfig>(r#"{"sandbox": "maybe"}"#);
        assert!(parsed.is_err(), "{parsed:?}");
    }
}
