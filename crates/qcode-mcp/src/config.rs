//! `[[mcp.servers]]` configuration.
//!
//! The type lives here (not in the CLI) so the client and the config loader
//! share one definition of a server. Validation is a pure function: it runs
//! when the config is loaded, before any process is spawned.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;

fn default_true() -> bool {
    true
}

fn default_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the server process, on top of the inherited one.
    /// Servers that need a token or a cache directory get it here instead of a
    /// wrapper script.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-request timeout for the handshake, `tools/list` and `tools/call`.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl McpServerConfig {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    /// Public tool name: `mcp__<server>__<tool>`. The prefix keeps MCP tools
    /// from colliding with built-ins or with each other.
    pub fn tool_name(&self, tool: &str) -> String {
        format!("mcp__{}__{}", self.name, tool)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
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
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            timeout_secs: 120,
        }
    }

    #[test]
    fn accepts_unique_names() {
        let config = McpConfig {
            servers: vec![server("filesystem"), server("github")],
        };
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn rejects_bad_names() {
        assert_eq!(
            McpConfig {
                servers: vec![server("")]
            }
            .validate(),
            Err(McpConfigError::EmptyName)
        );
        assert_eq!(
            McpConfig {
                servers: vec![server("file system")]
            }
            .validate(),
            Err(McpConfigError::InvalidName("file system".to_string()))
        );
    }

    #[test]
    fn rejects_duplicates() {
        assert_eq!(
            McpConfig {
                servers: vec![server("fs"), server("fs")]
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
                servers: vec![disabled]
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
}
