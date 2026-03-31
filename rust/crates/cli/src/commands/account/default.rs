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

        if !accounts.accounts.contains_key(&self.name) {
            let available: Vec<_> = accounts.accounts.keys().collect();
            if available.is_empty() {
                return Err(pay_core::Error::Config(
                    "No accounts found. Run `pay account new` first.".to_string(),
                ));
            }
            return Err(pay_core::Error::Config(format!(
                "Account '{}' not found. Available: {}",
                self.name,
                available
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        accounts.default_account = Some(self.name.clone());
        accounts.save()?;

        super::list::print_account_list(&accounts, Some(super::list::Highlight::Green(&self.name)));

        Ok(())
    }
}
