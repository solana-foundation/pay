use std::process::{Command, Stdio};

use clap::Args;

const ALLOWED_TOOLS: &str = "mcp__pay__curl,mcp__pay__search_skills,mcp__pay__list_skills,mcp__pay__get_skill_endpoints,mcp__pay__create_skill";

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

        #[cfg(windows)]
        return launch_windows(mcp_config, &self.args);

        #[cfg(not(windows))]
        {
            let status = Command::new("claude")
                .arg("--mcp-config")
                .arg(mcp_config.to_string())
                .arg("--allowedTools")
                .arg(ALLOWED_TOOLS)
                .arg("--append-system-prompt")
                .arg(pay_core::instructions::INSTRUCTIONS)
                .args(&self.args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| {
                    pay_core::Error::Config(format!(
                        "Failed to launch claude: {e}. Is it installed?"
                    ))
                })?;

            Ok(status.code().unwrap_or(1))
        }
    }
}

// On Windows, cmd.exe (used to execute .cmd batch wrappers like claude.cmd) rejects
// arguments containing angle brackets, backticks, or double-quotes. The instructions
// and mcp config both have these characters. We work around this by:
//   1. Writing the mcp config JSON to a temp file (--mcp-config accepts a file path).
//   2. Generating a PowerShell script that uses a single-quoted here-string for the
//      system prompt — here-strings are 100% literal so no character escaping is needed.
//   3. Invoking powershell -File <script> so the script handles all the quoting.
#[cfg(windows)]
fn launch_windows(mcp_config: serde_json::Value, extra_args: &[String]) -> pay_core::Result<i32> {
    let tmp_dir = std::env::temp_dir();

    let config_path = tmp_dir.join("pay_mcp_config.json");
    std::fs::write(&config_path, mcp_config.to_string())
        .map_err(|e| pay_core::Error::Config(format!("Failed to write MCP config: {e}")))?;

    // Escape single quotes in the path for use inside a PS single-quoted string ('').
    let config_path_str = config_path.to_string_lossy().replace('\'', "''");

    // PowerShell single-quoted here-string: content is 100% literal — backticks,
    // angle brackets, quotes, etc. all pass through without interpretation.
    let script = format!(
        "& claude --mcp-config '{config_path_str}' --allowedTools '{ALLOWED_TOOLS}' --append-system-prompt @'\n{instructions}\n'@ @args\n",
        instructions = pay_core::instructions::INSTRUCTIONS,
    );

    let script_path = tmp_dir.join("pay_claude_launcher.ps1");
    std::fs::write(&script_path, &script)
        .map_err(|e| pay_core::Error::Config(format!("Failed to write launcher script: {e}")))?;

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script_path)
        .args(extra_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            pay_core::Error::Config(format!("Failed to launch claude: {e}. Is it installed?"))
        })?;

    Ok(status.code().unwrap_or(1))
}
