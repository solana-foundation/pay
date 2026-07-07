mod payer;
mod translate;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;

use crate::commands::server::inference::discovery::{self, DiscoveredProvider};
use crate::commands::server::inference::providers::{
    Dialect, InferenceProvider, catalog as catalog_providers,
};
use crate::tui::{ProviderSelection, select_provider};

const ALLOWED_TOOLS: &str = "mcp__pay__curl,mcp__pay__search_catalog,mcp__pay__list_catalog,mcp__pay__get_catalog_entry,mcp__pay__get_balance,mcp__pay__topup,mcp__pay__create_skill";
const GATEWAY_BASE_URL: &str = "http://127.0.0.1:1402";
const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// Hosted catalog gateways are remote (TLS handshake included) — give their
/// reachability/model probes more room than the localhost ones.
const CATALOG_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Where OpenAI-compatible upstreams serve chat completions unless their
/// paid endpoints say otherwise (Alibaba: `compatible-mode/v1/chat/completions`).
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "v1/chat/completions";

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
    pub fn run(
        self,
        pay_bin: &str,
        active_account_name: Option<&str>,
        network_override: Option<&str>,
    ) -> pay_core::Result<i32> {
        let launch = prepare_claude_launch(&self.args, network_override, active_account_name)?;

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
    base_url: Option<String>,
    model: Option<String>,
    args: Vec<String>,
}

/// Decide where Claude Code's traffic goes and put the 402-paying payer
/// proxy in front of it.
///
/// `pay claude` never spawns a gateway itself — it routes:
///
/// 1. **Hosted catalog provider selected** (Gemini, Model Studio, … from
///    the pay catalog) → payer proxy targets its `service_url` directly
///    and settles the gateway's MPP 402 challenges per request.
/// 2. **Gateway on 127.0.0.1:1402** (the user ran `pay serve inference`,
///    possibly priced, in another terminal) → payer proxy targets the
///    gateway and settles its MPP 402 challenges.
/// 3. **No gateway** → run local provider discovery and target the
///    selected provider directly (e.g. Ollama on :11434) — unmetered
///    passthrough, no 402s.
/// 4. **None of the above** → error with a hint.
fn prepare_claude_launch(
    args: &[String],
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> pay_core::Result<ClaudeLaunch> {
    if claude_metadata_requested(args) {
        return Ok(ClaudeLaunch {
            base_url: None,
            model: None,
            args: args.to_vec(),
        });
    }

    let requested_model = model_arg(args);
    let gateway_up = gateway_listening();
    let mut providers = discover_local_providers()?;
    providers.extend(discover_catalog_providers());

    let choice = if providers.is_empty() {
        None
    } else {
        Some(select_provider_choice(
            providers,
            requested_model.as_deref(),
        )?)
    };

    if let Some(choice) = &choice {
        match choice.provider.provider.dialect() {
            Dialect::Anthropic => {}
            Dialect::OpenAiCompat => eprintln!(
                "⏺ translating anthropic → openai for {}",
                choice.provider.title()
            ),
            dialect => eprintln!(
                "⚠ {} speaks {dialect}; no translator yet — expect errors from claude",
                choice.provider.title()
            ),
        }
    }

    let (upstream, model) = match choice {
        // A hosted catalog provider is its own upstream: the payer proxy
        // pays its 402s directly; the local gateway never fronts it.
        Some(choice) if choice.provider.hosted() => {
            eprintln!(
                "⏺ routing claude → {} {} (hosted, paid per request)",
                choice.provider.slug(),
                choice.provider.base_url
            );
            let upstream = payer_upstream(&choice.provider, choice.provider.base_url.clone());
            (upstream, Some(choice.model))
        }
        // Direct discovery still supplies the model list for the
        // ANTHROPIC_DEFAULT_* env vars; the gateway routes by model.
        Some(choice) if gateway_up => {
            eprintln!("⏺ routing claude → gateway {GATEWAY_BASE_URL}");
            let upstream = payer_upstream(&choice.provider, GATEWAY_BASE_URL.to_string());
            (upstream, Some(choice.model))
        }
        Some(choice) => {
            eprintln!(
                "⏺ routing claude → {} {} (direct, unmetered)",
                choice.provider.slug(),
                choice.provider.base_url
            );
            let upstream = payer_upstream(&choice.provider, choice.provider.base_url.clone());
            (upstream, Some(choice.model))
        }
        None if gateway_up => {
            eprintln!("⏺ routing claude → gateway {GATEWAY_BASE_URL}");
            let upstream = payer::PayerUpstream {
                base_url: GATEWAY_BASE_URL.to_string(),
                dialect: Dialect::Anthropic,
                chat_path: DEFAULT_CHAT_COMPLETIONS_PATH.to_string(),
            };
            (upstream, requested_model)
        }
        None => {
            return Err(pay_core::Error::Config(format!(
                "no gateway on {GATEWAY_BASE_URL} and no local inference provider detected — \
                 start one, e.g. `ollama serve`, or run `pay serve inference`."
            )));
        }
    };

    let upstream_base = upstream.base_url.clone();
    let payer = payer::start_background(upstream, network_override, account_override)?;
    eprintln!(
        "⏺ payer proxy on {} → {} (paying as {})",
        payer.base_url,
        upstream_base,
        payer
            .payer_pubkey
            .as_deref()
            .unwrap_or("unresolved account")
    );

    let args = claude_args_with_model(args, model.as_deref());

    Ok(ClaudeLaunch {
        base_url: Some(payer.base_url),
        model,
        args,
    })
}

/// Payer upstream for a picked provider: its dialect plus the
/// chat-completions path translated `/v1/messages` requests are sent to.
fn payer_upstream(provider: &DiscoveredProvider, base_url: String) -> payer::PayerUpstream {
    payer::PayerUpstream {
        base_url,
        dialect: provider.provider.dialect(),
        chat_path: chat_completions_path(provider.provider.as_ref()),
    }
}

/// The provider's chat-completions path, from its paid endpoints (that's
/// where the catalog pins Alibaba's `compatible-mode/v1/chat/completions`),
/// falling back to the OpenAI-compatible default.
fn chat_completions_path(provider: &dyn InferenceProvider) -> String {
    provider
        .paid_endpoints()
        .into_iter()
        .filter(|ep| matches!(ep.method, pay_types::metering::HttpMethod::Post))
        .map(|ep| ep.path)
        .find(|path| path.to_ascii_lowercase().contains("chat/completions"))
        .unwrap_or_else(|| DEFAULT_CHAT_COMPLETIONS_PATH.to_string())
}

fn select_provider_choice(
    providers: Vec<DiscoveredProvider>,
    requested_model: Option<&str>,
) -> pay_core::Result<crate::tui::ProviderChoice> {
    match select_provider("claude", providers, requested_model)
        .map_err(|e| pay_core::Error::Config(format!("Provider selection failed: {e}")))?
    {
        ProviderSelection::Selected(choice) => Ok(choice),
        ProviderSelection::Cancelled => Err(pay_core::Error::Config(
            "Claude provider selection cancelled".to_string(),
        )),
    }
}

fn discover_local_providers() -> pay_core::Result<Vec<DiscoveredProvider>> {
    let registry =
        discovery::load_registry().map_err(|e| pay_core::Error::Config(format!("{e}")))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("tokio runtime: {e}")))?;
    Ok(rt.block_on(discovery::discover(&registry, PROVIDER_PROBE_TIMEOUT, None)))
}

