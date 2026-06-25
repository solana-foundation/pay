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

/// Full view: one service, all endpoints in a table.
fn print_full_service(hits: &[SearchHit]) {
    let first = &hits[0];
    print_service_header(&first.service, &first.service_title, &first.service_url);
    let refs: Vec<&SearchHit> = hits.iter().collect();
    eprintln!("{}", render_endpoint_table(&refs));
    eprintln!("  {}", format!("{} endpoints", hits.len()).dimmed());
}

/// Condensed view: multiple services, top metered + a few free per service.
fn print_condensed(hits: &[SearchHit], services: &[String]) {
    for (i, svc_name) in services.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }
        let svc_hits: Vec<&SearchHit> = hits.iter().filter(|h| &h.service == svc_name).collect();
        let first = svc_hits[0];
        print_service_header(&first.service, &first.service_title, &first.service_url);

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

        eprintln!("{}", render_endpoint_table(&shown_hits));

        let total = svc_hits.len();
        let shown = shown_metered + shown_free;
        if total > shown {
            eprintln!(
                "  {}",
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

fn print_service_header(slug: &str, title: &str, url: &str) {
    eprintln!();
    eprintln!("  {} {}", slug.bold(), format!("— {title}").dimmed());
    if !url.is_empty() {
        eprintln!("  {}", url.dimmed());
    }
}

/// A comfy-table of endpoints — `Method | Endpoint | Price | Description` —
/// indented to line up under the service header.
fn render_endpoint_table(hits: &[&SearchHit]) -> String {
    use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table, presets};

    let mut table = Table::new();
    table.load_preset(presets::UTF8_BORDERS_ONLY);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    let header = |s: &str| {
        Cell::new(s)
            .add_attribute(Attribute::Bold)
            .fg(Color::DarkGrey)
    };
    table.set_header(vec![
        header("METHOD"),
        header("ENDPOINT"),
        header("PRICE"),
        header("DESCRIPTION"),
    ]);

    for hit in hits {
        let method_color = match hit.method.as_str() {
            "GET" => Color::Green,
            "POST" => Color::Blue,
            "PUT" | "PATCH" => Color::Yellow,
            "DELETE" => Color::Red,
            _ => Color::Grey,
        };
        let price = format_price(hit.pricing.as_ref(), hit.metered);
        let price_color = if hit.metered {
            Color::Green
        } else {
            Color::DarkGrey
        };
        table.add_row(vec![
            Cell::new(&hit.method).fg(method_color),
            Cell::new(&hit.path),
            Cell::new(price).fg(price_color),
            Cell::new(truncate(&hit.description, 64)).fg(Color::DarkGrey),
        ]);
    }

    indent(&table.to_string(), 2)
}

/// Indent every line by `n` spaces (the table preset draws flush-left).
fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

/// Compact price string from the endpoint's `pricing` JSON. Handles the public
/// catalog `dimensions` shape, the local `{"usd"}` flat shape, and
/// `{"subscription"}` gating; falls back to `metered` / `free`.
fn format_price(pricing: Option<&serde_json::Value>, metered: bool) -> String {
    let Some(p) = pricing else {
        return if metered {
            "metered".into()
        } else {
            "free".into()
        };
    };
    if let Some(sub) = p.get("subscription") {
        let price = sub.get("price_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let period = sub
            .get("period")
            .and_then(|v| v.as_str())
            .unwrap_or("period");
        return format!("{} / {period}", fmt_usd(price));
    }
    if let Some(dims) = p.get("dimensions").and_then(|d| d.as_array()) {
        let parts: Vec<String> = dims.iter().filter_map(format_dimension).collect();
        if !parts.is_empty() {
            return parts.join("  +  ");
        }
    }
    if let Some(usd) = p.get("usd").and_then(|v| v.as_f64()) {
        return if usd <= 0.0 {
            "free".into()
        } else {
            fmt_usd(usd)
        };
    }
    if metered {
        "metered".into()
    } else {
        "free".into()
    }
}

/// One metering dimension → e.g. `$0.001 / req`, `$5 in / 1M tok`, `$1–2 / req`.
fn format_dimension(d: &serde_json::Value) -> Option<String> {
    let unit = d.get("unit").and_then(|v| v.as_str()).unwrap_or("unit");
    let scale = d.get("scale").and_then(|v| v.as_u64()).unwrap_or(1);
    let direction = d.get("direction").and_then(|v| v.as_str());
    let tiers = d.get("tiers").and_then(|v| v.as_array())?;
    let prices: Vec<f64> = tiers
        .iter()
        .filter_map(|t| t.get("price_usd").and_then(|v| v.as_f64()))
        .collect();
    if prices.is_empty() {
        return None;
    }
    let dir = match direction {
        Some("input") => " in",
        Some("output") => " out",
        _ => "",
    };
    let label = unit_label(unit, scale);
    let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() < f64::EPSILON {
        Some(format!("{}{dir} / {label}", fmt_usd(min)))
    } else {
        Some(format!("{}–{}{dir} / {label}", fmt_usd(min), fmt_usd(max)))
    }
}

/// Human unit label: `scale` is the number of `unit`s one `price_usd` covers.
fn unit_label(unit: &str, scale: u64) -> String {
    let u = match unit {
        "requests" => "req",
        "tokens" => "tok",
        "characters" => "char",
        "minutes" => "min",
        "pages" => "page",
        "images" => "image",
        other => other,
    };
    let prefix = match scale {
        1 => String::new(),
        1_000 => "1K ".to_string(),
        1_000_000 => "1M ".to_string(),
        1_000_000_000 => "1B ".to_string(),
        n => format!("{n} "),
    };
    format!("{prefix}{u}")
}

/// Format a USD amount compactly: `$0.001`, `$1.50`, `$5`, `$0`.
fn fmt_usd(n: f64) -> String {
    if n <= 0.0 {
        return "$0".to_string();
    }
    let s = if n >= 0.01 {
        format!("{n:.2}")
    } else {
        format!("{n:.6}")
    };
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("${s}")
}

fn print_endpoints_tip() {
    eprintln!();
    eprintln!(
        "  {}",
        "drill into a provider: `pay skills endpoints <fqn>` (the bold slug above).".dimmed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn fmt_usd_compact() {
        assert_eq!(fmt_usd(0.001), "$0.001");
        assert_eq!(fmt_usd(1.5), "$1.5");
        assert_eq!(fmt_usd(5.0), "$5");
        assert_eq!(fmt_usd(9.99), "$9.99");
        assert_eq!(fmt_usd(0.0), "$0");
    }

    #[test]
    fn format_price_shapes() {
        assert_eq!(format_price(None, false), "free");
        assert_eq!(format_price(None, true), "metered");
        assert_eq!(format_price(Some(&json!({"usd": 0.001})), true), "$0.001");
        assert_eq!(format_price(Some(&json!({"usd": 0.0})), true), "free");
        assert_eq!(
            format_price(
                Some(&json!({"subscription": {"period": "30d", "price_usd": 9.99}})),
                true
            ),
            "$9.99 / 30d"
        );
        // public catalog flat-per-request shape
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"requests","scale":1,"tiers":[{"price_usd":0.001}]}]})
                ),
                true
            ),
            "$0.001 / req"
        );
        // tokens with scale + direction
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"tokens","scale":1000000,"direction":"input","tiers":[{"price_usd":5.0}]}]})
                ),
                true
            ),
            "$5 in / 1M tok"
        );
        // tiered → price range
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"requests","scale":1,"tiers":[{"price_usd":1.0},{"price_usd":2.0}]}]})
                ),
                true
            ),
            "$1–$2 / req"
        );
        // multi-dimension (input + output) joined
        assert_eq!(
            format_price(
                Some(&json!({"dimensions":[
                    {"unit":"tokens","scale":1000000,"direction":"input","tiers":[{"price_usd":0.5}]},
                    {"unit":"tokens","scale":1000000,"direction":"output","tiers":[{"price_usd":1.5}]}
                ]})),
                true
            ),
            "$0.5 in / 1M tok  +  $1.5 out / 1M tok"
        );
    }

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
