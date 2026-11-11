//! Approval policy: deterministic rules over tool name and arguments.
//!
//! `deny` always wins. Otherwise the most specific matching rule wins. With no
//! match, the default depends on the call class: reads and workspace-internal
//! writes are allowed, writes outside the workspace and process execution ask.

use globset::Glob;
use serde::Deserialize;
use serde_json::Value;

use crate::event::Scope;
use crate::tool::ApprovalClass;

/// The characters `globset` treats as metacharacters.
///
/// Used to decide whether a `tool` value is a literal name or a pattern. A
/// literal comparison is not just a fast path: it also keeps a name containing
/// `_`, `:` or `.` out of the glob parser entirely, so a future globset change
/// to escapes cannot silently turn an exact allowlist entry into something
/// broader.
const GLOB_METACHARACTERS: [char; 6] = ['*', '?', '[', ']', '{', '}'];

/// Does `pattern` contain glob syntax at all?
fn is_pattern(pattern: &str) -> bool {
    pattern.contains(GLOB_METACHARACTERS)
}

/// Match a tool name against a literal or a glob.
///
/// An unparsable pattern matches nothing. That direction is deliberate: the
/// alternative (treating a broken pattern as `*`) turns a typo in a `deny` rule
/// into a silent allow-everything. `Rule::validate` is what reports the typo;
/// this only has to stay on the safe side while it does.
fn tool_matches(pattern: &str, tool: &str) -> bool {
    if !is_pattern(pattern) {
        return pattern == tool;
    }
    match Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(tool),
        Err(_) => false,
    }
}

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
    /// Exact tool name, or a glob such as `mcp__wordcraft__*`.
    ///
    /// The glob form exists because an MCP server contributes a dozen tools at
    /// once and hand-listing them is the step people skip: the first real use
    /// of this field as an allowlist needed 19 rules for one server, and the
    /// natural spelling `mcp__wordcraft__*` silently matched nothing until it
    /// was supported (it then fell through to `default: external tool: ask`).
    /// An exact name always outranks a pattern, so `mcp__*` cannot shadow a
    /// precise `deny`.
    #[serde(default)]
    pub tool: Option<String>,
    /// Matches when the `command` argument starts with this prefix.
    #[serde(default)]
    pub command_prefix: Option<String>,
    /// Matches the `path` argument against a glob (e.g. `src/**`).
    #[serde(default)]
    pub path_glob: Option<String>,
    pub action: Action,
    /// Where the rule came from; project rules are downgraded to ask on load.
    #[serde(default)]
    pub scope: Option<Scope>,
}

