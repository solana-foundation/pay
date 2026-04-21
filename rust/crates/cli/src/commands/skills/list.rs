use owo_colors::OwoColorize;

pub fn run() -> pay_core::Result<()> {
    let catalog = pay_core::skills::load_skills()?;

    if catalog.services.is_empty() {
        eprintln!(
            "{}",
            "  No services. Run `pay install <source>` to add a provider.".dimmed()
        );
        return Ok(());
    }

    // Group by provider
    let mut by_provider: std::collections::BTreeMap<String, Vec<&pay_core::skills::Service>> =
        std::collections::BTreeMap::new();
    for svc in &catalog.services {
        let key = if svc.provider.is_empty() {
            "(unknown)".to_string()
        } else {
            svc.provider.clone()
        };
        by_provider.entry(key).or_default().push(svc);
    }

    eprintln!();
    for (provider, services) in &by_provider {
        if services.len() == 1 {
            // Single service — inline on one line
            let svc = services[0];
            eprintln!(
                "  {:<25} {:<38} {}",
                format!("{}/{}", provider, svc.name).bold(),
                svc.title.dimmed(),
                format_stats(svc),
            );
        } else {
            // Multiple services — group header + indented list
            eprintln!("  {}", provider.bold());
            for svc in services {
                eprintln!(
                    "    {:<23} {:<38} {}",
                    svc.name,
                    svc.title.dimmed(),
                    format_stats(svc),
                );
            }
            eprintln!();
        }
    }

    eprintln!(
        "  {}",
        format!(
            "{} services, {} endpoints",
            catalog.services.len(),
            catalog
                .services
                .iter()
                .map(|s| s.endpoints.len())
                .sum::<usize>()
        )
        .dimmed()
    );
    eprintln!();
    Ok(())
}

fn format_stats(svc: &pay_core::skills::Service) -> String {
    let metered = svc.endpoints.iter().filter(|e| e.pricing.is_some()).count();
    let total = svc.endpoints.len();
    if metered > 0 {
        format!("{total} endpoints, {metered} paid")
            .yellow()
            .to_string()
    } else {
        format!("{total} endpoints").dimmed().to_string()
    }
}
