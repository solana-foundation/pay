//! Skills — service discovery for paid APIs.
//!
//! The skills catalog is a cached index of API providers and their endpoints.
//! Provider sources are managed in `~/.config/pay/skills.yaml` (see
//! [`config::SkillsConfig`]) and merged into a single consolidated cache.
//!
//! Query functions ([`search`], [`service_detail`], [`resource_endpoints`])
//! are pure — no I/O at query time. The I/O boundary is [`load_skills`].

pub mod build;
pub mod config;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Accept both `"1"` (string) and `1` (integer) for the version field.
fn deserialize_version<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<String, D::Error> {
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(serde::de::Error::custom(
            "expected string or number for version",
        )),
    }
}

// ── Catalog schema ──────────────────────────────────────────────────────────
//
// Matches the shape of the GCS `sandbox.json` file published by the
// agent-gateway CI. The `pay-skills` build script produces the same shape
// so both sources can feed the same code path.

/// Top-level catalog — the full index downloaded from the CDN.
///
/// Accepts both the GCS `sandbox.json` shape (field `services`) and
/// the `pay-skills/index.json` shape (field `providers`). The CLI
/// normalizes both into `services` internally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(alias = "version", deserialize_with = "deserialize_version")]
    pub schema_version: String,
    pub generated_at: String,
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub totals: Option<CatalogTotals>,
    /// Service list — populated from `services` (GCS) or `providers`
    /// (pay-skills). Both map to the same `Service` struct.
    #[serde(alias = "providers")]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogTotals {
    pub services: u32,
    pub endpoints: u32,
    #[serde(default)]
    pub metered_endpoints: u32,
    #[serde(default)]
    pub free_endpoints: u32,
}

/// A single API service (e.g. "bigquery", "generativelanguage").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    /// Which provider/source this service came from (e.g. "google").
    /// Set from the catalog's top-level `provider` field or the source
    /// name during merge.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub subdomain: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub service_url: String,
    #[serde(default)]
    pub endpoint_count: u32,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    #[serde(default)]
    pub free_tier: Option<serde_json::Value>,
}

/// A single API endpoint within a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub full_path: String,
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pricing: Option<serde_json::Value>,
}

// ── Query results ───────────────────────────────────────────────────────────

/// A search hit: one endpoint within a service, with enough context to
/// construct a `pay curl` command directly. The primary result type of
/// [`search`] — users should never need a second command to get from a
/// search result to an actionable URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Service this endpoint belongs to.
    pub service: String,
    pub service_title: String,
    pub service_url: String,
    /// The endpoint itself.
    pub method: String,
    pub path: String,
    pub full_path: String,
    pub description: String,
    pub resource: String,
    /// Pricing — `None` means free (pass-through).
    pub pricing: Option<serde_json::Value>,
    /// Whether this endpoint is metered (pay adds value).
    pub metered: bool,
}

/// Grouped search result — service metadata + matching endpoints.
/// Used by the CLI `--json` output and the MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultGroup {
    pub service: String,
    pub title: String,
    pub url: String,
    pub endpoints: Vec<EndpointHit>,
}

/// A single endpoint within a search result group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointHit {
    pub method: String,
    /// Complete, ready-to-use URL (`service_url + full_path`). The agent
    /// can paste this directly into `pay curl` without any assembly.
    pub url: String,
    pub path: String,
    pub description: String,
    pub resource: String,
    pub metered: bool,
}

/// Group flat `SearchHit` results by service for structured output.
pub fn group_search_results(hits: &[SearchHit]) -> Vec<SearchResultGroup> {
    let mut groups: Vec<SearchResultGroup> = Vec::new();
    for hit in hits {
        if groups.last().map(|g| g.service.as_str()) != Some(&hit.service) {
            groups.push(SearchResultGroup {
                service: hit.service.clone(),
                title: hit.service_title.clone(),
                url: hit.service_url.clone(),
                endpoints: Vec::new(),
            });
        }
        groups.last_mut().unwrap().endpoints.push(EndpointHit {
            method: hit.method.clone(),
            url: build_endpoint_url(&hit.service_url, &hit.path),
            path: hit.path.clone(),
            description: hit.description.clone(),
            resource: hit.resource.clone(),
            metered: hit.metered,
        });
    }
    groups
}

