//! Build the pay-skills index from a registry directory.
//!
//! Reads `.md` files with YAML frontmatter from `providers/`, `affiliates/`,
//! and `aggregators/` directories. Produces:
//!
//! - `dist/skills.json` — lightweight index for search
//! - `dist/providers/<org>/<name>.json` — per-provider detail files

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

// Re-export types from pay-types so callers can use `pay_core::skills::build::*`.
pub use pay_types::registry::{
    AffiliateFrontmatter, AffiliatePolicy, AggregatorFrontmatter, EndpointSpec, KNOWN_CATEGORIES,
    ProviderFrontmatter, validate_affiliate, validate_provider,
};

// ── Output types ───────────────────────────────────────────────────────────

/// The top-level `skills.json` index.
#[derive(Debug, Serialize)]
pub struct SkillsIndex {
    pub version: u32,
    pub generated_at: String,
    pub base_url: String,
    pub provider_count: usize,
    pub affiliate_count: usize,
    pub aggregator_count: usize,
    pub providers: Vec<ProviderIndexEntry>,
    pub affiliates: Vec<AffiliateEntry>,
    pub aggregators: Vec<AggregatorEntry>,
}

/// Lightweight provider entry in the index — enough for search, no endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderIndexEntry {
    pub fqn: String,
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
    pub endpoint_count: usize,
    pub has_metering: bool,
    pub has_free_tier: bool,
    pub min_price_usd: f64,
    pub max_price_usd: f64,
    pub sha: String,
}

/// Full provider detail — written to `dist/providers/<fqn>.json`.
#[derive(Debug, Serialize)]
pub struct ProviderDetail {
    pub fqn: String,
    pub name: String,
    /// The operator/aggregator serving this API (top-level dir under providers/).
    pub operator: String,
    /// The origin org whose API is being proxied. Same as operator for native APIs.
    pub origin: String,
    #[serde(flatten)]
    pub meta: pay_types::registry::ServiceMeta,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openapi: Option<pay_types::registry::OpenapiSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affiliate_policy: Option<AffiliatePolicy>,
    pub source: ProviderSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The full OpenAPI / Discovery document, inlined at build time when the
    /// spec declares `openapi: { url: ... }`. Lets consumers get the
    /// upstream schema/types/components after `pay skills update` without a
    /// follow-up HTTP round-trip. `None` when the spec uses inline
    /// `endpoints:` or the build couldn't parse the doc as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openapi_doc: Option<serde_json::Value>,
    pub endpoints: Vec<DetailEndpoint>,
}

/// A published endpoint — the spec fields plus probe-derived metadata.
///
/// Probed metadata (`protocol`, `supported_usd`, `probe_status`,
/// `probe_description`) is empty when the build was run with probing
/// disabled. Pricing comes from the probe when available, falling back to
/// the spec's inline `pricing` field for offline/no-probe builds.
#[derive(Debug, Clone, Serialize)]
pub struct DetailEndpoint {
    #[serde(flatten)]
    pub spec: EndpointSpec,
    /// Solana protocols this endpoint accepts (e.g. `["mpp", "x402"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocol: Vec<String>,
    /// USD-pegged stablecoin symbols accepted on Solana.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_usd: Vec<String>,
    /// Stable label for the probe outcome (`"ok"`, `"auth_required"`, etc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_status: Option<String>,
    /// Endpoint description sourced from the 402 challenge body, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderSource {
    pub skill: String,
    pub repo: String,
    pub path: String,
}

