//! Network seam shared by tools and the CLI.
//!
//! Tools never construct a `reqwest::Client` directly when a `Network` is
//! available: the implementation points every request at the session's egress
//! proxy. Tests and offline tools fall back to a plain client.

use std::sync::Arc;

/// One per qcode session (or process, for v2.0).
pub trait Network: Send + Sync + std::fmt::Debug {
    /// A client already routed through the egress proxy.
    fn client(&self) -> reqwest::Client;

    /// Proxy URL, including the session token.
    fn proxy_url(&self) -> String;

    /// Add a host to the session allowlist (after user approval).
    fn allow_host(&self, host: &str);

    /// Environment variables for child processes (shell, MCP).
    fn env(&self) -> Vec<(String, String)>;
}

pub type SharedNetwork = Arc<dyn Network>;