/// A service summary — used by the MCP `skills_search` tool for context
/// efficiency (agents don't want 2,617 endpoints in one response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub service_url: String,
    pub endpoint_count: u32,
    pub metered_endpoints: u32,
    pub free_endpoints: u32,
    pub min_price_usd: f64,
    pub max_price_usd: f64,
}

/// Level 2 result: a resource group returned by [`service_detail`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceGroup {
    pub name: String,
    pub endpoint_count: u32,
    pub metered_count: u32,
    pub methods: Vec<String>,
}

/// Level 2 wrapper: service metadata + resource breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDetail {
    pub name: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub service_url: String,
    pub resources: Vec<ResourceGroup>,
}

/// Level 3 result: endpoints for a specific resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEndpoints {
    pub service: String,
    pub resource: String,
    pub service_url: String,
    pub endpoints: Vec<Endpoint>,
}

// ── Pure query functions ────────────────────────────────────────────────────

/// Search services and endpoints by keyword and/or category.
///
/// Matches against service name/title/description AND endpoint
/// path/description (case-insensitive substring). Returns individual
/// endpoint hits grouped by service, metered endpoints first — each
/// hit contains enough info to construct a `pay curl` command.
///
/// When `query` is None and `category` is None, returns all metered
/// endpoints (the ones pay adds value for) across all services.
pub fn search(catalog: &Catalog, query: Option<&str>, category: Option<&str>) -> Vec<SearchHit> {
    let query_lower = query.map(|q| q.to_lowercase());

    let mut hits: Vec<SearchHit> = Vec::new();

    for svc in &catalog.services {
        // Category filter (service-level)
        if let Some(cat) = category
            && !svc.category.eq_ignore_ascii_case(cat)
        {
            continue;
        }

        // Check if the service itself matches the keyword
        let service_matches = match &query_lower {
            Some(q) => {
                let haystack =
                    format!("{} {} {}", svc.name, svc.title, svc.description).to_lowercase();
                haystack.contains(q.as_str())
            }
            None => true,
        };

        for ep in &svc.endpoints {
            // An endpoint is a hit if:
            // - the service matched the keyword, OR
            // - the endpoint's own path/description matches the keyword
            let endpoint_matches = if service_matches {
                true
            } else if let Some(ref q) = query_lower {
                let haystack =
                    format!("{} {} {}", ep.path, ep.full_path, ep.description).to_lowercase();
                haystack.contains(q.as_str())
            } else {
                false
            };

            if !endpoint_matches {
                continue;
            }

            hits.push(SearchHit {
                service: svc.name.clone(),
                service_title: svc.title.clone(),
                service_url: svc.service_url.clone(),
                method: ep.method.clone(),
                path: ep.path.clone(),
                full_path: ep.full_path.clone(),
                description: ep.description.clone(),
                resource: ep.resource.clone(),
                pricing: ep.pricing.clone(),
                metered: ep.pricing.is_some(),
            });
        }
    }

    // Sort: group by service, metered first within each service.
    hits.sort_by(|a, b| {
        a.service
            .cmp(&b.service)
            .then_with(|| b.metered.cmp(&a.metered))
            .then_with(|| a.path.cmp(&b.path))
    });

    // Hoist services that have metered endpoints to the top (so the
    // first thing the user sees is pay-able endpoints, not free ones).
    let has_metered: std::collections::HashSet<_> = hits
        .iter()
        .filter(|h| h.metered)
        .map(|h| h.service.clone())
        .collect();
    hits.sort_by(|a, b| {
        let a_has = has_metered.contains(&a.service);
        let b_has = has_metered.contains(&b.service);
        b_has
            .cmp(&a_has)
            .then_with(|| a.service.cmp(&b.service))
            .then_with(|| b.metered.cmp(&a.metered))
            .then_with(|| a.path.cmp(&b.path))
    });

    hits
}

