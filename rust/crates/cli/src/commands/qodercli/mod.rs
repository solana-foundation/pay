//! `pay qodercli` — launch Qoder CLI with the pay MCP server injected at
//! runtime.
//!
//! Uses runtime injection to integrate the pay MCP server: instead of
//! persisting an MCP registration in qodercli's own config, we build an
//! inline MCP config JSON and pass it to `qodercli` via `--mcp-config`,
//! together with `--allowed-tools` and `--append-system-prompt`. This keeps
//! the launcher stateless — every invocation reflects the current pay
//! binary path, active account, and PAY_* environment variables.

use std::process::{Command, Stdio};

use clap::Args;

/// Allow-list of pay MCP tools surfaced to qodercli. Must stay in sync
/// with the tool set exposed by other pay launchers.
const ALLOWED_TOOLS: &str = "mcp__pay__curl,mcp__pay__search_catalog,mcp__pay__list_catalog,mcp__pay__get_catalog_entry,mcp__pay__get_balance,mcp__pay__topup,mcp__pay__create_skill";

/// Run Qoder CLI (qodercli) with 402 payment support.
///
/// Launches `qodercli` with the pay MCP server injected via `--mcp-config`.
/// All extra arguments are passed through to the `qodercli` binary.
#[derive(Args)]
#[command(disable_help_flag = true)]
pub struct QodercliCommand {
    /// Arguments forwarded to qodercli.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Build the inline MCP config JSON that gets passed to `qodercli
/// --mcp-config`. Pulls `PAY_ACTIVE_ACCOUNT` from `active_account_name`
/// and the remaining PAY_* vars from `var_lookup`. Extracted so it can
/// be unit-tested without touching the real process environment.
fn build_mcp_config<F>(
    pay_bin: &str,
    active_account_name: Option<&str>,
    var_lookup: F,
) -> serde_json::Value
where
    F: Fn(&str) -> Option<String>,
{
    let mut mcp_server = serde_json::json!({
        "command": pay_bin,
        "args": ["mcp"]
    });

    let mut env = serde_json::Map::new();
    if let Some(source) = active_account_name {
        env.insert(
            "PAY_ACTIVE_ACCOUNT".to_string(),
            serde_json::Value::String(source.to_string()),
        );
    }
    for var in [
        "PAY_RPC_URL",
        "PAY_NETWORK_ENFORCED",
        "PAY_PROTOCOL_ENFORCED",
        "PAY_DEBUGGER_PROXY",
    ] {
        if let Some(value) = var_lookup(var) {
            env.insert(var.to_string(), serde_json::Value::String(value));
        }
    }
    if !env.is_empty() {
        mcp_server["env"] = serde_json::Value::Object(env);
    }

    serde_json::json!({
        "mcpServers": {
            "pay": mcp_server
        }
    })
}

impl QodercliCommand {
    pub fn run(self, pay_bin: &str, active_account_name: Option<&str>) -> pay_core::Result<i32> {
        let mcp_config =
            build_mcp_config(pay_bin, active_account_name, |var| std::env::var(var).ok());

        #[cfg(windows)]
        return launch_windows(mcp_config, &self.args);

        #[cfg(not(windows))]
        {
            let status = Command::new("qodercli")
                .arg("--mcp-config")
                .arg(mcp_config.to_string())
                .arg("--allowed-tools")
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
                        "Failed to launch qodercli: {e}. Install: `curl -fsSL https://qoder.com/install | bash` (or see https://qoder.com)."
                    ))
                })?;

            Ok(status.code().unwrap_or(1))
        }
    }
}

