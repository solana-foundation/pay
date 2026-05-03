mod commands;
pub mod components;
pub mod debugger_proxy;
mod no_dna;
mod observability;
mod output;
pub mod system;
pub mod tui;

use clap::{CommandFactory, FromArgMatches, Parser};
use owo_colors::OwoColorize;
use tracing_subscriber::EnvFilter;

use commands::Command;
use pay_core::{Config, LogFormat};

#[derive(Parser)]
#[command(
    name = "pay",
    version,
    long_about = pay_core::instructions::INSTRUCTIONS,
)]
struct Opts {
    #[clap(subcommand)]
    command: Option<Command>,

    /// Automatically satisfy 402 challenges up to this stablecoin cap.
    #[arg(long = "yolo-upto", value_name = "AMOUNT", value_parser = parse_stablecoin_cap)]
    yolo_upto: Option<u64>,

    /// Sandbox mode: force network=localnet and route to the hosted
    /// Surfpool RPC (https://402.surfnet.dev:8899). Ephemeral wallets
    /// are auto-generated and funded on first use.
    #[arg(short = 's', long, conflicts_with = "mainnet")]
    sandbox: bool,

    /// Mainnet mode: force network=mainnet, use the wallet bound to
    /// `mainnet` in ~/.config/pay/accounts.yml. Overrides whatever the
    /// challenge advertises — useful when you know what you want.
    #[arg(long, conflicts_with = "sandbox")]
    mainnet: bool,

    /// Alias for --sandbox (hidden).
    #[arg(long, hide = true)]
    dev: bool,

    /// Local sandbox: force network=localnet but route to a localhost
    /// Surfpool (http://localhost:8899) instead of the hosted one.
    #[arg(long, conflicts_with = "mainnet")]
    local: bool,

    /// Force NO_DNA mode for machine-readable output and non-interactive
    /// defaults.
    #[arg(long)]
    no_dna: bool,

    /// Show verbose output (tracing logs, payment details).
    #[arg(short, long)]
    verbose: bool,

    /// Use a specific named account from `~/.config/pay/accounts.yml`.
    /// For `--local` / `--sandbox`, this selects a wallet within `localnet`.
    #[arg(long)]
    account: Option<String>,

    /// Launch the Payment Debugger proxy on port 1402. All MCP curl
    /// requests are routed through it, and the PDB UI is served at
    /// http://127.0.0.1:1402/
    #[arg(long)]
    debugger: bool,
}