/// Search at the service level (for MCP progressive disclosure).
///
/// Same matching logic as [`search`] but returns service summaries
/// instead of individual endpoints — keeps the response compact so
/// agents don't consume 2,617 endpoints of context when all they
/// need is "which services exist".
pub fn search_services(
    catalog: &Catalog,
    query: Option<&str>,
    category: Option<&str>,
) -> Vec<ServiceSummary> {
    let query_lower = query.map(|q| q.to_lowercase());

    catalog
        .services
        .iter()
        .filter(|svc| {
            if let Some(cat) = category
                && !svc.category.eq_ignore_ascii_case(cat)
            {
                return false;
            }
            if let Some(ref q) = query_lower {
                // Match service-level OR any endpoint-level
                let svc_haystack =
                    format!("{} {} {}", svc.name, svc.title, svc.description).to_lowercase();
                if svc_haystack.contains(q.as_str()) {
                    return true;
                }
                return svc.endpoints.iter().any(|ep| {
                    let ep_haystack =
                        format!("{} {} {}", ep.path, ep.full_path, ep.description).to_lowercase();
                    ep_haystack.contains(q.as_str())
                });
            }
            true
        })
        .map(summarize_service)
        .collect()
}

/// Level 2: list resources within a service.
pub fn service_detail(catalog: &Catalog, service_name: &str) -> Option<ServiceDetail> {
    let svc = catalog
        .services
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(service_name))?;

    // Group endpoints by resource
    let mut groups: BTreeMap<String, (u32, u32, Vec<String>)> = BTreeMap::new();
    for ep in &svc.endpoints {
        let resource = if ep.resource.is_empty() {
            "(default)"
        } else {
            &ep.resource
        };
        let entry = groups
            .entry(resource.to_string())
            .or_insert((0, 0, Vec::new()));
        entry.0 += 1;
        if ep.pricing.is_some() {
            entry.1 += 1;
        }
        if !entry.2.contains(&ep.method) {
            entry.2.push(ep.method.clone());
        }
    }

    Some(ServiceDetail {
        name: svc.name.clone(),
        title: svc.title.clone(),
        description: svc.description.clone(),
        category: svc.category.clone(),
        service_url: svc.service_url.clone(),
        resources: groups
            .into_iter()
            .map(|(name, (count, metered, methods))| ResourceGroup {
                name,
                endpoint_count: count,
                metered_count: metered,
                methods,
            })
            .collect(),
    })
}

/// Level 3: list endpoints for a specific resource within a service.
pub fn resource_endpoints(
    catalog: &Catalog,
    service_name: &str,
    resource_name: &str,
) -> Option<ResourceEndpoints> {
    let svc = catalog
        .services
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(service_name))?;

    let endpoints: Vec<Endpoint> = svc
        .endpoints
        .iter()
        .filter(|ep| ep.resource.eq_ignore_ascii_case(resource_name))
        .cloned()
        .collect();

    if endpoints.is_empty() {
        return None;
    }

    Some(ResourceEndpoints {
        service: svc.name.clone(),
        resource: resource_name.to_string(),
        service_url: svc.service_url.clone(),
        endpoints,
    })
}

// ── Catalog loading + caching ───────────────────────────────────────────────

