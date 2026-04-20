//! `pay account default` — set the default account.

/// Set which account is used by default.
#[derive(clap::Args)]
pub struct DefaultCommand {
    /// Account name to make the default.
    pub name: String,
}

impl DefaultCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut accounts = pay_core::accounts::AccountsFile::load()?;

        let exists = accounts
            .accounts
            .get(pay_core::accounts::MAINNET_NETWORK)
            .is_some_and(|net| net.contains_key(&self.name));

        if !exists {
            let available: Vec<String> = accounts
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .map(|net| net.keys().cloned().collect())
                .unwrap_or_default();
            if available.is_empty() {
                return Err(pay_core::Error::Config(
                    "No accounts found. Run `pay account new` first.".to_string(),
                ));
            }
            return Err(pay_core::Error::Config(format!(
                "Account '{}' not found. Available: {}",
                self.name,
                available.join(", ")
            )));
        }

        accounts.set_active(pay_core::accounts::MAINNET_NETWORK, &self.name);
        accounts.save()?;

        super::list::print_account_list(
            &accounts,
            Some(super::list::Highlight::Green {
                network: pay_core::accounts::MAINNET_NETWORK,
                name: &self.name,
            }),
        );

        Ok(())
    }
}
