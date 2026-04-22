use std::process::{Command, Stdio};

use clap::Args;

/// Run Claude Code with 402 payment support.
///
/// Launches Claude Code with the pay MCP server injected automatically.
/// All arguments are passed through to the `claude` binary.
#[derive(Args)]
pub struct ClaudeCommand {
    /// Arguments forwarded to claude.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl ClaudeCommand {
    pub fn run(self, pay_bin: &str, active_account_name: Option<&str>) -> pay_core::Result<i32> {
        let mut mcp_server = serde_json::json!({
            "command": pay_bin,
            "args": ["mcp"]
        });

        // Pass config to the MCP server via env vars
        let mut env = serde_json::Map::new();
        if let Some(source) = active_account_name {
            env.insert(
                "PAY_ACTIVE_ACCOUNT".to_string(),
                serde_json::Value::String(source.to_string()),
            );
        }
        if let Ok(url) = std::env::var("PAY_RPC_URL") {
            env.insert("PAY_RPC_URL".to_string(), serde_json::Value::String(url));
        }
        if let Ok(network) = std::env::var("PAY_NETWORK_ENFORCED") {
            env.insert(
                "PAY_NETWORK_ENFORCED".to_string(),
                serde_json::Value::String(network),
            );
        }
        if let Ok(proxy) = std::env::var("PAY_DEBUGGER_PROXY") {
            env.insert(
                "PAY_DEBUGGER_PROXY".to_string(),
                serde_json::Value::String(proxy),
            );
        }
        if !env.is_empty() {
            mcp_server["env"] = serde_json::Value::Object(env);
        }

        let mcp_config = serde_json::json!({
            "mcpServers": {
                "pay": mcp_server
            }
        });

        let status = Command::new("claude")
            .arg("--mcp-config")
            .arg(mcp_config.to_string())
            .arg("--allowedTools")
            .arg("mcp__pay__curl,mcp__pay__search_skills,mcp__pay__list_skills,mcp__pay__get_skill_endpoints,mcp__pay__create_skill")
            .arg("--append-system-prompt")
            .arg(pay_core::instructions::INSTRUCTIONS)
            .args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| {
                pay_core::Error::Config(format!("Failed to launch claude: {e}. Is it installed?"))
            })?;

        Ok(status.code().unwrap_or(1))
    }
}
