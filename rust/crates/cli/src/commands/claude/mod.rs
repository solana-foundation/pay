use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;

use crate::commands::server::inference::discovery::{self, DiscoveredProvider};
use crate::tui::{ClaudeProviderSelection, select_claude_provider};

const ALLOWED_TOOLS: &str = "mcp__pay__curl,mcp__pay__search_catalog,mcp__pay__list_catalog,mcp__pay__get_catalog_entry,mcp__pay__get_balance,mcp__pay__topup,mcp__pay__create_skill";
const GATEWAY_BIND: &str = "127.0.0.1:1402";
const GATEWAY_BASE_URL: &str = "http://127.0.0.1:1402";
const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
const GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Run Claude Code with 402 payment support.
///
/// Launches Claude Code with the pay MCP server injected automatically.
/// All arguments are passed through to the `claude` binary.
#[derive(Args)]
#[command(disable_help_flag = true)]
pub struct ClaudeCommand {
    /// Arguments forwarded to claude.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

impl ClaudeCommand {
    pub fn run(self, pay_bin: &str, active_account_name: Option<&str>) -> pay_core::Result<i32> {
        let launch = prepare_claude_launch(pay_bin, &self.args)?;

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
        if let Ok(protocol) = std::env::var("PAY_PROTOCOL_ENFORCED") {
            env.insert(
                "PAY_PROTOCOL_ENFORCED".to_string(),
                serde_json::Value::String(protocol),
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
        return launch_windows(
            mcp_config,
            &launch.args,
            launch.base_url.as_deref(),
            launch.model.as_deref(),
        );

        #[cfg(not(windows))]
        {
            let mut command = Command::new("claude");
            command
                .arg("--mcp-config")
                .arg(mcp_config.to_string())
                .arg("--strict-mcp-config")
                .arg("--allowedTools")
                .arg(ALLOWED_TOOLS)
                .arg("--append-system-prompt")
                .arg(pay_core::instructions::INSTRUCTIONS)
                .args(&launch.args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());

            if let Some(base_url) = launch.base_url.as_deref() {
                command.envs(claude_env(base_url, launch.model.as_deref()));
            }

            let status = command.status().map_err(|e| {
                pay_core::Error::Config(format!("Failed to launch claude: {e}. Is it installed?"))
            })?;

            Ok(status.code().unwrap_or(1))
        }
    }
}

struct ClaudeLaunch {
    _gateway: Option<ClaudeGateway>,
    base_url: Option<String>,
    model: Option<String>,
    args: Vec<String>,
}

struct ClaudeGateway {
    child: Child,
}

impl Drop for ClaudeGateway {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn prepare_claude_launch(pay_bin: &str, args: &[String]) -> pay_core::Result<ClaudeLaunch> {
    if claude_metadata_requested(args) {
        return Ok(ClaudeLaunch {
            _gateway: None,
            base_url: None,
            model: None,
            args: args.to_vec(),
        });
    }

    let requested_model = model_arg(args);
    let providers = discover_local_providers()?;
    let choice = match select_claude_provider(providers, requested_model.as_deref())
        .map_err(|e| pay_core::Error::Config(format!("Provider selection failed: {e}")))?
    {
        ClaudeProviderSelection::Selected(choice) => choice,
        ClaudeProviderSelection::Cancelled => {
            return Err(pay_core::Error::Config(
                "Claude provider selection cancelled".to_string(),
            ));
        }
    };

    let gateway = start_gateway(pay_bin, &choice.provider)?;
    let args = claude_args_with_model(args, Some(&choice.model));

    Ok(ClaudeLaunch {
        _gateway: Some(gateway),
        base_url: Some(GATEWAY_BASE_URL.to_string()),
        model: Some(choice.model),
        args,
    })
}

fn discover_local_providers() -> pay_core::Result<Vec<DiscoveredProvider>> {
    let registry =
        discovery::load_registry().map_err(|e| pay_core::Error::Config(format!("{e}")))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("tokio runtime: {e}")))?;
    let providers = rt.block_on(discovery::discover(&registry, PROVIDER_PROBE_TIMEOUT, None));
    if providers.is_empty() {
        return Err(pay_core::Error::Config(
            "No local inference providers found. Start Ollama or another supported provider and retry."
                .to_string(),
        ));
    }
    Ok(providers)
}

fn start_gateway(pay_bin: &str, provider: &DiscoveredProvider) -> pay_core::Result<ClaudeGateway> {
    let mut child = Command::new(pay_bin)
        .args([
            "server",
            "inference",
            "--providers",
            &provider.spec.slug,
            "--bind",
            GATEWAY_BIND,
            "--no-tui",
            "--no-web",
            "--watch-interval",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            pay_core::Error::Config(format!("Failed to start pay inference gateway: {e}"))
        })?;

    match wait_for_gateway(&mut child, provider, GATEWAY_READY_TIMEOUT) {
        Ok(()) => Ok(ClaudeGateway { child }),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(err)
        }
    }
}

fn wait_for_gateway(
    child: &mut Child,
    provider: &DiscoveredProvider,
    timeout: Duration,
) -> pay_core::Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if provider_gateway_ready(provider) {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|e| {
            pay_core::Error::Config(format!("Failed to monitor pay inference gateway: {e}"))
        })? {
            return Err(pay_core::Error::Config(format!(
                "pay inference gateway exited before it was ready: {status}"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Err(pay_core::Error::Config(format!(
        "Timed out waiting for pay inference gateway on {GATEWAY_BASE_URL}; is port 1402 available?"
    )))
}

fn provider_gateway_ready(provider: &DiscoveredProvider) -> bool {
    let Some(probe) = provider.spec.identify.first() else {
        return false;
    };
    let path = if probe.path.starts_with('/') {
        probe.path.clone()
    } else {
        format!("/{}", probe.path)
    };
    let url = format!("{GATEWAY_BASE_URL}{path}");
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .and_then(|client| client.get(url).send())
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

fn claude_metadata_requested(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--version" | "-v"))
}

fn model_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(model) = arg.strip_prefix("--model=") {
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
        if matches!(arg.as_str(), "--model" | "-m")
            && let Some(model) = iter.next()
            && !model.trim().is_empty()
        {
            return Some(model.to_string());
        }
    }
    None
}

fn claude_args_with_model(args: &[String], model: Option<&str>) -> Vec<String> {
    let Some(model) = model else {
        return args.to_vec();
    };
    if model_arg(args).is_some() {
        return args.to_vec();
    }
    let mut out = vec!["--model".to_string(), model.to_string()];
    out.extend(args.iter().cloned());
    out
}

fn claude_env(base_url: &str, model: Option<&str>) -> Vec<(String, String)> {
    let mut env = vec![
        ("ANTHROPIC_BASE_URL".to_string(), base_url.to_string()),
        ("ANTHROPIC_API_KEY".to_string(), String::new()),
        ("ANTHROPIC_AUTH_TOKEN".to_string(), "ollama".to_string()),
        (
            "CLAUDE_CODE_ATTRIBUTION_HEADER".to_string(),
            "0".to_string(),
        ),
    ];

    if let Some(model) = model {
        env.extend([
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
                model.to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
                model.to_string(),
            ),
            (
                "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
                model.to_string(),
            ),
            ("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), model.to_string()),
        ]);
    }

    env
}

// On Windows, cmd.exe (used to execute .cmd batch wrappers like claude.cmd) rejects
// arguments containing angle brackets, backticks, or double-quotes. The instructions
// and mcp config both have these characters. We work around this by:
//   1. Writing the mcp config JSON to a temp file (--mcp-config accepts a file path).
//   2. Generating a PowerShell script that uses a single-quoted here-string for the
//      system prompt — here-strings are 100% literal so no character escaping is needed.
//   3. Invoking powershell -File <script> so the script handles all the quoting.
#[cfg(windows)]
fn launch_windows(
    mcp_config: serde_json::Value,
    extra_args: &[String],
    base_url: Option<&str>,
    model: Option<&str>,
) -> pay_core::Result<i32> {
    let tmp_dir = std::env::temp_dir();

    let config_path = tmp_dir.join("pay_mcp_config.json");
    std::fs::write(&config_path, mcp_config.to_string())
        .map_err(|e| pay_core::Error::Config(format!("Failed to write MCP config: {e}")))?;

    // Escape single quotes in the path for use inside a PS single-quoted string ('').
    let config_path_str = config_path.to_string_lossy().replace('\'', "''");

    // PowerShell single-quoted here-string: content is 100% literal — backticks,
    // angle brackets, quotes, etc. all pass through without interpretation.
    let script = format!(
        "& claude --mcp-config '{config_path_str}' --strict-mcp-config --allowedTools '{ALLOWED_TOOLS}' --append-system-prompt @'\n{instructions}\n'@ @args\n",
        instructions = pay_core::instructions::INSTRUCTIONS,
    );

    let script_path = tmp_dir.join("pay_claude_launcher.ps1");
    std::fs::write(&script_path, &script)
        .map_err(|e| pay_core::Error::Config(format!("Failed to write launcher script: {e}")))?;

    let mut command = Command::new("powershell");
    command
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
        .stderr(Stdio::inherit());

    if let Some(base_url) = base_url {
        command.envs(claude_env(base_url, model));
    }

    let status = command.status().map_err(|e| {
        pay_core::Error::Config(format!(
            "Failed to launch `claude`: {e}. Install: `npm install -g @anthropic-ai/claude-code` (or see https://claude.com/claude-code)."
        ))
    })?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn model_arg_reads_long_and_short_forms() {
        assert_eq!(
            model_arg(&["--model".into(), "llama3.2".into()]),
            Some("llama3.2".into())
        );
        assert_eq!(
            model_arg(&["--model=qwen3.5".into()]),
            Some("qwen3.5".into())
        );
        assert_eq!(
            model_arg(&["-m".into(), "gemma4".into()]),
            Some("gemma4".into())
        );
        assert_eq!(model_arg(&["--model".into()]), None);
    }

    #[test]
    fn claude_args_inject_model_when_missing() {
        assert_eq!(
            claude_args_with_model(&["-p".into(), "hi".into()], Some("llama3.2")),
            vec!["--model", "llama3.2", "-p", "hi"]
        );
        assert_eq!(
            claude_args_with_model(&["--model".into(), "qwen3.5".into()], Some("llama3.2")),
            vec!["--model", "qwen3.5"]
        );
    }

    #[test]
    fn claude_env_points_anthropic_to_gateway_and_model_tiers() {
        let env = claude_env("http://127.0.0.1:1402", Some("llama3.2"));

        assert!(env.contains(&(
            "ANTHROPIC_BASE_URL".to_string(),
            "http://127.0.0.1:1402".to_string()
        )));
        assert!(env.contains(&("ANTHROPIC_API_KEY".to_string(), String::new())));
        assert!(env.contains(&("ANTHROPIC_AUTH_TOKEN".to_string(), "ollama".to_string())));
        assert!(env.contains(&(
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
            "llama3.2".to_string()
        )));
        assert!(env.contains(&(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            "llama3.2".to_string()
        )));
    }
}
