pub mod demo;
pub mod scaffold;
pub mod start;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Start the payment gateway proxy for an API spec.
    Start(start::StartCommand),
    /// Start the gateway with a bundled demo spec (payment-debugger).
    Demo(demo::DemoCommand),
    /// Generate a starter provider YAML spec.
    Scaffold(scaffold::ScaffoldCommand),
}

impl ServerCommand {
    pub fn run(self, active_account_name: Option<&str>, sandbox: bool) -> pay_core::Result<()> {
        match self {
            Self::Start(cmd) => cmd.run(active_account_name, sandbox),
            Self::Demo(cmd) => cmd.run(active_account_name, sandbox),
            Self::Scaffold(cmd) => cmd.run(),
        }
    }
}
