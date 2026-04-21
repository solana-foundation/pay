use owo_colors::OwoColorize;

pub fn run() -> pay_core::Result<()> {
    let catalog = pay_core::skills::load_skills()?;

    if catalog.providers.is_empty() {
        eprintln!(
            "{}",
            "  No providers. Run `pay skills add <source>` to add one.".dimmed()
        );
        return Ok(());
    }

    eprintln!();
    for svc in &catalog.providers {
        let stats = if svc.has_metering {
            format!("{} endpoints", svc.endpoint_count)
                .yellow()
                .to_string()
        } else {
            format!("{} endpoints", svc.endpoint_count)
                .dimmed()
                .to_string()
        };
        eprintln!(
            "  {:<45} {:<38} {}",
            svc.fqn.bold(),
            svc.title.dimmed(),
            stats,
        );
    }

    eprintln!();
    eprintln!(
        "  {}",
        format!(
            "{} providers, {} total endpoints",
            catalog.providers.len(),
            catalog
                .providers
                .iter()
                .map(|s| s.endpoint_count)
                .sum::<u32>()
        )
        .dimmed()
    );
    eprintln!();
    Ok(())
}
