//! Approval policy: deterministic rules over tool name and arguments.
//!
//! `deny` always wins. Otherwise the most specific matching rule wins. With no
//! match, the default depends on the call class: reads and workspace-internal
//! writes are allowed, writes outside the workspace and process execution ask.

use serde::Deserialize;
use serde_json::Value;

use crate::tool::ApprovalClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub tool: Option<String>,
    /// Matches when the `command` argument starts with this prefix.
    #[serde(default)]
    pub command_prefix: Option<String>,
    pub action: Action,
}

impl Rule {
    fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool.as_deref().is_some_and(|name| name != tool) {
            return false;
        }
        if let Some(prefix) = &self.command_prefix {
            let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
                return false;
            };
            if !command.starts_with(prefix) {
                return false;
            }
        }
        true
    }

    fn specificity(&self) -> usize {
        usize::from(self.tool.is_some()) + usize::from(self.command_prefix.is_some())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub rule_id: Option<String>,
    pub reason: String,
}

impl Policy {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn evaluate(&self, tool: &str, class: ApprovalClass, args: &Value) -> Decision {
        let matching: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|rule| rule.matches(tool, args))
            .collect();

        if let Some(rule) = matching.iter().find(|rule| rule.action == Action::Deny) {
            return Decision {
                action: Action::Deny,
                rule_id: Some(rule.id.clone()),
                reason: format!("denied by rule '{}'", rule.id),
            };
        }

        if let Some(rule) = matching
            .iter()
            .filter(|rule| rule.action != Action::Deny)
            .max_by_key(|rule| rule.specificity())
        {
            return Decision {
                action: rule.action,
                rule_id: Some(rule.id.clone()),
                reason: format!("matched rule '{}'", rule.id),
            };
        }

        let (action, reason) = match class {
            ApprovalClass::Read => (Action::Allow, "default: read-only tool"),
            ApprovalClass::WriteInside => (Action::Allow, "default: write inside workspace"),
            ApprovalClass::WriteOutside => (Action::Ask, "default: write outside workspace"),
            ApprovalClass::Execute => (Action::Ask, "default: process execution"),
        };
        Decision {
            action,
            rule_id: None,
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(id: &str, tool: Option<&str>, prefix: Option<&str>, action: Action) -> Rule {
        Rule {
            id: id.to_string(),
            tool: tool.map(str::to_string),
            command_prefix: prefix.map(str::to_string),
            action,
        }
    }

    #[test]
    fn defaults_follow_call_class() {
        let policy = Policy::default();
        assert_eq!(
            policy
                .evaluate("read_file", ApprovalClass::Read, &json!({}))
                .action,
            Action::Allow
        );
        assert_eq!(
            policy
                .evaluate("write_file", ApprovalClass::WriteInside, &json!({}))
                .action,
            Action::Allow
        );
        assert_eq!(
            policy
                .evaluate("write_file", ApprovalClass::WriteOutside, &json!({}))
                .action,
            Action::Ask
        );
        assert_eq!(
            policy
                .evaluate("run_shell", ApprovalClass::Execute, &json!({}))
                .action,
            Action::Ask
        );
    }

    #[test]
    fn deny_wins_over_allow() {
        let policy = Policy::new(vec![
            rule("allow-all-shell", Some("run_shell"), None, Action::Allow),
            rule("deny-rm", Some("run_shell"), Some("rm -rf"), Action::Deny),
        ]);
        let decision = policy.evaluate(
            "run_shell",
            ApprovalClass::Execute,
            &json!({"command": "rm -rf /tmp/x"}),
        );
        assert_eq!(decision.action, Action::Deny);
        assert_eq!(decision.rule_id.as_deref(), Some("deny-rm"));
    }

    #[test]
    fn prefix_rule_matches() {
        let policy = Policy::new(vec![rule(
            "allow-cargo-test",
            Some("run_shell"),
            Some("cargo test"),
            Action::Allow,
        )]);
        assert_eq!(
            policy
                .evaluate(
                    "run_shell",
                    ApprovalClass::Execute,
                    &json!({"command": "cargo test --workspace"})
                )
                .action,
            Action::Allow
        );
        assert_eq!(
            policy
                .evaluate(
                    "run_shell",
                    ApprovalClass::Execute,
                    &json!({"command": "cargo publish"})
                )
                .action,
            Action::Ask
        );
    }

    #[test]
    fn most_specific_rule_wins() {
        let policy = Policy::new(vec![
            rule("allow-shell", Some("run_shell"), None, Action::Allow),
            rule("ask-push", Some("run_shell"), Some("git push"), Action::Ask),
        ]);
        assert_eq!(
            policy
                .evaluate(
                    "run_shell",
                    ApprovalClass::Execute,
                    &json!({"command": "git push origin main"})
                )
                .action,
            Action::Ask
        );
    }

    #[test]
    fn tool_specific_rule_does_not_leak() {
        let policy = Policy::new(vec![rule(
            "allow-read",
            Some("read_file"),
            None,
            Action::Allow,
        )]);
        assert_eq!(
            policy
                .evaluate(
                    "run_shell",
                    ApprovalClass::Execute,
                    &json!({"command": "ls"})
                )
                .action,
            Action::Ask
        );
    }

    #[test]
    fn command_prefix_requires_a_command_argument() {
        let policy = Policy::new(vec![rule(
            "allow-cargo",
            Some("run_shell"),
            Some("cargo"),
            Action::Allow,
        )]);
        assert_eq!(
            policy
                .evaluate("run_shell", ApprovalClass::Execute, &json!({}))
                .action,
            Action::Ask
        );
    }
}