/// Affiliate in the index (inline — they're small).
#[derive(Debug, Serialize)]
pub struct AffiliateEntry {
    pub name: String,
    pub title: String,
    #[serde(rename = "type")]
    pub affiliate_type: String,
    pub account: String,
    pub network: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub contact: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Aggregator in the index (inline — they're small).
#[derive(Debug, Serialize)]
pub struct AggregatorEntry {
    pub name: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_url: Option<String>,
    pub contact: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

// ── Build result ───────────────────────────────────────────────────────────

pub struct BuildResult {
    pub index: SkillsIndex,
    /// Map of `"providers/<org>/<name>.json"` → serialized JSON.
    pub detail_files: HashMap<String, String>,
    pub errors: Vec<String>,
}

/// Options controlling a build run.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Probe each endpoint to derive pricing, accepted protocols, and the
    /// stablecoin set. Disable for fast offline builds and unit tests; CI
    /// should always leave this on.
    pub probe: bool,
    /// Probe configuration when `probe == true`.
    pub probe_config: crate::skills::probe::ProbeConfig,
    /// When `Some`, only the listed provider FQNs are (re)built from source;
    /// every other provider is copied verbatim from `previous_dist`. Used to
    /// turn a full merge-time rebuild into a fast partial rebuild.
    pub only: Option<HashSet<String>>,
    /// Path to a previously-built `dist/` directory. Required when `only` is
    /// `Some` so unchanged providers can be copied through without probing.
    pub previous_dist: Option<PathBuf>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            probe: true,
            probe_config: crate::skills::probe::ProbeConfig::default(),
            only: None,
            previous_dist: None,
        }
    }
}

// ── Parsing ────────────────────────────────────────────────────────────────

/// Split a markdown file into YAML frontmatter and body content.
pub fn parse_frontmatter(text: &str) -> Result<(String, String)> {
    if !text.starts_with("---") {
        return Ok((String::new(), text.trim().to_string()));
    }

    let rest = &text[3..];
    let end = rest
        .find("\n---")
        .ok_or_else(|| Error::Config("unterminated frontmatter (missing closing ---)".into()))?;

    let yaml = rest[..end].trim().to_string();
    let content = rest[end + 4..].trim().to_string();
    Ok((yaml, content))
}

// ── Price helpers ──────────────────────────────────────────────────────────

fn collect_prices(value: &serde_json::Value) -> Vec<f64> {
    let mut prices = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            if let Some(p) = map.get("price_usd").and_then(|v| v.as_f64()) {
                prices.push(p);
            }
            for v in map.values() {
                prices.extend(collect_prices(v));
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                prices.extend(collect_prices(v));
            }
        }
        _ => {}
    }
    prices
}

// ── Content hash ───────────────────────────────────────────────────────────

fn content_sha(json: &str) -> String {
    let mut hasher = DefaultHasher::new();
    json.hash(&mut hasher);
    format!("{:012x}", hasher.finish())
}

// ── Collectors ─────────────────────────────────────────────────────────────

/// Container for the previous build artifacts indexed by FQN, used to copy
/// unchanged providers through during a partial rebuild.
#[allow(dead_code)]
struct PreviousDist {
    entries: HashMap<String, ProviderIndexEntry>,
    detail_json: HashMap<String, String>,
}

/// Read the previous `dist/` directory: skills.json (for index entries) and
/// every `providers/**.json` file (for the detail bodies).
fn load_previous_dist(dir: &Path) -> Result<PreviousDist> {
    #[derive(Deserialize)]
    struct PartialIndex {
        providers: Vec<ProviderIndexEntry>,
    }
    let index_path = dir.join("skills.json");
    let raw = fs::read_to_string(&index_path)
        .map_err(|e| Error::Config(format!("previous dist: read {}: {e}", index_path.display())))?;
    let parsed: PartialIndex = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("previous dist: skills.json parse error: {e}")))?;
    let entries: HashMap<String, ProviderIndexEntry> = parsed
        .providers
        .into_iter()
        .map(|p| (p.fqn.clone(), p))
        .collect();

    let providers_root = dir.join("providers");
    let mut detail_json = HashMap::new();
    if providers_root.is_dir() {
        collect_detail_files(&providers_root, &providers_root, &mut detail_json)?;
    }

    Ok(PreviousDist {
        entries,
        detail_json,
    })
}

fn collect_detail_files(dir: &Path, root: &Path, out: &mut HashMap<String, String>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|e| Error::Config(format!("read previous {}: {e}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_detail_files(&path, root, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let fqn = rel
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/")
                .to_string();
            let json = fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("read previous {}: {e}", path.display())))?;
            out.insert(fqn, json);
        }
    }
    Ok(())
}

