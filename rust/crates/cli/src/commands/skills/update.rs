use owo_colors::OwoColorize;

#[derive(clap::Args)]
pub struct UpdateCommand {
    /// Bypass CDN cache by appending a cache-buster query parameter.
    #[arg(long, short)]
    pub force: bool,
}

impl UpdateCommand {
    pub fn run(self) -> pay_core::Result<()> {
        eprintln!("{}", "Updating skills catalog...".dimmed());
        let catalog = pay_core::skills::update_skills(self.force)?;
        eprintln!(
            "  {} {} providers",
            "Updated:".green(),
            catalog.providers.len(),
        );
        Ok(())
    }
}
