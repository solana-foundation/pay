pub mod default;
pub mod destroy;
pub mod export;
pub mod import;
pub mod list;
pub mod new;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Create a new account and store it securely.
    New(new::NewCommand),
    /// Import an account from a JSON key file.
    Import(import::ImportCommand),
    /// List all registered accounts with balances.
    #[command(alias = "ls")]
    List(list::ListCommand),
    /// Set the default account.
    Default(default::DefaultCommand),
    /// Permanently delete an account and its secret key.
    #[command(alias = "rm", alias = "destroy")]
    Remove(destroy::DestroyCommand),
    /// Export an account to a JSON key file.
    #[command(alias = "backup")]
    Export(export::ExportCommand),
}

impl AccountCommand {
    pub fn run(self, keypair_override: Option<&str>) -> pay_core::Result<()> {
        match self {
            Self::New(cmd) => cmd.run(),
            Self::Import(cmd) => cmd.run(),
            Self::List(cmd) => cmd.run(),
            Self::Default(cmd) => cmd.run(),
            Self::Remove(cmd) => cmd.run(),
            Self::Export(cmd) => cmd.run(keypair_override),
        }
    }
}
