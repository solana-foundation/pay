mod commands;
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
    #[arg(long, global = true)]
    yolo: bool,

    /// Dev mode: generate a fresh ephemeral keypair funded via Surfpool.
    /// Uses 402.surfnet.dev by default; combine with --local for localhost:8899.
    #[arg(long, global = true)]
    dev: bool,

    /// Use local Surfpool (localhost:8899) instead of the public devnet.
    #[arg(long, global = true)]
    local: bool,

    /// Output format for status messages (text or json).
    /// Defaults to json when NO_DNA is set or stdout is piped.
    #[arg(long, global = true)]
    output: Option<OutputFormat>,

    /// Show verbose output (tracing logs, payment details).
    #[arg(short, long, global = true)]
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

    // Resolve the keypair source:
    // 1. --dev → ephemeral keypair funded on localnet
    // 2. Keychain (from `pay setup`) → Touch ID protected
    // 3. ~/.config/solana/id.json → file-based fallback
    // 4. None → tell user to run `pay setup`
    let keypair_override: Option<String>;

    if opts.dev {
        let rpc_url = if opts.local {
            pay_core::config::LOCAL_RPC_URL.to_string()
        } else if let Ok(url) = std::env::var("PAY_DEV_RPC_URL") {
            url
        } else {
            pay_core::config::DEV_RPC_URL.to_string()
        };
        match pay_core::dev::setup_dev_keypair(&rpc_url) {
            Ok(kp) => {
                if opts.verbose {
                    eprintln!(
                        "{}",
                        format!("Dev mode: ephemeral account {} ({})", kp.pubkey, rpc_url)
                            .dimmed()
                    );
                }
                keypair_override = Some(kp.path.clone());
                // Make the dev RPC URL available to payment signing and MCP subprocesses
                // SAFETY: called before any threads are spawned
                unsafe { std::env::set_var("PAY_RPC_URL", &rpc_url) };
                // Keep the DevKeypair alive (owns the temp file)
                std::mem::forget(kp);
            }
            Err(err) => {
                eprintln!(
                    "{}",
                    format!("Error setting up dev keypair: {err}").dimmed()
                );
                std::process::exit(1);
            }
        }
    } else if matches!(opts.command, Command::Setup(_)) {
        keypair_override = None;
    } else if matches!(opts.command, Command::Topup(_)) {
        // Topup tries to resolve but doesn't exit if missing (--account fallback)
        keypair_override = config.default_keypair_source();
    } else {
        // All other commands require a keypair
        keypair_override = resolve_keypair(&config);
    }

    let is_agent = no_dna::is_agent();
    let is_http_tool = matches!(
        opts.command,
        Command::Curl(_) | Command::Wget(_) | Command::Httpie(_) | Command::Fetch(_)
    );
    // HTTP tools always auto-pay (Touch ID is the approval gate, not the TUI)
    let auto_pay = opts.yolo || opts.dev || is_agent || is_http_tool || config.auto_pay;

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
    if let Err(err) =
        opts.command
            .execute(auto_pay, output_fmt, keypair_override.as_deref(), verbose)
    {
        if no_dna::should_json(output_fmt) {
            output::error_json(&err.to_string());
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
        "  pay --dev ...          Ephemeral keypair on localnet".dimmed()
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
