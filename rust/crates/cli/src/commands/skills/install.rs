use owo_colors::OwoColorize;

/// Add a provider source to the skills catalog.
///
/// Accepts a GitHub `org/repo` shorthand or a full URL to a catalog JSON.
#[derive(clap::Args)]
pub struct InstallCommand {
    /// Provider source — GitHub `org/repo` or a full URL.
    pub source: String,
}

impl InstallCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut cfg = pay_core::skills::config::SkillsConfig::load()?;
        if cfg.add_source(&self.source) {
            cfg.save()?;
            eprintln!("  {} {}", "Added:".green(), self.source);
            eprintln!("{}", "  Updating cache...".dimmed());
            let catalog = pay_core::skills::update_skills()?;
            eprintln!(
                "  {} {} providers",
                "Ready:".green(),
                catalog.providers.len(),
            );
        } else {
            eprintln!("{}", "  Already installed.".dimmed());
        }
        Ok(())
    }
}
