pub mod scaffold;
pub mod start;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Start the payment gateway proxy for an API spec.
    Start(start::StartCommand),
    /// Generate a starter provider YAML spec.
    Scaffold(scaffold::ScaffoldCommand),
}

impl ServerCommand {
    pub fn run(self, keypair_source: Option<&str>, dev: bool) -> pay_core::Result<()> {
        match self {
            Self::Start(cmd) => cmd.run(keypair_source, dev),
            Self::Scaffold(cmd) => cmd.run(),
        }
    }
}
