use owo_colors::OwoColorize;

use pay_core::skills::pin::PinStore;

/// Remove a provider source — or a pinned provider — from the skills catalog.
///
/// If the argument matches a pinned FQN, the pin is removed from the
/// overlay. Otherwise the argument is treated as a catalog source URL
/// or `org/repo` shorthand and removed from `skills.yaml`. Use `--pin`
/// to force pin-only resolution when both could match.
#[derive(clap::Args)]
pub struct RemoveCommand {
    /// Source URL / `org/repo` shorthand — OR provider FQN (e.g. `venice/ai`)
    /// if a matching pin exists in the overlay.
    pub source: String,

    /// Only remove a pinned overlay entry; ignore source lookup.
    #[arg(long)]
    pub pin: bool,
}

impl RemoveCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = PinStore::open_default();
        let has_pin = store.get(&self.source)?.is_some();

        if self.pin || has_pin {
            if store.remove(&self.source)? {
                eprintln!("  {} pin {}", "Removed:".green(), self.source);
                return Ok(());
            }
            if self.pin {
                eprintln!(
                    "{}",
                    format!("  No pin matching `{}`.", self.source).dimmed()
                );
                return Ok(());
            }
            // has_pin was true a moment ago; race condition or false positive.
        }

        let mut cfg = pay_core::skills::config::SkillsConfig::load()?;
        if cfg.remove_source(&self.source) {
            cfg.save()?;
            eprintln!("  {} {}", "Removed:".green(), self.source);
            eprintln!("{}", "  Updating cache...".dimmed());
            let catalog = pay_core::skills::blocking::update_skills(false)?;
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