/// Hosted pay-catalog providers appended to the picker after local
/// discovery. Everything degrades silently to local-only: catalog
/// unavailable, an fqn not (yet) published, or an unreachable gateway all
/// skip the entry with a debug log.
fn discover_catalog_providers() -> Vec<DiscoveredProvider> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    rt.block_on(async {
        let mut catalog = match load_catalog_quietly().await {
            Ok(catalog) => catalog,
            Err(e) => {
                tracing::debug!(error = %e, "skills catalog unavailable — hosted providers skipped");
                return Vec::new();
            }
        };
        let Ok(client) = reqwest::Client::builder()
            .timeout(CATALOG_PROBE_TIMEOUT)
            .build()
        else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        let resolved = catalog_providers::resolve_catalog_providers(
            &mut catalog,
            catalog_providers::DEFAULT_CATALOG_FQNS,
        )
        .await;
        for provider in resolved {
            let base_url = provider.service_url().to_string();
            let provider: Arc<dyn InferenceProvider> = Arc::new(provider);
            let Some(version) = provider.identify(&client, &base_url).await else {
                tracing::debug!(
                    slug = provider.slug(),
                    %base_url,
                    "hosted catalog provider unreachable — skipping"
                );
                continue;
            };
            let models = provider.list_models(&client, &base_url).await;
            discovered.push(DiscoveredProvider {
                provider,
                base_url,
                models,
                version,
            });
        }
        discovered
    })
}

/// Load the skills catalog without waking the local gateway when possible.
///
/// `pay_core::skills::load_skills()` re-fetches every *ephemeral* source on
/// each call — including the `/.well-known/pay-skills.json` a running
/// `pay serve inference` auto-registers — and that fetch goes through the
/// payment gate, polluting the gateway's CONNECTIONS panel with an
/// anonymous 127.0.0.1 row on every `pay claude` launch. The hosted
/// defaults are durable CDN entries, so a fresh on-disk cache (pure disk
/// read) is all we need; the full network load only runs when the cache is
/// stale or missing — the same cadence as any other `pay skills` command.
async fn load_catalog_quietly() -> pay_core::Result<pay_core::skills::Catalog> {
    let cache_is_fresh = pay_core::skills::config::SkillsConfig::load()
        .map(|cfg| cfg.valid_cache_path().is_some())
        .unwrap_or(false);
    if cache_is_fresh && let Ok(catalog) = pay_core::skills::load_cached_skills() {
        return Ok(catalog);
    }
    pay_core::skills::load_skills().await
}

/// Whether an inference gateway is already serving HTTP on 127.0.0.1:1402.
///
/// `/` answers with a 307 redirect (to `/__402/ui/`), not a 200, so any
/// HTTP response at all counts as "gateway present" — only a failed
/// connection means the port is free. `/__402/pdb/api/config` returns
/// 200 JSON on a healthy gateway.
fn gateway_listening() -> bool {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .and_then(|client| {
            client
                .get(format!("{GATEWAY_BASE_URL}/__402/pdb/api/config"))
                .send()
        })
        .is_ok()
}

fn claude_metadata_requested(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--version" | "-v"))
}

fn model_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(model) = arg.strip_prefix("--model=")
            && !model.is_empty()
        {
            return Some(model.to_string());
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
