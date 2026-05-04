pub mod account;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod curl;
pub mod fetch;
pub mod help;
pub mod http;
pub mod send;
pub mod server;
pub mod setup;
pub mod skills;
pub mod topup;
pub mod wget;
pub mod whoami;

use clap::Subcommand;
use owo_colors::OwoColorize;
use pay_core::mpp;
use pay_core::runner::RunOutcome;
use pay_core::x402;
use pay_core::x402::Challenge as X402Challenge;
use pay_core::{run_curl_with_headers, run_httpie_with_headers, run_wget_with_headers};
use pay_types::Stablecoin;
use solana_mpp::{ChargeRequest, SessionRequest};

use crate::no_dna;
use crate::output::{self, OutputFormat};

#[derive(Subcommand)]
pub enum Command {
    /// Make an HTTP request via curl, handling 402 Payment Required flows.
    Curl(curl::CurlCommand),
    /// Download a resource via wget, handling 402 Payment Required flows.
    Wget(wget::WgetCommand),
    /// Make an HTTP request via HTTPie, handling 402 Payment Required flows.
    Http(http::HttpCommand),
    /// Fetch a URL using the built-in HTTP client (no external tool required).
    Fetch(fetch::FetchCommand),
    /// Run Claude Code with 402 payment support.
    Claude(claude::ClaudeCommand),
    /// Run Codex with 402 payment support.
    Codex(codex::CodexCommand),
    /// Manage accounts (new, import, list, default, remove, export).
    /// With no subcommand, lists accounts and prints the available subcommands.
    #[command(alias = "accounts")]
    Account {
        #[command(subcommand)]
        command: Option<account::AccountCommand>,
    },
    /// Show the system user, the active mainnet account, and its stablecoin
    /// balances.
    Whoami(whoami::WhoamiCommand),
    /// Send stablecoins to a recipient address.
    #[command(alias = "push")]
    Send(send::SendCommand),
    /// Generate a keypair, store it, and fund your account.
    Setup(setup::SetupCommand),
    /// Import funds from Venmo, PayPal, or a mobile wallet.
    Topup(topup::TopupCommand),
    /// As a developer, use pay to gate your API with stablecoin payments.
    Server {
        #[command(subcommand)]
        command: server::ServerCommand,
    },
    /// Browse, search, and inspect API providers from the skills catalog.
    Skills {
        #[command(subcommand)]
        command: skills::SkillsCommand,
    },
    /// Author providers for the skills catalog: scaffold, build, probe, validate.
    Catalog {
        #[command(subcommand)]
        command: catalog::CatalogCommand,
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
    Http,
    Fetch,
    Claude,
    Codex,
    Mcp,
}

impl Command {
    pub fn otlp_sidecar(&self) -> Option<&str> {
        match self {
            Command::Server { command } => command.otlp_sidecar(),
            _ => None,
        }
    }

    /// Whether this command needs a configured pay account before it can
    /// run usefully. Used by `main` to auto-run `pay setup` on a fresh
    /// install when the user invokes a payment-bearing command directly
    /// (e.g. `npx @solana/pay claude "buy me some flowers"`).
    ///
    /// Setup itself, account-management subcommands, and informational
    /// commands (whoami, skills, mcp, server) are excluded — they either
    /// don't need an account or handle the missing-account case
    /// gracefully on their own.
    pub fn requires_account(&self) -> bool {
        match self {
            Command::Curl(_)
            | Command::Wget(_)
            | Command::Http(_)
            | Command::Fetch(_)
            | Command::Claude(_)
            | Command::Codex(_)
            | Command::Send(_)
            | Command::Topup(_) => true,
            Command::Setup(_)
            | Command::Account { .. }
            | Command::Whoami(_)
            | Command::Skills { .. }
            | Command::Catalog { .. }
            | Command::Install(_)
            | Command::Server { .. }
            | Command::Mcp => false,
        }
    }

