mod commands;
pub mod components;
mod no_dna;
mod output;
pub mod tui;

use clap::Parser;
use owo_colors::OwoColorize;
use tracing_subscriber::EnvFilter;

use commands::Command;
use output::OutputFormat;
use pay_core::{Config, LogFormat};

#[derive(Parser)]
#[command(
    name = "pay",
    version,
    about = "HTTP client with 402 Payment Required support"
)]
struct Opts {
    #[clap(subcommand)]
    command: Command,

    /// Automatically pay 402 challenges without prompting (no cap).
    /// Implied when NO_DNA is set.
    #[arg(long)]
    yolo: bool,

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

    /// Output format for status messages (text or json).
    /// Defaults to json when NO_DNA is set or stdout is piped.
    #[arg(long)]
    output: Option<OutputFormat>,

    /// Show verbose output (tracing logs, payment details).
    #[arg(short, long)]
    verbose: bool,
}

fn main() {
    let opts = Opts::parse();
    let config = Config::load().unwrap_or_else(|err| {
        eprintln!("{}", format!("Error: {err}").dimmed());
        std::process::exit(1);
    });

    // MCP server — needs its own runtime, exit early
    if matches!(opts.command, Command::Mcp) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");
        if let Err(err) = rt.block_on(pay_mcp::run_server(&pay_mcp::McpOptions::default())) {
            eprintln!("MCP server error: {err}");
            std::process::exit(1);
        }
        return;
    }

    init_logging(config.log_format, opts.verbose);

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
        Some("localnet".to_string())
    } else if opts.mainnet {
        Some("mainnet".to_string())
    } else {
        None
    };

    // ── Legacy keypair source for non-payment commands ─────────────────────
    //
    // `pay account`, `pay send`, `pay solana`, `pay topup`, `pay claude`
    // and friends still use the original keystore-source-string flow.
    // Payment commands (`pay curl`/`wget`/`fetch`) don't read this — they
    // resolve the wallet via `network_override` + `accounts.yml` instead.
    //
    // In sandbox mode, NO command should probe the keychain — that would
    // defeat the whole point of `--sandbox`. The server start path
    // resolves its own ephemeral via the network-aware loader instead.
    let keypair_override: Option<String> = if sandbox_mode
        || matches!(
            opts.command,
            Command::Setup(_)
                | Command::Account { .. }
                | Command::Curl(_)
                | Command::Wget(_)
                | Command::Fetch(_)
        ) {
        None
    } else if matches!(opts.command, Command::Server { .. } | Command::Topup(_)) {
        config.default_keypair_source()
    } else {
        resolve_keypair(&config)
    };

    let is_agent = no_dna::is_agent();
    let is_http_tool = matches!(
        opts.command,
        Command::Curl(_) | Command::Wget(_) | Command::Fetch(_)
    );
    // HTTP tools always auto-pay (Touch ID is the approval gate, not the TUI)
    let auto_pay = opts.yolo || sandbox_mode || is_agent || is_http_tool || config.auto_pay;

    // If not auto-paying and stderr is a TTY, show the session setup TUI
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let auto_pay =
        if !auto_pay && has_tty && matches!(opts.command, Command::Claude(_) | Command::Codex(_)) {
            let tool_kind = opts.command.tool_kind();
            match tui::setup_session(tool_kind) {
                Ok(tui::SessionSetup::Approved { cap, expires_in }) => {
                    eprintln!(
                        "{}",
                        format!(
                            "Session started (cap: {} USDC, expires: {})",
                            format_cap(cap),
                            format_duration(expires_in)
                        )
                        .dimmed()
                    );
                    true
                }
                Ok(tui::SessionSetup::Cancelled) => {
                    eprintln!("{}", "Session cancelled.".dimmed());
                    std::process::exit(0);
                }
                Err(_) => false,
            }
        } else {
            auto_pay
        };

    let output_fmt = opts.output;

    let verbose = opts.verbose;
    if let Err(err) = opts.command.execute(
        auto_pay,
        output_fmt,
        keypair_override.as_deref(),
        network_override.as_deref(),
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

fn init_logging(log_format: LogFormat, verbose: bool) {
    let default = if verbose { "pay=info,warn" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

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
}

/// Find a usable keypair, or tell the user to run `pay setup`.
fn resolve_keypair(config: &Config) -> Option<String> {
    if let Some(source) = config.default_keypair_source() {
        return Some(source);
    }

    // No wallet configured
    eprintln!("{}", "No wallet configured.".dimmed());
    eprintln!();
    eprintln!(
        "{}",
        "  pay setup              Generate a keypair (macOS Keychain + Touch ID)".dimmed()
    );
    eprintln!(
        "{}",
        "  PAY_SECRET_KEY=<path>     Use an existing keypair file".dimmed()
    );
    eprintln!(
        "{}",
        "  pay --sandbox ...      Ephemeral keypair on Surfpool sandbox".dimmed()
    );
    std::process::exit(1);
}

fn format_cap(cap: u64) -> String {
    let usdc = cap as f64 / 1_000_000.0;
    if usdc < 1.0 {
        format!("{:.2}", usdc)
    } else {
        format!("{:.0}", usdc)
    }
}

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
