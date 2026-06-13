use owo_colors::OwoColorize;

use pay_core::skills::pin::PinStore;

pub fn run() -> pay_core::Result<()> {
    let catalog = pay_core::skills::blocking::load_skills()?;
    let pins = PinStore::open_default().read_all();

    if catalog.providers.is_empty() && pins.is_empty() {
        eprintln!(
            "{}",
            "  No providers. Run `pay skills add <source>` to add one.".dimmed()
        );
        return Ok(());
    }

    eprintln!();
    let pinned_fqns: std::collections::HashSet<&str> =
        pins.iter().map(|(m, _)| m.fqn.as_str()).collect();
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
        let pinned_tag = if pinned_fqns.contains(svc.fqn.as_str()) {
            " (pinned)".cyan().to_string()
        } else {
            String::new()
        };
        eprintln!(
            "  {:<45} {:<38} {}{}",
            svc.fqn.bold(),
            svc.meta.title.dimmed(),
            stats,
            pinned_tag,
        );
    }

    if !pins.is_empty() {
        eprintln!();
        eprintln!("  {}", "Pinned providers (overlay):".bold());
        for (m, _) in &pins {
            let merged = if m.merged {
                " merged".green().to_string()
            } else {
                String::new()
            };
            eprintln!(
                "    {:<32} {:<20} {} ({})",
                m.fqn.bold(),
                m.anchor_label().cyan(),
                m.short_sha().dimmed(),
                format!("from {}{}", m.head_repo, merged).dimmed(),
            );
        }
    }

    eprintln!();
    eprintln!(
        "  {}",
        format!(
            "{} providers, {} total endpoints, {} pinned",
            catalog.providers.len(),
            catalog
                .providers
                .iter()
                .map(|s| s.endpoint_count)
                .sum::<u32>(),
            pins.len(),
        )
        .dimmed()
    );
    eprintln!();
    Ok(())
}
