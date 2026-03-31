pub mod account;
pub mod claude;
pub mod codex;
pub mod curl;
pub mod fetch;
pub mod send;
pub mod setup;
pub mod topup;
pub mod wget;

use clap::Subcommand;
use owo_colors::OwoColorize;
use pay_core::mpp;
use pay_core::runner::RunOutcome;
use pay_core::x402;
use pay_core::x402::Challenge as X402Challenge;
use pay_core::{Config, run_curl_with_headers, run_wget_with_headers};
use solana_mpp::ChargeRequest;

use crate::no_dna;
use crate::output::{self, OutputFormat};

#[derive(Subcommand)]
pub enum Command {
    /// Make an HTTP request via curl, handling 402 Payment Required flows.
    Curl(curl::CurlCommand),
    /// Download a resource via wget, handling 402 Payment Required flows.
    Wget(wget::WgetCommand),
    /// Fetch a URL using the built-in HTTP client (no external tool required).
    Fetch(fetch::FetchCommand),
    /// Run Claude Code with 402 payment support.
    Claude(claude::ClaudeCommand),
    /// Run Codex with 402 payment support.
    Codex(codex::CodexCommand),
    /// Manage accounts (new, import, list, destroy, export).
    #[command(alias = "accounts")]
    Account {
        #[command(subcommand)]
        command: account::AccountCommand,
    },
    /// Send SOL to a recipient address.
    Send(send::SendCommand),
    /// Generate a keypair, store it, and fund your account.
    Setup(setup::SetupCommand),
    /// Fund your account on localnet via Surfpool.
    Topup(topup::TopupCommand),
    /// Start the MCP server (for Claude Code, Cursor, etc.)
    Mcp,
}

/// Identifies which tool is being wrapped.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    Curl,
    Wget,
    Fetch,
    Claude,
    Codex,
    Mcp,
}

impl Command {
    /// Which tool this command wraps.
    pub fn tool_kind(&self) -> ToolKind {
        match self {
            Command::Curl(_) => ToolKind::Curl,
            Command::Wget(_) => ToolKind::Wget,
            Command::Fetch(_) => ToolKind::Fetch,
            Command::Claude(_) => ToolKind::Claude,
            Command::Codex(_) => ToolKind::Codex,
            Command::Account { .. }
            | Command::Send(_)
            | Command::Setup(_)
            | Command::Topup(_) => ToolKind::Mcp,
            Command::Mcp => ToolKind::Mcp,
        }
    }
}

/// Which underlying tool to use for retry.
enum Tool<'a> {
    Curl(&'a [String]),
    Wget(&'a [String]),
    Fetch { url: &'a str },
}

impl Command {
    pub fn execute(
        self,
        auto_pay: bool,
        output_fmt: Option<OutputFormat>,
        keypair_override: Option<&str>,
        verbose: bool,
    ) -> pay_core::Result<()> {
        let pay_bin = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "pay".to_string());

        match self {
            Command::Account { command } => return command.run(keypair_override),
            Command::Send(cmd) => return cmd.run(keypair_override, verbose),
            Command::Setup(cmd) => return cmd.run(),
            Command::Topup(cmd) => return cmd.run(keypair_override),
            Command::Mcp => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        pay_core::Error::Config(format!("Failed to create runtime: {e}"))
                    })?;
                return rt
                    .block_on(pay_mcp::run_server(&pay_mcp::McpOptions::default()))
                    .map_err(pay_core::Error::Config);
            }
            Command::Claude(cmd) => std::process::exit(cmd.run(&pay_bin, keypair_override)?),
            Command::Codex(cmd) => std::process::exit(cmd.run(&pay_bin, keypair_override)?),
            _ => {}
        }

        let (outcome, tool) = match &self {
            Command::Curl(cmd) => (pay_core::run_curl(&cmd.args)?, Tool::Curl(&cmd.args)),
            Command::Wget(cmd) => (pay_core::run_wget(&cmd.args)?, Tool::Wget(&cmd.args)),
            Command::Fetch(cmd) => {
                let parsed_headers = parse_header_args(&cmd.headers);
                let outcome = pay_core::fetch::fetch(&cmd.url, &parsed_headers)?;
                let tool = Tool::Fetch { url: &cmd.url };
                return handle_outcome(
                    outcome,
                    &tool,
                    auto_pay,
                    output_fmt,
                    Some(parsed_headers),
                    keypair_override,
                    verbose,
                );
            }
            _ => unreachable!("handled above"),
        };

        handle_outcome(
            outcome,
            &tool,
            auto_pay,
            output_fmt,
            None,
            keypair_override,
            verbose,
        )
    }
}

