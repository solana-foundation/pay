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

    // NO_DNA / agent mode → machine-readable JSON.
    if crate::no_dna::is_agent() {
        let items: Vec<serde_json::Value> = catalog
            .providers
            .iter()
            .map(|svc| {
                serde_json::json!({
                    "fqn": svc.fqn,
                    "title": svc.meta.title,
                    "description": svc.meta.description,
                    "category": svc.meta.category,
                    "service_url": svc.meta.service_url,
                    "endpoint_count": svc.endpoint_count,
                    "metered": svc.has_metering,
                    "free_tier": svc.has_free_tier,
                    "pinned": pinned_fqns.contains(svc.fqn.as_str()),
                })
            })
            .collect();
        let json = serde_json::to_string_pretty(&items)
            .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
        println!("{json}");
        return Ok(());
    }

    // One bordered box per provider: title + colored category, the wrapped
    // description, then the dimmed fqn (and a pinned marker) — same framed
    // look as `pay skills search`.
    eprintln!();
    for svc in &catalog.providers {
        // Title line: bold title + the colored category (in place of an
        // endpoint count).
        let (line1, line1_visible) = if svc.meta.category.is_empty() {
            (
                svc.meta.title.bold().to_string(),
                svc.meta.title.chars().count(),
            )
        } else {
            (
                format!(
                    "{}  ·  {}",
                    svc.meta.title.bold(),
                    boxed::category_color(&svc.meta.category, &svc.meta.category)
                ),
                svc.meta.title.chars().count() + 5 + svc.meta.category.chars().count(),
            )
        };
        let mut lines: Vec<(String, usize)> = vec![(line1, line1_visible)];

        if !svc.meta.description.is_empty() {
            for seg in boxed::wrap(&svc.meta.description, boxed::INNER) {
                let visible = seg.chars().count();
                lines.push((seg.dimmed().to_string(), visible));
            }
            // Blank line separating the description from the fqn.
            lines.push((String::new(), 0));
        }

        if pinned_fqns.contains(svc.fqn.as_str()) {
            lines.push((
                format!("{}  {}", svc.fqn.dimmed(), "(pinned)".cyan()),
                svc.fqn.chars().count() + 2 + "(pinned)".len(),
            ));
        } else {
            lines.push((svc.fqn.dimmed().to_string(), svc.fqn.chars().count()));
        }

        eprintln!("{}", boxed::frame(&[lines]));
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
