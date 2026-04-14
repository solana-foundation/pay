use owo_colors::OwoColorize;

/// Remove a provider source from the bazaar.
#[derive(clap::Args)]
pub struct RemoveCommand {
    /// Provider source to remove — must match what was added.
    pub source: String,
}

impl RemoveCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut cfg = pay_core::bazaar::config::BazaarConfig::load()?;
        if cfg.remove_source(&self.source) {
            cfg.save()?;
            eprintln!("  {} {}", "Removed:".green(), self.source);
            eprintln!("{}", "  Updating cache...".dimmed());
            let catalog = pay_core::bazaar::update_bazaar()?;
            eprintln!(
                "  {} {} services, {} endpoints",
                "Ready:".green(),
                catalog.services.len(),
                catalog
                    .services
                    .iter()
                    .map(|s| s.endpoints.len())
                    .sum::<usize>()
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
