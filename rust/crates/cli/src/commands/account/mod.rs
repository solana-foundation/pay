pub mod default;
pub mod destroy;
pub mod export;
pub mod import;
pub mod list;
pub mod new;

use clap::Subcommand;
use owo_colors::OwoColorize;

#[derive(Subcommand)]
pub enum AccountCommand {
    /// Create a new account and store it securely.
    New(new::NewCommand),
    /// Import an account from a JSON key file.
    Import(import::ImportCommand),
    /// List all registered accounts with balances.
    #[command(alias = "ls")]
    List(list::ListCommand),
    /// Update default account.
    Default(default::DefaultCommand),
    /// Permanently delete an account and its secret key.
    #[command(alias = "rm", alias = "destroy")]
    Remove(destroy::DestroyCommand),
    /// Export an account to a JSON key file.
    #[command(alias = "backup")]
    Export(export::ExportCommand),
}

impl AccountCommand {
    pub fn run(self, network_override: Option<&str>) -> pay_core::Result<()> {
        match self {
            Self::New(cmd) => cmd.run(),
            Self::Import(cmd) => cmd.run(),
            Self::List(cmd) => cmd.run(),
            Self::Default(cmd) => cmd.run(),
            Self::Remove(cmd) => cmd.run(network_override),
            Self::Export(cmd) => cmd.run(network_override),
        }
    }
}

/// Resolve the destructive/export command's network once, including the
/// root CLI flags. Conflicting scopes must fail before reading any secret.
fn account_network(sandbox: bool, network_override: Option<&str>) -> pay_core::Result<&str> {
    if sandbox {
        if let Some(network) = network_override
            && network != "localnet"
        {
            return Err(pay_core::Error::Config(format!(
                "--sandbox conflicts with the selected {network} network"
            )));
        }
        Ok("localnet")
    } else {
        Ok(network_override.unwrap_or(pay_core::accounts::MAINNET_NETWORK))
    }
}

/// Default behaviour when `pay account` (or its alias `pay accounts`) is run
/// without a subcommand: list accounts and print the available subcommands so
/// the user discovers them.
pub fn run_default() -> pay_core::Result<()> {
    list::ListCommand.run()?;

    eprintln!("{}", "Subcommands:".dimmed());
    for (name, summary) in SUBCOMMAND_HELP {
        eprintln!(
            "{}",
            format!("  pay account {name:<10}  {summary}").dimmed()
        );
    }
    Ok(())
}

const SUBCOMMAND_HELP: &[(&str, &str)] = &[
    ("new", "Create a new account"),
    ("import", "Import an account from a JSON key file"),
    ("default", "Update default account"),
    ("rm", "Remove an account (alias: destroy)"),
    (
        "export",
        "Export an account to a JSON key file (alias: backup)",
    ),
];
