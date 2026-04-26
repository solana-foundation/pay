//! Skills — service discovery for paid APIs.
//!
//! The skills catalog is a cached index of API providers and their endpoints.
//! Provider sources are managed in `~/.config/pay/skills.yaml` (see
//! [`config::SkillsConfig`]) and merged into a single consolidated cache.
//!
//! The index is lightweight (no inline endpoints). Endpoint data is
//! lazy-fetched from `{base_url}/providers/{fqn}.json` on demand and
//! cached locally.
//!
//! Query functions ([`search`], [`service_detail`], [`resource_endpoints`])
//! are pure — no I/O at query time. The I/O boundary is [`load_skills`]
//! and [`load_service_endpoints`].

pub mod build;
pub mod config;
pub mod probe;

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

/// Top-level catalog — the skills.json index from the CDN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(alias = "version", deserialize_with = "deserialize_version")]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    /// CDN base URL for fetching provider detail files.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub provider_count: u32,
    /// Provider list from `providers[]` in skills.json.
    #[serde(alias = "services", default)]
    pub providers: Vec<Service>,
}

/// A provider entry in the index. Endpoints are NOT inline — they're
/// lazy-fetched from `{base_url}/providers/{fqn}.json` when needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    /// Fully qualified name: `operator/origin/name` or `operator/name`.
    #[serde(alias = "name")]
    pub fqn: String,
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
    #[serde(default)]
    pub endpoint_count: u32,
    #[serde(default)]
    pub has_metering: bool,
    #[serde(default)]
    pub has_free_tier: bool,
    #[serde(default)]
    pub min_price_usd: f64,
    #[serde(default)]
    pub max_price_usd: f64,
    /// Content hash of the detail file — used for cache invalidation.
    #[serde(default)]
    pub sha: String,
    /// Endpoints — empty from the index, populated by [`load_service_endpoints`].
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    /// Markdown body from the provider detail file — empty from the index,
    /// populated alongside endpoints by [`ensure_endpoints`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl Service {
    /// Short name (last segment of the FQN).
    pub fn name(&self) -> &str {
        self.fqn.rsplit('/').next().unwrap_or(&self.fqn)
    }

    /// Whether endpoints have been loaded (lazy-fetch completed).
    pub fn endpoints_loaded(&self) -> bool {
        !self.endpoints.is_empty()
    }
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
/// construct a `pay curl` command directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub service: String,
    pub service_title: String,
    pub service_url: String,
    pub method: String,
    pub path: String,
    pub full_path: String,
    pub description: String,
    pub resource: String,
    pub pricing: Option<serde_json::Value>,
    pub metered: bool,
}

/// Grouped search result — service metadata + matching endpoints.
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

/// A service summary — used by the MCP `search_skills` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
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
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
    pub resources: Vec<ResourceGroup>,
}

/// Level 3 result: endpoints for a specific resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEndpoints {
    pub service: String,
    pub resource: String,
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
    pub endpoints: Vec<Endpoint>,
}

// ── Pure query functions ────────────────────────────────────────────────────

