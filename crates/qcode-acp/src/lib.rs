//! `qc acp`: the Agent Client Protocol server.
//!
//! The agent speaks ACP v1 over stdio so editors (Zed, Neovim, …) can drive the
//! same engine the CLI and TUI use. The crate is transport-free apart from
//! [`stdio`]; [`server::Client`] is the only outward port, which is what the
//! tests substitute.

pub mod jsonrpc;
pub mod protocol;
pub mod server;
pub mod stdio;

pub use protocol::PROTOCOL_VERSION;
pub use server::{Agent, Client, Deps, RpcError};
