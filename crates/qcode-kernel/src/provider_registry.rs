//! Provider route registry (M0 of SETTINGS-MODELS-V2).
//!
//! Owns route → adapter construction: given a resolved route it builds the
//! adapter for the route's wire [`Protocol`] (OpenAI-compatible, Anthropic or
//! mock), replacing `build_provider`'s previous name-based match. It also
//! answers the configurable-provider directory the settings page renders.

use qcode_core::event::Usage;
use qcode_core::provider::{
    Provider, StopReason, StreamEvent,
};
use qcode_providers::{
    AnthropicConfig, AnthropicProvider, MockProvider, OpenAiCompatConfig,
    OpenAiCompatProvider,
};

use crate::config::{
    CatalogEntry, Config, Protocol, ProviderConfig, catalog_entries, catalog_entry,
};

/// Directory-level credential state of one provider route (M3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    /// A key is present: the env var is set, or a key command is configured.
    Configured,
    /// The route needs a key and none is currently visible.
    Missing,
    /// The route needs no key (local Ollama, mock).
    Keyless,
}

/// One provider route the settings page can show and edit.
#[derive(Debug, Clone)]
pub struct ConfigurableProvider {
    /// Provider route key (`providers.<route>` in the config).
    pub route: String,
    /// Name shown by configuration surfaces.
    pub display_name: String,
    /// Wire protocol this route speaks (catalog default or config override).
    pub protocol: Protocol,
    /// Whether the user/project config carries a profile for this route.
    pub configured: bool,
    /// Credential state of this route: key stored, key missing, or keyless.
    pub credential_state: CredentialState,
}

/// The provider registry: catalog directory + route → adapter construction.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    /// Egress proxy URL passed to every adapter that reaches the network.
    proxy_url: Option<String>,
}

impl ProviderRegistry {
    /// Create a registry bound to one egress proxy (or none).
    pub fn new(proxy_url: Option<String>) -> Self {
        Self { proxy_url }
    }