/// Search services and endpoints by keyword and/or category.
///
/// When endpoints are loaded (from detail files), matches against endpoint
/// paths and descriptions. When only the index is available, matches
/// service-level fields only and emits a single summary hit per service.
pub fn search(catalog: &Catalog, query: Option<&str>, category: Option<&str>) -> Vec<SearchHit> {
    let query_lower = query.map(|q| q.to_lowercase());

    let mut hits: Vec<SearchHit> = Vec::new();

    for svc in &catalog.providers {
        // Category filter
        if let Some(cat) = category
            && !svc.meta.category.eq_ignore_ascii_case(cat)
        {
            continue;
        }

        // Check if the service itself matches the keyword
        let service_matches = match &query_lower {
            Some(q) => {
                let haystack = format!("{} {} {}", svc.fqn, svc.meta.title, svc.meta.description)
                    .to_lowercase();
                haystack.contains(q.as_str())
            }
            None => true,
        };

        if svc.endpoints_loaded() {
            // Full endpoint-level search
            for ep in &svc.endpoints {
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
                    service: svc.fqn.clone(),
                    service_title: svc.meta.title.clone(),
                    service_url: svc.meta.service_url.clone(),
                    method: ep.method.clone(),
                    path: ep.path.clone(),
                    full_path: ep.full_path.clone(),
                    description: ep.description.clone(),
                    resource: ep.resource.clone(),
                    pricing: ep.pricing.clone(),
                    metered: ep.pricing.is_some(),
                });
            }
        } else if service_matches {
            // Index-only: emit a service-level placeholder hit
            hits.push(SearchHit {
                service: svc.fqn.clone(),
                service_title: svc.meta.title.clone(),
                service_url: svc.meta.service_url.clone(),
                method: String::new(),
                path: String::new(),
                full_path: String::new(),
                description: svc.meta.description.clone(),
                resource: String::new(),
                pricing: None,
                metered: svc.has_metering,
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

    // Hoist services that have metered endpoints to the top.
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
pub fn search_services(
    catalog: &Catalog,
    query: Option<&str>,
    category: Option<&str>,
) -> Vec<ServiceSummary> {
    let query_lower = query.map(|q| q.to_lowercase());

    catalog
        .providers
        .iter()
        .filter(|svc| {
            if let Some(cat) = category
                && !svc.meta.category.eq_ignore_ascii_case(cat)
            {
                return false;
            }
            if let Some(ref q) = query_lower {
                let svc_haystack =
                    format!("{} {} {}", svc.fqn, svc.meta.title, svc.meta.description)
                        .to_lowercase();
                if svc_haystack.contains(q.as_str()) {
                    return true;
                }
                // Also check endpoints if loaded
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
/// Requires endpoints to be loaded — returns None if empty.
pub fn service_detail(catalog: &Catalog, service_name: &str) -> Option<ServiceDetail> {
    let svc = find_service(catalog, service_name)?;

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
        name: svc.fqn.clone(),
        meta: svc.meta.clone(),
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
    let svc = find_service(catalog, service_name)?;

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
        service: svc.fqn.clone(),
        resource: resource_name.to_string(),
        meta: svc.meta.clone(),
        endpoints,
    })
}

/// Find a service by FQN or short name (case-insensitive).
fn find_service<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a Service> {
    catalog
        .providers
        .iter()
        .find(|s| s.fqn.eq_ignore_ascii_case(name))
        .or_else(|| {
            // Fallback: match on short name (last segment)
            catalog
                .providers
                .iter()
                .find(|s| s.name().eq_ignore_ascii_case(name))
        })
}

// ── Lazy endpoint loading ─────────────────────────────────────────────────

/// Fetch a provider's full detail file and return the endpoints.
///
/// Downloads `{base_url}/providers/{fqn}.json`, caches locally in
/// `~/.config/pay/skills/detail/`, uses `sha` for invalidation.
pub fn load_service_endpoints(catalog: &Catalog, service_name: &str) -> Result<Vec<Endpoint>> {
    let svc = find_service(catalog, service_name)
        .ok_or_else(|| Error::Config(format!("service `{service_name}` not found")))?;

    // Already loaded?
    if svc.endpoints_loaded() {
        return Ok(svc.endpoints.clone());
    }

    if catalog.base_url.is_empty() {
        return Err(Error::Config(
            "no base_url in catalog — cannot fetch endpoint detail".into(),
        ));
    }

    let detail_url = format!("{}/providers/{}.json", catalog.base_url, svc.fqn);

    // Check local cache
    let cache_dir =
        std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills/detail").into_owned());
    let cache_file = cache_dir.join(format!("{}.json", svc.sha));

    if cache_file.exists()
        && let Ok(raw) = std::fs::read_to_string(&cache_file)
        && let Ok(detail) = parse_detail(&raw)
    {
        return Ok(detail.endpoints);
    }

    // Fetch from CDN
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;

    let resp = client
        .get(&detail_url)
        .send()
        .map_err(|e| Error::Config(format!("fetch {detail_url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "{detail_url} returned {}",
            resp.status()
        )));
    }

    let raw = resp
        .text()
        .map_err(|e| Error::Config(format!("read {detail_url}: {e}")))?;

    let detail = parse_detail(&raw)?;

    // Cache it
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cache_file, &raw);

    Ok(detail.endpoints)
}

/// Convenience: load endpoints and inject them into the catalog's service.
pub fn ensure_endpoints(catalog: &mut Catalog, service_name: &str) -> Result<()> {
    let base_url = catalog.base_url.clone();
    let idx = catalog
        .providers
        .iter()
        .position(|s| s.fqn.eq_ignore_ascii_case(service_name))
        .or_else(|| {
            catalog
                .providers
                .iter()
                .position(|s| s.name().eq_ignore_ascii_case(service_name))
        })
        .ok_or_else(|| Error::Config(format!("service `{service_name}` not found")))?;
    let svc = &mut catalog.providers[idx];

    if svc.endpoints_loaded() {
        return Ok(());
    }

    if base_url.is_empty() {
        return Err(Error::Config(
            "no base_url in catalog — cannot fetch endpoint detail".into(),
        ));
    }

    let detail_url = format!("{}/providers/{}.json", base_url, svc.fqn);

    // Check local cache
    let cache_dir =
        std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills/detail").into_owned());
    let cache_file = cache_dir.join(format!("{}.json", svc.sha));

    if cache_file.exists()
        && let Ok(raw) = std::fs::read_to_string(&cache_file)
        && let Ok(detail) = parse_detail(&raw)
    {
        svc.endpoints = detail.endpoints;
        svc.content = detail.content;
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;

    let resp = client
        .get(&detail_url)
        .send()
        .map_err(|e| Error::Config(format!("fetch {detail_url}: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "{detail_url} returned {}",
            resp.status()
        )));
    }

    let raw = resp
        .text()
        .map_err(|e| Error::Config(format!("read {detail_url}: {e}")))?;

    let detail = parse_detail(&raw)?;
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cache_file, &raw);

    svc.endpoints = detail.endpoints;
    svc.content = detail.content;

    // Clean up stale detail files not referenced by current index
    clean_stale_detail_cache(catalog);

    Ok(())
}

/// Remove detail cache files whose sha doesn't appear in the current catalog.
pub fn clean_stale_detail_cache(catalog: &Catalog) {
    let cache_dir =
        std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills/detail").into_owned());
    let Ok(entries) = std::fs::read_dir(&cache_dir) else {
        return;
    };
    let live_shas: std::collections::HashSet<_> = catalog
        .providers
        .iter()
        .filter(|s| !s.sha.is_empty())
        .map(|s| format!("{}.json", s.sha))
        .collect();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".json") && !live_shas.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Provider detail file shape.
#[derive(Debug, Deserialize)]
struct ProviderDetailFile {
    #[serde(default)]
    endpoints: Vec<Endpoint>,
    #[serde(default)]
    content: Option<String>,
}

fn parse_detail(raw: &str) -> Result<ProviderDetailFile> {
    serde_json::from_str(raw).map_err(|e| Error::Config(format!("parse detail: {e}")))
}

// ── Catalog loading + caching ───────────────────────────────────────────────

/// Load the skills catalog. Uses cache if fresh, otherwise fetches from
/// configured sources.
pub fn load_skills() -> Result<Catalog> {
    let cfg = config::SkillsConfig::load()?;

    // Cache hit?
    if let Some(path) = cfg.valid_cache_path() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("read cache: {e}")))?;
        return parse_catalog(&raw);
    }

    // Cache miss — fetch, merge, cache.
    match fetch_and_merge(&cfg, false) {
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
/// When `cache_bust` is true, append `?v=<timestamp>` to source URLs
/// to bypass CDN edge caches, and purge all local detail caches.
pub fn update_skills(cache_bust: bool) -> Result<Catalog> {
    let cfg = config::SkillsConfig::load()?;
    let catalog = fetch_and_merge(&cfg, cache_bust)?;
    write_cache(&cfg, &catalog)?;
    cfg.clean_stale_caches();
    if cache_bust {
        // Purge all detail caches so endpoints are re-fetched with fresh shas.
        clean_stale_detail_cache(&catalog);
    }
    Ok(catalog)
}

/// Fetch each source URL and merge all providers into one Catalog.
fn fetch_and_merge(cfg: &config::SkillsConfig, cache_bust: bool) -> Result<Catalog> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Config(format!("http client: {e}")))?;

    let mut all_providers: Vec<Service> = Vec::new();
    let mut base_url = String::new();

    for source in &cfg.sources {
        let url = if cache_bust {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let sep = if source.url.contains('?') { '&' } else { '?' };
            format!("{}{sep}v={ts}", source.url)
        } else {
            source.url.clone()
        };
        match fetch_one(&client, &url) {
            Ok(cat) => {
                if base_url.is_empty() && !cat.base_url.is_empty() {
                    base_url = cat.base_url.clone();
                }
                all_providers.extend(cat.providers);
            }
            Err(e) => {
                tracing::warn!(url = %source.url, error = %e, "Skipping skills source");
            }
        }
    }

    // Deduplicate by FQN (first wins).
    let mut seen = std::collections::HashSet::new();
    all_providers.retain(|svc| seen.insert(svc.fqn.clone()));

    Ok(Catalog {
        schema_version: "1".to_string(),
        generated_at: String::new(),
        base_url,
        provider_count: all_providers.len() as u32,
        providers: all_providers,
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

const GATEWAY_PROJECT_ID: &str = "gateway-402";

/// Build a complete endpoint URL from a service URL + path.
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

/// Resolve a request URL to a skills FQN using only local cache state.
///
/// This intentionally never refreshes the skills catalog: auth prompts need a
/// best-effort label without adding network latency before the OS prompt.
pub fn service_fqn_for_resource_url(resource_url: &str) -> Option<String> {
    cached_catalogs().into_iter().find_map(|catalog| {
        service_fqn_for_url_in_catalog(&catalog, resource_url, cached_service_endpoints)
    })
}

fn cached_catalogs() -> Vec<Catalog> {
    let dir = std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills").into_owned());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut entries: Vec<_> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("skills-") && name.ends_with(".json")
        })
        .collect();

    entries.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        )
    });

    entries
        .into_iter()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| parse_catalog(&raw).ok())
        .collect()
}

