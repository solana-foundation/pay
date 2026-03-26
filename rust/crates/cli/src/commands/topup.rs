/// Choose how to fund your pay account.
#[derive(clap::Args)]
pub struct TopupCommand {
    /// Account address to fund. Defaults to your pay account.
    #[arg(long)]
    pub account: Option<String>,
}

impl TopupCommand {
    pub fn run(self, keypair_source: Option<&str>) -> pay_core::Result<()> {
        let config = pay_core::Config::load().unwrap_or_default();
        // Default to mainnet for topup — that's where real funds arrive.
        // Respect explicit config override for dev/localnet use.
        let rpc_url = config
            .rpc_url
            .clone()
            .unwrap_or_else(pay_core::balance::mainnet_rpc_url);

        let pubkey = if let Some(addr) = &self.account {
            addr.clone()
        } else {
            match keypair_source {
                Some(source) => {
                    use solana_mpp::solana_keychain::SolanaSigner;
                    let signer = pay_core::signer::load_signer(source)?;
                    signer.pubkey().to_string()
                }
                None => {
                    return Err(pay_core::Error::Config(
                        "No account found. Run `pay setup` first, or use --account <address>."
                            .to_string(),
                    ));
                }
            }
        };

        crate::tui::run_topup_flow(&pubkey, &rpc_url)
    }
}
