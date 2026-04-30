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

        if !refreshed.failed_services.is_empty() {
            eprintln!(
                "{}",
                format_hydration_warning(&refreshed.failed_services).yellow()
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
            eprintln!(
                "{}",
                "No results. Try a broader search or `pay skills search` to list all.".dimmed()
            );
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
    failed_services: Vec<String>,
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
            failed_services: Vec::new(),
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
    let failed_services = services
        .into_iter()
        .filter(|service| {
            hydration_failures.contains(service) && !service_has_endpoint_hits(&hits, service)
        })
        .collect();

    RefreshedSearchHits {
        hits,
        failed_services,
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

fn service_has_endpoint_hits(hits: &[SearchHit], service: &str) -> bool {
    hits.iter()
        .any(|hit| hit.service == service && !hit.path.is_empty())
}

fn format_hydration_warning(failed_services: &[String]) -> String {
    let services = failed_services.join(", ");
    if failed_services.len() == 1 {
        format!(
            "Warning: endpoint detail unavailable for {services}; showing service-level results."
        )
    } else {
        format!(
            "Warning: endpoint detail unavailable for {services}; showing service-level results for those services."
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
        "{}",
        "use `pay skills endpoints <fqn> <resource>` to inspect a provider.".dimmed()
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
            resource: resource.to_string(),
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

        assert!(refreshed.failed_services.is_empty());
        assert_eq!(
            distinct_services(&refreshed.hits),
            vec!["solana-foundation/google/bigquery".to_string()]
        );
        assert_eq!(refreshed.hits.len(), 2);
        assert!(refreshed.hits.iter().all(|hit| !hit.method.is_empty()));
        assert!(refreshed.hits.iter().all(|hit| !hit.path.is_empty()));
        assert!(refreshed.hits.iter().all(|hit| !hit.resource.is_empty()));
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

        assert!(refreshed.failed_services.is_empty());
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
        assert_eq!(grouped[1].endpoints[0].resource, "images");
    }

    #[test]
    fn refresh_search_hits_falls_back_to_placeholder_on_failure() {
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
            refreshed.failed_services,
            vec!["solana-foundation/google/vision".to_string()]
        );
        assert_eq!(
            format_hydration_warning(&refreshed.failed_services),
            "Warning: endpoint detail unavailable for solana-foundation/google/vision; showing service-level results."
        );

        let bigquery_hits: Vec<_> = refreshed
            .hits
            .iter()
            .filter(|hit| hit.service == "solana-foundation/google/bigquery")
            .collect();
        assert!(bigquery_hits.iter().all(|hit| !hit.path.is_empty()));

        let vision_hits: Vec<_> = refreshed
            .hits
            .iter()
            .filter(|hit| hit.service == "solana-foundation/google/vision")
            .collect();
        assert_eq!(vision_hits.len(), 1);
        assert!(vision_hits[0].method.is_empty());
        assert!(vision_hits[0].path.is_empty());
        assert!(vision_hits[0].resource.is_empty());
    }
}
