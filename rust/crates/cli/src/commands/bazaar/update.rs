use owo_colors::OwoColorize;

pub fn run() -> pay_core::Result<()> {
    eprintln!("{}", "Updating bazaar catalog...".dimmed());
    let catalog = pay_core::bazaar::update_bazaar()?;
    eprintln!(
        "  {} {} services, {} endpoints",
        "Updated:".green(),
        catalog.services.len(),
        catalog
            .services
            .iter()
            .map(|s| s.endpoints.len())
            .sum::<usize>()
    );
    Ok(())
}
