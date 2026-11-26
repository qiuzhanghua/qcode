//! Provider route registry (M0 of SETTINGS-MODELS-V2).
//!
//! Owns route → adapter construction: given a resolved route it builds the
//! adapter for the route's wire [`Protocol`] (OpenAI-compatible, Anthropic or
//! mock), replacing `build_provider`'s previous name-based match. It also
//! answers the configurable-provider directory the settings page renders.

use qcode_core::event::Usage;
use qcode_core::provider::{Provider, StopReason, StreamEvent};
use qcode_providers::{
    AnthropicConfig, AnthropicProvider, MockProvider, OpenAiCompatConfig, OpenAiCompatProvider,
    TjAuthSettings, TjProvider, TjProviderConfig,
};

use crate::config::{
    AuthMode, Config, ModelEntry, Protocol, ProviderConfig, SecurityLevel, catalog_entries,
    catalog_entry,
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
    /// 套餐名（`按量计费` / `Coding Plan` / …）——该 route 属于某个厂商的
    /// 套餐时存在。完整显示名由 [`Self::label`] 派生：厂商名 (套餐名)。
    pub package_label: Option<&'static str>,
    /// Wire protocol this route speaks (catalog default or config override).
    pub protocol: Protocol,
    /// 安全级别标识（最安全 / 比较安全 / 不安全）。目录里的**每个** Provider
    /// 都有：套餐行取厂商级别，catalog 行取条目级别，手写 route 取默认。
    pub security: SecurityLevel,
    /// Whether the user/project config carries a profile for this route.
    pub configured: bool,
    /// Credential state of this route: key stored, key missing, or keyless.
    pub credential_state: CredentialState,
}

impl ConfigurableProvider {
    /// The display name for configuration surfaces.
    ///
    /// 套餐条目显示「厂商名 (套餐名)」（如 `火山引擎 (Coding Plan)`）；其余
    /// 条目直接显示厂商名（如 `DeepSeek`，不带 route）；手写 route 原样返回。
    pub fn label(&self) -> String {
        if let Some(package) = self.package_label {
            format!("{} ({})", self.display_name, package)
        } else {
            self.display_name.to_string()
        }
    }
}

/// The provider registry: catalog directory + route → adapter construction.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    /// Egress proxy URL passed to every adapter that reaches the network.
    proxy_url: Option<String>,
    /// `tj_auth.json` 的路径（太极统一认证）；None = 默认（~/.config/qcode/）。
    tj_store_path: Option<std::path::PathBuf>,
}

impl ProviderRegistry {
    /// Create a registry bound to one egress proxy (or none).
    pub fn new(proxy_url: Option<String>) -> Self {
        Self {
            proxy_url,
            tj_store_path: None,
        }
    }