fn main() {
    if root_overview_help_requested() {
        if args_include_no_dna() {
            no_dna::enable_for_process();
        }
        handle_missing_command();
        return;
    }

    let mut opts = parse_opts();
    if opts.no_dna {
        no_dna::enable_for_process();
    }

    let Some(command) = opts.command.take() else {
        handle_missing_command();
        return;
    };

    let config = Config::load().unwrap_or_else(|err| {
        eprintln!("{}", format!("Error: {err}").dimmed());
        std::process::exit(1);
    });

    // MCP server — needs its own runtime, exit early
    if matches!(command, Command::Mcp) {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("Failed to start MCP server: could not create tokio runtime: {err}");
                std::process::exit(1);
            }
        };
        if let Err(err) = rt.block_on(pay_mcp::run_server(&pay_mcp::McpOptions::default())) {
            eprintln!("MCP server error: {err}");
            std::process::exit(1);
        }
        return;
    }

    let otlp_sidecar = command.otlp_sidecar().map(str::to_owned);
    let _otel_guard = init_logging(config.log_format, opts.verbose, otlp_sidecar.as_deref());

    // ── Debugger proxy ─────────────────────────────────────────────────────
    //
    // When `--debugger` is set, spin up a forward proxy + PDB on port 1402
    // BEFORE launching the agent. The MCP curl tool will route through it
    // (via PAY_DEBUGGER_PROXY env var), capturing all traffic for the PDB UI.
    if opts.debugger {
        match debugger_proxy::start_background() {
            Ok(proxy_url) => {
                // SAFETY: called before any threads that read this var.
                unsafe { std::env::set_var("PAY_DEBUGGER_PROXY", &proxy_url) };
            }
            Err(e) => {
                eprintln!("{}", format!("Debugger proxy failed: {e}").dimmed());
            }
        }
    }

    // ── Network override + RPC URL ─────────────────────────────────────────
    //
    // The `--sandbox` / `--local` / `--mainnet` flags FORCE a specific
    // Solana network slug for wallet routing, regardless of what the 402
    // challenge advertises. With no flag, the network is read from the
    // challenge.
    //
    // For sandbox flavors, also pin the RPC URL via `PAY_RPC_URL` so the
    // mpp/x402 client talks to the right Surfpool instance. The wallet
    // itself is generated lazily on first use by the network-aware
    // `load_signer_for_network` path — no eager bootstrap, no Touch ID.
    let sandbox_mode = opts.sandbox || opts.local || opts.dev;

    if let Some(ref name) = opts.account {
        let accounts = pay_core::accounts::AccountsFile::load().unwrap_or_default();
        let enforced_network = if sandbox_mode {
            Some("localnet")
        } else if opts.mainnet {
            Some("mainnet")
        } else {
            None
        };
        if let Some(network) = enforced_network {
            let exists = accounts
                .accounts
                .get(network)
                .is_some_and(|net| net.contains_key(name.as_str()));
            if !exists {
                eprintln!("Error: account '{name}' not found in {network}.");
                std::process::exit(1);
            }
        }
    }

    let network_override: Option<String> = if sandbox_mode {
        let rpc_url = if opts.local {
            pay_core::config::LOCAL_RPC_URL.to_string()
        } else if let Ok(url) = std::env::var("PAY_RPC_URL") {
            url
        } else {
            pay_core::config::SANDBOX_RPC_URL.to_string()
        };
        // SAFETY: called before any threads are spawned.
        unsafe { std::env::set_var("PAY_RPC_URL", &rpc_url) };
        unsafe { std::env::set_var("PAY_NETWORK_ENFORCED", "localnet") };
        Some("localnet".to_string())
    } else if opts.mainnet {
        // SAFETY: called before any threads are spawned.
        unsafe { std::env::set_var("PAY_NETWORK_ENFORCED", "mainnet") };
        Some("mainnet".to_string())
    } else {
        None
    };

    // ── Legacy keypair source for non-payment commands ─────────────────────
    //
    // `pay topup`, `pay claude`, and friends still use the original
    // keystore-source-string flow.
    // Payment commands (`pay curl`/`wget`/`fetch`) don't read this — they
    // resolve the wallet via `network_override` + `accounts.yml` instead.
    //
    // In sandbox mode, NO command should probe the keychain — that would
    // defeat the whole point of `--sandbox`. The server start path
    // resolves its own ephemeral via the network-aware loader instead.
    let keypair_override: Option<String> = if sandbox_mode
        || matches!(
            command,
            Command::Setup(_)
                | Command::Account { .. }
                | Command::Whoami(_)
                | Command::Skills { .. }
                | Command::Install(_)
                | Command::Send(_)
                | Command::Curl(_)
                | Command::Wget(_)
                | Command::Http(_)
                | Command::Fetch(_)
        ) {
        None
    } else if matches!(command, Command::Server { .. } | Command::Topup(_)) {
        config.default_active_account_name()
    } else {
        resolve_keypair(&config)
    };

    let is_agent = no_dna::is_agent();
    let is_http_tool = matches!(
        command,
        Command::Curl(_) | Command::Wget(_) | Command::Http(_) | Command::Fetch(_)
    );
    // HTTP tools always auto-pay (Touch ID is the approval gate, not the TUI)
    let auto_pay =
        opts.yolo_upto.is_some() || sandbox_mode || is_agent || is_http_tool || config.auto_pay;

    // TODO: session budget TUI — skipped for now, not ready.
    let _has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

    let output_fmt = None;

    let verbose = opts.verbose;

    // First-run UX: when a payment-bearing command is invoked on a fresh
    // install (e.g. `npx @solana/pay claude "buy me some flowers"`), run
    // `pay setup` first so the user lands in the wizard instead of a
    // cryptic "no account configured" error mid-flight. Sandbox flows
    // generate ephemeral wallets on first use, so they're exempt.
    if command.requires_account() && !sandbox_mode && !has_any_account() {
        eprintln!(
            "{}",
            "No pay account configured — running `pay setup` first…".dimmed()
        );
        if let Err(err) = (commands::setup::SetupCommand {
            force: false,
            backend: None,
            vault: None,
            update: false,
        })
        .run()
        {
            eprintln!("{}", format!("Error: setup failed: {err}").dimmed());
            std::process::exit(1);
        }
    }

    if let Err(err) = command.execute(
        auto_pay,
        output_fmt,
        opts.yolo_upto,
        keypair_override.as_deref(),
        network_override.as_deref(),
        opts.account.as_deref(),
        verbose,
        sandbox_mode,
    ) {
        if no_dna::should_json(output_fmt) {
            output::error_json(&err.to_string());
        } else if let pay_core::Error::PaymentRejected(detail) = &err {
            eprintln!(
                "{}",
                components::notice(components::NoticeLevel::Warning, "Payment rejected", detail,)
            );
        } else {
            eprintln!("{}", format!("Error: {err}").dimmed());
        }
        std::process::exit(1);
    }
}

