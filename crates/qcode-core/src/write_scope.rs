//! Write-scope ladder for command execution (SANDBOX-MODE.md).
//!
//! How much of the machine a `run_shell` call may touch, orthogonal to the OS
//! sandbox backend. The ladder is strict: a call may escalate to a wider scope
//! (which the approval flow gates) but never to a narrower gap — a request must
//! name one of the scopes reachable from the session's current scope.
//!
//! v1 scope: this module defines the ladder and the escalation rule, and
//! `run_shell` classifies its calls by whether they ask to escalate. The
//! session-level scope itself and the OS-backend mapping are the next step.

use serde::{Deserialize, Serialize};

/// How much a command may touch. `read_only` is narrowest, `full_access` widest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteScope {
    /// The workspace is read-only and the network is off.
    ReadOnly,
    /// Only the task's roots may be written.
    #[default]
    WorkspaceWrite,
    /// Writes outside the workspace, direct network, system-level changes.
    FullAccess,
}

/// Scopes a given scope may escalate to (strict ladder, dsh `WIDER_MODES`-style).
pub fn wider_scopes(scope: WriteScope) -> &'static [WriteScope] {
    match scope {
        WriteScope::ReadOnly => &[WriteScope::WorkspaceWrite, WriteScope::FullAccess],
        WriteScope::WorkspaceWrite => &[WriteScope::FullAccess],
        WriteScope::FullAccess => &[],
    }
}

/// `requested` must be reachable from `current` by the strict ladder. A request
/// at or below the current scope is a no-op, not an escalation.
pub fn is_escalation(current: WriteScope, requested: WriteScope) -> bool {
    requested != current && wider_scopes(current).contains(&requested)
}

impl WriteScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::FullAccess => "full_access",
        }
    }

    /// Parse the `sandbox_permissions` tool argument. Unknown values are
    /// rejected so a typo never silently runs at a different scope.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "read_only" | "read-only" => Some(Self::ReadOnly),
            "workspace_write" | "workspace-write" => Some(Self::WorkspaceWrite),
            "full_access" | "full-access" => Some(Self::FullAccess),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_escalates_strictly() {
        assert_eq!(
            wider_scopes(WriteScope::ReadOnly),
            &[WriteScope::WorkspaceWrite, WriteScope::FullAccess]
        );
        assert_eq!(
            wider_scopes(WriteScope::WorkspaceWrite),
            &[WriteScope::FullAccess]
        );
        assert!(wider_scopes(WriteScope::FullAccess).is_empty());
    }

    #[test]
    fn escalation_requires_a_wider_reachable_scope() {
        assert!(is_escalation(WriteScope::ReadOnly, WriteScope::FullAccess));
        assert!(is_escalation(
            WriteScope::ReadOnly,
            WriteScope::WorkspaceWrite
        ));
        assert!(is_escalation(
            WriteScope::WorkspaceWrite,
            WriteScope::FullAccess
        ));
        // Same scope or a step down is not an escalation.
        assert!(!is_escalation(
            WriteScope::FullAccess,
            WriteScope::FullAccess
        ));
        assert!(!is_escalation(
            WriteScope::WorkspaceWrite,
            WriteScope::ReadOnly
        ));
    }

    #[test]
    fn parse_accepts_both_spellings_and_rejects_garbage() {
        assert_eq!(WriteScope::parse("read_only"), Some(WriteScope::ReadOnly));
        assert_eq!(
            WriteScope::parse("workspace-write"),
            Some(WriteScope::WorkspaceWrite)
        );
        assert_eq!(
            WriteScope::parse("full_access"),
            Some(WriteScope::FullAccess)
        );
        assert_eq!(WriteScope::parse("sudo"), None);
    }
}
