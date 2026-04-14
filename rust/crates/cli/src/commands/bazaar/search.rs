use owo_colors::OwoColorize;
use pay_core::bazaar::SearchHit;

/// Max metered endpoints to show per service in condensed mode.
const CONDENSED_METERED_LIMIT: usize = 5;
/// Max total endpoints to show per service in condensed mode.
const CONDENSED_TOTAL_LIMIT: usize = 8;

/// Search for API providers and endpoints.
///
/// Adaptive output:
/// - **Single service match**: shows all endpoints (like `bazaar endpoints`)
/// - **Multiple services**: condensed view — metered endpoints first, capped,
///   with a hint to drill down via `pay bazaar endpoints <service>`
#[derive(clap::Args)]
pub struct SearchCommand {
    /// Keyword to search for (matches service names, endpoint paths, descriptions).
    pub query: Option<String>,

    /// Filter by category (ai_ml, data, compute, maps, etc.).
    #[arg(long, short)]
    pub category: Option<String>,

    /// Output as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl SearchCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let catalog = pay_core::bazaar::load_bazaar()?;
        let hits =
            pay_core::bazaar::search(&catalog, self.query.as_deref(), self.category.as_deref());

        if self.json {
            let grouped = pay_core::bazaar::group_search_results(&hits);
            let json = serde_json::to_string_pretty(&grouped)
                .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
            println!("{json}");
            return Ok(());
        }

        if hits.is_empty() {
            eprintln!(
                "{}",
                "No results. Try a broader search or `pay bazaar search` to list all.".dimmed()
            );
            return Ok(());
        }

        // Count distinct services to decide display mode
        let services: Vec<String> = {
            let mut seen = Vec::new();
            for h in &hits {
                if !seen.contains(&h.service) {
                    seen.push(h.service.clone());
                }
            }
            seen
        };

        if services.len() == 1 {
            // Single service — show everything (like `bazaar endpoints`)
            print_full_service(&hits);
        } else {
            // Multiple services — condensed per service
            print_condensed(&hits, &services);
        }

        Ok(())
    }
}

/// Full view: one service, all endpoints grouped by resource.
fn print_full_service(hits: &[SearchHit]) {
    let first = &hits[0];
    eprintln!(
        "  {} — {}",
        first.service.bold(),
        first.service_title.dimmed()
    );
    if !first.service_url.is_empty() {
        eprintln!("  {}", first.service_url.dimmed());
    }
    eprintln!();

    let mut current_resource = String::new();
    for hit in hits {
        if hit.resource != current_resource && !hit.resource.is_empty() {
            if !current_resource.is_empty() {
                eprintln!();
            }
            current_resource = hit.resource.clone();
            eprintln!("  {}", current_resource.bold());
        }
        print_endpoint(hit);
    }
    eprintln!();
    eprintln!("  {}", format!("{} endpoints", hits.len()).dimmed());
}

/// Condensed view: multiple services, show top metered + a few free per service.
fn print_condensed(hits: &[SearchHit], services: &[String]) {
    for (i, svc_name) in services.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }

        let svc_hits: Vec<&SearchHit> = hits.iter().filter(|h| &h.service == svc_name).collect();
        let first = svc_hits[0];

        eprintln!(
            "  {} — {}",
            first.service.bold(),
            first.service_title.dimmed()
        );
        if !first.service_url.is_empty() {
            eprintln!("  {}", first.service_url.dimmed());
        }
        eprintln!();

        // Show metered first, capped
        let metered: Vec<&&SearchHit> = svc_hits.iter().filter(|h| h.metered).collect();
        let free: Vec<&&SearchHit> = svc_hits.iter().filter(|h| !h.metered).collect();

        let shown_metered = metered.len().min(CONDENSED_METERED_LIMIT);
        let remaining_budget = CONDENSED_TOTAL_LIMIT.saturating_sub(shown_metered);
        let shown_free = free.len().min(remaining_budget);

        for hit in metered.iter().take(shown_metered) {
            print_endpoint(hit);
        }
        for hit in free.iter().take(shown_free) {
            print_endpoint(hit);
        }

        let total = svc_hits.len();
        let shown = shown_metered + shown_free;
        if total > shown {
            eprintln!(
                "    {}",
                format!(
                    "... {} more — `pay bazaar endpoints {}`",
                    total - shown,
                    svc_name
                )
                .dimmed()
            );
        }
    }

    eprintln!();
    eprintln!(
        "  {}",
        format!(
            "{} services, {} total endpoints",
            services.len(),
            hits.len()
        )
        .dimmed()
    );
}

fn print_endpoint(hit: &SearchHit) {
    let method_colored = match hit.method.as_str() {
        "GET" => format!("{:<7}", hit.method).green().to_string(),
        "POST" => format!("{:<7}", hit.method).blue().to_string(),
        "PUT" | "PATCH" => format!("{:<7}", hit.method).yellow().to_string(),
        "DELETE" => format!("{:<7}", hit.method).red().to_string(),
        _ => format!("{:<7}", hit.method).dimmed().to_string(),
    };

    let path = &hit.path;

    let metered_indicator = if hit.metered { "$" } else { "" };
    eprintln!(
        "    {} {} {}",
        method_colored,
        path,
        metered_indicator.yellow()
    );

    if !hit.description.is_empty() {
        let desc = if hit.description.len() > 72 {
            format!("{}...", &hit.description[..69])
        } else {
            hit.description.clone()
        };
        eprintln!("           {}", desc.dimmed());
    }
}