    /// Build the adapter for one resolved route.
    pub fn build(
        &self,
        resolved: &crate::config::Resolved,
    ) -> Result<Box<dyn Provider>, Box<dyn std::error::Error>> {
        Ok(match resolved.protocol {
            Protocol::Mock => Box::new(self.mock(resolved)),
            Protocol::Anthropic => Box::new(AnthropicProvider::new(AnthropicConfig {
                name: resolved.name.clone(),
                base_url: resolved
                    .base_url
                    .clone()
                    .expect("non-mock provider always has a base URL"),
                api_key: resolved.api_key.clone(),
                proxy_url: self.proxy_url.clone(),
                headers: resolved.headers.clone(),
            })?),
            Protocol::OpenAi => Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
                name: resolved.name.clone(),
                base_url: resolved
                    .base_url
                    .clone()
                    .expect("non-mock provider always has a base URL"),
                api_key: resolved.api_key.clone(),
                stream_usage: resolved.stream_usage,
                proxy_url: self.proxy_url.clone(),
                extra_body: resolved.extra_body.clone(),
                headers: resolved.headers.clone(),
            })?),
        })
    }

    /// The configurable-provider directory: catalog routes first (presentation
    /// order), then configured routes the catalog does not ship.
    pub fn list_configurable_providers(&self, config: &Config) -> Vec<ConfigurableProvider> {
        let mut rows = Vec::new();
        for entry in catalog_entries() {
            rows.push(ConfigurableProvider {
                route: entry.route.to_string(),
                display_name: entry.display_name.to_string(),
                protocol: entry.protocol,
                configured: config.providers.contains_key(entry.route),
                credential_state: credential_state(config.providers.get(entry.route), Some(entry)),
            });
        }
        for (route, profile) in &config.providers {
            if catalog_entry(route).is_some() {
                continue;
            }
            rows.push(ConfigurableProvider {
                route: route.clone(),
                display_name: route.clone(),
                protocol: profile.protocol.unwrap_or(Protocol::OpenAi),
                configured: true,
                credential_state: credential_state(Some(profile), None),
            });
        }
        rows
    }

    /// Mock adapters, keyed by the resolved route name. Deliberately scripted
    /// so the offline walkthroughs stay readable.
    fn mock(&self, resolved: &crate::config::Resolved) -> MockProvider {
        match resolved.name.as_str() {
            "mock" => MockProvider::text(
                "Hello from the mock provider. The event log and replay path are working.",
            ),
            // Offline rehearsal of the approval UI: one run_shell call (ask by
            // default) followed by a closing message. Deliberately tiny so the
            // manual M1-10 walkthrough stays readable.
            "mock-approval" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "approval-1".to_string(),
                        name: "run_shell".to_string(),
                        arguments: serde_json::json!({"command": "echo qcode-approval-demo"}),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta("审批流程演示结束。".to_string()),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            "mock-tools" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "demo-1".to_string(),
                        name: "list_dir".to_string(),
                        arguments: serde_json::json!({"path": "."}),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta("工具循环跑通了。".to_string()),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            "mock-todo" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "todo-1".to_string(),
                        name: "todo_write".to_string(),
                        arguments: serde_json::json!({
                            "items": [
                                {"content": "读 SPEC 的工具契约", "status": "completed"},
                                {"content": "实现 todo_write", "status": "in_progress"},
                                {"content": "跑测试与 TUI 验收"}
                            ]
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta("待办清单已更新。".to_string()),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            "mock-subagent" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "sub-1".to_string(),
                        name: "subagent".to_string(),
                        arguments: serde_json::json!({
                            "task": "在子会话里总结仓库结构",
                            "max_turns": 4
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta("子 agent：仓库有 core/store/cli 三个 crate。".to_string()),
                    StreamEvent::Usage(Usage {
                        input_tokens: Some(12),
                        output_tokens: Some(6),
                        ..Usage::default()
                    }),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta("父 agent 收到子任务结果。".to_string()),
                    StreamEvent::Usage(Usage {
                        input_tokens: Some(12),
                        output_tokens: Some(6),
                        ..Usage::default()
                    }),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            other => MockProvider::text(format!("mock provider: {other}")),
        }
    }
}

/// Credential state for one route (M3). Directory metadata only: it inspects
/// the env-var *name* and whether a key command is configured, never the key
/// itself. Precedence mirrors [`crate::config::resolve`]: the profile's
/// `api_key_env` wins, then the catalog route's.
///
/// A key command counts as configured (it supplies the key at run time, e.g.
/// from Keychain/libsecret, and we cannot verify it here without running it).
/// An env-var name that is set and non-empty is configured; an unset one is
/// missing. No reference at all (or a profile explicitly blanking it) is
/// keyless, like the local Ollama and mock routes.
fn credential_state(
    profile: Option<&ProviderConfig>,
    catalog: Option<&CatalogEntry>,
) -> CredentialState {
    let profile = profile.cloned().unwrap_or_default();
    let env = profile
        .api_key_env
        .clone()
        .or_else(|| catalog.map(|entry| entry.api_key_env.to_string()))
        .unwrap_or_default();
    if profile
        .api_key_command
        .as_deref()
        .is_some_and(|argv| !argv.is_empty())
    {
        return CredentialState::Configured;
    }
    if env.is_empty() {
        return CredentialState::Keyless;
    }
    match std::env::var(env) {
        Ok(value) if !value.is_empty() => CredentialState::Configured,
        _ => CredentialState::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};

    #[test]
    fn directory_lists_catalog_then_hand_declared_routes() {
        let mut config = Config::default();
        config.providers.insert(
            "ark".to_string(),
            ProviderConfig {
                base_url: Some("https://ark.cn-beijing.volces.com/api/v3".to_string()),
                protocol: Some(Protocol::OpenAi),
                api_key_env: Some("ARK_API_KEY".to_string()),
                ..ProviderConfig::default()
            },
        );
        // Credential state reads `std::env`, which is process-global, so pin
        // the variables this test inspects *before* the directory is built and
        // restore them afterwards: an ambient DEEPSEEK_API_KEY/ARK_API_KEY on
        // the host cannot flake it.
        let saved = [
            ("DEEPSEEK_API_KEY", std::env::var_os("DEEPSEEK_API_KEY")),
            ("ARK_API_KEY", std::env::var_os("ARK_API_KEY")),
        ];
        for (name, _) in &saved {
            unsafe { std::env::remove_var(name) };
        }
        let registry = ProviderRegistry::new(None);
        let rows = registry.list_configurable_providers(&config);
        let routes: Vec<&str> = rows.iter().map(|row| row.route.as_str()).collect();
        // Catalog routes in presentation order first, then declared routes.
        assert!(routes.starts_with(&["openai", "deepseek", "ollama", "minimax", "anthropic"]));
        assert!(routes.contains(&"ark"));
        let deepseek = rows.iter().find(|row| row.route == "deepseek").unwrap();
        assert_eq!(deepseek.protocol, Protocol::OpenAi);
        assert!(!deepseek.configured);
        let ark = rows.iter().find(|row| row.route == "ark").unwrap();
        assert!(ark.configured);
        // ollama's catalog entry has an empty api_key_env and no key command:
        // keyless regardless of the environment.
        let ollama = rows.iter().find(|row| row.route == "ollama").unwrap();
        assert_eq!(ollama.credential_state, CredentialState::Keyless);
        // deepseek needs DEEPSEEK_API_KEY (catalog default) and the declared
        // ark row needs ARK_API_KEY; both are unset here, so both are missing.
        assert_eq!(deepseek.credential_state, CredentialState::Missing);
        assert_eq!(ark.credential_state, CredentialState::Missing);
        for (name, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    #[test]
    fn build_dispatches_by_protocol() {
        let registry = ProviderRegistry::new(None);
        let mock = crate::config::resolve(
            &Config::default(),
            &crate::config::Overrides::default(),
            &|_| None,
        )
        .unwrap();
        assert!(registry.build(&mock).is_ok());
    }
}