fn handle_outcome(
    outcome: RunOutcome,
    tool: &Tool,
    auto_pay: bool,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    keypair_override: Option<&str>,
    verbose: bool,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(output_fmt);

    match outcome {
        RunOutcome::MppChallenge {
            challenge,
            resource_url,
        } => {
            let req: ChargeRequest = challenge.request.decode().unwrap_or_default();
            if auto_pay {
                if verbose && !is_json {
                    eprintln!(
                        "{}",
                        format!(
                            "402 Payment Required (MPP) — {} {}",
                            req.amount, req.currency
                        )
                        .dimmed()
                    );
                }
                return pay_mpp_and_retry(
                    &challenge,
                    tool,
                    output_fmt,
                    fetch_headers,
                    keypair_override,
                    verbose,
                );
            }

            if is_json {
                let network = req
                    .method_details
                    .as_ref()
                    .and_then(|v| v.get("network"))
                    .and_then(|v| v.as_str());
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "protocol": "mpp",
                    "challenge": {
                        "amount": req.amount,
                        "currency": req.currency,
                        "recipient": req.recipient,
                        "description": req.description,
                        "network": network,
                    },
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "402 Payment Required (MPP) — {} {} — use --yolo to pay automatically",
                        req.amount, req.currency
                    )
                    .dimmed()
                );
            }
        }

        RunOutcome::X402Challenge {
            requirements,
            resource_url,
        } => {
            if auto_pay {
                if verbose && !is_json {
                    eprintln!(
                        "{}",
                        format!(
                            "402 Payment Required (x402) — {} {}",
                            requirements.amount, requirements.currency
                        )
                        .dimmed()
                    );
                }
                return pay_x402_and_retry(
                    &requirements,
                    tool,
                    output_fmt,
                    fetch_headers,
                    keypair_override,
                    verbose,
                );
            }

            if is_json {
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "protocol": "x402",
                    "challenge": {
                        "amount": requirements.amount,
                        "currency": requirements.currency,
                        "recipient": requirements.recipient,
                        "description": requirements.description,
                        "cluster": requirements.cluster,
                    },
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "402 Payment Required (x402) — {} {} — use --yolo to pay automatically",
                        requirements.amount, requirements.currency
                    )
                    .dimmed()
                );
            }
        }

        RunOutcome::UnknownPaymentRequired {
            headers: _,
            resource_url,
        } => {
            if is_json {
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "protocol": "unknown",
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!();
                eprintln!(
                    "{}",
                    "402 Payment Required (no recognized payment protocol)".dimmed()
                );
                eprintln!("{}", format!("  Resource: {resource_url}").dimmed());
            }
        }

        RunOutcome::Completed { exit_code, body } => {
            if let Some(body) = body {
                print!("{body}");
            }
            std::process::exit(exit_code);
        }
    }

    Ok(())
}

fn pay_mpp_and_retry(
    challenge: &mpp::Challenge,
    tool: &Tool,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    keypair_override: Option<&str>,
    verbose: bool,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(output_fmt);
    let config = Config::load()?;
    let keypair_path = keypair_override
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| config.default_keypair_source())
        .ok_or_else(|| {
            pay_core::Error::Config("No keypair configured. Run `pay setup`.".to_string())
        })?;

    if verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let auth_header = mpp::build_credential(challenge, &keypair_path)?;

    if verbose && !is_json {
        eprintln!("{}", "Payment signed, retrying...\n".dimmed());
    }

    let retry_outcome = retry_with_header(tool, "Authorization", &auth_header, fetch_headers)?;
    handle_retry_outcome(retry_outcome, is_json)
}