fn collect_providers(
    root: &Path,
    options: &BuildOptions,
    errors: &mut Vec<String>,
) -> Vec<(ProviderIndexEntry, String)> {
    let mut results = Vec::new();
    let dir = root.join("providers");
    if !dir.is_dir() {
        return results;
    }

    // Partial-build mode — load the previous dist so we can copy unchanged
    // providers through without touching the network.
    #[allow(unused_variables)]
    let previous = if options.only.is_some() {
        match options.previous_dist.as_deref() {
            Some(prev_dir) => match load_previous_dist(prev_dir) {
                Ok(p) => Some(p),
                Err(e) => {
                    errors.push(format!("previous dist load failed: {e}"));
                    return results;
                }
            },
            None => {
                errors
                    .push("--only requires --previous-dist (path to a prior build's dist/)".into());
                return results;
            }
        }
    } else {
        None
    };

    // Walk: every directory under providers/ that contains a `PAY.md` is a
    // provider. The FQN is the path from `providers/` to that directory:
    //   providers/<op>/<name>/PAY.md             → FQN: op/name
    //   providers/<op>/<origin>/<name>/PAY.md    → FQN: op/origin/name
    walk_for_pay_md(
        &dir,
        &dir,
        root,
        options,
        previous.as_ref(),
        errors,
        &mut results,
    );

    results
}

/// Recurse through `providers/` looking for directories that contain a
/// `PAY.md`. Each such directory is one provider; deeper directories under
/// it are not searched.
#[allow(clippy::too_many_arguments)]
fn walk_for_pay_md(
    dir: &Path,
    providers_root: &Path,
    root: &Path,
    options: &BuildOptions,
    previous: Option<&PreviousDist>,
    errors: &mut Vec<String>,
    results: &mut Vec<(ProviderIndexEntry, String)>,
) {
    let pay_md = dir.join("PAY.md");
    if pay_md.is_file() {
        let rel = dir.strip_prefix(providers_root).unwrap_or(dir);
        let fqn = rel.to_string_lossy().replace('\\', "/");
        let segments: Vec<&str> = fqn.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            errors.push(format!(
                "{}: PAY.md cannot live directly under providers/ — \
                 wrap it in an operator/name directory",
                pay_md.display()
            ));
            return;
        }
        let operator = segments[0].to_string();
        let name = segments[segments.len() - 1].to_string();
        let origin = if segments.len() >= 3 {
            segments[segments.len() - 2].to_string()
        } else {
            operator.clone()
        };
        dispatch_provider(
            &pay_md, &fqn, &name, &operator, &origin, root, options, previous, errors, results,
        );
        return;
    }

    for entry in sorted_subdirs(dir) {
        walk_for_pay_md(
            &entry,
            providers_root,
            root,
            options,
            previous,
            errors,
            results,
        );
    }
}

/// Decide whether to rebuild a provider from source (full
/// `process_provider_md`) or copy the previous dist's entry through. Honors
/// `options.only` — when set, FQNs not in the set are copied; FQNs in the
/// set are rebuilt.
#[allow(clippy::too_many_arguments)]
fn dispatch_provider(
    path: &Path,
    fqn: &str,
    name: &str,
    operator: &str,
    origin: &str,
    root: &Path,
    options: &BuildOptions,
    previous: Option<&PreviousDist>,
    errors: &mut Vec<String>,
    results: &mut Vec<(ProviderIndexEntry, String)>,
) {
    if let Some(only) = &options.only
        && !only.contains(fqn)
    {
        // Copy through from previous dist.
        if let Some(prev) = previous {
            match (prev.entries.get(fqn), prev.detail_json.get(fqn)) {
                (Some(entry), Some(json)) => {
                    eprintln!("  provider: {fqn} (copied)");
                    results.push((entry.clone(), json.clone()));
                }
                _ => {
                    errors.push(format!(
                        "{fqn}: not in --only list and missing from previous dist"
                    ));
                }
            }
        } else {
            errors.push(format!(
                "{fqn}: not in --only list and no previous dist provided"
            ));
        }
        return;
    }

    eprintln!("  provider: {fqn}");
    process_provider_md(
        path, fqn, name, operator, origin, root, options, errors, results,
    );
}

fn sorted_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

