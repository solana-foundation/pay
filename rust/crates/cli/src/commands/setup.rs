//! `pay setup` — generate a keypair, store it, and fund your account.
//!
//! Convenience command that combines `pay account new` + `pay topup`.

use owo_colors::OwoColorize;

/// Generate a keypair, store it securely, and fund your account.
#[derive(clap::Args)]
pub struct SetupCommand {
    /// Replace existing account with a new one.
    #[arg(long)]
    pub force: bool,

    /// Storage backend: "keychain" (macOS), "gnome-keyring" (Linux),
    /// "windows-hello" (Windows), "1password".
    #[arg(long)]
    pub backend: Option<String>,

    /// 1Password vault name.
    #[arg(long)]
    pub vault: Option<String>,
}

impl SetupCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let pubkey = super::account::new::create_account(
            "default",
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;

        eprintln!();
        eprintln!("  {} {pubkey}", "Your account:".dimmed());
        eprintln!();
        eprintln!(
            "{}",
            "  Next: fund your account, then run `pay curl <url>` to access paid APIs.".dimmed()
        );
        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        crate::tui::run_topup_flow(&pubkey, &rpc_url)
    }
}
