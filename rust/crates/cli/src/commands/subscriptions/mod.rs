//! `pay subscriptions` — manage MPP `subscription`-intent delegations.
//!
//! Subscriptions are persisted alongside accounts in
//! `~/.config/pay/accounts.yml`, one entry per `(network, account, plan)`.
//! Current receipts separate the server-issued opaque `subscriptionId` from
//! the base58 on-chain `subscriptionDelegation` PDA.

pub mod cancel;
pub mod list;
pub mod new;
pub mod refresh;
pub mod status;

use clap::Subcommand;
use owo_colors::OwoColorize;

#[derive(Subcommand)]
pub enum SubscriptionCommand {
    /// List subscriptions across every account, or filter by --account / --network.
    #[command(alias = "ls")]
    List(list::ListCommand),
    /// Show detail for a single subscription by its locally stored id.
    Status(status::StatusCommand),
    /// Activate a new subscription against an explicit on-chain Plan PDA.
    New(new::NewCommand),
    /// Cancel a subscription by its locally stored id.
    #[command(alias = "rm")]
    Cancel(cancel::CancelCommand),
    /// Backfill missing on-chain data (e.g. activation_signature) on local entries.
    Refresh(refresh::RefreshCommand),
}

impl SubscriptionCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match self {
            Self::List(cmd) => cmd.run(),
            Self::Status(cmd) => cmd.run(),
            Self::New(cmd) => cmd.run(),
            Self::Cancel(cmd) => cmd.run(),
            Self::Refresh(cmd) => cmd.run(),
        }
    }
}

/// Default behaviour when `pay subscriptions` is run without a subcommand:
/// list everything and print the available verbs so the user discovers them.
pub fn run_default() -> pay_core::Result<()> {
    list::ListCommand::default().run()?;

    eprintln!("{}", "Subcommands:".dimmed());
    for (name, summary) in SUBCOMMAND_HELP {
        eprintln!(
            "{}",
            format!("  pay subscriptions {name:<8}  {summary}").dimmed()
        );
    }
    Ok(())
}

const SUBCOMMAND_HELP: &[(&str, &str)] = &[
    ("list", "List subscriptions (alias: ls)"),
    ("status", "Show detail for one subscription"),
    ("new", "Activate against a plan"),
    ("cancel", "Cancel a subscription (alias: rm)"),
    ("refresh", "Backfill missing on-chain data"),
];
