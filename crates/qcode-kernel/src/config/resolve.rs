use super::*;

#[derive(Debug, Default)]
pub struct Overrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub stream_usage: Option<bool>,
}

/// 一个 route 的认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 普通 API key（env / key command / keyless）。
    ApiKey,
    /// 太极统一认证（public client + PKCE，token 生命周期由 TjProvider 管理）。
    TjOAuth,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    /// `env NAME`, `command NAME`, `keyless`, or `tj-auth`, for doctor output.
    pub api_key_source: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub stream_usage: bool,
    pub is_mock: bool,
    /// Wire protocol driving the adapter family; the config's `protocol`
    /// override wins, then the catalog route's, then OpenAI-compatible.
    pub protocol: Protocol,
    /// How this route authenticates; the adapter family is chosen by this.
    pub auth_mode: AuthMode,
    /// Provider-specific body fields, already merged with the builtin defaults.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Whether the model accepts image input. The route decides (catalog) and
    /// `providers.<route>.images` overrides it; the engine degrades images to a
    /// text placeholder when this is false (ADR-0061 §8).
    pub images: bool,
    /// Deployment-level request headers (gateway auth, custom headers).
    pub headers: std::collections::BTreeMap<String, String>,
}

/// Default models of the builtin providers, for forms that need to tell a
/// stale value (left over from another provider) apart from a real choice.
pub fn builtin_models() -> Vec<(&'static str, &'static str)> {
    ["openai", "deepseek", "ollama", "anthropic", "minimax"]
        .into_iter()
        .filter_map(|name| catalog_entry(name).map(|c| (name, c.default_model.unwrap_or(""))))
        .filter(|(_, model)| !model.is_empty())
        .collect()
}

/// Resolve the price table for a model and validate the budget pair.
///
/// `--max-cost` without a price entry is a hard error: silently treating the
/// cost as zero would make the flag a no-op.
pub fn pricing_for(
    config: &Config,
    model: &str,
    max_cost: Option<f64>,
) -> Result<Option<Pricing>, ConfigError> {
    if let Some(limit) = max_cost
        && !(limit.is_finite() && limit >= 0.0)
    {
        return Err(ConfigError::InvalidMaxCost);
    }

    match config.pricing.get(model) {
        Some(pricing) => {
            if !pricing.validate() {
                return Err(ConfigError::InvalidPricing {
                    model: model.to_string(),
                });
            }
            Ok(Some(*pricing))
        }
        None if max_cost.is_some() => Err(ConfigError::PricingRequired {
            model: model.to_string(),
        }),
        None => Ok(None),
    }
}

/// The mock wire-protocol routes: never a real provider, and never picked as
/// the "first usable" default.
fn is_mock_name(name: &str) -> bool {
    matches!(
        name,
        "mock"
            | "mock-tools"
            | "mock-approval"
            | "mock-todo"
            | "mock-subagent"
            | "mock-ask"
            | "mock-schedule"
    )
}

