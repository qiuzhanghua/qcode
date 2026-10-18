//! Human-in-the-loop approval.
//!
//! The engine turns an `Ask` decision into an `ApprovalRequest` and awaits an
//! `Approver`. Headless runs use `AutoApprove` (`--yes`) or `AutoDeny`; the
//! interactive implementation reads a line from stdin.

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    /// Short human-readable form of the call: command, path, or compressed args.
    pub summary: String,
    pub reason: String,
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny,
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest) -> Answer;
}

pub struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, _request: &ApprovalRequest) -> Answer {
        Answer::Allow
    }
}

pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _request: &ApprovalRequest) -> Answer {
        Answer::Deny
    }
}

pub struct InteractiveApprover;

#[async_trait]
impl Approver for InteractiveApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Answer {
        use std::io::Write;

        eprintln!();
        eprintln!("[approval] {}: {}", request.tool, request.summary);
        eprintln!("  reason: {}", request.reason);
        eprint!("  allow? [y/N] ");
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Answer::Deny;
        }

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Answer::Allow,
            _ => Answer::Deny,
        }
    }
}
