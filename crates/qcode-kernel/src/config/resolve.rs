use super::*;

#[derive(Debug, Default)]
pub struct Overrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub stream_usage: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    /// `env NAME`, `command NAME`, or `keyless`, for doctor output.
    pub api_key_source: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub stream_usage: bool,
    pub is_mock: bool,
    /// Wire protocol driving the adapter family; the config's `protocol`
    /// override wins, then the catalog route's, then OpenAI-compatible.
    pub protocol: Protocol,
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
        "mock" | "mock-tools" | "mock-approval" | "mock-todo" | "mock-subagent"
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
            // A legal endpoint must exist, from the config or the catalog.
            let Some(base_url) = entry
                .base_url
                .clone()
                .or_else(|| catalog.map(|catalog| catalog.base_url.to_string()))
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
                || !entry.models.is_empty()
        })
        .cloned()
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
        .ok_or_else(|| ConfigError::UnknownProvider(name.clone()))?;

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ConfigError::InvalidBaseUrl {
            provider: name,
            url: base_url,
        });
    }

    let api_key_env = entry
        .api_key_env
        .clone()
        .or_else(|| catalog.map(|catalog| catalog.api_key_env.to_string()))
        .unwrap_or_default();

    let command = entry.api_key_command.as_deref();
    let (api_key, api_key_source) = if api_key_env.is_empty() {
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
        images,
        extra_body,
        headers: entry.headers.clone(),
    })
}
