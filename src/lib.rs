//! GrokBot Control MCP — a Rust MCP server for managing a GrokBot
//! (xAI Grok-powered bot) from any MCP-capable AI client.

pub mod auth;
pub mod backend;
pub mod cli;
pub mod mcp;
pub mod server;
pub mod state;
pub mod tools;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SERVER_NAME: &str = "grokbot-control";
pub const DEFAULT_PROTOCOL: &str = "2024-11-05";
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