impl Rule {
    /// Report a pattern that cannot be parsed, so loading fails instead of the
    /// rule quietly never matching.
    ///
    /// Called by both loaders (config `[[approval.rules]]` and a
    /// `--approval-policy` file). Not a method on `Deserialize` because the
    /// error should name the rule id and the field, which TOML's own message
    /// cannot.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(pattern) = &self.tool
            && is_pattern(pattern)
            && let Err(error) = Glob::new(pattern)
        {
            return Err(format!(
                "rule '{}' has an invalid `tool` pattern {:?}: {error}",
                self.id, pattern
            ));
        }
        if let Some(pattern) = &self.path_glob
            && let Err(error) = Glob::new(pattern)
        {
            return Err(format!(
                "rule '{}' has an invalid `path_glob` {:?}: {error}",
                self.id, pattern
            ));
        }
        Ok(())
    }

    /// How specific a tool predicate is: exact beats glob beats absent.
    ///
    /// A glob is *not* "as specific as a name" — that is the whole risk of
    /// adding wildcards. Ranking them separately is what makes
    /// `tool = "mcp__*"` lose to `tool = "mcp__github__pr_merge"` regardless of
    /// the order the two rules appear in the file, so an allowlist stays
    /// auditable by reading it.
    fn tool_rank(&self) -> u8 {
        match self.tool.as_deref() {
            None => 0,
            Some(pattern) if is_pattern(pattern) => 1,
            Some(_) => 2,
        }
    }

    /// Number of further predicates; more conditions win between otherwise
    /// equal rules.
    fn extra_predicates(&self) -> usize {
        usize::from(self.command_prefix.is_some()) + usize::from(self.path_glob.is_some())
    }

    fn matches(&self, tool: &str, args: &Value) -> bool {
        if let Some(pattern) = &self.tool
            && !tool_matches(pattern, tool)
        {
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
        if let Some(pattern) = &self.path_glob {
            let Some(path) = args.get("path").and_then(|value| value.as_str()) else {
                return false;
            };
            let Ok(glob) = Glob::new(pattern) else {
                return false;
            };
            if !glob.compile_matcher().is_match(path) {
                return false;
            }
        }
        true
    }

    fn scope_rank(&self) -> u8 {
        match self.scope.unwrap_or(Scope::User) {
            Scope::Session => 2,
            Scope::Project => 1,
            Scope::User => 0,
        }
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
    /// Scope of the matching rule; `None` for defaults or an empty policy.
    pub scope: Option<Scope>,
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
                scope: Some(rule.scope.unwrap_or(Scope::User)),
            };
        }

        if let Some(rule) = matching
            .iter()
            .filter(|rule| rule.action != Action::Deny)
            .max_by_key(|rule| {
                (
                    rule.tool_rank(),
                    rule.extra_predicates(),
                    rule.scope_rank(),
                    rule.command_prefix.as_deref().map_or(0, str::len),
                    rule.path_glob.as_deref().map_or(0, str::len),
                )
            })
        {
            return Decision {
                action: rule.action,
                rule_id: Some(rule.id.clone()),
                reason: format!("matched rule '{}'", rule.id),
                scope: Some(rule.scope.unwrap_or(Scope::User)),
            };
        }

        let (action, reason) = match class {
            ApprovalClass::Read => (Action::Allow, "default: read-only tool"),
            ApprovalClass::WriteInside => (Action::Allow, "default: write inside workspace"),
            ApprovalClass::WriteOutside => (Action::Ask, "default: write outside workspace"),
            ApprovalClass::Execute => (Action::Ask, "default: process execution"),
            ApprovalClass::External => (Action::Ask, "default: external tool"),
            ApprovalClass::BulkWrite => (Action::Ask, "default: multi-file write"),
            ApprovalClass::Network => (Action::Ask, "default: network access"),
        };
        Decision {
            action,
            rule_id: None,
            reason: reason.to_string(),
            scope: None,
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
            path_glob: None,
            action,
            scope: None,
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
        assert_eq!(
            policy
                .evaluate("mcp__filesystem__read", ApprovalClass::External, &json!({}))
                .action,
            Action::Ask
        );
        assert_eq!(
            policy
                .evaluate("apply_patch", ApprovalClass::BulkWrite, &json!({}))
                .action,
            Action::Ask
        );
        assert_eq!(
            policy
                .evaluate("web_fetch", ApprovalClass::Network, &json!({}))
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

    /// One rule for a whole MCP server, which is the case that motivated the
    /// glob: a server contributes a dozen tools and listing them by hand is the
    /// step people skip.
    #[test]
    fn tool_pattern_covers_a_whole_server() {
        let policy = Policy::new(vec![rule(
            "allow-wordcraft",
            Some("mcp__wordcraft__*"),
            None,
            Action::Allow,
        )]);
        for tool in [
            "mcp__wordcraft__new_document",
            "mcp__wordcraft__save_document",
            "mcp__wordcraft__batch",
            "mcp__wordcraft__x",
        ] {
            assert_eq!(
                policy
                    .evaluate(tool, ApprovalClass::External, &json!({}))
                    .action,
                Action::Allow,
                "{tool} should be allowed"
            );
        }
        // Another server, and the built-ins, stay on the default.
        for tool in [
            "mcp__github__pr_merge",
            "mcp__wordcraft",
            "mcp__wordcraftX__a",
        ] {
            assert_eq!(
                policy
                    .evaluate(tool, ApprovalClass::External, &json!({}))
                    .action,
                Action::Ask,
                "{tool} should not be allowed"
            );
        }
    }

    /// The pattern must not reach across the `__` separator by accident: a `*`
    /// is greedy, so `mcp__wordcraft__*` has to be anchored by the literal
    /// prefix rather than matching `mcp__other__wordcraft__x`.
    #[test]
    fn a_tool_pattern_matches_the_whole_name_not_a_substring() {
        let policy = Policy::new(vec![rule(
            "allow-wordcraft",
            Some("mcp__wordcraft__*"),
            None,
            Action::Allow,
        )]);
        assert_eq!(
            policy
                .evaluate("pre_mcp__wordcraft__x", ApprovalClass::External, &json!({}))
                .action,
            Action::Ask,
            "a glob is anchored at both ends"
        );
    }

    /// An exact name outranks a pattern, whatever the order in the file. This is
    /// the property that keeps a wildcard from shadowing a precise exception.
    #[test]
    fn an_exact_tool_name_beats_a_pattern() {
        for rules in [
            vec![
                rule("allow-all-mcp", Some("mcp__*"), None, Action::Allow),
                rule("ask-dangerous", Some("mcp__fs__delete"), None, Action::Ask),
            ],
            // Same two rules, opposite order: the answer must not depend on it.
            vec![
                rule("ask-dangerous", Some("mcp__fs__delete"), None, Action::Ask),
                rule("allow-all-mcp", Some("mcp__*"), None, Action::Allow),
            ],
        ] {
            let policy = Policy::new(rules);
            assert_eq!(
                policy
                    .evaluate("mcp__fs__delete", ApprovalClass::External, &json!({}))
                    .action,
                Action::Ask
            );
            assert_eq!(
                decision_rule(&policy, "mcp__fs__delete").as_deref(),
                Some("ask-dangerous")
            );
            assert_eq!(
                policy
                    .evaluate("mcp__fs__read", ApprovalClass::External, &json!({}))
                    .action,
                Action::Allow
            );
        }
    }

    /// `deny` wins over a matching allow pattern too — the rule the module doc
    /// states first, re-checked now that patterns can match.
    #[test]
    fn deny_beats_an_allow_pattern() {
        let policy = Policy::new(vec![
            rule("allow-all-mcp", Some("mcp__*"), None, Action::Allow),
            rule("deny-delete", Some("mcp__fs__delete"), None, Action::Deny),
        ]);
        assert_eq!(
            policy
                .evaluate("mcp__fs__delete", ApprovalClass::External, &json!({}))
                .action,
            Action::Deny
        );
    }

    /// A broken pattern matches nothing and is reported by `validate`, so the
    /// two halves agree: never a silent allow-everything, never a silent lie.
    #[test]
    fn an_invalid_tool_pattern_matches_nothing_and_is_reported() {
        let broken = "mcp__[__x";
        let policy = Policy::new(vec![rule(
            "allow-broken",
            Some(broken),
            None,
            Action::Allow,
        )]);
        assert_eq!(
            policy
                .evaluate("mcp__fs__read", ApprovalClass::External, &json!({}))
                .action,
            Action::Ask,
            "an unparsable pattern must not match"
        );
        let error = policy.rules()[0].validate().expect_err("must be reported");
        assert!(error.contains("allow-broken"), "{error}");
        assert!(error.contains("tool"), "{error}");

        // And a literal name containing no metacharacter is never parsed as a
        // glob, so it cannot be reported as broken either.
        assert!(
            rule("literal", Some("mcp__fs__read"), None, Action::Allow)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn an_invalid_path_glob_is_reported() {
        let bad = Rule {
            id: "bad-path".to_string(),
            tool: None,
            command_prefix: None,
            path_glob: Some("src/[".to_string()),
            action: Action::Deny,
            scope: None,
        };
        let error = bad.validate().expect_err("must be reported");
        assert!(error.contains("bad-path"), "{error}");
        assert!(error.contains("path_glob"), "{error}");
    }

    fn decision_rule(policy: &Policy, tool: &str) -> Option<String> {
        policy
            .evaluate(tool, ApprovalClass::External, &json!({}))
            .rule_id
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

    #[test]
    fn path_glob_matches_the_path_argument() {
        let policy = Policy::new(vec![Rule {
            id: "deny-src".to_string(),
            tool: Some("write_file".to_string()),
            command_prefix: None,
            path_glob: Some("src/**".to_string()),
            action: Action::Deny,
            scope: None,
        }]);

        assert_eq!(
            policy
                .evaluate(
                    "write_file",
                    ApprovalClass::WriteInside,
                    &json!({"path": "src/lib.rs"})
                )
                .action,
            Action::Deny
        );
        assert_eq!(
            policy
                .evaluate(
                    "write_file",
                    ApprovalClass::WriteInside,
                    &json!({"path": "README.md"})
                )
                .action,
            Action::Allow
        );
        // No path argument: the rule does not apply.
        assert_eq!(
            policy
                .evaluate("write_file", ApprovalClass::WriteInside, &json!({}))
                .action,
            Action::Allow
        );
    }

    #[test]
    fn scope_breaks_ties_toward_the_narrower_scope() {
        let policy = Policy::new(vec![
            rule("user-allow", Some("run_shell"), None, Action::Allow),
            Rule {
                id: "project-ask".to_string(),
                tool: Some("run_shell".to_string()),
                command_prefix: None,
                path_glob: None,
                action: Action::Ask,
                scope: Some(Scope::Project),
            },
        ]);

        let decision = policy.evaluate("run_shell", ApprovalClass::Execute, &json!({}));
        assert_eq!(decision.action, Action::Ask);
        assert_eq!(decision.scope, Some(Scope::Project));
    }
}
