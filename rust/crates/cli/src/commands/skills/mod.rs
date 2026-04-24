pub mod build;
pub mod endpoints;
pub mod install;
pub mod list;
pub mod provider;
pub mod remove;
pub mod search;
pub mod update;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum SkillsCommand {
    /// Search for API providers and endpoints.
    Search(search::SearchCommand),
    /// List all endpoints for a specific service.
    Endpoints(endpoints::EndpointsCommand),
    /// Add a provider source (GitHub org/repo or catalog URL).
    Add(install::InstallCommand),
    /// Remove a provider source.
    #[command(alias = "rm")]
    Remove(remove::RemoveCommand),
    /// List configured provider sources.
    #[command(alias = "ls")]
    List,
    /// Refresh the local skills cache from all sources.
    Update(update::UpdateCommand),
    /// Build the skills index from a pay-skills registry directory.
    Build(build::BuildCommand),
    /// Manage providers in the registry.
    Provider {
        #[command(subcommand)]
        command: provider::ProviderCommand,
    },
}

impl SkillsCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match self {
            Self::Search(cmd) => cmd.run(),
            Self::Endpoints(cmd) => cmd.run(),
            Self::Add(cmd) => cmd.run(),
            Self::Remove(cmd) => cmd.run(),
            Self::List => list::run(),
            Self::Update(cmd) => cmd.run(),
            Self::Build(cmd) => cmd.run(),
            Self::Provider { command } => command.run(),
        }
    }
}
