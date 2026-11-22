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
}

/// The built-in provider catalog, in presentation order.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        route: "openai",
        display_name: "OpenAI",
        protocol: Protocol::OpenAi,
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
        base_url: "https://api.z.ai/api/coding/paas/v4",
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
        base_url: "https://router.huggingface.co/v1",
        api_key_env: "HF_TOKEN",
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