// On Windows, cmd.exe (used to execute .cmd batch wrappers) rejects
// arguments containing angle brackets, backticks, or double-quotes. Both
// the system-prompt and the inline MCP config can carry these. Workaround:
//   1. Write the MCP config JSON to a temp file (--mcp-config accepts a
//      file path).
//   2. Generate a PowerShell script that uses a single-quoted here-string
//      for the system prompt — here-strings are 100% literal so no
//      character escaping is needed.
//   3. Invoke powershell -File <script> so the script handles all the
//      quoting.
#[cfg(windows)]
fn launch_windows(mcp_config: serde_json::Value, extra_args: &[String]) -> pay_core::Result<i32> {
    let tmp_dir = std::env::temp_dir();
    let pid = std::process::id();

    let config_path = tmp_dir.join(format!("pay_qodercli_mcp_config_{pid}.json"));
    std::fs::write(&config_path, mcp_config.to_string())
        .map_err(|e| pay_core::Error::Config(format!("Failed to write MCP config: {e}")))?;

    // Escape single quotes in the path for use inside a PS single-quoted string ('').
    let config_path_str = config_path.to_string_lossy().replace('\'', "''");

    // PowerShell here-string: the closing '@ MUST be the first characters
    // on its own line with no trailing content.
    let script = format!(
        "$prompt = @'\n{instructions}\n'@\n& qodercli --mcp-config '{config_path_str}' --allowed-tools '{ALLOWED_TOOLS}' --append-system-prompt $prompt @args\n",
        instructions = pay_core::instructions::INSTRUCTIONS,
    );

    let script_path = tmp_dir.join(format!("pay_qodercli_launcher_{pid}.ps1"));
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
            pay_core::Error::Config(format!(
                "Failed to launch `qodercli`: {e}. Install: see https://qoder.com/install for platform-specific instructions."
            ))
        })?;

    // Best-effort cleanup of temp files.
    let _ = std::fs::remove_file(&config_path);
    let _ = std::fs::remove_file(&script_path);

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn allowed_tools_include_all_pay_mcp_tools() {
        for tool in [
            "mcp__pay__curl",
            "mcp__pay__search_catalog",
            "mcp__pay__list_catalog",
            "mcp__pay__get_catalog_entry",
            "mcp__pay__get_balance",
            "mcp__pay__topup",
            "mcp__pay__create_skill",
        ] {
            assert!(ALLOWED_TOOLS.split(',').any(|allowed| allowed == tool));
        }
    }

    #[test]
    fn allowed_tools_consistent_across_launchers() {
        // All pay launchers must surface the same tool set so users get
        // a consistent experience regardless of the AI tool they choose.
        let expected_tools: Vec<&str> = "mcp__pay__curl,mcp__pay__search_catalog,mcp__pay__list_catalog,mcp__pay__get_catalog_entry,mcp__pay__get_balance,mcp__pay__topup,mcp__pay__create_skill".split(',').collect();
        let qodercli_tools: Vec<&str> = ALLOWED_TOOLS.split(',').collect();
        assert_eq!(expected_tools, qodercli_tools);
    }

    #[test]
    fn mcp_config_has_pay_server_with_command_and_args() {
        let config = build_mcp_config("/usr/local/bin/pay", None, |_| None);

        let pay = &config["mcpServers"]["pay"];
        assert_eq!(pay["command"].as_str(), Some("/usr/local/bin/pay"));
        assert_eq!(pay["args"], serde_json::json!(["mcp"]));
        // Without an active account or env vars, the `env` field must be omitted.
        assert!(pay.get("env").is_none());
    }

    #[test]
    fn mcp_config_includes_active_account_in_env() {
        let config = build_mcp_config("pay", Some("alice"), |_| None);

        let env = config["mcpServers"]["pay"]["env"]
            .as_object()
            .expect("env object");
        assert_eq!(
            env.get("PAY_ACTIVE_ACCOUNT").and_then(|v| v.as_str()),
            Some("alice")
        );
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn mcp_config_forwards_known_pay_env_vars_only() {
        let mut vars: HashMap<&str, &str> = HashMap::new();
        vars.insert("PAY_RPC_URL", "https://rpc.example.com");
        vars.insert("PAY_NETWORK_ENFORCED", "mainnet");
        vars.insert("PAY_PROTOCOL_ENFORCED", "x402");
        vars.insert("PAY_DEBUGGER_PROXY", "http://localhost:9000");
        // Unrelated vars must be ignored even if `var_lookup` returns them.
        vars.insert("PAY_UNRELATED", "should-be-ignored");

        let config = build_mcp_config("pay", Some("bob"), |k| {
            vars.get(k).map(|v| (*v).to_string())
        });

        let env = config["mcpServers"]["pay"]["env"]
            .as_object()
            .expect("env object");
        assert_eq!(
            env.get("PAY_ACTIVE_ACCOUNT").and_then(|v| v.as_str()),
            Some("bob")
        );
        assert_eq!(
            env.get("PAY_RPC_URL").and_then(|v| v.as_str()),
            Some("https://rpc.example.com")
        );
        assert_eq!(
            env.get("PAY_NETWORK_ENFORCED").and_then(|v| v.as_str()),
            Some("mainnet")
        );
        assert_eq!(
            env.get("PAY_PROTOCOL_ENFORCED").and_then(|v| v.as_str()),
            Some("x402")
        );
        assert_eq!(
            env.get("PAY_DEBUGGER_PROXY").and_then(|v| v.as_str()),
            Some("http://localhost:9000")
        );
        assert!(!env.contains_key("PAY_UNRELATED"));
        // Active account + four forwarded vars.
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn mcp_config_skips_missing_env_vars() {
        let mut vars: HashMap<&str, &str> = HashMap::new();
        vars.insert("PAY_RPC_URL", "https://rpc.example.com");

        let config = build_mcp_config("pay", None, |k| vars.get(k).map(|v| (*v).to_string()));

        let env = config["mcpServers"]["pay"]["env"]
            .as_object()
            .expect("env object");
        assert_eq!(env.len(), 1);
        assert!(env.contains_key("PAY_RPC_URL"));
        assert!(!env.contains_key("PAY_NETWORK_ENFORCED"));
        assert!(!env.contains_key("PAY_PROTOCOL_ENFORCED"));
        assert!(!env.contains_key("PAY_DEBUGGER_PROXY"));
        assert!(!env.contains_key("PAY_ACTIVE_ACCOUNT"));
    }
}