/// Load the consolidated skills catalog. Uses the cached version if
/// fresh, otherwise fetches all sources from `~/.config/pay/skills.yaml`
/// and merges them.
pub fn load_skills() -> Result<Catalog> {
    let cfg = config::SkillsConfig::load()?;

    // Cache hit?
    if let Some(path) = cfg.valid_cache_path() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("read cache: {e}")))?;
        return parse_catalog(&raw);
    }

    // Cache miss — fetch, merge, cache.
    match fetch_and_merge(&cfg) {
        Ok(catalog) => {
            let _ = write_cache(&cfg, &catalog);
            cfg.clean_stale_caches();
            Ok(catalog)
        }
        Err(fetch_err) => {
            // Try ANY existing cache file as a fallback (even stale).
            let dir =
                std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills").into_owned());
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("skills-")
                        && name.ends_with(".json")
                        && let Ok(raw) = std::fs::read_to_string(entry.path())
                        && let Ok(cat) = parse_catalog(&raw)
                    {
                        return Ok(cat);
                    }
                }
            }
            Err(fetch_err)
        }
    }
}

/// Force-refresh: fetch all sources, merge, write cache.
pub fn update_skills() -> Result<Catalog> {
    let cfg = config::SkillsConfig::load()?;
    let catalog = fetch_and_merge(&cfg)?;
    write_cache(&cfg, &catalog)?;
    cfg.clean_stale_caches();
    Ok(catalog)
}

/// Fetch each source URL and merge all services into one Catalog.
fn fetch_and_merge(cfg: &config::SkillsConfig) -> Result<Catalog> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;

    let mut all_services: Vec<Service> = Vec::new();

    for source in &cfg.sources {
        match fetch_one(&client, &source.url) {
            Ok(cat) => {
                // Tag each service with its provider. Prefer the
                // catalog's own `provider` field; fall back to the
                // source name from skills.yaml.
                let provider_tag = if cat.provider.is_empty() {
                    &source.name
                } else {
                    &cat.provider
                };
                for mut svc in cat.services {
                    if svc.provider.is_empty() {
                        svc.provider = provider_tag.to_string();
                    }
                    all_services.push(svc);
                }
            }
            Err(e) => {
                tracing::warn!(url = %source.url, error = %e, "Skipping skills source");
            }
        }
    }

    // Deduplicate by service name (first wins).
    let mut seen = std::collections::HashSet::new();
    all_services.retain(|svc| seen.insert(svc.name.clone()));

    Ok(Catalog {
        schema_version: "1".to_string(),
        generated_at: String::new(),
        environment: String::new(),
        provider: String::new(),
        totals: None,
        services: all_services,
    })
}

fn fetch_one(client: &reqwest::blocking::Client, url: &str) -> Result<Catalog> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Config(format!("fetch {url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "skills source {url} returned {}",
            resp.status()
        )));
    }

    let raw = resp
        .text()
        .map_err(|e| Error::Config(format!("read {url}: {e}")))?;
    parse_catalog(&raw)
}

fn parse_catalog(raw: &str) -> Result<Catalog> {
    serde_json::from_str(raw).map_err(|e| Error::Config(format!("parse catalog: {e}")))
}

fn write_cache(cfg: &config::SkillsConfig, catalog: &Catalog) -> Result<()> {
    let path = cfg.new_cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Config(format!("create cache dir: {e}")))?;
    }
    let json = serde_json::to_string(catalog)
        .map_err(|e| Error::Config(format!("serialize catalog: {e}")))?;
    std::fs::write(&path, json).map_err(|e| Error::Config(format!("write cache: {e}")))?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Default project ID for gateway queries. The gateway rewrites this
/// to the operator's actual project, so the value here is just a
/// routing token — but agents need a concrete value to avoid guessing.
const GATEWAY_PROJECT_ID: &str = "gateway-402";

/// Build a complete endpoint URL from a service URL + path.
/// Resolves `{projectsId}` and `{project}` placeholders to the
/// gateway project ID so the URL is truly copy-paste-ready.
pub fn build_endpoint_url(service_url: &str, path: &str) -> String {
    let base = service_url.trim_end_matches('/');
    let p = path.trim_start_matches('/');
    if p.is_empty() {
        return base.to_string();
    }
    let resolved = p
        .replace("{projectsId}", GATEWAY_PROJECT_ID)
        .replace("{project}", GATEWAY_PROJECT_ID);
    format!("{base}/{resolved}")
}

