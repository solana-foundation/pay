//! `pay sign` -- sign and submit a Solana transaction.

/// Sign and submit a base64-encoded Solana transaction.
///
/// Examples:
///   pay sign <base64-transaction>
///   pay --sandbox sign <base64-transaction>
///   pay --account ludo sign <base64-transaction>
#[derive(clap::Args)]
pub struct SignCommand {
    /// Base64-encoded legacy or v0 Solana transaction.
    #[arg(value_name = "BASE64_TX")]
    pub transaction: String,
}

impl SignCommand {
    pub fn run(
        self,
        network_override: Option<&str>,
        account_override: Option<&str>,
    ) -> pay_core::Result<()> {
        let network = network_override.unwrap_or(pay_core::accounts::MAINNET_NETWORK);
        let signature = pay_core::sign::sign_and_submit_base64_transaction(
            &self.transaction,
            network,
            account_override,
        )?;

        println!("{signature}");
        Ok(())
    }
}
