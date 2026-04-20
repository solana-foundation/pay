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
        // Abort before any prompts if the default account already exists.
        if !self.force
            && let Ok(accounts) = pay_core::accounts::AccountsFile::load()
            && accounts
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .is_some_and(|net| net.contains_key("default"))
        {
            super::account::list::print_account_list(
                &accounts,
                None::<super::account::list::Highlight>,
            );
            eprintln!(
                "{}",
                "  A default account already exists. Use --force to replace it, or `pay account new --name <name>` to add another.".dimmed()
            );
            eprintln!();
            return Ok(());
        }

        let (pubkey, backend_name) = super::account::new::create_account(
            "default",
            self.backend.as_deref(),
            self.vault.as_deref(),
            self.force,
        )?;

        eprintln!();

        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);
        let received = crate::tui::run_topup_flow(&pubkey, &rpc_url, "default")?;
        super::account::new::print_next_steps("default", backend_name, received.as_ref());
        Ok(())
    }
}