/// Pick the first usable provider from `config.providers` as the fallback
/// default (V2.1: "第一个可用 provider 的第一个模型").
///
/// Candidates are iterated in `BTreeMap` order (alphabetical), mock routes
/// excluded. A provider is usable when it carries a legal http(s) base URL, a
/// resolvable credential (env key, key command, or keyless), and a derivable
/// model (`default_model`, the catalog default, or `models[0]`). Returns
/// `None` when no provider qualifies, letting the caller fall back to mock.
fn first_usable_provider(config: &Config, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    config
        .providers
        .keys()
        .find(|name| {
            if is_mock_name(name) {
                return false;
            }
            let Some(entry) = config.providers.get(*name) else {
                return false;
            };
            let catalog = catalog_entry(name);
            // 套餐 route 的端点/密钥 env 默认值在 package 表里（catalog 没有），
            // 与 `resolve` 主路径的兜底顺序保持一致，否则省略了 base_url /
            // api_key_env、依赖套餐默认的 route 会在兜底被误判为不可用。
            let package = package_entry(name);
            // A legal endpoint must exist, from the config, the catalog, or the
            // package table.
            let Some(base_url) = entry
                .base_url
                .clone()
                .or_else(|| catalog.map(|catalog| catalog.base_url.to_string()))
                .or_else(|| package.map(|(_, pkg)| pkg.base_url.to_string()))
            else {
                return false;
            };
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                return false;
            }
            // A credential must resolve: an env key present, a key command, or
            // keyless (empty env name).
            let api_key_env = entry
                .api_key_env
                .clone()
                .or_else(|| catalog.map(|catalog| catalog.api_key_env.to_string()))
                .or_else(|| package.map(|(_, pkg)| pkg.api_key_env.to_string()))
                .unwrap_or_default();
            if !api_key_env.is_empty()
                && env(&api_key_env).is_none_or(|key| key.is_empty())
                && entry.api_key_command.is_none()
            {
                return false;
            }
            // A model must be derivable once resolved.
            entry.default_model.is_some()
                || catalog.is_some_and(|catalog| catalog.default_model.is_some())
                || package.is_some_and(|(_, pkg)| pkg.default_model.is_some())
                || !entry.models.is_empty()
        })
        .cloned()
}

/// 把 `[default-model]` / 遗留 `default_provider` 里指向的、已经不在
/// `config.providers` 中的 route 修掉。
///
/// 场景：设置页的套餐单选（同一厂商只留一个套餐行，见 `apply_tables` 的
/// 去重）会把旧套餐的 route 从 providers 里移走；若它恰好是默认，整份配置
/// 就再也 `resolve` 不了——设置页看着一切正常（它读的是表格与内存 config），
/// 主窗口却退化成 unconfigured（它读的是 `resolve`）。两边的状态读的不是
/// 同一个东西，就是这个差异。
///
/// 策略：默认 route 不在了，先找同一厂商剩下的套餐行接棒（切套餐的语义就是
/// 默认跟着厂商走），并把模型换成新行可推导的第一个；推导不出来或没有同厂商
/// 行就清掉默认，让 `resolve` 兜底到 `first_usable_provider`。清理之后仍没有
/// 任何默认、且只有唯一一个 provider 时，自动把它钉为默认。
pub fn settle_default_provider(config: &mut Config) {
    // 一个 route 被移出 providers 后还能不能自己解析：配置行还在、或目录里有
    // 默认模型（端点目录必有）都算；套餐与自定义 route 没有目录兜底，移走即
    // 失效。目录 route 的默认不该被这次自愈动到——它本来就解析得了。
    let self_sufficient = |route: &str| {
        config.providers.contains_key(route)
            || catalog_entry(route)
                .map(|catalog| catalog.default_model.is_some())
                .unwrap_or(false)
    };
    let default_missing = config
        .default_model
        .provider
        .as_deref()
        .is_some_and(|route| !self_sufficient(route));
    let legacy_missing = config
        .default_provider
        .as_deref()
        .is_some_and(|route| !self_sufficient(route));
    if default_missing {
        let stale = config.default_model.provider.as_deref();
        let replacement = same_vendor_replacement(config, stale);
        match replacement
            .and_then(|route| first_derivable_model(config, &route).map(|model| (route, model)))
        {
            Some((route, model)) => {
                config.default_model.provider = Some(route);
                config.default_model.model = Some(model);
            }
            None => {
                config.default_model.provider = None;
                config.default_model.model = None;
            }
        }
    }
    if legacy_missing {
        config.default_provider = None;
    }
    // 清理之后仍没有任何默认时，若只有一个「已配置模型」的 Provider 就自动
    // 把它钉为默认：用户添加第一个 Provider 后无需手动「设为默认」。判据与
    // 设置页/主窗口的列表过滤一致（`configured_models`：models 非空或
    // default_model 已设）——残留的、没配置模型的 provider 行（如 ollama /
    // openai 空行）不算数，否则「唯一可用 Provider」会被它们挡住。只在该
    // provider 能推导出模型时钉 model（否则 resolve 会因缺模型失败），并清掉
    // 遗留 `default_provider` 避免双默认。
    //
    // 注意必须在清理**之后**判断：残留的坏默认（如指向已删 route）若先被
    // 读到会令 `default_model.provider` 非 None，唯一的 provider 就漏掉了。
    if config.default_model.provider.is_none() {
        let configured = config
            .providers
            .iter()
            .filter(|(_, entry)| !entry.models.is_empty() || entry.default_model.is_some())
            .map(|(route, _)| route)
            .collect::<Vec<_>>();
        if configured.len() == 1 {
            let route = configured[0];
            if let Some(model) = first_derivable_model(config, route) {
                config.default_model.provider = Some(route.clone());
                config.default_model.model = Some(model);
                config.default_provider = None;
            }
        }
    }
}

