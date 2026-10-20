//! Kernel: configuration and, next, the one place that assembles the engine.
//!
//! ADR-0036: CLI, TUI, ACP and GUI are frontends over this crate, so the
//! provider/tool/sandbox wiring lives here instead of in the `qc` binary.

pub mod config;

/// Kept here so configuration stops reaching into the binary crate.
pub const DEFAULT_REPO: &str = "qiuzhanghua/qcode";

use std::path::Path;

use qcode_core::event::Scope;
use qcode_core::policy::Policy;

/// Turn configuration into the approval policy for one invocation.
///
/// A policy file is scoped to the invocation, so its rules always carry
/// [`Scope::Session`]; config rules keep their own scope.
pub fn build_policy(
    loaded: &config::Config,
    explicit: Option<&Path>,
) -> Result<Policy, Box<dyn std::error::Error>> {
    let rules = match explicit {
        Some(path) => {
            if !path.exists() {
                return Err(format!("approval policy not found: {}", path.display()).into());
            }
            tracing::info!(path = %path.display(), "loaded approval policy file");
            let mut rules = config::load_policy(path)?;
            for rule in &mut rules {
                rule.scope = Some(Scope::Session);
            }
            rules
        }
        None => loaded.approval.rules.clone(),
    };
    Ok(Policy::new(rules))
}

use std::sync::Arc;

use qcode_net::{Egress, EgressOptions, UnknownPolicy};

/// Start the loopback egress proxy when `[network] enabled` is set.
///
/// The provider's own host is added to the allowlist so the model endpoint works
/// without a separate entry; `unknown` maps "ask" onto deny because an egress
/// proxy cannot ask a human.
pub async fn build_egress(
    loaded: &config::Config,
    data_dir: &Path,
    base_url: Option<&str>,
) -> Result<Option<Arc<Egress>>, Box<dyn std::error::Error>> {
    if !loaded.network.enabled {
        return Ok(None);
    }
    let mut allow = loaded.network.allow.clone();
    if let Some(base_url) = base_url
        && let Ok(url) = reqwest::Url::parse(base_url)
        && let Some(host) = url.host_str()
    {
        let port = url.port_or_known_default().unwrap_or(443);
        allow.push(format!("{host}:{port}"));
    }
    let unknown = match loaded.network.unknown {
        config::UnknownHost::Allow => UnknownPolicy::Allow,
        config::UnknownHost::Ask | config::UnknownHost::Deny => UnknownPolicy::Deny,
    };
    let audit_path = loaded
        .network
        .audit
        .then(|| data_dir.join("logs").join("network.jsonl"));
    let egress = Egress::start(EgressOptions {
        allow,
        allow_private: loaded.network.allow_private,
        unknown,
        audit_path,
    })
    .await?;
    tracing::info!(port = egress.port(), "egress proxy started");
    Ok(Some(Arc::new(egress)))
}

/// The assembled kernel: everything a frontend needs, built once.
///
/// ADR-0036: the pieces are assembled in one place so the TUI, ACP and GUI
/// cannot drift apart. `egress` is kept here so the proxy lives exactly as long
/// as the kernel does.
pub struct Kernel {
    pub config: config::Config,
    pub resolved: config::Resolved,
    pub egress: Option<Arc<Egress>>,
}
