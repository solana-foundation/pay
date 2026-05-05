use std::collections::BTreeSet;

use owo_colors::OwoColorize;
use pay_core::skills::{self, SearchHit, blocking::ensure_endpoints};

/// Max metered endpoints to show per service in condensed mode.
const CONDENSED_METERED_LIMIT: usize = 5;
/// Max total endpoints to show per service in condensed mode.
const CONDENSED_TOTAL_LIMIT: usize = 8;

/// Search for API providers and endpoints.
///
/// Adaptive output:
/// - **Single service match**: shows all endpoints (like `skills endpoints`)
/// - **Multiple services**: condensed view — metered endpoints first, capped,
///   with a hint to drill down via `pay skills endpoints <service>`
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
        let mut catalog = skills::blocking::load_skills()?;
        let refreshed = refresh_search_hits(
            &mut catalog,
            self.query.as_deref(),
            self.category.as_deref(),
            ensure_endpoints,
        );
        let hits = refreshed.hits;

        if !refreshed.unavailable_services.is_empty() {
            eprintln!(
                "{}",
                format_unavailable_warning(&refreshed.unavailable_services).yellow()
            );
        }
        if !refreshed.empty_services.is_empty() {
            eprintln!(
                "{}",
                format_empty_warning(&refreshed.empty_services).yellow()
            );
        }

        if self.json {
            let grouped = skills::group_search_results(&hits);
            let json = serde_json::to_string_pretty(&grouped)
                .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
            println!("{json}");
            return Ok(());
        }

        if hits.is_empty() {
            if refreshed.unavailable_services.is_empty() && refreshed.empty_services.is_empty() {
                eprintln!(
                    "{}",
                    "No results. Try a broader search or `pay skills search` to list all.".dimmed()
                );
            } else {
                eprintln!(
                    "{}",
                    "No actionable endpoint results. Matching services were skipped.".dimmed()
                );
            }
            return Ok(());
        }

        // Count distinct services to decide display mode
        let services = distinct_services(&hits);

        if services.len() == 1 {
            // Single service — show everything (like `skills endpoints`)
            print_full_service(&hits);
        } else {
            // Multiple services — condensed per service
            print_condensed(&hits, &services);
        }

        print_endpoints_tip();

        Ok(())
    }
}

struct RefreshedSearchHits {
    hits: Vec<SearchHit>,
    unavailable_services: Vec<String>,
    empty_services: Vec<String>,
}

fn refresh_search_hits<F>(
    catalog: &mut skills::Catalog,
    query: Option<&str>,
    category: Option<&str>,
    mut hydrate: F,
) -> RefreshedSearchHits
where
    F: FnMut(&mut skills::Catalog, &str) -> pay_core::Result<()>,
{
    let initial_hits = skills::search(catalog, query, category);
    if initial_hits.is_empty() {
        return RefreshedSearchHits {
            hits: initial_hits,
            unavailable_services: Vec::new(),
            empty_services: Vec::new(),
        };
    }

    let services = distinct_services(&initial_hits);
    let mut hydration_failures = BTreeSet::new();

    for service in &services {
        if hydrate(catalog, service).is_err() {
            hydration_failures.insert(service.clone());
        }
    }

    let hits = skills::search(catalog, query, category);
    let actionable_services: BTreeSet<_> = hits
        .iter()
        .filter(|hit| !hit.path.is_empty())
        .map(|hit| hit.service.clone())
        .collect();
    let unavailable_services = services
        .iter()
        .filter(|service| {
            hydration_failures.contains(*service) && !actionable_services.contains(*service)
        })
        .cloned()
        .collect();
    let empty_services = services
        .iter()
        .filter(|service| {
            !hydration_failures.contains(*service) && !actionable_services.contains(*service)
        })
        .cloned()
        .collect();
    let hits = hits
        .into_iter()
        .filter(|hit| actionable_services.contains(&hit.service))
        .collect();

    RefreshedSearchHits {
        hits,
        unavailable_services,
        empty_services,
    }
}

fn distinct_services(hits: &[SearchHit]) -> Vec<String> {
    let mut seen = Vec::new();
    for hit in hits {
        if !seen.contains(&hit.service) {
            seen.push(hit.service.clone());
        }
    }
    seen
}

fn format_unavailable_warning(services: &[String]) -> String {
    let services = services.join(", ");
    if services.contains(", ") {
        format!(
            "Warning: endpoint detail unavailable for {services}; skipping those services from results."
        )
    } else {
        format!(
            "Warning: endpoint detail unavailable for {services}; skipping that service from results."
        )
    }
}

