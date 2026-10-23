//! Recall: a self-hosted, agent-agnostic personal memory served over MCP.

pub mod auth;
pub mod config;
pub mod embed;
pub mod guard;
pub mod http;
pub mod mcp;
pub mod remote;
pub mod setup;
pub mod store;
pub mod tailnet;
pub mod text;
pub mod web;
pub mod when;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
