use std::process::{Command, Stdio};

use clap::Args;

/// Run Codex with 402 payment support.
///
/// Launches Codex with the pay MCP server injected automatically.
/// All arguments are passed through to the `codex` binary.
#[derive(Args)]
pub struct CodexCommand {
    /// Arguments forwarded to codex.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl CodexCommand {
    pub fn run(self, pay_bin: &str, keypair_source: Option<&str>) -> pay_core::Result<i32> {
        let mut cmd = Command::new("codex");
        cmd.arg("-c")
            .arg(format!("mcp_servers.pay.command=\"{pay_bin}\""))
            .arg("-c")
            .arg("mcp_servers.pay.args=[\"mcp\"]");

        // Pass config to MCP server via env
        let mut env_parts = Vec::new();
        if let Some(source) = keypair_source {
            env_parts.push(format!("PAY_SECRET_KEY=\"{source}\""));
        }
        if let Ok(url) = std::env::var("PAY_RPC_URL") {
            env_parts.push(format!("PAY_RPC_URL=\"{url}\""));
        }
        if !env_parts.is_empty() {
            cmd.arg("-c")
                .arg(format!("mcp_servers.pay.env={{{}}}", env_parts.join(",")));
        }

        let status = cmd
            .arg("-c")
            .arg("instructions=\"For ANY HTTP request, use the pay MCP curl tool (pay.curl) instead of Bash curl/wget. The pay curl tool handles 402 Payment Required responses automatically.\"")
            .args(&self.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| {
                pay_core::Error::Config(format!(
                    "Failed to launch codex: {e}. Is it installed?"
                ))
            })?;

        Ok(status.code().unwrap_or(1))
    }
}
