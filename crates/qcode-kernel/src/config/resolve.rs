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
    /// Provider-specific body fields, already merged with the builtin defaults.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

/// Default models of the builtin providers, for forms that need to tell a
/// stale value (left over from another provider) apart from a real choice.
pub fn builtin_models() -> Vec<(&'static str, &'static str)> {
    ["openai", "deepseek", "ollama", "anthropic", "minimax"]
        .into_iter()
        .filter_map(|name| builtin(name).map(|b| (name, b.default_model.unwrap_or(""))))
        .filter(|(_, model)| !model.is_empty())
        .collect()
}

struct Builtin {
    base_url: &'static str,
    api_key_env: &'static str,
    default_model: Option<&'static str>,
    /// MiniMax ships its thinking inside `content` unless asked to split it out.
    reasoning_split: bool,
}

fn builtin(name: &str) -> Option<Builtin> {
    match name {
        "openai" => Some(Builtin {
            base_url: "https://api.openai.com/v1",
            api_key_env: "OPENAI_API_KEY",
            default_model: None,
            reasoning_split: false,
        }),
        "deepseek" => Some(Builtin {
            base_url: "https://api.deepseek.com/v1",
            api_key_env: "DEEPSEEK_API_KEY",
            default_model: Some("deepseek-flash"),
            reasoning_split: false,
        }),
        "ollama" => Some(Builtin {
            base_url: "http://localhost:11434/v1",
            api_key_env: "",
            default_model: None,
            reasoning_split: false,
        }),
        // MiniMax exposes an OpenAI-compatible Chat Completions API, so it needs
        // no new provider code, only the endpoint and key variable.
        // MiniMax exposes an OpenAI-compatible Chat Completions API, so it needs
        // no new provider code, only the endpoint and key variable. The default
        // host is the mainland platform; international accounts override
        // `base_url` to https://api.minimax.io/v1.
        "minimax" => Some(Builtin {
            base_url: "https://api.minimaxi.com/v1",
            api_key_env: "MINIMAX_API_KEY",
            default_model: Some("MiniMax-M3"),
            reasoning_split: true,
        }),
        // Anthropic Messages API. `claude` is accepted as a friendlier alias;
        // both resolve to the same provider and key.
        "anthropic" | "claude" => Some(Builtin {
            base_url: "https://api.anthropic.com",
            api_key_env: "ANTHROPIC_API_KEY",
            default_model: Some("claude-sonnet-4-5"),
            reasoning_split: false,
        }),
        _ => None,
    }
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

pub fn resolve(
    config: &Config,
    overrides: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Resolved, ConfigError> {
    let name = overrides
        .provider
        .clone()
        .or_else(|| env("QCODE_PROVIDER"))
        .or_else(|| config.default_provider.clone())
        .unwrap_or_else(|| "mock".to_string());

    if name == "mock"
        || name == "mock-tools"
        || name == "mock-approval"
        || name == "mock-todo"
        || name == "mock-subagent"
    {
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
            extra_body: serde_json::Map::new(),
        });
    }

    let entry = config.providers.get(&name).cloned().unwrap_or_default();
    let builtin = builtin(&name);
    // Builtin defaults first, then whatever the config says: a user can always
    // turn a field off.
    let mut extra_body = serde_json::Map::new();
    if let Some(builtin) = &builtin
        && builtin.reasoning_split
    {
        extra_body.insert("reasoning_split".to_string(), serde_json::json!(true));
    }
    for (key, value) in &entry.extra_body {
        extra_body.insert(key.clone(), value.clone());
    }

    let base_url = overrides
        .base_url
        .clone()
        .or_else(|| env("QCODE_BASE_URL"))
        .or_else(|| entry.base_url.clone())
        .or_else(|| builtin.as_ref().map(|builtin| builtin.base_url.to_string()))
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
        .or_else(|| {
            builtin
                .as_ref()
                .map(|builtin| builtin.api_key_env.to_string())
        })
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
        .or_else(|| entry.default_model.clone())
        .or_else(|| {
            builtin
                .as_ref()
                .and_then(|builtin| builtin.default_model.map(str::to_string))
        })
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
        extra_body,
    })
}
