pub mod demo;
pub mod scaffold;
pub mod start;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Start a local demo with a dashboard for tracing payments.
    Demo(demo::DemoCommand),
    /// Start a proxy that enables stablecoin payments for your API.
    Start(start::StartCommand),
    /// Create a YAML file that defines endpoints and payment requirements.
    Scaffold(scaffold::ScaffoldCommand),
}

impl ServerCommand {
    pub fn otlp_sidecar(&self) -> Option<&str> {
        match self {
            Self::Demo(cmd) => cmd.otlp_sidecar.as_deref(),
            Self::Start(cmd) => cmd.otlp_sidecar.as_deref(),
            Self::Scaffold(_) => None,
        }
    }

    pub fn run(self, active_account_name: Option<&str>, sandbox: bool) -> pay_core::Result<()> {
        match self {
            Self::Demo(cmd) => cmd.run(active_account_name, sandbox),
            Self::Start(cmd) => cmd.run(active_account_name, sandbox),
            Self::Scaffold(cmd) => cmd.run(),
        }
    }
}
