//! Concrete provider implementations.
//!
//! M0 ships only a scripted mock used by tests and by `coder run --provider mock`.
//! The OpenAI-compatible streaming client lands in M0-8.

pub mod mock;

pub use mock::MockProvider;
