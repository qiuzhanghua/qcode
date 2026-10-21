//! Concrete provider implementations.
//!
//! M0 ships a scripted mock plus an OpenAI-compatible streaming client that
//! covers OpenAI, DeepSeek, Ollama and vLLM.

pub mod anthropic;
pub mod mock;
pub mod openai_compat;
pub mod sse;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use mock::MockProvider;
pub use openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