fn format_empty_warning(services: &[String]) -> String {
    let services = services.join(", ");
    if services.contains(", ") {
        format!(
            "Warning: no published endpoints available for {services}; skipping those services from results."
        )
    } else {
        format!(
            "Warning: no published endpoints available for {services}; skipping that service from results."
        )
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

    let refs: Vec<&SearchHit> = hits.iter().collect();
    print_resource_groups(&refs);
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
        let shown_hits: Vec<&SearchHit> = metered
            .iter()
            .take(shown_metered)
            .chain(free.iter().take(shown_free))
            .copied()
            .copied()
            .collect();

        print_resource_groups(&shown_hits);

        let total = svc_hits.len();
        let shown = shown_metered + shown_free;
        if total > shown {
            eprintln!(
                "    {}",
                format!(
                    "... {} more — `pay skills endpoints {}`",
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

fn print_resource_groups(hits: &[&SearchHit]) {
    let mut current_resource = String::new();
    for hit in hits {
        if let Some(hit_resource) = hit.resource.as_deref()
            && hit_resource != current_resource
            && !hit_resource.is_empty()
        {
            if !current_resource.is_empty() {
                eprintln!();
            }
            current_resource = hit_resource.to_string();
            eprintln!("  {} {}", "resource:".dimmed(), current_resource.bold());
        }
        print_endpoint(hit);
    }
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
        eprintln!("            {}", desc.dimmed());
    }
}

fn print_endpoints_tip() {
    eprintln!();
    eprintln!(
        "  {}",
        "use `pay skills endpoints <fqn> <resource>` to inspect a provider.".dimmed()
    );
    eprintln!(
        "  {}",
        "The bold service slug above is the <fqn>; each `resource:` heading names the <resource>."
            .dimmed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn catalog_index_only() -> skills::Catalog {
        let json = r#"{
            "version": "1",
            "generated_at": "2026-04-21T00:00:00Z",
            "base_url": "https://cdn.example.com/v1",
            "providers": [
                {
                    "fqn": "solana-foundation/google/bigquery",
                    "title": "BigQuery API",
                    "description": "Serverless data warehouse. SQL over petabyte-scale data.",
                    "category": "data",
                    "service_url": "https://gw.example.com",
                    "endpoint_count": 47,
                    "has_metering": true,
                    "has_free_tier": true,
                    "sha": "abc123"
                },
                {
                    "fqn": "solana-foundation/google/vision",
                    "title": "Cloud Vision API",
                    "description": "Detect objects, faces, text (OCR) in images.",
                    "category": "ai_ml",
                    "service_url": "https://gw.example.com",
                    "endpoint_count": 38,
                    "has_metering": true,
                    "has_free_tier": true,
                    "sha": "def456"
                }
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    fn endpoint(
        method: &str,
        path: &str,
        resource: &str,
        description: &str,
        metered: bool,
    ) -> skills::Endpoint {
        skills::Endpoint {
            method: method.to_string(),
            path: path.to_string(),
            full_path: String::new(),
            resource: Some(resource.to_string()),
            description: description.to_string(),
            pricing: metered
                .then(|| json!({ "dimensions": [{ "tiers": [{ "price_usd": 1.0 }] }] })),
        }
    }

    fn set_service_endpoints(
        catalog: &mut skills::Catalog,
        service: &str,
        endpoints: Vec<skills::Endpoint>,
    ) {
        let svc = catalog
            .providers
            .iter_mut()
            .find(|svc| svc.fqn == service)
            .unwrap();
        svc.endpoints = endpoints;
    }

    fn hydrate_bigquery(catalog: &mut skills::Catalog) {
        set_service_endpoints(
            catalog,
            "solana-foundation/google/bigquery",
            vec![
                endpoint(
                    "POST",
                    "v2/projects/{projectsId}/queries",
                    "jobs",
                    "Run a SQL query",
                    true,
                ),
                endpoint(
                    "GET",
                    "v2/projects/{projectsId}/datasets",
                    "datasets",
                    "List datasets",
                    false,
                ),
            ],
        );
    }

    fn hydrate_vision(catalog: &mut skills::Catalog) {
        set_service_endpoints(
            catalog,
            "solana-foundation/google/vision",
            vec![endpoint(
                "POST",
                "v1/images:annotate",
                "images",
                "Annotate images",
                true,
            )],
        );
    }

    #[test]
    fn refresh_search_hits_rehydrates_single_service() {
        let mut catalog = catalog_index_only();

        let refreshed =
            refresh_search_hits(&mut catalog, Some("bigquery"), None, |catalog, service| {
                assert_eq!(service, "solana-foundation/google/bigquery");
                hydrate_bigquery(catalog);
                Ok(())
            });

        assert!(refreshed.unavailable_services.is_empty());
        assert!(refreshed.empty_services.is_empty());
        assert_eq!(
            distinct_services(&refreshed.hits),
            vec!["solana-foundation/google/bigquery".to_string()]
        );
        assert_eq!(refreshed.hits.len(), 2);
        assert!(refreshed.hits.iter().all(|hit| !hit.method.is_empty()));
        assert!(refreshed.hits.iter().all(|hit| !hit.path.is_empty()));
        assert_eq!(refreshed.hits[0].resource.as_deref(), Some("jobs"));
        assert_eq!(refreshed.hits[1].resource.as_deref(), Some("datasets"));
    }

    #[test]
    fn refresh_search_hits_rehydrates_multiple_services_in_order() {
        let mut catalog = catalog_index_only();

        let refreshed =
            refresh_search_hits(&mut catalog, Some("google"), None, |catalog, service| {
                match service {
                    "solana-foundation/google/bigquery" => hydrate_bigquery(catalog),
                    "solana-foundation/google/vision" => hydrate_vision(catalog),
                    other => panic!("unexpected service: {other}"),
                }
                Ok(())
            });

        assert!(refreshed.unavailable_services.is_empty());
        assert!(refreshed.empty_services.is_empty());
        assert_eq!(
            distinct_services(&refreshed.hits),
            vec![
                "solana-foundation/google/bigquery".to_string(),
                "solana-foundation/google/vision".to_string(),
            ]
        );
        assert_eq!(refreshed.hits.len(), 3);
        assert!(refreshed.hits.iter().all(|hit| !hit.path.is_empty()));
    }

    #[test]
    fn grouped_results_use_rehydrated_hits() {
        let mut catalog = catalog_index_only();

        let refreshed =
            refresh_search_hits(&mut catalog, Some("google"), None, |catalog, service| {
                match service {
                    "solana-foundation/google/bigquery" => hydrate_bigquery(catalog),
                    "solana-foundation/google/vision" => hydrate_vision(catalog),
                    other => panic!("unexpected service: {other}"),
                }
                Ok(())
            });

        let grouped = skills::group_search_results(&refreshed.hits);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].service, "solana-foundation/google/bigquery");
        assert_eq!(grouped[0].endpoints.len(), 2);
        assert!(!grouped[0].endpoints[0].path.is_empty());
        assert!(
            grouped[0].endpoints[0]
                .url
                .contains("/v2/projects/gateway-402/queries")
        );
        assert_eq!(grouped[1].service, "solana-foundation/google/vision");
        assert_eq!(grouped[1].endpoints[0].resource, Some("images".to_string()));
    }

    #[test]
    fn refresh_search_hits_skips_service_on_hydration_failure() {
        let mut catalog = catalog_index_only();

        let refreshed = refresh_search_hits(
            &mut catalog,
            Some("google"),
            None,
            |catalog, service| match service {
                "solana-foundation/google/bigquery" => {
                    hydrate_bigquery(catalog);
                    Ok(())
                }
                "solana-foundation/google/vision" => Err(pay_core::Error::Config("boom".into())),
                other => panic!("unexpected service: {other}"),
            },
        );

        assert_eq!(
            refreshed.unavailable_services,
            vec!["solana-foundation/google/vision".to_string()]
        );
        assert!(refreshed.empty_services.is_empty());
        assert_eq!(
            format_unavailable_warning(&refreshed.unavailable_services),
            "Warning: endpoint detail unavailable for solana-foundation/google/vision; skipping that service from results."
        );

        let bigquery_hits: Vec<_> = refreshed
            .hits
            .iter()
            .filter(|hit| hit.service == "solana-foundation/google/bigquery")
            .collect();
        assert!(bigquery_hits.iter().all(|hit| !hit.path.is_empty()));

        assert!(
            refreshed
                .hits
                .iter()
                .all(|hit| hit.service != "solana-foundation/google/vision")
        );
    }

    #[test]
    fn refresh_search_hits_skips_service_with_no_published_endpoints() {
        let mut catalog = catalog_index_only();

        let refreshed =
            refresh_search_hits(&mut catalog, Some("google"), None, |catalog, service| {
                match service {
                    "solana-foundation/google/bigquery" => hydrate_bigquery(catalog),
                    "solana-foundation/google/vision" => {}
                    other => panic!("unexpected service: {other}"),
                }
                Ok(())
            });

        assert!(refreshed.unavailable_services.is_empty());
        assert_eq!(
            refreshed.empty_services,
            vec!["solana-foundation/google/vision".to_string()]
        );
        assert_eq!(
            format_empty_warning(&refreshed.empty_services),
            "Warning: no published endpoints available for solana-foundation/google/vision; skipping that service from results."
        );
        assert_eq!(
            distinct_services(&refreshed.hits),
            vec!["solana-foundation/google/bigquery".to_string()]
        );
    }

    #[test]
    fn refresh_search_hits_returns_no_hits_when_only_empty_services_match() {
        let mut catalog = catalog_index_only();

        let refreshed =
            refresh_search_hits(&mut catalog, Some("vision"), None, |_catalog, service| {
                assert_eq!(service, "solana-foundation/google/vision");
                Ok(())
            });

        assert!(refreshed.hits.is_empty());
        assert!(refreshed.unavailable_services.is_empty());
        assert_eq!(
            refreshed.empty_services,
            vec!["solana-foundation/google/vision".to_string()]
        );
    }
}
