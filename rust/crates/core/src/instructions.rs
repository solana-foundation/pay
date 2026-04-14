//! Agent instructions — single source of truth for the system prompt
//! injected into Claude, Codex, and the MCP server.
//!
//! Edit `instructions.md` to update.

pub const INSTRUCTIONS: &str = include_str!("instructions.md");