/// 同厂商剩下的套餐行：`route` 属于某个套餐厂商时，返回 `providers` 里
/// 该厂商仍在的行（`BTreeMap` 顺序，第一个），否则 `None`。
fn same_vendor_replacement(config: &Config, route: Option<&str>) -> Option<String> {
    let group = route.and_then(|route| package_entry(route).map(|(group, _)| group))?;
    config
        .providers
        .keys()
        .find(|candidate| package_entry(candidate).is_some_and(|(g, _)| g.vendor == group.vendor))
        .cloned()
}

/// 一个 provider 自己可推导的第一个模型：行 `default_model` → 行模型列表
/// 第一个 → 套餐默认 → 目录默认。与 `resolve` 的模型推导顺序一致。
fn first_derivable_model(config: &Config, route: &str) -> Option<String> {
    let entry = config.providers.get(route)?;
    entry
        .default_model
        .clone()
        .or_else(|| entry.models.first().map(|model| model.id.clone()))
        .or_else(|| package_entry(route).and_then(|(_, pkg)| pkg.default_model.map(str::to_string)))
        .or_else(|| {
            catalog_entry(route).and_then(|catalog| catalog.default_model.map(str::to_string))
        })
}

pub fn resolve(
    config: &Config,
    overrides: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Resolved, ConfigError> {
    let name = overrides
        .provider
        .clone()
        .or_else(|| env("QCODE_PROVIDER"))
        // `[default-model].provider` is the authoritative default; the legacy
        // `default_provider` only applies when no `[default-model]` is set.
        .or_else(|| config.default_model.provider.clone())
        .or_else(|| config.default_provider.clone())
        .unwrap_or_else(|| {
            first_usable_provider(config, env).unwrap_or_else(|| "mock".to_string())
        });

    if is_mock_name(&name) {
        let model = overrides
            .model
            .clone()
            .or_else(|| env("QCODE_MODEL"))
            .unwrap_or_else(|| "mock-1".to_string());
        return Ok(Resolved {
            name,
            api_key_source: "mock".to_string(),
            base_url: None,
            api_key: None,
            model,
            stream_usage: false,
            is_mock: true,
            protocol: Protocol::Mock,
            auth_mode: AuthMode::ApiKey,
            extra_body: serde_json::Map::new(),
            images: false,
            headers: std::collections::BTreeMap::new(),
        });
    }

    let entry = config.providers.get(&name).cloned().unwrap_or_default();
    let catalog = catalog_entry(&name);
    // Builtin defaults first, then whatever the config says: a user can always
    // turn a field off.
    let mut extra_body = serde_json::Map::new();
    if let Some(catalog) = &catalog
        && catalog.reasoning_split
    {
        extra_body.insert("reasoning_split".to_string(), serde_json::json!(true));
    }
    for (key, value) in &entry.extra_body {
        extra_body.insert(key.clone(), value.clone());
    }

    let protocol = entry
        .protocol
        .or_else(|| catalog.map(|catalog| catalog.protocol))
        .unwrap_or(Protocol::OpenAi);

    // Image input is a property of the deployment, not of the wire protocol:
    // DeepSeek answers images over its OpenAI-compatible endpoint *and* its
    // Anthropic-compatible one, while a self-hosted gateway in front of a
    // text-only model does neither. So: catalog first, config overrides, and
    // an unknown route is text-only (the engine then says so in the prompt
    // instead of letting the server 400).
    let images = entry
        .images
        .or_else(|| catalog.map(|catalog| catalog.images))
        .unwrap_or(false);

    let base_url = overrides
        .base_url
        .clone()
        .or_else(|| env("QCODE_BASE_URL"))
        .or_else(|| entry.base_url.clone())
        .or_else(|| catalog.map(|catalog| catalog.base_url.to_string()))
        // 套餐 provider：config 缺 base_url 时用套餐默认端点兜底（旧版自动
        // 保存可能把空值写盘，导致 config 里没有 base_url）。
        .or_else(|| crate::config::package_entry(&name).map(|(_, pkg)| pkg.base_url.to_string()))
        // 太极统一认证的默认模型请求地址（需求 4c）。
        .or_else(|| {
            crate::providers::tj::is_tj_route(&name)
                .then(|| crate::providers::tj::TJ_BASE_URL.to_string())
        })
        .ok_or_else(|| ConfigError::UnknownProvider(name.clone()))?;

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ConfigError::InvalidBaseUrl {
            provider: name,
            url: base_url,
        });
    }

    let auth_mode = if crate::providers::tj::is_tj_route(&name) {
        AuthMode::TjOAuth
    } else {
        AuthMode::ApiKey
    };

    let api_key_env = entry
        .api_key_env
        .clone()
        .or_else(|| catalog.map(|catalog| catalog.api_key_env.to_string()))
        .or_else(|| crate::config::package_entry(&name).map(|(_, pkg)| pkg.api_key_env.to_string()))
        .unwrap_or_default();

    let command = entry.api_key_command.as_deref();
    // 太极统一认证不走 env/key command：token 由 TjProvider 从 tj_auth.json
    // 与授权码流程管理（PROVIDER-PACKAGES-TJ-AUTH §4）。
    let (api_key, api_key_source) = if auth_mode == AuthMode::TjOAuth {
        (None, "tj-auth".to_string())
    } else if api_key_env.is_empty() {
        match command {
            Some(argv) => (Some(key_from_command(&name, argv)?), command_source(argv)),
            None => (None, "keyless".to_string()),
        }
    } else {
        match env(&api_key_env) {
            Some(key) if !key.is_empty() => (Some(key), format!("env {api_key_env}")),
            _ => match command {
                Some(argv) => (Some(key_from_command(&name, argv)?), command_source(argv)),
                None => {
                    return Err(ConfigError::MissingKey {
                        provider: name,
                        env: api_key_env,
                    });
                }
            },
        }
    };

    let model = overrides
        .model
        .clone()
        .or_else(|| env("QCODE_MODEL"))
        .or_else(|| {
            // `[default-model].model` is scoped to the provider it names (or to
            // whichever provider resolves when it names none), so a default
            // meant for one provider never silently replaces another route's.
            let applies = config
                .default_model
                .provider
                .as_deref()
                .is_none_or(|provider| provider == name.as_str());
            applies
                .then(|| config.default_model.model.clone())
                .flatten()
        })
        .or_else(|| entry.default_model.clone())
        .or_else(|| catalog.and_then(|catalog| catalog.default_model.map(str::to_string)))
        .or_else(|| {
            crate::config::package_entry(&name)
                .and_then(|(_, pkg)| pkg.default_model.map(str::to_string))
        })
        .or_else(|| entry.models.first().map(|model| model.id.clone()))
        .ok_or_else(|| ConfigError::MissingModel {
            provider: name.clone(),
        })?;

    Ok(Resolved {
        name,
        api_key_source,
        base_url: Some(base_url),
        api_key,
        model,
        stream_usage: overrides.stream_usage.unwrap_or(entry.stream_usage),
        is_mock: false,
        protocol,
        auth_mode,
        images,
        extra_body,
        headers: entry.headers.clone(),
    })
}