#[allow(clippy::too_many_arguments)]
fn process_provider_md(
    path: &Path,
    fqn: &str,
    name: &str,
    operator: &str,
    origin: &str,
    root: &Path,
    options: &BuildOptions,
    errors: &mut Vec<String>,
    results: &mut Vec<(ProviderIndexEntry, String)>,
) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{fqn}: read error: {e}"));
            return;
        }
    };

    let (yaml_str, content) = match parse_frontmatter(&text) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("{fqn}: {e}"));
            return;
        }
    };

    let spec: ProviderFrontmatter = match serde_yml::from_str(&yaml_str) {
        Ok(s) => s,
        Err(e) => {
            errors.push(format!("{fqn}: frontmatter parse error: {e}"));
            return;
        }
    };

    if spec.name != name {
        errors.push(format!(
            "{fqn}: name=`{}` but parent directory is `{name}`",
            spec.name
        ));
        return;
    }

    let errs = validate_provider(&spec, fqn);
    if !errs.is_empty() {
        errors.extend(errs);
        return;
    }

    // Resolve openapi: fetch the source doc (when set), parse it, synthesize
    // the endpoint list, and keep the parsed document for inlining into the
    // published detail JSON. Specs with inline `endpoints:` skip the fetch
    // and just wrap each spec entry as a body-less `ResolvedEndpoint`.
    // `openapi.path` is resolved relative to the .md file's parent directory.
    let spec_dir = path.parent();
    let resolved = match crate::skills::openapi::effective_openapi_relative_to(&spec, spec_dir) {
        Ok(r) => r,
        Err(e) => {
            errors.push(format!("{fqn}: openapi resolve failed: {e}"));
            return;
        }
    };
    let openapi_doc = resolved.document;

    // Probe each endpoint (when probing is on) and synthesize the rich
    // `DetailEndpoint` shape: probe-derived pricing wins over any inline
    // pricing in the spec, with the spec value as fallback for offline builds.
    let detail_endpoints =
        build_detail_endpoints(fqn, &spec.meta.service_url, resolved.endpoints, options);

    let mut all_prices = Vec::new();
    let mut has_metering = false;
    let mut has_free_tier = false;

    for ep in &detail_endpoints {
        if let Some(ref pricing) = ep.spec.pricing {
            has_metering = true;
            all_prices.extend(collect_prices(pricing));
        } else {
            has_free_tier = true;
        }
    }

    let rel_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let detail = ProviderDetail {
        fqn: fqn.to_string(),
        name: spec.name.clone(),
        operator: operator.to_string(),
        origin: origin.to_string(),
        meta: spec.meta.clone(),
        version: spec.version.clone(),
        openapi: spec.openapi.clone(),
        affiliate_policy: spec.affiliate_policy.clone(),
        source: ProviderSource {
            skill: "pay-skills".into(),
            repo: "solana-foundation/pay-skills".into(),
            path: rel_path,
        },
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        openapi_doc,
        endpoints: detail_endpoints,
    };

    let detail_json = serde_json::to_string_pretty(&detail).expect("detail serialization failed");
    let sha = content_sha(&detail_json);

    let index_entry = ProviderIndexEntry {
        fqn: fqn.to_string(),
        meta: spec.meta,
        endpoint_count: detail.endpoints.len(),
        has_metering,
        has_free_tier,
        min_price_usd: all_prices.iter().copied().reduce(f64::min).unwrap_or(0.0),
        max_price_usd: all_prices.iter().copied().reduce(f64::max).unwrap_or(0.0),
        sha,
    };

    let _ = detail; // detail is owned by detail_json now
    results.push((index_entry, detail_json));
}

