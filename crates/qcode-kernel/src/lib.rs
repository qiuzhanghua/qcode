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