    /// Set the `tj_auth.json` location (太极统一认证 token 存储）。
    pub fn with_tj_store(mut self, path: std::path::PathBuf) -> Self {
        self.tj_store_path = Some(path);
        self
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
                images: resolved.images,
            })?),
            // 太极统一认证：public client + PKCE，token 生命周期在 TjProvider
            // 内部闭环（ensure/refresh/401 重试）。
            //
            // 引擎侧不挂授权回调（prompt = None）：任务运行时未登录会直接报
            // 「太极内网未登录」，由界面错误横幅上的「太极内网登录」按钮
            // （`tj::refresh_or_login`，用全局回调）显式触发授权码流程，而不是
            // 每次跑任务都自动弹浏览器登录页。已登录但 token 过期时的静默
            // refresh 仍正常（那是「登录过、到期了」，不是「没登录」）。
            Protocol::OpenAi if resolved.auth_mode == AuthMode::TjOAuth => {
                Box::new(TjProvider::new(TjProviderConfig {
                    name: resolved.name.clone(),
                    base_url: resolved
                        .base_url
                        .clone()
                        .expect("non-mock provider always has a base URL"),
                    settings: tj_settings(),
                    store_path: self.tj_store_path.clone().unwrap_or_else(default_tj_store),
                    prompt: None,
                    proxy_url: self.proxy_url.clone(),
                    stream_usage: resolved.stream_usage,
                    extra_body: resolved.extra_body.clone(),
                    headers: resolved.headers.clone(),
                    images: resolved.images,
                })?)
            }
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
                images: resolved.images,
            })?),
        })
    }

    /// The configurable-provider directory: catalog routes first (presentation
    /// order), then the package table's routes（套餐，catalog 同 route 者由套餐表
    /// 取代并带套餐标识）, then configured routes neither table ships.
    pub fn list_configurable_providers(&self, config: &Config) -> Vec<ConfigurableProvider> {
        let package_routes: Vec<&'static str> = crate::config::package_routes().collect();
        let is_package_route = |route: &str| package_routes.contains(&route);
        let mut rows = Vec::new();
        for entry in catalog_entries() {
            // 该 route 由套餐表提供（带套餐标识），catalog 版本让位。
            if is_package_route(entry.route) {
                continue;
            }
            // 太极统一认证的凭证状态由 `tj_auth.json` 是否有有效 token 决定，
            // 不走 env/key command（需求 4c：无钥匙串逻辑）。
            let credential_state = if crate::providers::tj::is_tj_route(entry.route) {
                tj_credential_state()
            } else {
                credential_state(config.providers.get(entry.route), Some(entry.api_key_env))
            };
            rows.push(ConfigurableProvider {
                route: entry.route.to_string(),
                display_name: entry.display_name.to_string(),
                package_label: None,
                protocol: entry.protocol,
                security: entry.security,
                configured: config.providers.contains_key(entry.route),
                credential_state,
            });
        }
        for group in crate::config::package_groups() {
            for pkg in group.packages {
                rows.push(ConfigurableProvider {
                    route: pkg.route.to_string(),
                    display_name: group.display_name.to_string(),
                    package_label: Some(pkg.label),
                    protocol: pkg.protocol,
                    security: group.security,
                    configured: config.providers.contains_key(pkg.route),
                    credential_state: credential_state(
                        config.providers.get(pkg.route),
                        Some(pkg.api_key_env),
                    ),
                });
            }
        }
        for (route, profile) in &config.providers {
            if catalog_entry(route).is_some() || is_package_route(route.as_str()) {
                continue;
            }
            rows.push(ConfigurableProvider {
                route: route.clone(),
                display_name: route.clone(),
                package_label: None,
                protocol: profile.protocol.unwrap_or(Protocol::OpenAi),
                security: crate::config::security_level(route),
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
            // The scripts below feed the approval demos. `run_shell` only
            // prompts when it asks to widen its write scope (SANDBOX-MODE.md),
            // so the demo call carries an escalation request — a plain `echo`
            // would run unprompted and the demos would have nothing to show.
            "mock-approval" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "approval-1".to_string(),
                        name: "run_shell".to_string(),
                        arguments: serde_json::json!({
                            "command": "echo qcode-approval-demo",
                            "sandbox_permissions": "full_access",
                            "justification": "approval demo: the mock asks to widen its write scope"
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta(
                        qcode_i18n::tr("kernel.mock.approval.closing").to_string(),
                    ),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            // Points the tool loop at `ask_user`. Exists so the headless wiring
            // in `qcode run` can be tested end to end: with no TTY the runner
            // wires no questioner at all, and the tool has to say so instead of
            // hanging (crates/qcode/tests/ask_headless.rs). `--yes` must not
            // change that — it auto-approves tool *permissions*, and there is
            // no sensible way to auto-answer a question aimed at a human.
            "mock-ask" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "ask-1".to_string(),
                        name: "ask_user".to_string(),
                        arguments: serde_json::json!({
                            "questions": [{
                                "id": "q1",
                                "question": qcode_i18n::tr("kernel.mock.approval.ask")
                            }]
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta(qcode_i18n::tr("kernel.mock.ask.closing").to_string()),
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
                    StreamEvent::TextDelta(qcode_i18n::tr("kernel.mock.tools.closing").to_string()),
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
                            // 三条待办是「假模型决定的内容」，跟界面语言走；状态
                            // 是工具契约的一部分，不动。
                            "items": [
                                {
                                    "content": qcode_i18n::tr("kernel.mock.todo.items.spec"),
                                    "status": "completed"
                                },
                                {
                                    "content": qcode_i18n::tr("kernel.mock.todo.items.implement"),
                                    "status": "in_progress"
                                },
                                {
                                    "content": qcode_i18n::tr("kernel.mock.todo.items.verify")
                                }
                            ]
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta(qcode_i18n::tr("kernel.mock.todo.closing").to_string()),
                    StreamEvent::Ended {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            // 一次 `schedule_create` + 收尾文本。存在的理由和 `mock-todo` 一样：
            // 端到端判定「工具真的接上了、结果真的回到模型」，而不是只测工具本体
            // （ADR-0088 的验收要求：`qc run` / TUI 里登记必须成功并说明驱动）。
            "mock-schedule" => MockProvider::scripted(vec![
                vec![
                    StreamEvent::Started,
                    StreamEvent::ToolCallDone {
                        call_id: "sched-1".to_string(),
                        name: "schedule_create".to_string(),
                        arguments: serde_json::json!({
                            "title": "daily-report",
                            "prompt": qcode_i18n::tr("kernel.mock.schedule.prompt"),
                            "rule": "daily 09:00",
                            "target": "new",
                            "cwd": "/tmp"
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta(
                        qcode_i18n::tr("kernel.mock.schedule.closing").to_string(),
                    ),
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
                            "task": qcode_i18n::tr("kernel.mock.subagent.task"),
                            "max_turns": 4
                        }),
                    },
                    StreamEvent::Ended {
                        stop_reason: StopReason::ToolUse,
                    },
                ],
                vec![
                    StreamEvent::Started,
                    StreamEvent::TextDelta(
                        qcode_i18n::tr("kernel.mock.subagent.child").to_string(),
                    ),
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
                    StreamEvent::TextDelta(
                        qcode_i18n::tr("kernel.mock.subagent.closing").to_string(),
                    ),
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

/// kernel 侧硬编码的太极配置 → providers 层的 `TjAuthSettings`（依赖方向：
/// qcode-providers 不依赖 kernel，值由这里搬运）。
fn tj_settings() -> TjAuthSettings {
    let config = crate::providers::tj::TJ_AUTH;
    TjAuthSettings {
        auth_url: config.auth_url.to_string(),
        token_url: config.token_url.to_string(),
        userinfo_url: config.userinfo_url.to_string(),
        jwk_set_url: config.jwk_set_url.to_string(),
        client_id: config.client_id.to_string(),
        client_name: config.client_name.to_string(),
        redirect_uri: config.redirect_uri.to_string(),
    }
}

/// `tj_auth.json` 的默认位置：与 config.toml 同目录（`~/.config/qcode/`）。
/// GUI 删除太极内网 provider 时也要清掉这份 token（绿点/运行时读的是它）。
pub fn default_tj_store() -> std::path::PathBuf {
    qcode_core::real_home()
        .map(|home| home.join(".config/qcode/tj_auth.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("tj_auth.json"))
}

/// 太极统一认证的凭证状态：`tj_auth.json` 里有未过期的 access token 才算
/// Configured，否则 Missing（未登录）。不走 env / key command。
fn tj_credential_state() -> CredentialState {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let store = qcode_providers::TjTokenStore::new(default_tj_store());
    match store.load() {
        Ok(tokens)
            if !tokens.access_token.is_empty()
                && (tokens.expires_at == 0 || now < tokens.expires_at) =>
        {
            CredentialState::Configured
        }
        _ => CredentialState::Missing,
    }
}

/// Whether a route counts as "has models configured": a profile exists in
/// `config.providers` and its `models` list is non-empty or `default_model` is
/// set. Catalog built-in model lists do not count — the user made no choice.
/// Both the settings page and the main-window picker filter on this.
pub fn configured_models(route: &str, config: &Config) -> bool {
    let Some(profile) = config.providers.get(route) else {
        return false;
    };
    !profile.models.is_empty() || profile.default_model.is_some()
}

/// The effective model list of a route: `providers.<route>.models` overrides
/// the catalog, otherwise the catalog's built-in models are served (for a
/// provider whose `default_model` is set but whose `models` is empty).
pub fn effective_models(route: &str, config: &Config) -> Vec<ModelEntry> {
    match config.providers.get(route) {
        Some(profile) if !profile.models.is_empty() => profile.models.clone(),
        _ => crate::config::catalog_models(route),
    }
}

/// Credential state for one route (M3). Directory metadata only: it inspects
/// the env-var *name* and whether a key command is configured, never the key
/// itself. Precedence mirrors [`crate::config::resolve`]: the profile's
/// `api_key_env` wins, then the route's default env (`default_env`, from the
/// catalog row or the package table).
///
/// A key command counts as configured (it supplies the key at run time, e.g.
/// from Keychain/libsecret, and we cannot verify it here without running it).
/// An env-var name that is set and non-empty is configured; an unset one is
/// missing. No reference at all (or a profile explicitly blanking it) is
/// keyless, like the local Ollama and mock routes.
fn credential_state(
    profile: Option<&ProviderConfig>,
    default_env: Option<&str>,
) -> CredentialState {
    let profile = profile.cloned().unwrap_or_default();
    let env = profile
        .api_key_env
        .clone()
        .or_else(|| default_env.map(str::to_string))
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
    fn directory_serves_package_rows_with_labels() {
        // 套餐条目：display_name 是厂商名，package_label 是套餐名，label() 拼成
        // 「厂商名 (套餐名)」；凭证状态按套餐 api_key_env 判定（未设置 → Missing）。
        let registry = ProviderRegistry::new(None);
        let rows = registry.list_configurable_providers(&Config::default());
        let coding = rows
            .iter()
            .find(|row| row.route == "volcano-coding")
            .unwrap();
        assert_eq!(coding.display_name, "火山引擎");
        assert_eq!(coding.package_label, Some("Coding Plan"));
        assert_eq!(coding.label(), "火山引擎 (Coding Plan)");
        assert!(!coding.configured);
        assert_eq!(coding.credential_state, CredentialState::Missing);
        // xiaomi / zai 同时存在于 catalog 与套餐表：目录只出套餐表版本（带套餐
        // 标识），避免同一 route 重复两行。
        let xiaomi: Vec<_> = rows.iter().filter(|row| row.route == "xiaomi").collect();
        assert_eq!(xiaomi.len(), 1);
        assert_eq!(xiaomi[0].package_label, Some("按量计费"));
        assert_eq!(xiaomi[0].label(), "小米 (按量计费)");
        // catalog 普通条目不带套餐标识，label 直接显示厂商名（不带 route）。
        let deepseek = rows.iter().find(|row| row.route == "deepseek").unwrap();
        assert_eq!(deepseek.package_label, None);
        assert_eq!(deepseek.label(), "DeepSeek");
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