/// Build a single provider from a `.md` (or `PAY.md`) file. The FQN, operator,
/// origin, and name are passed in by the caller because a standalone file has
/// no surrounding `providers/` tree to derive them from. Used by
/// `pay catalog build --file <PATH>` to validate one provider in isolation.
pub fn build_single_provider(
    path: &Path,
    fqn: &str,
    name: &str,
    operator: &str,
    origin: &str,
    options: &BuildOptions,
) -> BuildResult {
    let mut errors = Vec::new();
    let mut results = Vec::new();
    let root = path.parent().unwrap_or(Path::new("."));

    process_provider_md(
        path,
        fqn,
        name,
        operator,
        origin,
        root,
        options,
        &mut errors,
        &mut results,
    );

    let providers: Vec<(ProviderIndexEntry, String)> = results;
    let mut detail_files = HashMap::new();
    for (entry, json) in &providers {
        detail_files.insert(format!("providers/{}.json", entry.fqn), json.clone());
    }

    let providers_index: Vec<ProviderIndexEntry> = providers.into_iter().map(|(e, _)| e).collect();
    let provider_count = providers_index.len();
    let index = SkillsIndex {
        version: 1,
        generated_at: String::new(),
        base_url: String::new(),
        provider_count,
        affiliate_count: 0,
        aggregator_count: 0,
        providers: providers_index,
        affiliates: Vec::new(),
        aggregators: Vec::new(),
    };

    BuildResult {
        index,
        detail_files,
        errors,
    }
}

fn collect_affiliates(root: &Path, errors: &mut Vec<String>) -> Vec<AffiliateEntry> {
    let mut entries = Vec::new();
    let dir = root.join("affiliates");
    if !dir.is_dir() {
        return entries;
    }

    let Ok(files) = fs::read_dir(&dir) else {
        return entries;
    };
    let mut file_entries: Vec<_> = files.filter_map(|e| e.ok()).collect();
    file_entries.sort_by_key(|e| e.file_name());

    for file_entry in file_entries {
        let path = file_entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        eprintln!("  affiliate: {name}");

        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("affiliate/{name}: read error: {e}"));
                continue;
            }
        };

        let (yaml_str, content) = match parse_frontmatter(&text) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("affiliate/{name}: {e}"));
                continue;
            }
        };

        let spec: AffiliateFrontmatter = match serde_yml::from_str(&yaml_str) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("affiliate/{name}: frontmatter parse error: {e}"));
                continue;
            }
        };

        if spec.name != name {
            errors.push(format!(
                "affiliate/{name}: name=`{}` but filename is `{name}`",
                spec.name
            ));
            continue;
        }

        let errs = validate_affiliate(&spec, &name);
        if !errs.is_empty() {
            errors.extend(errs);
            continue;
        }

        entries.push(AffiliateEntry {
            name: spec.name,
            title: spec.title,
            affiliate_type: spec.affiliate_type,
            account: spec.account,
            network: spec.network,
            url: spec.url,
            contact: spec.contact,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
        });
    }

    entries
}

fn collect_aggregators(root: &Path, errors: &mut Vec<String>) -> Vec<AggregatorEntry> {
    let mut entries = Vec::new();
    let dir = root.join("aggregators");
    if !dir.is_dir() {
        return entries;
    }

    let Ok(files) = fs::read_dir(&dir) else {
        return entries;
    };
    let mut file_entries: Vec<_> = files.filter_map(|e| e.ok()).collect();
    file_entries.sort_by_key(|e| e.file_name());

    for file_entry in file_entries {
        let path = file_entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();

        eprintln!("  aggregator: {name}");

        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("aggregator/{name}: read error: {e}"));
                continue;
            }
        };

        let (yaml_str, content) = match parse_frontmatter(&text) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("aggregator/{name}: {e}"));
                continue;
            }
        };

        let spec: AggregatorFrontmatter = match serde_yml::from_str(&yaml_str) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("aggregator/{name}: frontmatter parse error: {e}"));
                continue;
            }
        };

        if spec.name != name {
            errors.push(format!(
                "aggregator/{name}: name=`{}` but filename is `{name}`",
                spec.name
            ));
            continue;
        }

        entries.push(AggregatorEntry {
            name: spec.name,
            title: spec.title,
            description: spec.description,
            url: spec.url,
            catalog_url: spec.catalog_url,
            contact: spec.contact,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
        });
    }

    entries
}

