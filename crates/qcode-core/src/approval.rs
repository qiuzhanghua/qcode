//! Human-in-the-loop approval.
//!
//! The engine turns an `Ask` decision into an `ApprovalRequest` and awaits an
//! `Approver`. Headless runs use `AutoApprove` (`--yes`) or `AutoDeny`; the
//! interactive implementation reads a line from stdin.
//!
//! Approval is cancellable: an approver that can block must watch the token and
//! answer `Deny` promptly. The engine distinguishes that from a human denial by
//! checking the token afterwards, and records `cancelled` instead of `denied`.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    /// Short human-readable form of the call: command, path, or compressed args.
    pub summary: String,
    pub reason: String,
    pub rule_id: Option<String>,
    /// Command/path shown as `argv` in the approval panel.
    pub argv: Option<String>,
    /// Effective working directory for shell calls.
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny,
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer;
}

pub struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, _request: &ApprovalRequest, _cancel: CancellationToken) -> Answer {
        Answer::Allow
    }
}

pub struct AutoDeny;

#[async_trait]
impl Approver for AutoDeny {
    async fn approve(&self, _request: &ApprovalRequest, _cancel: CancellationToken) -> Answer {
        Answer::Deny
    }
}

pub struct InteractiveApprover;

#[async_trait]
impl Approver for InteractiveApprover {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer {
        let request = request.clone();
        // stdin reads are blocking. Park them on a blocking thread so Ctrl-C
        // can still win the race below instead of freezing the runtime.
        let mut task = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;

            eprintln!();
            eprintln!("[approval] {}: {}", request.tool, request.summary);
            if let Some(argv) = &request.argv {
                eprintln!("  argv: {argv}");
            }
            if let Some(cwd) = &request.cwd {
                eprintln!("  cwd: {cwd}");
            }
            if let Some(rule) = &request.rule_id {
                eprintln!("  rule: {rule}");
            }
            eprintln!("  reason: {}", request.reason);
            eprint!("  allow? [y/N] ");
            let _ = std::io::stderr().flush();

            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return Answer::Deny;
            }
            parse_answer(&line)
        });

        tokio::select! {
            biased;
            // The blocking read cannot be interrupted. It is detached and dies
            // with the process (one turn per process today).
            _ = cancel.cancelled() => Answer::Deny,
            answer = &mut task => answer.unwrap_or(Answer::Deny),
        }
    }
}

fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Allow,
        _ => Answer::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_parsing_is_forgiving_but_defaults_to_deny() {
        assert_eq!(parse_answer("y\n"), Answer::Allow);
        assert_eq!(parse_answer(" YES \n"), Answer::Allow);
        assert_eq!(parse_answer("n\n"), Answer::Deny);
        assert_eq!(parse_answer("\n"), Answer::Deny);
        assert_eq!(parse_answer("sure\n"), Answer::Deny);
    }
}
