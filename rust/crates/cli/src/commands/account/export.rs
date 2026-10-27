//! `pay account export` — export an account to a JSON key file.

/// Export an account to a JSON key file.
///
/// The output is compatible with the Solana CLI (`--keypair`).
///
/// Examples:
///   pay account export ludo                  # exports to ./pay-account-ludo-<pubkey>.json
///   pay account export ludo my-key.json      # exports to a specific path
///   pay account export ludo -                # print to stdout
///   pay account export ludo --sandbox        # export the localnet account
#[derive(clap::Args)]
pub struct ExportCommand {
    /// Account name to export (required).
    pub name: String,

    /// Output file path, or "-" for stdout. Defaults to ./pay-account-<name>-<pubkey>.json.
    pub path: Option<String>,

    /// Export from the sandbox (localnet) network instead of mainnet.
    #[arg(long)]
    pub sandbox: bool,
}

impl ExportCommand {
    pub fn run(self, network_override: Option<&str>) -> pay_core::Result<()> {
        let network = super::account_network(self.sandbox, network_override)?;
        let accounts = pay_core::accounts::AccountsFile::load()?;
        let account = accounts
            .accounts
            .get(network)
            .and_then(|net| net.iter().find(|(n, _)| *n == &self.name))
            .map(|(_, a)| a)
            .ok_or_else(|| {
                pay_core::Error::Config(format!("Account '{}' not found in {network}", self.name))
            })?;

        let intent = pay_core::keystore::AuthIntent::export_account(&self.name);
        let keypair_bytes = pay_core::signer::load_keypair_bytes_from_account_with_intent(
            account, &self.name, network, &intent,
        )?;
        let pubkey = bs58::encode(&keypair_bytes[32..64]).into_string();

        let short_pubkey = &pubkey[..8.min(pubkey.len())];
        let path = self
            .path
            .unwrap_or_else(|| format!("pay-account-{}-{}.json", self.name, short_pubkey));

        let json = serde_json::to_string(&*keypair_bytes)
            .map_err(|e| pay_core::Error::Config(format!("JSON error: {e}")))?;

        if path == "-" {
            println!("{json}");
        } else {
            {
                use std::io::Write;
                #[cfg(unix)]
                use std::os::unix::fs::OpenOptionsExt;

                let mut opts = std::fs::OpenOptions::new();
                opts.create(true).write(true).truncate(true);
                #[cfg(unix)]
                opts.mode(0o600);

                let mut file = opts.open(&path).map_err(|e| {
                    pay_core::Error::Config(format!("Failed to create {}: {e}", path))
                })?;
                writeln!(file, "{json}").map_err(|e| {
                    pay_core::Error::Config(format!("Failed to write {}: {e}", path))
                })?;
            }
            eprintln!("Exported to {} (pubkey: {})", path, pubkey);
        }

        Ok(())
    }
}
