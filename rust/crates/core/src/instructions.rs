//! Agent instructions — single source of truth for the system prompt
//! injected into Claude, Codex, and the MCP server.
//!
//! Edit `instructions.md` to update.

/// Instructions shared by local and hosted MCP servers.
pub const HOSTED_INSTRUCTIONS: &str = include_str!("instructions.md");

/// Local servers additionally expose tools that run on the user's machine.
pub const INSTRUCTIONS: &str = concat!(
    include_str!("instructions.md"),
    "\n",
    include_str!("instructions-local.md")
);