fn pay_x402_and_retry(
    requirements: &X402Challenge,
    tool: &Tool,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    keypair_override: Option<&str>,
    verbose: bool,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(output_fmt);
    let config = Config::load()?;
    let keypair_path = keypair_override
        .map(std::borrow::ToOwned::to_owned)
        .or_else(|| config.default_keypair_source())
        .ok_or_else(|| {
            pay_core::Error::Config("No keypair configured. Run `pay setup`.".to_string())
        })?;

    if verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let payment_json = x402::build_payment(requirements, &keypair_path)?;

    if verbose && !is_json {
        eprintln!("{}", "Payment signed, retrying...\n".dimmed());
    }

    let retry_outcome = retry_with_header(tool, "X-PAYMENT", &payment_json, fetch_headers)?;
    handle_retry_outcome(retry_outcome, is_json)
}

fn retry_with_header(
    tool: &Tool,
    header_name: &str,
    header_value: &str,
    fetch_headers: Option<Vec<(String, String)>>,
) -> pay_core::Result<RunOutcome> {
    match tool {
        Tool::Curl(args) => {
            let extra = vec![format!("{header_name}: {header_value}")];
            run_curl_with_headers(args, &extra)
        }
        Tool::Wget(args) => {
            let extra = vec![format!("{header_name}: {header_value}")];
            run_wget_with_headers(args, &extra)
        }
        Tool::Fetch { url, .. } => {
            let mut headers = fetch_headers.unwrap_or_default();
            headers.push((header_name.to_string(), header_value.to_string()));
            pay_core::fetch::fetch(url, &headers)
        }
    }
}

fn handle_retry_outcome(outcome: RunOutcome, is_json: bool) -> pay_core::Result<()> {
    match outcome {
        RunOutcome::Completed { exit_code, body } => {
            if let Some(body) = body {
                print!("{body}");
            }
            std::process::exit(exit_code);
        }
        _ => {
            if is_json {
                output::error_json("Server returned 402 again after payment");
            } else {
                eprintln!(
                    "{}",
                    "Error: Server returned 402 again after payment.".dimmed()
                );
            }
            std::process::exit(1);
        }
    }
}

/// Parse "Key: Value" header args into (key, value) pairs.
fn parse_header_args(args: &[String]) -> Vec<(String, String)> {
    args.iter()
        .filter_map(|h| {
            let (key, value) = h.split_once(':')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_header_args_basic() {
        let args: Vec<String> = vec![
            "Content-Type: application/json".to_string(),
            "Authorization: Bearer token123".to_string(),
        ];
        let headers = parse_header_args(&args);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "Content-Type");
        assert_eq!(headers[0].1, "application/json");
        assert_eq!(headers[1].0, "Authorization");
        assert_eq!(headers[1].1, "Bearer token123");
    }

    #[test]
    fn parse_header_args_empty() {
        let headers = parse_header_args(&[]);
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_header_args_no_colon() {
        let args: Vec<String> = vec!["no-colon-here".to_string()];
        let headers = parse_header_args(&args);
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_header_args_trims_whitespace() {
        let args: Vec<String> = vec!["  Key  :  Value  ".to_string()];
        let headers = parse_header_args(&args);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Key");
        assert_eq!(headers[0].1, "Value");
    }

    #[test]
    fn parse_header_args_value_with_colon() {
        let args: Vec<String> = vec!["Location: https://example.com:8080/path".to_string()];
        let headers = parse_header_args(&args);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Location");
        assert_eq!(headers[0].1, "https://example.com:8080/path");
    }

    #[test]
    fn tool_kind_curl() {
        let cmd = Command::Curl(curl::CurlCommand {
            args: vec!["https://example.com".to_string()],
        });
        assert!(matches!(cmd.tool_kind(), ToolKind::Curl));
    }

    #[test]
    fn tool_kind_wget() {
        let cmd = Command::Wget(wget::WgetCommand {
            args: vec!["https://example.com".to_string()],
        });
        assert!(matches!(cmd.tool_kind(), ToolKind::Wget));
    }

    #[test]
    fn tool_kind_mcp() {
        assert!(matches!(Command::Mcp.tool_kind(), ToolKind::Mcp));
    }
}
