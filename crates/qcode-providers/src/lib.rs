//! Concrete provider implementations.
//!
//! M0 ships a scripted mock plus an OpenAI-compatible streaming client that
//! covers OpenAI, DeepSeek, Ollama and vLLM.

pub mod anthropic;
pub mod discovery;
pub mod mock;
pub mod oauth2;
pub mod openai_compat;
pub mod sse;
pub mod tj;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use discovery::discover_models;
pub use mock::MockProvider;
pub use oauth2::{
    OAuth2AuthClient, OAuth2AuthSettings, OAuth2Mode, OAuth2Provider, OAuth2ProviderConfig,
};
pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
pub use tj::{
    AuthPrompt, TjAuthClient, TjAuthSettings, TjProvider, TjProviderConfig, TjTokenStore,
};