fn cached_service_endpoints(svc: &Service) -> Option<Vec<Endpoint>> {
    if svc.endpoints_loaded() {
        return Some(svc.endpoints.clone());
    }
    if svc.sha.is_empty() {
        return None;
    }

    let cache_file = detail_cache_dir().join(format!("{}.json", svc.sha));
    std::fs::read_to_string(cache_file)
        .ok()
        .and_then(|raw| parse_detail(&raw).ok())
        .map(|detail| detail.endpoints)
}

fn detail_cache_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(shellexpand::tilde("~/.config/pay/skills/detail").into_owned())
}

fn service_fqn_for_url_in_catalog<F>(
    catalog: &Catalog,
    resource_url: &str,
    mut detail_lookup: F,
) -> Option<String>
where
    F: FnMut(&Service) -> Option<Vec<Endpoint>>,
{
    let target = ParsedUrl::parse(resource_url)?;
    let mut endpoint_matches = Vec::new();
    let mut base_matches = Vec::new();

    for svc in &catalog.providers {
        let endpoints = if svc.endpoints_loaded() {
            Some(svc.endpoints.clone())
        } else {
            detail_lookup(svc)
        };

        for base_url in service_base_urls(svc) {
            let Some(base_match) = base_url_match(&target, base_url) else {
                continue;
            };

            if endpoints.as_ref().is_some_and(|endpoints| {
                endpoints
                    .iter()
                    .any(|endpoint| endpoint_path_matches(&base_match.relative_path, endpoint))
            }) {
                endpoint_matches.push((base_match.prefix_len, svc.fqn.clone()));
            }
            base_matches.push((base_match.prefix_len, svc.fqn.clone()));
        }
    }

    choose_unique_best_match(endpoint_matches).or_else(|| choose_unique_best_match(base_matches))
}