fn handle_missing_command() {
    let mut command = Opts::command();
    configure_help(&mut command, !no_dna::is_agent(), true);

    if no_dna::is_agent() {
        if let Err(err) = output::print_json(&command_catalog(&command)) {
            output::error_json(&err.to_string());
            std::process::exit(1);
        }
    } else {
        print_root_overview();
    }
}

fn print_root_overview() {
    println!("{}", components::pay_help_banner());
    println!();
    println!("{}", components::ROOT_COMMAND_SUMMARY);
}

fn parse_opts() -> Opts {
    let no_dna_requested = no_dna::is_agent() || args_include_no_dna();
    let mut command = Opts::command();
    configure_help(&mut command, !no_dna_requested, true);
    let matches = command.get_matches();
    Opts::from_arg_matches(&matches).unwrap_or_else(|err| err.exit())
}

fn args_include_no_dna() -> bool {
    std::env::args_os().any(|arg| arg == std::ffi::OsStr::new("--no-dna"))
}

fn parse_stablecoin_cap(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    let without_symbol = trimmed.strip_prefix('$').unwrap_or(trimmed).trim();
    let without_suffix = without_symbol
        .strip_suffix("USDC")
        .or_else(|| without_symbol.strip_suffix("usdc"))
        .unwrap_or(without_symbol)
        .trim();

    parse_decimal_micro_units(without_suffix)
        .and_then(|cap| {
            if cap == 0 {
                Err("cap must be greater than 0".to_string())
            } else {
                Ok(cap)
            }
        })
        .map_err(|err| format!("invalid stablecoin cap `{input}`: {err}"))
}

fn parse_decimal_micro_units(input: &str) -> Result<u64, String> {
    if input.is_empty() {
        return Err("amount must not be empty".to_string());
    }
    if input.starts_with('-') {
        return Err("amount must be positive".to_string());
    }

    let mut parts = input.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 6
    {
        return Err("use a decimal amount with at most 6 decimal places".to_string());
    }

    let whole_units = whole
        .parse::<u64>()
        .map_err(|_| "amount is too large".to_string())?
        .checked_mul(1_000_000)
        .ok_or_else(|| "amount is too large".to_string())?;

    let mut fraction_units = 0u64;
    for (index, byte) in fraction.bytes().enumerate() {
        let digit = (byte - b'0') as u64;
        let place = 10_u64.pow(5 - index as u32);
        fraction_units = fraction_units
            .checked_add(digit * place)
            .ok_or_else(|| "amount is too large".to_string())?;
    }

    whole_units
        .checked_add(fraction_units)
        .ok_or_else(|| "amount is too large".to_string())
}

