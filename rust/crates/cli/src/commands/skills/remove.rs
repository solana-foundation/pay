use owo_colors::OwoColorize;

/// Remove a provider source from the skills catalog.
#[derive(clap::Args)]
pub struct RemoveCommand {
    /// Provider source to remove — must match what was added.
    pub source: String,
}

impl RemoveCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut cfg = pay_core::skills::config::SkillsConfig::load()?;
        if cfg.remove_source(&self.source) {
            cfg.save()?;
            eprintln!("  {} {}", "Removed:".green(), self.source);
            eprintln!("{}", "  Updating cache...".dimmed());
            let catalog = pay_core::skills::update_skills()?;
            eprintln!(
                "  {} {} providers",
                "Ready:".green(),
                catalog.providers.len(),
            );
        } else {
            eprintln!(
                "{}",
                format!("  Source `{}` not found.", self.source).dimmed()
            );
        }
        Ok(())
    }
}
