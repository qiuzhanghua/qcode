//! Declarative catalog of built-in provider routes (M0 of SETTINGS-MODELS-V2).
//!
//! Replaces the hard-coded `builtin()` match in `resolve.rs`: every built-in
//! provider is one row in [`CATALOG`], carrying its wire protocol, endpoint,
//! credential reference, default model and model list. A route's resolution
//! falls back to its catalog row when the user config has no explicit value, so
//! the settings page and the resolver share one fact source.

use serde::Deserialize;

/// Wire protocol a provider route speaks. The adapter family is chosen by this,
/// not by the provider name (`build_provider`'s previous match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Protocol {
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "mock")]
    Mock,
}

impl Protocol {
    /// The config-file spelling of this protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Mock => "mock",
        }
    }

    /// Display label for configuration surfaces.
    ///
    /// 返回 `Cow` 而不是 `&'static str`：`OpenAI 兼容` 是**兼容性描述**，要翻；
    /// `Anthropic` / `Mock` 是产品名，两种语言下都原样。所以只有一支会分配。
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Protocol::OpenAi => qcode_i18n::tr("kernel.protocol.openai_compat"),
            Protocol::Anthropic => "Anthropic".into(),
            Protocol::Mock => "Mock".into(),
        }
    }
}

/// Provider 安全级别（按数据去向粗分三档）。每个 Provider 都必须有标识：
/// 太极内网与可配置 OAuth2 统一认证最安全，国内平台（火山、小米、DeepSeek
/// 等）比较安全，国外平台（OpenAI、Anthropic 等）不安全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityLevel {
    /// 最安全：太极内网，数据不出内网。
    Safest,
    /// 比较安全：国内平台，数据留在境内。
    Safe,
    /// 不安全：国外平台，数据出境。
    Unsafe,
}

impl SecurityLevel {
    /// Display label for configuration surfaces.
    ///
    /// 是界面文案（跟 locale 走），不是数据值，所以返回 `Cow` 而不是静态字符串。
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            SecurityLevel::Safest => qcode_i18n::tr("kernel.security.safest"),
            SecurityLevel::Safe => qcode_i18n::tr("kernel.security.safe"),
            SecurityLevel::Unsafe => qcode_i18n::tr("kernel.security.unsafe"),
        }
    }
}

/// One built-in provider route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Provider route key (`providers.<route>` in the config).
    pub route: &'static str,
    /// Name shown by configuration surfaces.
    ///
    /// **不翻译，故意如此。** 这里是厂商产品名（`小米 MiMo`、`智谱 Coding`、
    /// `通义 Token Plan`、`蚂蚁 Ling`），与 `Anthropic` 同级：英文界面写「小米 MiMo」
    /// 才是对的，译成 "Xiaomi MiMo" 等于替厂商改名。所以这几条会**永久留在门禁 2
    /// 的基线里**。基线只能表达「还存在」、不能表达「故意留着」，理由就落在这里 ——
    /// 将来有人想把它们清零，会先读到这段。
    pub display_name: &'static str,
    /// Wire protocol every model on this route speaks.
    pub protocol: Protocol,
    /// Default endpoint; a user `base_url` overrides it.
    pub base_url: &'static str,
    /// Default credential reference; empty means keyless (e.g. local Ollama).
    pub api_key_env: &'static str,
    /// Default model when the route carries none.
    pub default_model: Option<&'static str>,
    /// MiniMax ships its thinking inside `content` unless asked to split it out.
    pub reasoning_split: bool,
    /// Models this route advertises, for selection surfaces.
    pub models: &'static [&'static str],
    /// Whether this route's default models accept image input.
    ///
    /// This is a property of the *route*, not of the wire protocol: DeepSeek
    /// serves vision over both its Chat Completions and its Anthropic-compatible
    /// endpoint (verified 2026-11-11), while `claude` has always taken images.
    /// `providers.<route>.images` overrides it — a gateway may differ from the
    /// vendor it fronts, and a brand-new model may outrun this table.
    pub images: bool,
    /// 安全级别标识（最安全 / 比较安全 / 不安全），按厂商所在地与数据去向填。
    pub security: SecurityLevel,
}