    /// Which tool this command wraps.
    #[allow(dead_code)] // used by session budget TUI (currently disabled)
    pub fn tool_kind(&self) -> ToolKind {
        match self {
            Command::Curl(_) => ToolKind::Curl,
            Command::Wget(_) => ToolKind::Wget,
            Command::Http(_) => ToolKind::Http,
            Command::Fetch(_) => ToolKind::Fetch,
            Command::Claude(_) => ToolKind::Claude,
            Command::Codex(_) => ToolKind::Codex,
            Command::Account { .. }
            | Command::Whoami(_)
            | Command::Skills { .. }
            | Command::Catalog { .. }
            | Command::Install(_)
            | Command::Send(_)
            | Command::Setup(_)
            | Command::Topup(_)
            | Command::Server { .. } => ToolKind::Mcp,
            Command::Mcp => ToolKind::Mcp,
        }
    }
}

/// Which underlying tool to use for retry.
enum Tool<'a> {
    Curl(&'a [String]),
    Wget(&'a [String]),
    Http(&'a [String]),
    Fetch { url: &'a str },
}

impl Command {
    #[allow(clippy::too_many_arguments)]
    pub fn execute(
        self,
        auto_pay: bool,
        output_fmt: Option<OutputFormat>,
        payment_cap: Option<u64>,
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
            Command::Account { command } => match command {
                Some(cmd) => return cmd.run(),
                None => return account::run_default(),
            },
            Command::Whoami(cmd) => return cmd.run(network_override, account_override),
            Command::Skills { command } => return command.run(),
            Command::Catalog { command } => return command.run(),
            Command::Install(cmd) => return cmd.run(),
            Command::Send(cmd) => {
                return cmd.run(network_override, account_override, verbose);
            }
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
            Command::Http(cmd) => (pay_core::run_httpie(&cmd.args)?, Tool::Http(&cmd.args)),
            Command::Fetch(cmd) => {
                let parsed_headers = parse_header_args(&cmd.headers);
                pay_core::skills::validate_cached_catalog_request("GET", &cmd.url, None)?;
                let outcome = pay_core::fetch::fetch(&cmd.url, &parsed_headers)?;
                let tool = Tool::Fetch { url: &cmd.url };
                return handle_outcome(
                    outcome,
                    &tool,
                    auto_pay,
                    output_fmt,
                    payment_cap,
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
            payment_cap,
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
    payment_cap: Option<u64>,
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
            alternatives,
            resource_url,
        } => {
            let req: ChargeRequest = challenge.request.decode().unwrap_or_default();
            let mut challenges = Vec::with_capacity(1 + alternatives.len());
            challenges.push((*challenge).clone());
            challenges.extend(alternatives);
            if auto_pay {
                let capped_challenges;
                let challenges_to_pay = if let Some(cap) = payment_cap {
                    capped_challenges = mpp_challenges_within_cap(&challenges, cap)?;
                    capped_challenges.as_slice()
                } else {
                    challenges.as_slice()
                };
                if verbose && !is_json {
                    let currencies = mpp_challenge_currencies(&challenges).join(", ");
                    eprintln!(
                        "{}",
                        format!(
                            "402 Payment Required (MPP) — {} {}",
                            req.amount,
                            if currencies.is_empty() {
                                req.currency.clone()
                            } else {
                                currencies
                            }
                        )
                        .dimmed()
                    );
                }
                return pay_mpp_and_retry(
                    challenges_to_pay,
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
                    "challenges": mpp_challenges_json(&challenges),
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
                        "402 Payment Required (MPP) — {} {}",
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
                enforce_session_cap(req.as_ref(), payment_cap)?;
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
                    format!("402 Payment Required (MPP session) — cap ${cap_usdc:.2} USDC")
                        .dimmed()
                );
            }
        }

        RunOutcome::X402Challenge {
            challenge,
            resource_url,
        } => {
            if auto_pay {
                enforce_payment_cap(
                    &challenge.requirements.amount,
                    &challenge.requirements.currency,
                    payment_cap,
                    "x402",
                )?;
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
                        "402 Payment Required (x402) — {} {}",
                        challenge.requirements.amount, challenge.requirements.currency
                    )
                    .dimmed()
                );
            }
        }

        RunOutcome::X402SignInChallenge {
            challenge,
            resource_url,
        } => {
            if auto_pay {
                if verbose && !is_json {
                    eprintln!("{}", "402 Sign-In Required (x402)".dimmed());
                }
                return pay_x402_siwx_and_retry(
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
                    "protocol": "x402-siwx",
                    "resource": resource_url,
                }))?;
            } else {
                eprintln!("{}", "402 Sign-In Required (x402)".dimmed());
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
                crate::components::print_notice(
                    crate::components::NoticeLevel::Error,
                    "Payment rejected by verifier",
                    &body,
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

fn mpp_challenges_within_cap(
    challenges: &[mpp::Challenge],
    payment_cap: u64,
) -> pay_core::Result<Vec<mpp::Challenge>> {
    let mut allowed = Vec::new();
    let mut lowest_required: Option<(u64, String, String)> = None;
    let mut unsupported_currencies = Vec::new();

    for challenge in challenges {
        let request: ChargeRequest = challenge.request.decode().map_err(|e| {
            pay_core::Error::Mpp(format!("Failed to decode challenge request: {e}"))
        })?;
        let amount_micro = match amount_as_stablecoin_micro(&request.amount, &request.currency) {
            Ok(amount_micro) => amount_micro,
            Err(pay_core::Error::PaymentRejected(_)) => {
                unsupported_currencies.push(request.currency);
                continue;
            }
            Err(err) => return Err(err),
        };

        if amount_micro <= payment_cap {
            allowed.push(challenge.clone());
        }

        if lowest_required
            .as_ref()
            .is_none_or(|(lowest, _, _)| amount_micro < *lowest)
        {
            lowest_required = Some((amount_micro, request.amount, request.currency));
        }
    }

    if !allowed.is_empty() {
        return Ok(allowed);
    }

    if let Some((required_micro, _amount, currency)) = lowest_required {
        return Err(payment_cap_error(
            "MPP",
            &currency,
            required_micro,
            payment_cap,
        ));
    }

    unsupported_currencies.sort();
    unsupported_currencies.dedup();
    if !unsupported_currencies.is_empty() {
        return Err(pay_core::Error::PaymentRejected(format!(
            "The automatic payment cap is stablecoin-denominated and cannot price advertised MPP currencies automatically: {}",
            unsupported_currencies.join(", ")
        )));
    }

    Err(pay_core::Error::PaymentRejected(
        "no MPP payment challenge was available".to_string(),
    ))
}

fn enforce_session_cap(
    request: Option<&SessionRequest>,
    payment_cap: Option<u64>,
) -> pay_core::Result<()> {
    let Some(payment_cap) = payment_cap else {
        return Ok(());
    };
    let Some(request) = request else {
        return Err(pay_core::Error::Mpp(
            "session payment cap requires a decoded SessionRequest".to_string(),
        ));
    };
    let required_micro = request
        .cap
        .parse::<u64>()
        .map_err(|e| pay_core::Error::Mpp(format!("Invalid session cap: {e}")))?;

    if required_micro <= payment_cap {
        return Ok(());
    }

    Err(payment_cap_error(
        "MPP session",
        "USDC",
        required_micro,
        payment_cap,
    ))
}

fn enforce_payment_cap(
    amount: &str,
    currency: &str,
    payment_cap: Option<u64>,
    protocol: &str,
) -> pay_core::Result<()> {
    let Some(payment_cap) = payment_cap else {
        return Ok(());
    };
    let required_micro = amount_as_stablecoin_micro(amount, currency)?;
    if required_micro <= payment_cap {
        return Ok(());
    }
    Err(payment_cap_error(
        protocol,
        currency,
        required_micro,
        payment_cap,
    ))
}

fn payment_cap_error(
    protocol: &str,
    currency: &str,
    required_micro: u64,
    payment_cap: u64,
) -> pay_core::Error {
    pay_core::Error::PaymentRejected(format!(
        "{protocol} payment requires {} {currency}, above the automatic payment cap of {} stablecoins",
        format_stablecoin_amount(required_micro),
        format_stablecoin_amount(payment_cap),
    ))
}

fn amount_as_stablecoin_micro(amount: &str, currency: &str) -> pay_core::Result<u64> {
    let raw = amount
        .parse::<u64>()
        .map_err(|e| pay_core::Error::Mpp(format!("Invalid payment amount `{amount}`: {e}")))?;

    if is_known_stablecoin(currency) {
        return Ok(raw);
    }

    Err(pay_core::Error::PaymentRejected(format!(
        "The automatic payment cap is stablecoin-denominated and cannot price `{currency}` payments automatically"
    )))
}

fn is_known_stablecoin(currency: &str) -> bool {
    Stablecoin::parse_symbol(currency).is_some() || Stablecoin::from_mint(currency).is_some()
}

fn format_stablecoin_amount(amount: u64) -> String {
    let whole = amount / 1_000_000;
    let fraction = amount % 1_000_000;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut fraction = format!("{fraction:06}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
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
    challenges: &[mpp::Challenge],
    resource_url: &str,
    ctx: PaymentRetryContext<'_, '_>,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(ctx.output_fmt);
    validate_tool_request_before_signing(ctx.tool)?;

    if ctx.verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let challenge = mpp::select_challenge_by_balance(
        challenges,
        &store,
        ctx.network_override,
        ctx.account_override,
    )?
    .ok_or_else(|| {
        let networks = mpp_challenge_networks(challenges);
        let offered = if networks.is_empty() {
            "(none)".to_string()
        } else {
            networks.join(", ")
        };
        let active = ctx.network_override.unwrap_or("auto");
        pay_core::Error::Mpp(format!(
            "No MPP challenge matched the active network filter (active: {active}, offered: {offered}). \
             Drop `--network` or check `pay account list` for accounts on the offered networks."
        ))
    })?;
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

fn mpp_challenge_currencies(challenges: &[mpp::Challenge]) -> Vec<String> {
    challenges
        .iter()
        .filter_map(|challenge| {
            let request: ChargeRequest = challenge.request.decode().ok()?;
            Some(request.currency)
        })
        .collect()
}

/// Distinct networks advertised across MPP challenges, used by error messages
/// to tell the user which networks the server offered.
fn mpp_challenge_networks(challenges: &[mpp::Challenge]) -> Vec<String> {
    let mut out: Vec<String> = challenges
        .iter()
        .filter_map(|challenge| {
            let request: ChargeRequest = challenge.request.decode().ok()?;
            request
                .method_details
                .as_ref()
                .and_then(|v| v.get("network"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn mpp_challenges_json(challenges: &[mpp::Challenge]) -> serde_json::Value {
    let values: Vec<serde_json::Value> = challenges
        .iter()
        .filter_map(|challenge| {
            let request: ChargeRequest = challenge.request.decode().ok()?;
            let network = request
                .method_details
                .as_ref()
                .and_then(|v| v.get("network"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(serde_json::json!({
                "amount": request.amount,
                "currency": request.currency,
                "recipient": request.recipient,
                "description": request.description,
                "network": network,
            }))
        })
        .collect();
    serde_json::Value::Array(values)
}

fn pay_x402_and_retry(
    challenge: &X402Challenge,
    resource_url: &str,
    ctx: PaymentRetryContext<'_, '_>,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(ctx.output_fmt);
    validate_tool_request_before_signing(ctx.tool)?;

    if ctx.verbose && !is_json {
        eprintln!("{}", "Paying...".dimmed());
    }

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let built_payment = x402::build_payment(
        challenge,
        &store,
        ctx.network_override,
        ctx.account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = built_payment.ephemeral_notice {
        render_generated_wallet_notice(&resolved, is_json)?;
    }

    if ctx.verbose && !is_json {
        eprintln!("{}", "Payment signed, retrying...\n".dimmed());
    }

    let retry_outcome = retry_with_headers(ctx.tool, &built_payment.headers, ctx.fetch_headers)?;
    handle_retry_outcome(retry_outcome, is_json)
}

fn pay_x402_siwx_and_retry(
    challenge: &x402::SiwxAuthChallenge,
    resource_url: &str,
    ctx: PaymentRetryContext<'_, '_>,
) -> pay_core::Result<()> {
    let is_json = no_dna::should_json(ctx.output_fmt);
    validate_tool_request_before_signing(ctx.tool)?;

    if ctx.verbose && !is_json {
        eprintln!("{}", "Signing in...".dimmed());
    }

    let store = pay_core::accounts::FileAccountsStore::default_path();
    let built_payment = x402::build_siwx_auth_header(
        challenge,
        &store,
        ctx.network_override,
        ctx.account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = built_payment.ephemeral_notice {
        render_generated_wallet_notice(&resolved, is_json)?;
    }

    if ctx.verbose && !is_json {
        eprintln!("{}", "Sign-in signed, retrying...\n".dimmed());
    }

    let retry_outcome = retry_with_headers(ctx.tool, &built_payment.headers, ctx.fetch_headers)?;
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
    validate_tool_request_before_signing(tool)?;

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

fn validate_tool_request_before_signing(tool: &Tool) -> pay_core::Result<()> {
    match tool {
        Tool::Curl(args) => pay_core::runner::validate_curl_args_against_catalog(args),
        Tool::Fetch { url } => pay_core::skills::validate_cached_catalog_request("GET", url, None),
        Tool::Wget(args) => pay_core::runner::validate_wget_args_against_catalog(args),
        // TODO: catalog validation for HTTPie request-item syntax
        // (`Header:Value`, `field=value`, `field:=raw`, …).
        Tool::Http(_) => Ok(()),
    }
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
    crate::components::print_notice(
        crate::components::NoticeLevel::Info,
        &format!("Generated {} wallet", resolved.network),
        &body,
    );
    Ok(())
}

fn retry_with_header(
    tool: &Tool,
    header_name: &str,
    header_value: &str,
    fetch_headers: Option<Vec<(String, String)>>,
) -> pay_core::Result<RunOutcome> {
    retry_with_headers(
        tool,
        &[(header_name, header_value.to_string())],
        fetch_headers,
    )
}

fn retry_with_headers(
    tool: &Tool,
    headers_to_add: &[(&str, String)],
    fetch_headers: Option<Vec<(String, String)>>,
) -> pay_core::Result<RunOutcome> {
    match tool {
        Tool::Curl(args) => {
            let extra = retry_header_args(headers_to_add);
            run_curl_with_headers(args, &extra)
        }
        Tool::Wget(args) => {
            let extra = retry_header_args(headers_to_add);
            run_wget_with_headers(args, &extra)
        }
        Tool::Http(args) => {
            let extra = retry_header_args_httpie(headers_to_add);
            run_httpie_with_headers(args, &extra)
        }
        Tool::Fetch { url, .. } => {
            let mut headers = fetch_headers.unwrap_or_default();
            headers.extend(
                headers_to_add
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone())),
            );
            pay_core::fetch::fetch(url, &headers)
        }
    }
}

fn retry_header_args(headers_to_add: &[(&str, String)]) -> Vec<String> {
    headers_to_add
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect()
}

/// Format headers as HTTPie request items: `Name:value` (no space after colon).
fn retry_header_args_httpie(headers_to_add: &[(&str, String)]) -> Vec<String> {
    headers_to_add
        .iter()
        .map(|(name, value)| format!("{name}:{value}"))
        .collect()
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
                crate::components::print_notice(
                    crate::components::NoticeLevel::Error,
                    "Payment rejected by verifier",
                    &body,
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
    fn amount_as_stablecoin_micro_treats_known_stablecoins_as_six_decimals() {
        assert_eq!(
            amount_as_stablecoin_micro("1250000", "USDC").unwrap(),
            1_250_000
        );
        assert_eq!(amount_as_stablecoin_micro("5000", "CASH").unwrap(), 5_000);
        assert_eq!(amount_as_stablecoin_micro("5000", "USDG").unwrap(), 5_000);
        assert_eq!(
            amount_as_stablecoin_micro("1000000", pay_types::stablecoin_mints::USDC_MAINNET,)
                .unwrap(),
            1_000_000
        );
        assert_eq!(
            amount_as_stablecoin_micro("1000000", pay_types::stablecoin_mints::USDG_MAINNET)
                .unwrap(),
            1_000_000
        );
    }

    #[test]
    fn amount_as_stablecoin_micro_rejects_sol_under_stablecoin_cap() {
        assert!(amount_as_stablecoin_micro("1000000000", "SOL").is_err());
    }

    #[test]
    fn mpp_cap_filter_skips_unpriced_assets_when_stablecoin_fits() {
        let challenges = vec![
            mpp_challenge("SOL", "1000000000"),
            mpp_challenge("USDC", "500000"),
        ];
        let allowed = mpp_challenges_within_cap(&challenges, 1_000_000).unwrap();
        assert_eq!(allowed.len(), 1);
        let request: ChargeRequest = allowed[0].request.decode().unwrap();
        assert_eq!(request.currency, "USDC");
    }

    #[test]
    fn mpp_cap_filter_rejects_when_only_unpriced_assets_are_available() {
        let challenges = vec![mpp_challenge("SOL", "1000000000")];
        let err = mpp_challenges_within_cap(&challenges, 1_000_000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot price advertised MPP currencies"));
    }

    fn mpp_challenge(currency: &str, amount: &str) -> mpp::Challenge {
        let request = serde_json::json!({
            "amount": amount,
            "currency": currency,
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": { "network": "mainnet" }
        });
        mpp::Challenge::new(
            currency,
            "test",
            "solana",
            "charge",
            solana_mpp::Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    #[test]
    fn format_stablecoin_amount_trims_fraction() {
        assert_eq!(format_stablecoin_amount(1_000_000), "1");
        assert_eq!(format_stablecoin_amount(1_250_000), "1.25");
        assert_eq!(format_stablecoin_amount(1), "0.000001");
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
        assert_eq!(pay_core::x402::SIGN_IN_WITH_X_HEADER, "SIGN-IN-WITH-X");
    }

    #[test]
    fn retry_header_args_preserves_multiple_x402_headers() {
        let headers = retry_header_args(&[
            (
                pay_core::x402::X402_V2_PAYMENT_HEADER,
                "payment".to_string(),
            ),
            (pay_core::x402::SIGN_IN_WITH_X_HEADER, "sign-in".to_string()),
        ]);

        assert_eq!(
            headers,
            vec![
                "PAYMENT-SIGNATURE: payment".to_string(),
                "SIGN-IN-WITH-X: sign-in".to_string()
            ]
        );
    }
}
