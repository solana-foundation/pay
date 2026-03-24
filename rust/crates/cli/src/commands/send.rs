//! `pay send` — send SOL to a recipient.

use owo_colors::OwoColorize;

/// Send SOL to a recipient address.
///
/// Examples:
///   pay send 0.1 <address>      Send 0.1 SOL
///   pay send * <address>         Send entire balance (minus fee)
#[derive(clap::Args)]
pub struct SendCommand {
    /// Amount of SOL to send (e.g. "0.1"), or "*" to send the entire balance.
    pub amount: String,

    /// Recipient public key (base-58).
    pub recipient: String,
}

impl SendCommand {
    pub fn run(self, keypair_source: Option<&str>, verbose: bool) -> pay_core::Result<()> {
        let config = pay_core::Config::load().unwrap_or_default();
        let rpc_url = config.rpc_url().to_string();

        let keypair = keypair_source
            .map(|s| s.to_string())
            .or_else(|| config.default_keypair_source())
            .ok_or_else(|| {
                pay_core::Error::Config("No wallet configured. Run `pay setup` first.".to_string())
            })?;

        let sol_display = if self.amount == "*" {
            "all SOL".to_string()
        } else {
            format!("{} SOL", self.amount)
        };

        if verbose {
            eprintln!(
                "{}",
                format!("Sending {sol_display} to {}...", self.recipient).dimmed()
            );
        }

        let result = pay_core::send::send_sol(&self.amount, &self.recipient, &keypair, &rpc_url)?;

        let sol_sent = result.lamports as f64 / 1_000_000_000.0;
        eprintln!(
            "{}",
            format!("Sent {sol_sent:.9} SOL to {}", result.to).dimmed()
        );
        println!("{}", result.signature);

        Ok(())
    }
}