/// Build the per-endpoint `DetailEndpoint` list for one provider.
///
/// When `options.probe` is true, every endpoint is hit (using each
/// `ResolvedEndpoint.body_example` as the request body) and the probe results
/// override the spec's inline pricing. When false, endpoints pass through
/// unchanged with empty `protocol` / `supported_usd` / `probe_status` fields.
fn build_detail_endpoints(
    fqn: &str,
    service_url: &str,
    resolved: Vec<crate::skills::openapi::ResolvedEndpoint>,
    options: &BuildOptions,
) -> Vec<DetailEndpoint> {
    if !options.probe {
        return resolved
            .into_iter()
            .map(|r| DetailEndpoint {
                spec: r.spec,
                protocol: Vec::new(),
                supported_usd: Vec::new(),
                probe_status: None,
                probe_description: None,
            })
            .collect();
    }

    let probe_provider = pay_types::registry::ProbeProvider {
        fqn: fqn.to_string(),
        service_url: service_url.to_string(),
        endpoints: resolved
            .iter()
            .map(|r| pay_types::registry::ProbeEndpoint {
                method: r.spec.method.clone(),
                path: r.spec.path.clone(),
                metered: true,
                body: r.body_example.clone(),
            })
            .collect(),
    };
    let probe_result = crate::skills::probe::probe_provider(&probe_provider, &options.probe_config);

    resolved
        .into_iter()
        .zip(probe_result.endpoints)
        .map(|(r, probe)| {
            let mut spec = r.spec;
            // Probe-derived pricing wins. Fall back to the spec's inline
            // pricing when the probe could not classify the endpoint.
            if let Some(pricing) = crate::skills::probe::pricing_from_probe(&probe.paid) {
                spec.pricing = Some(pricing);
            }
            DetailEndpoint {
                spec,
                protocol: probe.paid.protocols,
                supported_usd: probe.paid.supported_usd,
                probe_status: Some(probe.probe_status),
                probe_description: probe.paid.description,
            }
        })
        .collect()
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Build the skills index from a registry directory using default options
/// (probing enabled).
///
/// `root` should point to the pay-skills repo root (containing `providers/`,
/// `affiliates/`, `aggregators/` directories). `base_url` is the CDN base
/// URL for detail file references.
pub fn build(root: &Path, base_url: &str, generated_at: String) -> BuildResult {
    build_with_options(root, base_url, generated_at, &BuildOptions::default())
}

/// Build the skills index with explicit options. Use this from the CLI to
/// thread a `--no-probe` flag in for offline builds.
pub fn build_with_options(
    root: &Path,
    base_url: &str,
    generated_at: String,
    options: &BuildOptions,
) -> BuildResult {
    let mut errors = Vec::new();

    eprintln!("Collecting providers...");
    let providers = collect_providers(root, options, &mut errors);

    eprintln!("Collecting affiliates...");
    let affiliates = collect_affiliates(root, &mut errors);

    eprintln!("Collecting aggregators...");
    let aggregators = collect_aggregators(root, &mut errors);

    // Check for duplicate FQNs
    let mut seen: HashMap<String, String> = HashMap::new();
    for (idx, _) in &providers {
        let skill = "pay-skills"; // TODO: support remote sources
        if let Some(prev) = seen.get(&idx.fqn) {
            errors.push(format!(
                "duplicate fqn `{}`: found in both `{prev}` and `{skill}`",
                idx.fqn
            ));
        }
        seen.insert(idx.fqn.clone(), skill.to_string());
    }

    // Build detail files map
    let mut detail_files = HashMap::new();
    for (entry, json) in &providers {
        let key = format!("providers/{}.json", entry.fqn);
        detail_files.insert(key, json.clone());
    }

    let mut provider_entries: Vec<ProviderIndexEntry> =
        providers.into_iter().map(|(idx, _)| idx).collect();
    provider_entries.sort_by(|a, b| a.fqn.cmp(&b.fqn));

    // ISO 8601 timestamp — passed in by the CLI so the core stays pure.
    let now = generated_at;

    let index = SkillsIndex {
        version: 2,
        generated_at: now,
        base_url: base_url.to_string(),
        provider_count: provider_entries.len(),
        affiliate_count: affiliates.len(),
        aggregator_count: aggregators.len(),
        providers: provider_entries,
        affiliates,
        aggregators,
    };

    BuildResult {
        index,
        detail_files,
        errors,
    }
}
