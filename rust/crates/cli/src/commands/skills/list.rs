use owo_colors::OwoColorize;

use pay_core::skills::pin::PinStore;

use super::boxed;

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

    let pinned_fqns: std::collections::HashSet<&str> =
        pins.iter().map(|(m, _)| m.fqn.as_str()).collect();

    // One boxed section per provider: title + endpoint count, then the dimmed
    // fqn (and a pinned marker) — the same framed look as `pay skills search`.
    let sections: Vec<Vec<(String, usize)>> = catalog
        .providers
        .iter()
        .map(|svc| {
            let ep = format!("{} endpoints", svc.endpoint_count);
            let ep_colored = if svc.has_metering {
                ep.yellow().to_string()
            } else {
                ep.dimmed().to_string()
            };
            let line1 = format!("{}  ·  {ep_colored}", svc.meta.title.bold());
            let line1_visible = svc.meta.title.chars().count() + 5 + ep.chars().count();

            let (line2, line2_visible) = if pinned_fqns.contains(svc.fqn.as_str()) {
                (
                    format!("{}  {}", svc.fqn.dimmed(), "(pinned)".cyan()),
                    svc.fqn.chars().count() + 2 + "(pinned)".len(),
                )
            } else {
                (svc.fqn.dimmed().to_string(), svc.fqn.chars().count())
            };

            vec![(line1, line1_visible), (line2, line2_visible)]
        })
        .collect();

    eprintln!();
    eprintln!("{}", boxed::frame(&sections));

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