/// Build an `EndpointHit` from raw service + endpoint data.
///
/// Uses `path` (the raw API path) — NOT `full_path` which prepends
/// the subdomain routing prefix. When each service has its own
/// `service_url`, the subdomain prefix is redundant and produces
/// broken double-prefixed URLs like `/bigquery/bigquery/v2/...`.
pub fn endpoint_to_hit(service_url: &str, ep: &Endpoint) -> EndpointHit {
    EndpointHit {
        method: ep.method.clone(),
        url: build_endpoint_url(service_url, &ep.path),
        path: ep.path.clone(),
        description: ep.description.clone(),
        resource: ep.resource.clone(),
        metered: ep.pricing.is_some(),
    }
}

fn summarize_service(svc: &Service) -> ServiceSummary {
    let mut metered = 0u32;
    let mut free = 0u32;
    let mut prices: Vec<f64> = Vec::new();

    for ep in &svc.endpoints {
        if ep.pricing.is_some() {
            metered += 1;
            collect_prices(&ep.pricing, &mut prices);
        } else {
            free += 1;
        }
    }

    ServiceSummary {
        name: svc.name.clone(),
        title: svc.title.clone(),
        description: svc.description.clone(),
        category: svc.category.clone(),
        service_url: svc.service_url.clone(),
        endpoint_count: svc.endpoint_count.max(metered + free),
        metered_endpoints: metered,
        free_endpoints: free,
        min_price_usd: prices.iter().copied().reduce(f64::min).unwrap_or(0.0),
        max_price_usd: prices.iter().copied().reduce(f64::max).unwrap_or(0.0),
    }
}

