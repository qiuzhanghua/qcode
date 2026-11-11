//! The Streamable HTTP probe server, for the kernel's egress test.
//!
//! Same file as `qcode-mcp`'s bin: `include!` rather than a copy, because two
//! implementations of "a minimal MCP HTTP server" would drift and the tests
//! would then be testing different servers. A Cargo bin path has to live inside
//! its own package, which is why this shim exists at all.

include!("../../../qcode-mcp/tests/support/http_probe_server.rs");
