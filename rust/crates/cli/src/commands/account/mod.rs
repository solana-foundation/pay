pub mod destroy;
pub mod export;
pub mod import;
pub mod list;
pub mod new;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Generate a new keypair and store it securely.
    New(new::NewCommand),
    /// Import an existing keypair from a Solana CLI JSON file.
    Import(import::ImportCommand),
    /// List all registered accounts with balances.
    List(list::ListCommand),
    /// Permanently delete an account and its secret key.
    Destroy(destroy::DestroyCommand),
    /// Export a keypair to a JSON file (Solana CLI format).
    Export(export::ExportCommand),
}

impl AccountCommand {
    pub fn run(self, keypair_override: Option<&str>) -> pay_core::Result<()> {
        match self {
            Self::New(cmd) => cmd.run(),
            Self::Import(cmd) => cmd.run(),
            Self::List(cmd) => cmd.run(),
            Self::Destroy(cmd) => cmd.run(),
            Self::Export(cmd) => cmd.run(keypair_override),
        }
    }
}
