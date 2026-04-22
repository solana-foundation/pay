//! Build the pay-skills index from a registry directory.
//!
//! Reads `.md` files with YAML frontmatter from `providers/`, `affiliates/`,
//! and `aggregators/` directories. Produces:
//!
//! - `dist/skills.json` — lightweight index for search
//! - `dist/providers/<org>/<name>.json` — per-provider detail files

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Serialize;

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
#[derive(Debug, Serialize)]
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
    pub openapi_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affiliate_policy: Option<AffiliatePolicy>,
    pub source: ProviderSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub endpoints: Vec<EndpointSpec>,
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

fn collect_providers(
    root: &Path,
    errors: &mut Vec<String>,
) -> Vec<(ProviderIndexEntry, ProviderDetail, String)> {
    let mut results = Vec::new();
    let dir = root.join("providers");
    if !dir.is_dir() {
        return results;
    }

    // Walk: providers/<operator>/<name>.md          → FQN: operator/name
    //       providers/<operator>/<origin>/<name>.md  → FQN: operator/origin/name
    let operators = sorted_subdirs(&dir);
    for operator in &operators {
        let operator_name = operator.file_name().unwrap().to_string_lossy().to_string();

        for entry in sorted_entries(operator) {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
                // 2-level: operator/name.md (native API)
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                let fqn = format!("{operator_name}/{name}");
                process_provider_md(
                    &path,
                    &fqn,
                    &name,
                    &operator_name,
                    &operator_name,
                    root,
                    errors,
                    &mut results,
                );
            } else if path.is_dir() {
                // 3-level: operator/origin/*.md (proxied APIs)
                let origin = path.file_name().unwrap().to_string_lossy().to_string();
                for md_entry in sorted_entries(&path) {
                    let md_path = md_entry.path();
                    if md_path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    let name = md_path.file_stem().unwrap().to_string_lossy().to_string();
                    let fqn = format!("{operator_name}/{origin}/{name}");
                    process_provider_md(
                        &md_path,
                        &fqn,
                        &name,
                        &operator_name,
                        &origin,
                        root,
                        errors,
                        &mut results,
                    );
                }
            }
        }
    }

    results
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

fn sorted_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    v.sort_by_key(|e| e.file_name());
    v
}

#[allow(clippy::too_many_arguments)]
fn process_provider_md(
    path: &Path,
    fqn: &str,
    name: &str,
    operator: &str,
    origin: &str,
    root: &Path,
    errors: &mut Vec<String>,
    results: &mut Vec<(ProviderIndexEntry, ProviderDetail, String)>,
) {
    eprintln!("  provider: {fqn}");

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
            "{fqn}: name=`{}` but filename is `{name}`",
            spec.name
        ));
        return;
    }

    let errs = validate_provider(&spec, fqn);
    if !errs.is_empty() {
        errors.extend(errs);
        return;
    }

    let mut all_prices = Vec::new();
    let mut has_metering = false;
    let mut has_free_tier = false;

    for ep in &spec.endpoints {
        if let Some(ref pricing) = ep.pricing {
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
        openapi_url: spec.openapi_url.clone(),
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
        endpoints: spec.endpoints,
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

    results.push((index_entry, detail, detail_json));
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

// ── Public API ─────────────────────────────────────────────────────────────

/// Build the skills index from a registry directory.
///
/// `root` should point to the pay-skills repo root (containing `providers/`,
/// `affiliates/`, `aggregators/` directories).
///
/// `base_url` is the CDN base URL for detail file references.
pub fn build(root: &Path, base_url: &str, generated_at: String) -> BuildResult {
    let mut errors = Vec::new();

    eprintln!("Collecting providers...");
    let providers = collect_providers(root, &mut errors);
    eprintln!();

    eprintln!("Collecting affiliates...");
    let affiliates = collect_affiliates(root, &mut errors);
    eprintln!();

    eprintln!("Collecting aggregators...");
    let aggregators = collect_aggregators(root, &mut errors);
    eprintln!();

    // Check for duplicate FQNs
    let mut seen: HashMap<String, String> = HashMap::new();
    for (idx, _, _) in &providers {
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
    for (_, detail, json) in &providers {
        let key = format!("providers/{}.json", detail.fqn);
        detail_files.insert(key, json.clone());
    }

    let mut provider_entries: Vec<ProviderIndexEntry> =
        providers.into_iter().map(|(idx, _, _)| idx).collect();
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
