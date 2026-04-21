use owo_colors::OwoColorize;

pub fn run() -> pay_core::Result<()> {
    eprintln!("{}", "Updating skills catalog...".dimmed());
    let catalog = pay_core::skills::update_skills()?;
    eprintln!(
        "  {} {} providers",
        "Updated:".green(),
        catalog.providers.len(),
    );
    Ok(())
}