/// The built-in provider catalog, in presentation order.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        route: "openai",
        display_name: "OpenAI",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        default_model: Some("gpt-5.4"),
        reasoning_split: false,
        models: &[
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.4-pro",
            "gpt-5.3-chat-latest",
            "gpt-5.2",
            "gpt-5.2-chat-latest",
            "gpt-5.2-pro",
            "gpt-5.1",
            "gpt-5",
            "gpt-5-mini",
            "gpt-5-pro",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "o4-mini",
            "o3",
            "o3-mini",
        ],
        images: true,
    },
    CatalogEntry {
        route: "deepseek",
        display_name: "DeepSeek",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://api.deepseek.com/v1",
        api_key_env: "DEEPSEEK_API_KEY",
        // What `GET https://api.deepseek.com/models` answers, as of 2026-11-10:
        // `deepseek-flash` (display name `DeepSeek-V4.1-Flash`) and
        // `deepseek-v4-pro`. The display name is not an id — typing it as one
        // gets a 400 from the API, which is exactly how this list went stale.
        default_model: Some("deepseek-flash"),
        reasoning_split: false,
        models: &["deepseek-flash", "deepseek-v4-pro"],
        images: true,
    },
    CatalogEntry {
        route: "ollama",
        display_name: "Ollama",
        protocol: Protocol::OpenAi,
        // 本地推理，数据不出本机；但按用户口径「只有太极内网是最安全的」，
        // 归为比较安全（数据不发送给任何第三方）。
        security: SecurityLevel::Safe,
        base_url: "http://localhost:11434/v1",
        api_key_env: "",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    // MiniMax exposes an OpenAI-compatible Chat Completions API, so it needs
    // no new provider code, only the endpoint and key variable. The default
    // host is the mainland platform; international accounts override
    // `base_url` to https://api.minimax.io/v1.
    CatalogEntry {
        route: "minimax",
        display_name: "MiniMax",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://api.minimaxi.com/v1",
        api_key_env: "MINIMAX_API_KEY",
        default_model: Some("MiniMax-M3"),
        reasoning_split: true,
        models: &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"],
        images: false,
    },
    // Anthropic Messages API. `claude` is accepted as a friendlier alias;
    // both resolve to the same provider and key.
    CatalogEntry {
        route: "anthropic",
        display_name: "Anthropic",
        protocol: Protocol::Anthropic,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.anthropic.com",
        api_key_env: "ANTHROPIC_API_KEY",
        default_model: Some("claude-sonnet-4-5"),
        reasoning_split: false,
        models: &[
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-haiku-4-5",
            "claude-fable-5",
        ],
        images: true,
    },
    // 以下条目按 deepseek-harness 的 pi-ai 目录补充（2026-11-19 核对）：端点
    // 与 key 环境变量取自 pi-ai provider-data，协议均为 OpenAI 兼容（google
    // 走官方 OpenAI 兼容端点 /v1beta/openai，mistral / xai 的官方 OpenAI
    // 兼容路径）。模型清单留空，由设置页「获取可用模型」探测填充——探测不到
    // 的网关可手动添加，与 ollama 同理；模型 id 变化快，静态清单只会过时。
    CatalogEntry {
        route: "openrouter",
        display_name: "OpenRouter",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "google",
        display_name: "Google Gemini",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        api_key_env: "GEMINI_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: true,
    },
    CatalogEntry {
        route: "groq",
        display_name: "Groq",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: "GROQ_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "mistral",
        display_name: "Mistral",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.mistral.ai/v1",
        api_key_env: "MISTRAL_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "xai",
        display_name: "xAI",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.x.ai/v1",
        api_key_env: "XAI_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "moonshotai",
        display_name: "Moonshot (Kimi)",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://api.moonshot.ai/v1",
        api_key_env: "MOONSHOT_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "zai",
        display_name: "Z.ai",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        // 按量计费端点（/api/paas/v4）。Z.ai 的 Coding Plan 走 /api/coding/paas/v4，
        // 与套餐表 `zai` = 按量计费保持一致——resolve 时 catalog 优先于套餐表，
        // 这里错了会把按量的 key 打到 Coding 端点上。
        base_url: "https://api.z.ai/api/paas/v4",
        api_key_env: "ZAI_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "zai-coding-cn",
        display_name: "智谱 Coding",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        api_key_env: "ZAI_CODING_CN_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "qwen-token-plan-cn",
        display_name: "通义 Token Plan",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        api_key_env: "QWEN_TOKEN_PLAN_CN_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "ant-ling",
        display_name: "蚂蚁 Ling",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://api.ant-ling.com/v1",
        api_key_env: "ANT_LING_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "xiaomi",
        display_name: "小米 MiMo",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safe,
        base_url: "https://api.xiaomimimo.com/v1",
        api_key_env: "XIAOMI_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "cerebras",
        display_name: "Cerebras",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.cerebras.ai/v1",
        api_key_env: "CEREBRAS_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "together",
        display_name: "Together AI",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.together.ai/v1",
        api_key_env: "TOGETHER_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "fireworks",
        display_name: "Fireworks AI",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://api.fireworks.ai/inference",
        api_key_env: "FIREWORKS_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "nvidia",
        display_name: "NVIDIA NIM",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://integrate.api.nvidia.com/v1",
        api_key_env: "NVIDIA_API_KEY",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    CatalogEntry {
        route: "huggingface",
        display_name: "Hugging Face",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Unsafe,
        base_url: "https://router.huggingface.co/v1",
        api_key_env: "HF_TOKEN",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    // 太极内网（PROVIDER-PACKAGES-TJ-AUTH §4）：public client + PKCE，
    // 没有 api_key_env（keyless）——token 由 TjProvider 从 tj_auth.json 与
    // 授权码流程管理。默认地址为需求 4c 指定的内网网关。
    CatalogEntry {
        route: crate::providers::tj::TJ_ROUTE,
        display_name: "太极内网",
        protocol: Protocol::OpenAi,
        // 唯一的最安全：数据不出内网（需求 4c 的太极统一认证网关）。
        security: SecurityLevel::Safest,
        base_url: crate::providers::tj::TJ_BASE_URL,
        api_key_env: "",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
    // 可配置 OAuth2 统一认证：端点/客户端标识在设置页编辑（`providers.<route>.
    // oauth2`），base_url 由用户填（catalog 留空 → resolve 缺 base_url 报错，
    // 不默认成任何网关）。token 由 OAuth2Provider 从 oauth2_auth.json 与授权
    // 流程管理，没有 api_key_env。与太极内网同级的统一认证网关（授权码/PKCE +
    // Bearer token 闭环），安全级别与太极一致（最安全）——模型选择按钮的配色
    // 跟着它走。
    CatalogEntry {
        route: crate::providers::oauth2::OAUTH2_ROUTE,
        display_name: "OAuth2",
        protocol: Protocol::OpenAi,
        security: SecurityLevel::Safest,
        base_url: "",
        api_key_env: "",
        default_model: None,
        reasoning_split: false,
        models: &[],
        images: false,
    },
];

/// Every built-in catalog route.
pub fn catalog_entries() -> impl Iterator<Item = &'static CatalogEntry> {
    CATALOG.iter()
}

/// The catalog entry for one route, when the catalog ships it.
///
/// `claude` is the friendlier alias of the `anthropic` route; both look up the
/// same entry so the resolver and the settings surface agree on one provider.
pub fn catalog_entry(route: &str) -> Option<&'static CatalogEntry> {
    let route = match route {
        "claude" => "anthropic",
        other => other,
    };
    CATALOG.iter().find(|entry| entry.route == route)
}

/// The catalog's static model list for one route, as editable draft rows
/// (U0). Catalog models carry only an id; name and capacities are filled in by
/// the probe or left to the provider's defaults.
pub fn catalog_models(route: &str) -> Vec<super::ModelEntry> {
    catalog_entry(route)
        .map(|entry| {
            entry
                .models
                .iter()
                .map(|id| super::ModelEntry {
                    id: (*id).to_string(),
                    ..super::ModelEntry::default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 一个 route 的安全级别，**保证每个 Provider 都有标识**：
/// - 套餐表（厂商级）优先：同一厂商的所有套餐共享一个级别（`PackageGroup.security`）；
/// - 其次 catalog 条目（太极内网与 OAuth2 统一认证 = 最安全、国内平台 = 比较安全、
///   国外 = 不安全）；
/// - 其余手写 route 默认「比较安全」——自定义网关多为境内自建/国内平台
///   （如火山方舟），与 `Protocol` 对未知 route 的默认同理。
///
/// 设置页编辑弹框（Provider 名字后面的灰色标识）与目录行共用这一个口径。
pub fn security_level(route: &str) -> SecurityLevel {
    if let Some((group, _)) = super::package_entry(route) {
        return group.security;
    }
    catalog_entry(route)
        .map(|entry| entry.security)
        .unwrap_or(SecurityLevel::Safe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_gateways_are_safest() {
        // 用户口径：只有统一认证网关（太极内网、可配置 OAuth2）是最安全的。
        // 钉死这条不变量，防止将来把某个国内平台误标成 Safest。
        for entry in catalog_entries() {
            let trusted = matches!(
                entry.route,
                crate::providers::tj::TJ_ROUTE | crate::providers::oauth2::OAUTH2_ROUTE
            );
            assert_eq!(
                entry.security == SecurityLevel::Safest,
                trusted,
                "只有太极内网与 OAuth2 是最安全的（{} 不是）",
                entry.route
            );
        }
    }

    #[test]
    fn security_level_covers_every_route() {
        // 太极内网与 OAuth2 统一认证 = 最安全；国内平台 = 比较安全；国外平台 =
        // 不安全；手写 route 落到「比较安全」默认；套餐 route 取厂商级。
        assert_eq!(
            security_level(crate::providers::tj::TJ_ROUTE),
            SecurityLevel::Safest
        );
        assert_eq!(
            security_level(crate::providers::oauth2::OAUTH2_ROUTE),
            SecurityLevel::Safest,
            "OAuth2 统一认证与太极同级（最安全）"
        );
        assert_eq!(security_level("deepseek"), SecurityLevel::Safe);
        assert_eq!(security_level("xiaomi"), SecurityLevel::Safe);
        assert_eq!(security_level("volcano-coding"), SecurityLevel::Safe);
        assert_eq!(security_level("openai"), SecurityLevel::Unsafe);
        assert_eq!(security_level("anthropic"), SecurityLevel::Unsafe);
        assert_eq!(
            security_level("claude"),
            SecurityLevel::Unsafe,
            "claude 是 anthropic 的别名"
        );
        assert_eq!(security_level("my-gateway"), SecurityLevel::Safe);
    }
}