fn root_overview_help_requested() -> bool {
    let mut saw_help = false;
    let mut expect_value_for: Option<&'static str> = None;

    for arg in std::env::args_os().skip(1) {
        if expect_value_for.take().is_some() {
            continue;
        }

        let Some(arg) = arg.to_str() else {
            return false;
        };

        match arg {
            "-h" | "--help" => saw_help = true,
            "help" => return std::env::args_os().len() == 2,
            "-s" | "--sandbox" | "--mainnet" | "--local" | "--no-dna" | "-v" | "--verbose"
            | "--debugger" | "--dev" => {}
            "--yolo-upto" => expect_value_for = Some("--yolo-upto"),
            _ if arg.starts_with("--yolo-upto=") => {}
            "--account" => expect_value_for = Some("--account"),
            _ if arg.starts_with("--account=") => {}
            _ => return false,
        }
    }

    saw_help
}

fn configure_help(command: &mut clap::Command, show_banner: bool, is_root: bool) {
    let mut configured = command.clone().term_width(80);

    if !is_root {
        configured = configured.disable_help_subcommand(true);
    }

    if show_banner && is_root {
        configured = configured.before_help(components::pay_help_banner());
    }

    if is_root {
        configured = configured
            .help_template(components::ROOT_HELP_TEMPLATE)
            .after_help(components::ROOT_COMMAND_SUMMARY);
    }

    *command = configured;

    for subcommand in command.get_subcommands_mut() {
        configure_help(subcommand, show_banner, false);
    }
}

fn command_catalog(command: &clap::Command) -> serde_json::Value {
    let mut flat_commands = Vec::new();
    let root_path = vec![command.get_name().to_string()];
    let commands = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| command_catalog_entry(subcommand, &root_path, &mut flat_commands))
        .collect::<Vec<_>>();

    serde_json::json!({
        "usage": command_usage(command, &[command.get_name().to_string()]),
        "hint": "Run `pay help <command>` for command-specific usage.",
        "categories": {
            "supported_pass_through": components::SUPPORTED_PASS_THROUGH_COMMANDS,
            "developers": components::DEVELOPER_COMMANDS,
            "agents": components::AGENT_COMMANDS,
            "account_management": components::ACCOUNT_MANAGEMENT_COMMANDS,
            "other": components::OTHER_COMMANDS,
        },
        "commands": commands,
        "flat_commands": flat_commands,
    })
}

fn command_catalog_entry(
    command: &clap::Command,
    parent_path: &[String],
    flat_commands: &mut Vec<String>,
) -> serde_json::Value {
    let mut path = parent_path.to_vec();
    path.push(command.get_name().to_string());
    let command_path = path.join(" ");
    flat_commands.push(command_path.clone());

    let subcommands = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| command_catalog_entry(subcommand, &path, flat_commands))
        .collect::<Vec<_>>();

    serde_json::json!({
        "name": command.get_name(),
        "command": command_path,
        "category": root_command_category(command.get_name(), parent_path),
        "aliases": command.get_all_aliases().collect::<Vec<_>>(),
        "short_flag_aliases": command
            .get_all_short_flag_aliases()
            .map(|alias| format!("-{alias}"))
            .collect::<Vec<_>>(),
        "long_flag_aliases": command
            .get_all_long_flag_aliases()
            .map(|alias| format!("--{alias}"))
            .collect::<Vec<_>>(),
        "summary": command_summary(command),
        "usage": command_usage(command, &path),
        "subcommands": subcommands,
    })
}

fn command_usage(command: &clap::Command, path: &[String]) -> String {
    let usage = command.clone().render_usage().to_string();
    let leaf_prefix = format!("Usage: {}", command.get_name());
    if let Some(rest) = usage.strip_prefix(&leaf_prefix) {
        format!("Usage: {}{rest}", path.join(" "))
    } else {
        usage
    }
}

fn command_summary(command: &clap::Command) -> Option<String> {
    command
        .get_about()
        .or_else(|| command.get_long_about())
        .map(|summary| summary.to_string())
}