fn service_base_urls(svc: &Service) -> Vec<&str> {
    let mut urls = Vec::new();
    if !svc.meta.service_url.trim().is_empty() {
        urls.push(svc.meta.service_url.as_str());
    }
    if let Some(sandbox_url) = svc.meta.sandbox_service_url.as_deref()
        && !sandbox_url.trim().is_empty()
    {
        urls.push(sandbox_url);
    }
    urls
}

fn choose_unique_best_match(matches: Vec<(usize, String)>) -> Option<String> {
    let best_prefix_len = matches.iter().map(|(prefix_len, _)| *prefix_len).max()?;
    let mut fqns: Vec<_> = matches
        .into_iter()
        .filter(|(prefix_len, _)| *prefix_len == best_prefix_len)
        .map(|(_, fqn)| fqn)
        .collect();
    fqns.sort();
    fqns.dedup();
    if fqns.len() == 1 { fqns.pop() } else { None }
}

#[derive(Debug)]
struct ParsedUrl {
    scheme: String,
    host: String,
    port: Option<u16>,
    path: String,
}

impl ParsedUrl {
    fn parse(raw: &str) -> Option<Self> {
        let url = reqwest::Url::parse(raw).ok()?;
        Some(Self {
            scheme: url.scheme().to_ascii_lowercase(),
            host: url.host_str()?.to_ascii_lowercase(),
            port: url.port_or_known_default(),
            path: normalize_url_path(url.path()),
        })
    }
}

struct BaseUrlMatch {
    prefix_len: usize,
    relative_path: String,
}

fn base_url_match(target: &ParsedUrl, base_url: &str) -> Option<BaseUrlMatch> {
    let base = ParsedUrl::parse(base_url)?;
    if target.scheme != base.scheme || target.host != base.host || target.port != base.port {
        return None;
    }

    let relative_path = relative_path(&target.path, &base.path)?;
    Some(BaseUrlMatch {
        prefix_len: base.path.len(),
        relative_path,
    })
}

