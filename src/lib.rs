//! Recall: a self-hosted, agent-agnostic personal memory served over MCP.

pub mod config;
pub mod embed;
pub mod guard;
pub mod http;
pub mod mcp;
pub mod store;
pub mod text;
pub mod when;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
