use std::collections::BTreeSet;

use owo_colors::OwoColorize;
use pay_core::skills::{self, SearchHit, blocking::ensure_endpoints};

/// Max metered endpoints to show per service in condensed mode.
const CONDENSED_METERED_LIMIT: usize = 5;
/// Max total endpoints to show per service in condensed mode.
const CONDENSED_TOTAL_LIMIT: usize = 8;

/// Endpoint-table render width. With the 2-space indent under the service
/// header this caps the table at 80 columns; comfy-table wraps long cells to
/// fit rather than truncating.
const TABLE_WIDTH: u16 = 78;

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
    eprintln!("  {}", title.bold());
    eprintln!("  {}", format!("fqn: {slug}").dimmed());
    if !url.is_empty() {
        eprintln!("  {}", format!("url: {url}").dimmed());
    }
}

/// Render endpoints as a single-column bordered box — no columns. Each endpoint
/// is its own section (divided by `├───┤` rules); inside a section the fields
/// stack on their own lines: `METHOD  price`, then the full path, then the
/// description. Long lines wrap (never truncate) to the inner width, so the box
/// stays within [`TABLE_WIDTH`] + 2-space indent = 80 columns.
fn render_endpoint_table(hits: &[&SearchHit]) -> String {
    // Box width = inner + "│ " + " │" (4). Inner content wraps to INNER cols.
    let inner = (TABLE_WIDTH as usize).saturating_sub(4);
    let bar = "│".dimmed().to_string();
    let rule = |left: char, right: char| {
        format!("{left}{}{right}", "─".repeat(inner + 2))
            .dimmed()
            .to_string()
    };
    // One framed content line: "│ <content padded to `inner`> │". `visible` is
    // the display width of `content` (computed from the plain text before
    // coloring), so the right border still lines up under ANSI styling.
    let framed = |content: String, visible: usize| {
        let pad = " ".repeat(inner.saturating_sub(visible.min(inner)));
        format!("{bar} {content}{pad} {bar}")
    };

    let mut out: Vec<String> = vec![rule('┌', '┐')];
    for (i, hit) in hits.iter().enumerate() {
        if i > 0 {
            out.push(rule('├', '┤'));
        }
        // Line 1: colored method (6-col) + price.
        let price = format_price(hit.pricing.as_ref(), hit.metered);
        let price_visible = price.chars().count();
        let price = if hit.metered {
            price.green().to_string()
        } else {
            price.dimmed().to_string()
        };
        out.push(framed(
            format!("{}  {price}", color_method(&hit.method)),
            METHOD_CELL + 2 + price_visible,
        ));
        // Full endpoint path (bold), wrapped.
        for seg in wrap(&hit.path, inner) {
            let visible = seg.chars().count();
            out.push(framed(seg.bold().to_string(), visible));
        }
        // Description (dimmed), wrapped.
        for seg in wrap(&hit.description, inner) {
            let visible = seg.chars().count();
            out.push(framed(seg.dimmed().to_string(), visible));
        }
    }
    out.push(rule('└', '┘'));
    indent(&out.join("\n"), 2)
}

/// Fixed display width of the method cell on a section's first line.
const METHOD_CELL: usize = 6;

/// Color + left-pad an HTTP method to [`METHOD_CELL`] columns. Pad before
/// coloring so the ANSI codes don't count toward the cell's display width.
fn color_method(method: &str) -> String {
    format!("{method:<width$}", width = METHOD_CELL)
        .cyan()
        .to_string()
}

/// Greedy word-wrap to `width` columns (by char count). Over-long tokens are
/// hard-split rather than truncated, so no content is ever lost.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            let mut idx = 0;
            while chars.len() - idx > width {
                lines.push(chars[idx..idx + width].iter().collect());
                idx += width;
            }
            cur = chars[idx..].iter().collect();
            continue;
        }
        let clen = cur.chars().count();
        let need = if clen == 0 { word.chars().count() } else { clen + 1 + word.chars().count() };
        if need > width {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Indent every line by `n` spaces.
fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
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
        "drill into a provider: `pay skills endpoints <fqn>` (the fqn shown above).".dimmed()
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