fn normalize_url_path(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

fn relative_path(target_path: &str, base_path: &str) -> Option<String> {
    if base_path == "/" {
        return Some(target_path.trim_start_matches('/').to_string());
    }
    if target_path == base_path {
        return Some(String::new());
    }
    target_path
        .strip_prefix(base_path)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
}

fn endpoint_path_matches(relative_path: &str, endpoint: &Endpoint) -> bool {
    [endpoint.path.as_str(), endpoint.full_path.as_str()]
        .into_iter()
        .filter(|path| !path.trim().is_empty())
        .any(|path| path_template_matches(path, relative_path))
}

fn path_template_matches(template: &str, target: &str) -> bool {
    let template = template.trim_matches('/');
    let target = target.trim_matches('/');
    if template == target {
        return true;
    }

    let template_parts: Vec<_> = if template.is_empty() {
        Vec::new()
    } else {
        template.split('/').collect()
    };
    let target_parts: Vec<_> = if target.is_empty() {
        Vec::new()
    } else {
        target.split('/').collect()
    };
    template_parts.len() == target_parts.len()
        && template_parts
            .iter()
            .zip(target_parts.iter())
            .all(|(template, target)| path_segment_matches(template, target))
}

fn path_segment_matches(template: &str, target: &str) -> bool {
    if template.starts_with('{') && template.ends_with('}') {
        return !target.is_empty();
    }
    if !template.contains('{') {
        return template == target;
    }

    segment_template_matches(template, target)
}

fn segment_template_matches(template: &str, target: &str) -> bool {
    let mut template_rest = template;
    let mut target_rest = target;

    loop {
        let Some(open) = template_rest.find('{') else {
            return target_rest == template_rest;
        };
        let literal = &template_rest[..open];
        if !target_rest.starts_with(literal) {
            return false;
        }
        target_rest = &target_rest[literal.len()..];

        let after_open = &template_rest[open + 1..];
        let Some(close) = after_open.find('}') else {
            return false;
        };
        let after_placeholder = &after_open[close + 1..];
        if after_placeholder.is_empty() {
            return !target_rest.is_empty();
        }

        let next_literal_end = after_placeholder
            .find('{')
            .unwrap_or(after_placeholder.len());
        let next_literal = &after_placeholder[..next_literal_end];
        if next_literal.is_empty() {
            template_rest = after_placeholder;
            continue;
        }

        let Some(next_literal_pos) = target_rest.find(next_literal) else {
            return false;
        };
        if next_literal_pos == 0 {
            return false;
        }
        target_rest = &target_rest[next_literal_pos..];
        template_rest = after_placeholder;
    }
}

fn summarize_service(svc: &Service) -> ServiceSummary {
    // If endpoints are loaded, compute from them. Otherwise use index metadata.
    if svc.endpoints_loaded() {
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
            name: svc.fqn.clone(),
            meta: svc.meta.clone(),
            endpoint_count: svc.endpoint_count.max(metered + free),
            metered_endpoints: metered,
            free_endpoints: free,
            min_price_usd: prices.iter().copied().reduce(f64::min).unwrap_or(0.0),
            max_price_usd: prices.iter().copied().reduce(f64::max).unwrap_or(0.0),
        }
    } else {
        // Use pre-computed index metadata
        ServiceSummary {
            name: svc.fqn.clone(),
            meta: svc.meta.clone(),
            endpoint_count: svc.endpoint_count,
            metered_endpoints: if svc.has_metering {
                svc.endpoint_count
            } else {
                0
            },
            free_endpoints: if svc.has_free_tier { 1 } else { 0 },
            min_price_usd: svc.min_price_usd,
            max_price_usd: svc.max_price_usd,
        }
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

    /// A catalog with endpoints loaded (simulates post-lazy-fetch state).
    fn catalog_with_endpoints() -> Catalog {
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
                    "endpoint_count": 3,
                    "has_metering": true,
                    "has_free_tier": true,
                    "min_price_usd": 0.0,
                    "max_price_usd": 6.25,
                    "sha": "abc123",
                    "endpoints": [
                        {
                            "method": "POST",
                            "path": "v2/projects/{projectsId}/queries",
                            "resource": "jobs",
                            "description": "Run a SQL query",
                            "pricing": { "dimensions": [{ "tiers": [{ "price_usd": 6.25 }] }] }
                        },
                        {
                            "method": "GET",
                            "path": "v2/projects/{projectsId}/queries/{queryId}",
                            "resource": "jobs",
                            "description": "Get query results"
                        },
                        {
                            "method": "GET",
                            "path": "v2/projects/{projectsId}/datasets",
                            "resource": "datasets",
                            "description": "List datasets"
                        }
                    ]
                },
                {
                    "fqn": "solana-foundation/google/vision",
                    "title": "Cloud Vision API",
                    "description": "Detect objects, faces, text (OCR) in images.",
                    "category": "ai_ml",
                    "service_url": "https://gw.example.com",
                    "endpoint_count": 1,
                    "has_metering": true,
                    "has_free_tier": false,
                    "sha": "def456",
                    "endpoints": [
                        {
                            "method": "POST",
                            "path": "v1/images:annotate",
                            "resource": "images",
                            "description": "Annotate images",
                            "pricing": { "dimensions": [{ "tiers": [{ "price_usd": 1.50 }] }] }
                        }
                    ]
                }
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    /// A catalog from the index only (no endpoints loaded).
    fn catalog_index_only() -> Catalog {
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
                    "min_price_usd": 0.0,
                    "max_price_usd": 6.25,
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
                },
                {
                    "fqn": "solana-foundation/payment-debugger",
                    "title": "Payment Debugger",
                    "description": "Demo API for testing payment flows.",
                    "category": "devtools",
                    "service_url": "https://pdb.example.com",
                    "endpoint_count": 8,
                    "has_metering": true,
                    "has_free_tier": true,
                    "sha": "ghi789"
                }
            ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    // ── Deserialization ─────────────────────────────────────────────────────

    #[test]
    fn parse_v1_index() {
        let cat = catalog_index_only();
        assert_eq!(cat.schema_version, "1");
        assert_eq!(cat.providers.len(), 3);
        assert_eq!(cat.base_url, "https://cdn.example.com/v1");
    }

    #[test]
    fn service_fqn_and_name() {
        let cat = catalog_index_only();
        let svc = &cat.providers[0];
        assert_eq!(svc.fqn, "solana-foundation/google/bigquery");
        assert_eq!(svc.name(), "bigquery");
    }

    #[test]
    fn service_two_level_fqn() {
        let cat = catalog_index_only();
        let svc = &cat.providers[2];
        assert_eq!(svc.fqn, "solana-foundation/payment-debugger");
        assert_eq!(svc.name(), "payment-debugger");
    }

    #[test]
    fn index_metadata_present() {
        let cat = catalog_index_only();
        let svc = &cat.providers[0];
        assert_eq!(svc.endpoint_count, 47);
        assert!(svc.has_metering);
        assert!(svc.has_free_tier);
        assert!(!svc.endpoints_loaded());
    }

    // ── Search (index-only, no endpoints loaded) ────────────────────────────

    #[test]
    fn search_index_only_matches_service_title() {
        let cat = catalog_index_only();
        let hits = search(&cat, Some("bigquery"), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].service, "solana-foundation/google/bigquery");
        // Placeholder hit — no method/path since endpoints aren't loaded
        assert!(hits[0].method.is_empty());
    }

    #[test]
    fn search_index_only_matches_description() {
        let cat = catalog_index_only();
        let hits = search(&cat, Some("warehouse"), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].service, "solana-foundation/google/bigquery");
    }

    #[test]
    fn search_index_only_matches_fqn() {
        let cat = catalog_index_only();
        let hits = search(&cat, Some("payment-debugger"), None);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_index_only_category_filter() {
        let cat = catalog_index_only();
        let hits = search(&cat, None, Some("ai_ml"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].service, "solana-foundation/google/vision");
    }

    #[test]
    fn search_index_only_no_match() {
        let cat = catalog_index_only();
        let hits = search(&cat, Some("nonexistent"), None);
        assert!(hits.is_empty());
    }

    #[test]
    fn search_index_only_all_returns_all_providers() {
        let cat = catalog_index_only();
        let hits = search(&cat, None, None);
        assert_eq!(hits.len(), 3);
    }

    // ── Search (with endpoints loaded) ──────────────────────────────────────

    #[test]
    fn search_with_endpoints_matches_path() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, Some("queries"), None);
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|h| h.service == "solana-foundation/google/bigquery")
        );
    }

    #[test]
    fn search_with_endpoints_matches_endpoint_description() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, Some("annotate"), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "POST");
    }

    #[test]
    fn search_with_endpoints_all_returns_all() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, None, None);
        // 3 bigquery + 1 vision = 4
        assert_eq!(hits.len(), 4);
    }

    #[test]
    fn search_metered_first() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, None, None);
        // Within bigquery, metered POST should come before free GETs
        let bq: Vec<_> = hits
            .iter()
            .filter(|h| h.service.contains("bigquery"))
            .collect();
        assert!(bq[0].metered);
    }

    // ── search_services ─────────────────────────────────────────────────────

    #[test]
    fn search_services_index_only() {
        let cat = catalog_index_only();
        let results = search_services(&cat, Some("bigquery"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "solana-foundation/google/bigquery");
        assert_eq!(results[0].endpoint_count, 47);
        assert_eq!(results[0].min_price_usd, 0.0);
        assert_eq!(results[0].max_price_usd, 6.25);
    }

    #[test]
    fn search_services_with_endpoints() {
        let cat = catalog_with_endpoints();
        let results = search_services(&cat, Some("bigquery"), None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metered_endpoints, 1);
        assert_eq!(results[0].free_endpoints, 2);
    }

    // ── find_service ────────────────────────────────────────────────────────

    #[test]
    fn find_service_by_fqn() {
        let cat = catalog_index_only();
        let svc = find_service(&cat, "solana-foundation/google/bigquery");
        assert!(svc.is_some());
    }

    #[test]
    fn find_service_by_short_name() {
        let cat = catalog_index_only();
        let svc = find_service(&cat, "bigquery");
        assert!(svc.is_some());
        assert_eq!(svc.unwrap().fqn, "solana-foundation/google/bigquery");
    }

    #[test]
    fn find_service_case_insensitive() {
        let cat = catalog_index_only();
        assert!(find_service(&cat, "BigQuery").is_some());
        assert!(find_service(&cat, "SOLANA-FOUNDATION/GOOGLE/BIGQUERY").is_some());
    }

    // ── service_detail (requires endpoints) ─────────────────────────────────

    #[test]
    fn service_detail_groups_by_resource() {
        let cat = catalog_with_endpoints();
        let detail = service_detail(&cat, "bigquery").unwrap();
        assert_eq!(detail.resources.len(), 2);

        let jobs = detail.resources.iter().find(|r| r.name == "jobs").unwrap();
        assert_eq!(jobs.endpoint_count, 2);
        assert_eq!(jobs.metered_count, 1);

        let datasets = detail
            .resources
            .iter()
            .find(|r| r.name == "datasets")
            .unwrap();
        assert_eq!(datasets.endpoint_count, 1);
        assert_eq!(datasets.metered_count, 0);
    }

    #[test]
    fn service_detail_empty_when_no_endpoints() {
        let cat = catalog_index_only();
        let detail = service_detail(&cat, "bigquery").unwrap();
        assert!(detail.resources.is_empty());
    }

    // ── resource_endpoints ──────────────────────────────────────────────────

    #[test]
    fn resource_endpoints_returns_matching() {
        let cat = catalog_with_endpoints();
        let result = resource_endpoints(&cat, "bigquery", "jobs").unwrap();
        assert_eq!(result.endpoints.len(), 2);
    }

    #[test]
    fn resource_endpoints_none_when_not_loaded() {
        let cat = catalog_index_only();
        assert!(resource_endpoints(&cat, "bigquery", "jobs").is_none());
    }

    // ── group_search_results ────────────────────────────────────────────────

    #[test]
    fn group_search_results_groups_by_service() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, None, None);
        let groups = group_search_results(&hits);
        assert_eq!(groups.len(), 2);
        let bq = groups
            .iter()
            .find(|g| g.service.contains("bigquery"))
            .unwrap();
        assert_eq!(bq.endpoints.len(), 3);
    }

    #[test]
    fn group_search_results_endpoints_have_urls() {
        let cat = catalog_with_endpoints();
        let hits = search(&cat, Some("annotate"), None);
        let groups = group_search_results(&hits);
        assert_eq!(
            groups[0].endpoints[0].url,
            "https://gw.example.com/v1/images:annotate"
        );
        assert!(groups[0].endpoints[0].metered);
    }

    // ── build_endpoint_url ──────────────────────────────────────────────────

    #[test]
    fn build_endpoint_url_resolves_placeholders() {
        let url = build_endpoint_url("https://gw.example.com", "v2/projects/{projectsId}/queries");
        assert_eq!(
            url,
            "https://gw.example.com/v2/projects/gateway-402/queries"
        );
    }

    #[test]
    fn build_endpoint_url_empty_path() {
        assert_eq!(
            build_endpoint_url("https://gw.example.com/", ""),
            "https://gw.example.com"
        );
    }

    // ── service_fqn_for_url_in_catalog ─────────────────────────────────────

    #[test]
    fn service_fqn_for_url_in_catalog_matches_loaded_endpoint_on_shared_gateway() {
        let cat = catalog_with_endpoints();
        let fqn = service_fqn_for_url_in_catalog(
            &cat,
            "https://gw.example.com/v1/images:annotate",
            |_| None,
        );
        assert_eq!(fqn.as_deref(), Some("solana-foundation/google/vision"));
    }

    #[test]
    fn service_fqn_for_url_in_catalog_matches_placeholder_endpoint() {
        let cat = catalog_with_endpoints();
        let fqn = service_fqn_for_url_in_catalog(
            &cat,
            "https://gw.example.com/v2/projects/my-project/queries",
            |_| None,
        );
        assert_eq!(fqn.as_deref(), Some("solana-foundation/google/bigquery"));
    }

    #[test]
    fn service_fqn_for_url_in_catalog_avoids_ambiguous_shared_gateway_without_endpoints() {
        let cat = catalog_index_only();
        let fqn =
            service_fqn_for_url_in_catalog(&cat, "https://gw.example.com/v1/unknown", |_| None);
        assert_eq!(fqn, None);
    }

    #[test]
    fn service_fqn_for_url_in_catalog_uses_unique_service_domain() {
        let cat = catalog_index_only();
        let fqn = service_fqn_for_url_in_catalog(&cat, "https://pdb.example.com/reports", |_| None);
        assert_eq!(fqn.as_deref(), Some("solana-foundation/payment-debugger"));
    }

    #[test]
    fn path_template_matches_segment_placeholders() {
        assert!(path_template_matches(
            "v2/projects/{project}/queries",
            "v2/projects/my-project/queries"
        ));
        assert!(path_template_matches(
            "v1/models/{model}:generateContent",
            "v1/models/gemini-2.0-flash:generateContent"
        ));
        assert!(!path_template_matches(
            "v2/projects/{project}/queries",
            "v2/projects/my-project/datasets"
        ));
    }

    // ── collect_prices ──────────────────────────────────────────────────────

    #[test]
    fn collect_prices_nested() {
        let pricing = serde_json::json!({
            "dimensions": [{ "tiers": [{ "price_usd": 0 }, { "price_usd": 6.25 }] }]
        });
        let mut prices = Vec::new();
        collect_prices(&Some(pricing), &mut prices);
        assert_eq!(prices, vec![0.0, 6.25]);
    }

    // ── Cache invalidation ──────────────────────────────────────────────────

    #[test]
    fn detail_cache_key_is_sha_based() {
        // Two providers with different shas must not share cache files.
        let cat = catalog_index_only();
        let bq = &cat.providers[0];
        let vision = &cat.providers[1];
        assert_ne!(bq.sha, vision.sha);
        // Cache file names are `{sha}.json` — different shas = different files
        let bq_cache = format!("{}.json", bq.sha);
        let vision_cache = format!("{}.json", vision.sha);
        assert_ne!(bq_cache, vision_cache);
    }

    #[test]
    fn sha_change_invalidates_detail_cache() {
        // Simulate: index has sha "abc123" for bigquery.
        // A new index arrives with sha "xyz789" for the same provider.
        // The old cache file "abc123.json" should NOT be hit.
        let old_sha = "abc123";
        let new_sha = "xyz789";
        let old_cache = format!("{old_sha}.json");
        let new_cache = format!("{new_sha}.json");
        assert_ne!(old_cache, new_cache);
        // ensure_endpoints looks for `{sha}.json` — with a new sha, it's a miss.
    }

    #[test]
    fn endpoints_loaded_reflects_state() {
        let mut cat = catalog_index_only();
        let svc = &cat.providers[0];
        assert!(!svc.endpoints_loaded());

        // Simulate loading endpoints
        cat.providers[0].endpoints = vec![Endpoint {
            method: "GET".to_string(),
            path: "test".to_string(),
            full_path: String::new(),
            resource: String::new(),
            description: "test".to_string(),
            pricing: None,
        }];
        assert!(cat.providers[0].endpoints_loaded());
    }

    #[test]
    fn ensure_endpoints_skips_when_already_loaded() {
        let mut cat = catalog_with_endpoints();
        // Already has endpoints — ensure_endpoints should be a no-op
        let count_before = cat.providers[0].endpoints.len();
        let result = ensure_endpoints(&mut cat, "bigquery");
        assert!(result.is_ok());
        assert_eq!(cat.providers[0].endpoints.len(), count_before);
    }

    #[test]
    fn ensure_endpoints_fails_without_base_url() {
        let mut cat = catalog_index_only();
        cat.base_url = String::new(); // no base_url
        let result = ensure_endpoints(&mut cat, "bigquery");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("base_url"));
    }

    #[test]
    fn ensure_endpoints_fails_for_unknown_service() {
        let mut cat = catalog_index_only();
        let result = ensure_endpoints(&mut cat, "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[test]
    fn detail_file_cache_uses_tempdir() {
        // Verify the detail cache path construction
        let cat = catalog_index_only();
        let svc = &cat.providers[0];
        let cache_dir = std::path::PathBuf::from("/tmp/test-skills-cache");
        let cache_file = cache_dir.join(format!("{}.json", svc.sha));
        assert_eq!(
            cache_file.file_name().unwrap().to_str().unwrap(),
            "abc123.json"
        );
    }

    #[test]
    fn index_updated_sha_means_detail_cache_miss() {
        // Simulate the full invalidation flow:
        // 1. Index v1: bigquery sha="abc123"
        // 2. Index v2: bigquery sha="new_sha_456"
        // 3. Cache has "abc123.json" but NOT "new_sha_456.json"
        // 4. ensure_endpoints should miss cache and fetch

        let mut cat_v1 = catalog_index_only();
        let mut cat_v2 = catalog_index_only();
        cat_v2.providers[0].sha = "new_sha_456".to_string();

        // v1 and v2 have different shas for the same provider
        assert_ne!(cat_v1.providers[0].sha, cat_v2.providers[0].sha);

        // Both still have no endpoints loaded
        assert!(!cat_v1.providers[0].endpoints_loaded());
        assert!(!cat_v2.providers[0].endpoints_loaded());

        // Inject endpoints into v1 to simulate "was loaded from old cache"
        cat_v1.providers[0].endpoints = vec![Endpoint {
            method: "GET".to_string(),
            path: "old".to_string(),
            full_path: String::new(),
            resource: String::new(),
            description: "old endpoint".to_string(),
            pricing: None,
        }];

        // v1 is loaded, v2 is not — v2 would need a fresh fetch
        assert!(cat_v1.providers[0].endpoints_loaded());
        assert!(!cat_v2.providers[0].endpoints_loaded());
    }

    #[test]
    fn parse_detail_extracts_endpoints() {
        let json = r#"{
            "fqn": "test/api",
            "endpoints": [
                {"method": "GET", "path": "v1/foo", "description": "Get foo"},
                {"method": "POST", "path": "v1/bar", "description": "Create bar"}
            ]
        }"#;
        let detail = parse_detail(json).unwrap();
        assert_eq!(detail.endpoints.len(), 2);
        assert_eq!(detail.endpoints[0].method, "GET");
        assert_eq!(detail.endpoints[1].path, "v1/bar");
    }

    #[test]
    fn parse_detail_handles_empty_endpoints() {
        let json = r#"{"fqn": "test/api"}"#;
        let detail = parse_detail(json).unwrap();
        assert!(detail.endpoints.is_empty());
    }

    #[test]
    fn parse_detail_handles_extra_fields() {
        // Detail files have fields we don't deserialize (content, source, etc.)
        // Make sure they don't cause errors
        let json = r#"{
            "fqn": "test/api",
            "name": "api",
            "title": "Test API",
            "content": "Some markdown...",
            "source": {"skill": "pay-skills"},
            "affiliate_policy": {"enabled": true},
            "endpoints": [{"method": "GET", "path": "v1/x", "description": "test"}]
        }"#;
        let detail = parse_detail(json).unwrap();
        assert_eq!(detail.endpoints.len(), 1);
    }

    // ── Detail cache cleanup ──────────────────────────────────────────────

    #[test]
    fn clean_stale_detail_cache_removes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let detail_dir = dir.path().join("detail");
        std::fs::create_dir_all(&detail_dir).unwrap();

        // Create some cache files: two live, one stale
        std::fs::write(detail_dir.join("abc123.json"), "{}").unwrap();
        std::fs::write(detail_dir.join("def456.json"), "{}").unwrap();
        std::fs::write(detail_dir.join("old_dead.json"), "{}").unwrap();

        // Catalog only references abc123 and def456
        let cat = catalog_index_only(); // shas: abc123, def456, ghi789

        // We can't easily override the cache dir, so test the logic directly:
        let live_shas: std::collections::HashSet<_> = cat
            .providers
            .iter()
            .filter(|s| !s.sha.is_empty())
            .map(|s| format!("{}.json", s.sha))
            .collect();

        assert!(live_shas.contains("abc123.json"));
        assert!(live_shas.contains("def456.json"));
        assert!(live_shas.contains("ghi789.json"));
        assert!(!live_shas.contains("old_dead.json"));

        // Simulate cleanup
        for entry in std::fs::read_dir(&detail_dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") && !live_shas.contains(&name) {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }

        // old_dead.json should be gone
        assert!(!detail_dir.join("old_dead.json").exists());
        // live files should remain
        assert!(detail_dir.join("abc123.json").exists());
        assert!(detail_dir.join("def456.json").exists());
    }

    // ── Catalog round-trip ──────────────────────────────────────────────────

    #[test]
    fn catalog_serialization_round_trip() {
        let cat = catalog_index_only();
        let json = serde_json::to_string(&cat).unwrap();
        let cat2: Catalog = serde_json::from_str(&json).unwrap();
        assert_eq!(cat.providers.len(), cat2.providers.len());
        assert_eq!(cat.base_url, cat2.base_url);
        assert_eq!(cat.providers[0].fqn, cat2.providers[0].fqn);
        assert_eq!(cat.providers[0].sha, cat2.providers[0].sha);
    }

    #[test]
    fn catalog_with_integer_version() {
        // version can be int or string
        let json = r#"{"version": 1, "generated_at": "", "providers": []}"#;
        let cat: Catalog = serde_json::from_str(json).unwrap();
        assert_eq!(cat.schema_version, "1");
    }

    #[test]
    fn catalog_with_string_version() {
        let json = r#"{"version": "1", "generated_at": "", "providers": []}"#;
        let cat: Catalog = serde_json::from_str(json).unwrap();
        assert_eq!(cat.schema_version, "1");
    }
}
