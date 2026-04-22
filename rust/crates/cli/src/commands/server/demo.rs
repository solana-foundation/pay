//! `pay server demo` — start the gateway with a bundled demo spec.
//!
//! Extracts the embedded payment-debugger.yml to `./pay-demo.yaml` in the
//! current working directory, then invokes `pay server start` with sandbox
//! and debugger forced on.

use crate::commands::server::start::StartCommand;

const DEMO_SPEC: &str = include_str!("payment-debugger.yml");

use owo_colors::OwoColorize;

#[derive(clap::Args)]
pub struct DemoCommand {
    /// Address to bind to.
    #[arg(long, default_value = "0.0.0.0:1402")]
    pub bind: String,

    /// Recipient wallet address for payments.
    #[arg(long)]
    pub recipient: Option<String>,

    /// Payment currency (SOL, USDC, etc.).
    #[arg(long, default_value = "USDC")]
    pub currency: String,

    /// Use hosted Surfpool sandbox (https://402.surfnet.dev:8899). Default.
    #[arg(long, conflicts_with = "local")]
    pub sandbox: bool,

    /// Use local Surfpool (http://localhost:8899) instead of hosted sandbox.
    #[arg(long)]
    pub local: bool,
}

impl DemoCommand {
    pub fn run(self, active_account_name: Option<&str>, sandbox: bool) -> pay_core::Result<()> {
        // Demo mode always uses sandbox — require top-level --sandbox so
        // main.rs has already set up an ephemeral keypair (avoids Touch ID).
        if !sandbox {
            return Err(pay_core::Error::Config(
                "pay server demo requires sandbox mode. Run:\n    pay --sandbox server demo".into(),
            ));
        }

        // Extract embedded spec to ./pay-demo.yaml in the current directory
        let spec_path = std::path::PathBuf::from("pay-demo.yaml");
        std::fs::write(&spec_path, DEMO_SPEC)
            .map_err(|e| pay_core::Error::Config(format!("Failed to write pay-demo.yaml: {e}")))?;
        eprintln!("  {} ./pay-demo.yaml", "Scaffolding".green());

        // Default to hosted sandbox. --local overrides to localhost.
        let rpc_url = if self.local {
            Some(pay_core::config::LOCAL_RPC_URL.to_string())
        } else {
            Some(pay_core::config::SANDBOX_RPC_URL.to_string())
        };

        let cmd = StartCommand {
            spec: spec_path.to_string_lossy().into_owned(),
            bind: self.bind,
            recipient: self.recipient,
            currency: self.currency,
            rpc_url,
            debugger: true,
        };
        cmd.run(active_account_name, true)
    }
}