/// Recursively extract USD prices from a pricing JSON value.
fn collect_prices(pricing: &Option<serde_json::Value>, out: &mut Vec<f64>) {
    let Some(val) = pricing else { return };
    match val {
        serde_json::Value::Object(map) => {
            if let Some(p) = map.get("price_usd").and_then(|v| v.as_f64()) {
                out.push(p);
            }
            for v in map.values() {
                collect_prices(&Some(v.clone()), out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_prices(&Some(v.clone()), out);
            }
        }
        _ => {}
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> Catalog {
        let json = r#"{
            "schema_version": "1",
            "generated_at": "2026-04-13T00:00:00Z",
            "environment": "test",
            "provider": "test",
            "services": [
                {
                    "name": "bigquery",
                    "title": "BigQuery API",
                    "description": "Serverless data warehouse",
                    "category": "data",
                    "version": "v2",
                    "service_url": "https://gw.example.com",
                    "endpoint_count": 3,
                    "endpoints": [
                        {
                            "method": "POST",
                            "path": "v2/projects/{p}/queries",
                            "full_path": "/bigquery/v2/projects/{p}/queries",
                            "resource": "jobs",
                            "description": "Run a query",
                            "pricing": {
                                "model": "tiered",
                                "dimensions": [{
                                    "tiers": [
                                        {"up_to": 1, "price_usd": 0},
                                        {"price_usd": 6.25}
                                    ]
                                }]
                            }
                        },
                        {
                            "method": "GET",
                            "path": "v2/projects/{p}/queries/{j}",
                            "full_path": "/bigquery/v2/projects/{p}/queries/{j}",
                            "resource": "jobs",
                            "description": "Get query results"
                        },
                        {
                            "method": "GET",
                            "path": "v2/projects/{p}/datasets",
                            "full_path": "/bigquery/v2/projects/{p}/datasets",
                            "resource": "datasets",
                            "description": "List datasets"
                        }
                    ]
                },
                {
                    "name": "vision",
                    "title": "Cloud Vision API",
                    "description": "Image recognition and OCR",
                    "category": "ai_ml",
                    "version": "v1",
                    "service_url": "https://gw.example.com",
                    "endpoint_count": 1,
                    "endpoints": [
                        {
                            "method": "POST",
                            "path": "v1/images:annotate",
                            "full_path": "/vision/v1/images:annotate",
                            "resource": "images",
                            "description": "Annotate images",
                            "pricing": {
                                "model": "tiered",
                                "dimensions": [{
                                    "tiers": [{"price_usd": 1.50}]
                                }]
                            }
                        }
                    ]
                }
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    // ── search (endpoint-level) ─────────────────────────────────────────

    #[test]
    fn search_no_filters_returns_all_endpoints() {
        let cat = test_catalog();
        let results = search(&cat, None, None);
        // 3 from bigquery + 1 from vision = 4
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn search_by_service_keyword_returns_endpoints() {
        let cat = test_catalog();
        let results = search(&cat, Some("warehouse"), None);
        // "warehouse" matches bigquery's description → all 3 bigquery endpoints
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|h| h.service == "bigquery"));
    }

    #[test]
    fn search_by_endpoint_keyword() {
        let cat = test_catalog();
        // "annotate" only appears in vision's endpoint description
        let results = search(&cat, Some("annotate"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].service, "vision");
        assert_eq!(results[0].method, "POST");
    }

    #[test]
    fn search_by_path_keyword() {
        let cat = test_catalog();
        // "queries" appears in bigquery endpoint paths
        let results = search(&cat, Some("queries"), None);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|h| h.service == "bigquery"));
    }

    #[test]
    fn search_case_insensitive() {
        let cat = test_catalog();
        let results = search(&cat, Some("BIGQUERY"), None);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_by_category() {
        let cat = test_catalog();
        let results = search(&cat, None, Some("ai_ml"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].service, "vision");
    }

    #[test]
    fn search_keyword_and_category_mismatch() {
        let cat = test_catalog();
        // "warehouse" matches bigquery (data), not ai_ml
        let results = search(&cat, Some("warehouse"), Some("ai_ml"));
        assert!(results.is_empty());
    }

    #[test]
    fn search_no_match() {
        let cat = test_catalog();
        let results = search(&cat, Some("nonexistent"), None);
        assert!(results.is_empty());
    }

    #[test]
    fn search_services_with_metered_sort_first() {
        let cat = test_catalog();
        let results = search(&cat, None, None);
        // Services with paid endpoints (bigquery, vision) sort before
        // services that are entirely free. Within each service, metered
        // endpoints come first.
        //
        // Both test services have metered endpoints, so we just check
        // that within each service block, metered comes before free.
        let bq: Vec<_> = results.iter().filter(|h| h.service == "bigquery").collect();
        if let Some(first_free) = bq.iter().position(|h| !h.metered) {
            let last_metered = bq.iter().rposition(|h| h.metered).unwrap_or(0);
            assert!(
                last_metered < first_free,
                "within bigquery: metered must come before free"
            );
        }
    }

    #[test]
    fn search_hit_has_full_context() {
        let cat = test_catalog();
        let results = search(&cat, Some("annotate"), None);
        let hit = &results[0];
        assert_eq!(hit.service, "vision");
        assert_eq!(hit.service_title, "Cloud Vision API");
        assert_eq!(hit.service_url, "https://gw.example.com");
        assert_eq!(hit.method, "POST");
        assert!(!hit.full_path.is_empty());
        assert!(hit.metered);
        assert!(hit.pricing.is_some());
    }

    // ── search_services (MCP level) ──────────────────────────────────────

    #[test]
    fn search_services_returns_summaries() {
        let cat = test_catalog();
        let results = search_services(&cat, Some("bigquery"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "bigquery");
        assert_eq!(results[0].metered_endpoints, 1);
        assert_eq!(results[0].free_endpoints, 2);
    }

    // ── service_detail ────────────────────────────────────────────────────

    #[test]
    fn service_detail_groups_by_resource() {
        let cat = test_catalog();
        let detail = service_detail(&cat, "bigquery").unwrap();
        assert_eq!(detail.resources.len(), 2);

        let jobs = detail.resources.iter().find(|r| r.name == "jobs").unwrap();
        assert_eq!(jobs.endpoint_count, 2);
        assert_eq!(jobs.metered_count, 1);
        assert!(jobs.methods.contains(&"POST".to_string()));
        assert!(jobs.methods.contains(&"GET".to_string()));

        let datasets = detail
            .resources
            .iter()
            .find(|r| r.name == "datasets")
            .unwrap();
        assert_eq!(datasets.endpoint_count, 1);
        assert_eq!(datasets.metered_count, 0);
    }

    #[test]
    fn service_detail_unknown_service() {
        let cat = test_catalog();
        assert!(service_detail(&cat, "nonexistent").is_none());
    }

    #[test]
    fn service_detail_case_insensitive() {
        let cat = test_catalog();
        assert!(service_detail(&cat, "BigQuery").is_some());
    }

    // ── resource_endpoints ────────────────────────────────────────────────

    #[test]
    fn resource_endpoints_returns_matching() {
        let cat = test_catalog();
        let result = resource_endpoints(&cat, "bigquery", "jobs").unwrap();
        assert_eq!(result.endpoints.len(), 2);
        assert_eq!(result.service, "bigquery");
        assert_eq!(result.resource, "jobs");
    }

    #[test]
    fn resource_endpoints_unknown_resource() {
        let cat = test_catalog();
        assert!(resource_endpoints(&cat, "bigquery", "nonexistent").is_none());
    }

    #[test]
    fn resource_endpoints_includes_pricing() {
        let cat = test_catalog();
        let result = resource_endpoints(&cat, "bigquery", "jobs").unwrap();
        let metered = result.endpoints.iter().find(|e| e.pricing.is_some());
        assert!(metered.is_some(), "should include the metered endpoint");
        let free = result.endpoints.iter().find(|e| e.pricing.is_none());
        assert!(free.is_some(), "should include the free endpoint");
    }

    // ── collect_prices ────────────────────────────────────────────────────

    #[test]
    fn collect_prices_recurses_nested_structures() {
        let pricing: serde_json::Value = serde_json::json!({
            "model": "tiered",
            "dimensions": [
                { "tiers": [
                    { "up_to": 1, "price_usd": 0 },
                    { "price_usd": 6.25 }
                ]}
            ]
        });
        let mut prices = Vec::new();
        collect_prices(&Some(pricing), &mut prices);
        assert_eq!(prices, vec![0.0, 6.25]);
    }

    #[test]
    fn collect_prices_handles_none() {
        let mut prices = Vec::new();
        collect_prices(&None, &mut prices);
        assert!(prices.is_empty());
    }

    // ── group_search_results ─────────────────────────────────────────────

    #[test]
    fn group_search_results_groups_by_service() {
        let cat = test_catalog();
        let hits = search(&cat, None, None);
        let groups = group_search_results(&hits);
        assert_eq!(groups.len(), 2);
        let bq = groups.iter().find(|g| g.service == "bigquery").unwrap();
        assert_eq!(bq.title, "BigQuery API");
        assert_eq!(bq.endpoints.len(), 3);
    }

    #[test]
    fn group_search_results_empty() {
        let groups = group_search_results(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn group_search_results_endpoints_have_urls() {
        let cat = test_catalog();
        let hits = search(&cat, Some("annotate"), None);
        let groups = group_search_results(&hits);
        assert_eq!(groups.len(), 1);
        let ep = &groups[0].endpoints[0];
        assert!(ep.url.starts_with("https://"));
        assert!(ep.url.contains("annotate"));
        assert!(ep.metered);
    }

    // ── build_endpoint_url ───────────────────────────────────────────────

    #[test]
    fn build_endpoint_url_basic() {
        let url = build_endpoint_url("https://gw.example.com", "v2/projects/foo/queries");
        assert_eq!(url, "https://gw.example.com/v2/projects/foo/queries");
    }

    #[test]
    fn build_endpoint_url_resolves_placeholders() {
        let url = build_endpoint_url("https://gw.example.com", "v2/projects/{projectsId}/queries");
        assert_eq!(
            url,
            "https://gw.example.com/v2/projects/gateway-402/queries"
        );
    }

    #[test]
    fn build_endpoint_url_resolves_project_placeholder() {
        let url = build_endpoint_url("https://gw.example.com", "v1/{project}/datasets");
        assert_eq!(url, "https://gw.example.com/v1/gateway-402/datasets");
    }

    #[test]
    fn build_endpoint_url_empty_path() {
        let url = build_endpoint_url("https://gw.example.com/", "");
        assert_eq!(url, "https://gw.example.com");
    }

    #[test]
    fn build_endpoint_url_strips_extra_slashes() {
        let url = build_endpoint_url("https://gw.example.com/", "/v2/foo");
        assert_eq!(url, "https://gw.example.com/v2/foo");
    }

    // ── endpoint_to_hit ──────────────────────────────────────────────────

    #[test]
    fn endpoint_to_hit_builds_correct_hit() {
        let ep = Endpoint {
            method: "POST".to_string(),
            path: "v1/images:annotate".to_string(),
            full_path: "/vision/v1/images:annotate".to_string(),
            resource: "images".to_string(),
            description: "Annotate images".to_string(),
            pricing: Some(serde_json::json!({"price_usd": 1.50})),
        };
        let hit = endpoint_to_hit("https://gw.example.com", &ep);
        assert_eq!(hit.method, "POST");
        assert_eq!(hit.url, "https://gw.example.com/v1/images:annotate");
        assert_eq!(hit.resource, "images");
        assert!(hit.metered);
    }

    #[test]
    fn endpoint_to_hit_free_endpoint() {
        let ep = Endpoint {
            method: "GET".to_string(),
            path: "v2/datasets".to_string(),
            full_path: "/bq/v2/datasets".to_string(),
            resource: "datasets".to_string(),
            description: "List datasets".to_string(),
            pricing: None,
        };
        let hit = endpoint_to_hit("https://gw.example.com", &ep);
        assert!(!hit.metered);
    }

    // ── summarize_service ────────────────────────────────────────────────

    #[test]
    fn summarize_service_counts_correct() {
        let cat = test_catalog();
        let svc = cat.services.iter().find(|s| s.name == "bigquery").unwrap();
        let summary = summarize_service(svc);
        assert_eq!(summary.name, "bigquery");
        assert_eq!(summary.metered_endpoints, 1);
        assert_eq!(summary.free_endpoints, 2);
        assert_eq!(summary.endpoint_count, 3);
        assert!(summary.min_price_usd <= summary.max_price_usd);
    }

    #[test]
    fn summarize_service_extracts_prices() {
        let cat = test_catalog();
        let svc = cat.services.iter().find(|s| s.name == "bigquery").unwrap();
        let summary = summarize_service(svc);
        // bigquery has tiers with price_usd: 0 and 6.25
        assert!((summary.min_price_usd - 0.0).abs() < f64::EPSILON);
        assert!((summary.max_price_usd - 6.25).abs() < f64::EPSILON);
    }

    #[test]
    fn summarize_service_all_metered() {
        let cat = test_catalog();
        let svc = cat.services.iter().find(|s| s.name == "vision").unwrap();
        let summary = summarize_service(svc);
        assert_eq!(summary.metered_endpoints, 1);
        assert_eq!(summary.free_endpoints, 0);
    }
}
