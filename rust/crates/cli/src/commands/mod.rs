pub mod account;
pub mod claude;
pub mod codex;
pub mod curl;
pub mod fetch;
pub mod send;
pub mod server;
pub mod setup;
pub mod skills;
pub mod solana;
pub mod topup;
pub mod wget;

use clap::Subcommand;
use owo_colors::OwoColorize;
use pay_core::mpp;
use pay_core::runner::RunOutcome;
use pay_core::x402;
use pay_core::x402::Challenge as X402Challenge;
use pay_core::{run_curl_with_headers, run_wget_with_headers};
use solana_mpp::{ChargeRequest, SessionRequest};

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
    /// Run a Solana CLI command with your pay account keypair.
    Solana(solana::SolanaCommand),
    /// Send SOL to a recipient address.
    Send(send::SendCommand),
    /// Generate a keypair, store it, and fund your account.
    Setup(setup::SetupCommand),
    /// Fund your account on localnet via Surfpool.
    Topup(topup::TopupCommand),
    /// Payment gateway server (start, scaffold).
    Server {
        #[command(subcommand)]
        command: server::ServerCommand,
    },
    /// Browse, search, and inspect API providers from the skills catalog.
    Skills {
        #[command(subcommand)]
        command: skills::SkillsCommand,
    },
    /// Add a provider source (shorthand for `skills add`).
    #[command(alias = "add", short_flag = 'i')]
    Install(skills::install::InstallCommand),
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
    #[allow(dead_code)] // used by session budget TUI (currently disabled)
    pub fn tool_kind(&self) -> ToolKind {
        match self {
            Command::Curl(_) => ToolKind::Curl,
            Command::Wget(_) => ToolKind::Wget,
            Command::Fetch(_) => ToolKind::Fetch,
            Command::Claude(_) => ToolKind::Claude,
            Command::Codex(_) => ToolKind::Codex,
            Command::Account { .. }
            | Command::Skills { .. }
            | Command::Install(_)
            | Command::Send(_)
            | Command::Setup(_)
            | Command::Topup(_)
            | Command::Solana(_)
            | Command::Server { .. } => ToolKind::Mcp,
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
    #[allow(clippy::too_many_arguments)]
    pub fn execute(
        self,
        auto_pay: bool,
        output_fmt: Option<OutputFormat>,
        keypair_override: Option<&str>,
        network_override: Option<&str>,
        account_override: Option<&str>,
        verbose: bool,
        sandbox: bool,
    ) -> pay_core::Result<()> {
        let pay_bin = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "pay".to_string());

        match self {
            Command::Account { command } => return command.run(keypair_override),
            Command::Skills { command } => return command.run(),
            Command::Install(cmd) => return cmd.run(),
            Command::Solana(cmd) => std::process::exit(cmd.run(keypair_override)?),
            Command::Send(cmd) => return cmd.run(keypair_override, verbose),
            Command::Setup(cmd) => return cmd.run(),
            Command::Topup(cmd) => return cmd.run(),
            Command::Server { command } => return command.run(keypair_override, sandbox),
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
            Command::Claude(cmd) => std::process::exit(cmd.run(&pay_bin, account_override)?),
            Command::Codex(cmd) => std::process::exit(cmd.run(&pay_bin, account_override)?),
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
                    network_override,
                    account_override,
                    sandbox,
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
            network_override,
            account_override,
            sandbox,
            verbose,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_outcome(
    outcome: RunOutcome,
    tool: &Tool,
    auto_pay: bool,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    network_override: Option<&str>,
    account_override: Option<&str>,
    sandbox: bool,
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
                    &resource_url,
                    PaymentRetryContext {
                        tool,
                        output_fmt,
                        fetch_headers,
                        network_override,
                        account_override,
                        verbose,
                    },
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

        RunOutcome::SessionChallenge {
            challenge,
            resource_url,
        } => {
            let req: Option<SessionRequest> = challenge.request.decode().ok();
            let cap_usdc = req
                .as_ref()
                .and_then(|r| r.cap.parse::<u64>().ok())
                .unwrap_or(0) as f64
                / 1_000_000.0;

            if auto_pay {
                if verbose && !is_json {
                    eprintln!(
                        "{}",
                        format!("402 Payment Required (MPP session) — cap ${cap_usdc:.2} USDC — opening session…").dimmed()
                    );
                }
                return pay_session_and_retry(
                    &challenge,
                    req.as_ref(),
                    tool,
                    output_fmt,
                    fetch_headers,
                    network_override,
                    account_override,
                    sandbox,
                    verbose,
                );
            }

            if is_json {
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "protocol": "mpp-session",
                    "challenge": {
                        "cap_usdc": cap_usdc,
                        "currency": req.as_ref().map(|r| &r.currency),
                        "network": req.as_ref().and_then(|r| r.network.as_deref()),
                        "min_voucher_delta": req.as_ref().and_then(|r| r.min_voucher_delta.as_deref()),
                        "recipient": req.as_ref().map(|r| &r.recipient),
                    },
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "402 Payment Required (MPP session) — cap ${cap_usdc:.2} USDC — use --yolo to open a session automatically",
                    )
                    .dimmed()
                );
            }
        }

        RunOutcome::X402Challenge {
            challenge,
            resource_url,
        } => {
            if auto_pay {
                if verbose && !is_json {
                    eprintln!(
                        "{}",
                        format!(
                            "402 Payment Required (x402) — {} {}",
                            challenge.requirements.amount, challenge.requirements.currency
                        )
                        .dimmed()
                    );
                }
                return pay_x402_and_retry(
                    &challenge,
                    &resource_url,
                    PaymentRetryContext {
                        tool,
                        output_fmt,
                        fetch_headers,
                        network_override,
                        account_override,
                        verbose,
                    },
                );
            }

            if is_json {
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "protocol": "x402",
                    "challenge": {
                        "amount": challenge.requirements.amount,
                        "currency": challenge.requirements.currency,
                        "recipient": challenge.requirements.recipient,
                        "description": challenge.requirements.description,
                        "cluster": challenge.requirements.cluster,
                    },
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "402 Payment Required (x402) — {} {} — use --yolo to pay automatically",
                        challenge.requirements.amount, challenge.requirements.currency
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

        RunOutcome::PaymentRejected {
            reason, retryable, ..
        } => {
            // First-call rejection: the request already carried an Authorization
            // header (e.g. cached from a previous run) and the server rejected
            // it. There's no point retrying with the same header — surface the
            // reason and exit.
            if is_json {
                output::print_json(&serde_json::json!({
                    "status": 402,
                    "error": "payment_rejected",
                    "reason": reason,
                    "retryable": retryable,
                }))?;
            } else {
                let body = if retryable {
                    format!("{reason}\n(retryable — try again)")
                } else {
                    reason
                };
                eprintln!(
                    "{}",
                    crate::components::notice(
                        crate::components::NoticeLevel::Error,
                        "Payment rejected by verifier",
                        &body,
                    )
                );
            }
            std::process::exit(1);
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

struct PaymentRetryContext<'a, 'tool> {
    tool: &'a Tool<'tool>,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    network_override: Option<&'a str>,
    account_override: Option<&'a str>,
    verbose: bool,
}

fn pay_mpp_and_retry(
    challenge: &mpp::Challenge,
    resource_url: &str,
    ctx: PaymentRetryContext<'_, '_>,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(ctx.output_fmt);

    if ctx.verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let (auth_header, ephemeral_notice) = mpp::build_credential(
        challenge,
        &store,
        ctx.network_override,
        ctx.account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = ephemeral_notice {
        render_generated_wallet_notice(&resolved, is_json)?;
    }

    if ctx.verbose && !is_json {
        eprintln!("{}", "Payment signed, retrying...\n".dimmed());
    }

    let retry_outcome =
        retry_with_header(ctx.tool, "Authorization", &auth_header, ctx.fetch_headers)?;
    handle_retry_outcome(retry_outcome, is_json)
}

fn pay_x402_and_retry(
    challenge: &X402Challenge,
    resource_url: &str,
    ctx: PaymentRetryContext<'_, '_>,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(ctx.output_fmt);

    if ctx.verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let (payment_header_name, payment_json, ephemeral_notice) = x402::build_payment(
        challenge,
        &store,
        ctx.network_override,
        ctx.account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = ephemeral_notice {
        render_generated_wallet_notice(&resolved, is_json)?;
    }

    if ctx.verbose && !is_json {
        eprintln!("{}", "Payment signed, retrying...\n".dimmed());
    }

    let retry_outcome = retry_with_header(
        ctx.tool,
        payment_header_name,
        &payment_json,
        ctx.fetch_headers,
    )?;
    handle_retry_outcome(retry_outcome, is_json)
}

#[allow(clippy::too_many_arguments)]
fn pay_session_and_retry(
    challenge: &mpp::Challenge,
    req: Option<&SessionRequest>,
    tool: &Tool,
    output_fmt: Option<OutputFormat>,
    fetch_headers: Option<Vec<(String, String)>>,
    network_override: Option<&str>,
    account_override: Option<&str>,
    sandbox: bool,
    verbose: bool,
) -> pay_core::Result<()> {
    use solana_mpp::SessionMode;

    let is_json = no_dna::should_json(output_fmt);

    // Deposit = min_voucher_delta * 1000, clamped to [1 USDC, cap].
    let min_delta = req
        .and_then(|r| r.min_voucher_delta.as_deref())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1_000);
    let cap = req
        .and_then(|r| r.cap.parse::<u64>().ok())
        .unwrap_or(1_000_000);
    let deposit = (min_delta * 1_000).max(1_000_000).min(cap);

    // Prefer pull mode if advertised — it doesn't require an on-chain Fiber channel.
    let use_pull = req
        .map(|r| r.modes.contains(&SessionMode::Pull))
        .unwrap_or(false);

    let auth_header = if use_pull {
        let Some(request) = req else {
            return Err(pay_core::Error::Mpp(
                "pull-mode session requires a decoded SessionRequest".to_string(),
            ));
        };

        if verbose && !is_json {
            eprintln!(
                "{}",
                format!(
                    "Opening pull-mode session (deposit {} µUSDC, operator {})…",
                    deposit,
                    &request.operator[..8.min(request.operator.len())]
                )
                .dimmed()
            );
        }

        let store = pay_core::accounts::FileAccountsStore::default_path();
        let (_handle, header) = pay_core::session::open_pull_session_header(
            challenge,
            request,
            &store,
            network_override,
            account_override,
            deposit,
            sandbox,
        )?;

        if verbose && !is_json {
            eprintln!(
                "{}",
                "Pull session ready — delegation txs built, sending request…\n".dimmed()
            );
        }

        header
    } else {
        if verbose && !is_json {
            eprintln!(
                "{}",
                format!("Opening push session (deposit {} µUSDC)…", deposit).dimmed()
            );
        }

        let (_handle, header) = pay_core::session::open_session_header(challenge, deposit)?;

        if verbose && !is_json {
            eprintln!("{}", "Push session opened — sending request…\n".dimmed());
        }

        header
    };

    let retry_outcome = retry_with_header(tool, "Authorization", &auth_header, fetch_headers)?;
    handle_retry_outcome(retry_outcome, is_json)
}

/// Render the "Generated <network> wallet" notice when an ephemeral
/// wallet was just lazy-created. Visible only in text mode — JSON output
/// gets the same info as a structured side-channel field via stderr so
/// pipelines don't break.
fn render_generated_wallet_notice(
    resolved: &pay_core::accounts::ResolvedEphemeral,
    is_json: bool,
) -> pay_core::Result<()> {
    if is_json {
        // Print to stderr so the program's primary stdout (the API
        // response body) stays clean for piping.
        let payload = serde_json::json!({
            "event": "ephemeral_wallet_created",
            "network": resolved.network,
            "account": resolved.account_name,
            "pubkey": resolved.account.pubkey,
        });
        eprintln!("{payload}");
        return Ok(());
    }
    let pubkey = resolved.account.pubkey.as_deref().unwrap_or("(unknown)");
    let body = format!(
        "{}\nStored at ~/.config/pay/accounts.yml — reused on subsequent runs.",
        pubkey
    );
    eprintln!(
        "{}",
        crate::components::notice(
            crate::components::NoticeLevel::Info,
            &format!("Generated {} wallet", resolved.network),
            &body,
        )
    );
    Ok(())
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
        RunOutcome::PaymentRejected {
            reason, retryable, ..
        } => {
            if is_json {
                output::error_json(&format!("Payment rejected by verifier: {reason}"));
            } else {
                let body = if retryable {
                    format!("{reason}\n(retryable — try again)")
                } else {
                    reason
                };
                eprintln!(
                    "{}",
                    crate::components::notice(
                        crate::components::NoticeLevel::Error,
                        "Payment rejected by verifier",
                        &body,
                    )
                );
            }
            std::process::exit(1);
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

    #[test]
    fn x402_retry_supports_v1_and_v2_header_names() {
        assert_eq!(pay_core::x402::X402_V1_PAYMENT_HEADER, "X-PAYMENT");
        assert_eq!(pay_core::x402::X402_V2_PAYMENT_HEADER, "PAYMENT-SIGNATURE");
    }
}