fn root_command_category(command_name: &str, parent_path: &[String]) -> Option<&'static str> {
    if parent_path.len() != 1 {
        return None;
    }

    if components::SUPPORTED_PASS_THROUGH_COMMANDS.contains(&command_name) {
        Some("supported_pass_through")
    } else if components::DEVELOPER_COMMANDS.contains(&command_name) {
        Some("developers")
    } else if components::AGENT_COMMANDS.contains(&command_name) {
        Some("agents")
    } else if components::ACCOUNT_MANAGEMENT_COMMANDS.contains(&command_name) {
        Some("account_management")
    } else if components::OTHER_COMMANDS.contains(&command_name) {
        Some("other")
    } else {
        None
    }
}

/// True if any pay account exists on disk (any network, any name).
/// Errors loading the file are treated as "no account" so the first-run
/// path triggers a clean setup instead of bailing on a corrupt config.
fn has_any_account() -> bool {
    pay_core::accounts::AccountsFile::load()
        .map(|f| f.accounts.values().any(|net| !net.is_empty()))
        .unwrap_or(false)
}

fn init_logging(
    log_format: LogFormat,
    verbose: bool,
    otlp_sidecar: Option<&str>,
) -> Option<observability::OtelGuard> {
    let default = if verbose || otlp_sidecar.is_some() {
        "pay=info,warn"
    } else {
        "warn"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    if let Some(sidecar) = otlp_sidecar {
        return match observability::init_otlp(sidecar, filter) {
            Ok(guard) => Some(guard),
            Err(err) => {
                eprintln!("{}", format!("Error: {err}").dimmed());
                std::process::exit(1);
            }
        };
    }

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);

    match log_format {
        LogFormat::Text => {
            builder.init();
        }
        LogFormat::Json => {
            builder.json().init();
        }
    }
    None
}

/// Find a usable keypair, or tell the user to run `pay setup`.
fn resolve_keypair(config: &Config) -> Option<String> {
    if let Some(source) = config.default_active_account_name() {
        return Some(source);
    }

    // No account configured
    eprintln!("{}", "No account configured.".dimmed());
    eprintln!();
    eprintln!(
        "{}",
        "  pay setup              Generate a keypair (macOS Keychain + Touch ID)".dimmed()
    );
    eprintln!(
        "{}",
        "  PAY_ACTIVE_ACCOUNT=<name>     Use a specific account from accounts.yml".dimmed()
    );
    eprintln!(
        "{}",
        "  pay --sandbox ...      Ephemeral keypair on Surfpool sandbox".dimmed()
    );
    std::process::exit(1);
}

#[allow(dead_code)] // used by session budget TUI (currently disabled)
fn format_cap(cap: u64) -> String {
    let usdc = cap as f64 / 1_000_000.0;
    if usdc < 1.0 {
        format!("{:.2}", usdc)
    } else {
        format!("{:.0}", usdc)
    }
}

#[allow(dead_code)] // used by session budget TUI (currently disabled)
fn format_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_cap_less_than_one() {
        assert_eq!(format_cap(500_000), "0.50"); // 0.5 USDC
    }

    #[test]
    fn format_cap_exactly_one() {
        assert_eq!(format_cap(1_000_000), "1"); // 1.0 USDC
    }

    #[test]
    fn format_cap_large() {
        assert_eq!(format_cap(100_000_000), "100"); // 100 USDC
    }

    #[test]
    fn format_cap_zero() {
        assert_eq!(format_cap(0), "0.00");
    }

    #[test]
    fn parse_stablecoin_cap_accepts_decimal_inputs() {
        assert_eq!(parse_stablecoin_cap("1").unwrap(), 1_000_000);
        assert_eq!(parse_stablecoin_cap("$1.25").unwrap(), 1_250_000);
        assert_eq!(parse_stablecoin_cap("0.000001 USDC").unwrap(), 1);
    }

    #[test]
    fn parse_stablecoin_cap_rejects_invalid_inputs() {
        assert!(parse_stablecoin_cap("0").is_err());
        assert!(parse_stablecoin_cap("-1").is_err());
        assert!(parse_stablecoin_cap("1.0000001").is_err());
        assert!(parse_stablecoin_cap("abc").is_err());
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(30), "30s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3599), "59m");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(7200), "2h");
    }

    #[test]
    fn format_duration_days() {
        assert_eq!(format_duration(86400), "1d");
        assert_eq!(format_duration(172800), "2d");
    }
}
