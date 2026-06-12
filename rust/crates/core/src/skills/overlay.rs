//! Merge pinned providers from the local overlay into a loaded
//! [`Catalog`](super::Catalog) so they show up in `search`, `list`, etc.
//! and any same-FQN entry from the canonical catalog is shadowed.
//!
//! Called from every [`super::load_skills`] return path so the overlay
//! is invisible to upstream callers — they receive a normal Catalog
//! that just happens to have pinned entries.
//!
//! Endpoints are eagerly populated from the pin's local PAY.md
//! (`build::build_single_provider`, no network probing). Downstream
//! consumers that call [`super::ensure_endpoints`] skip the CDN
//! fetch because `Service::endpoints_loaded()` is already true.

use std::path::Path;

use serde::Deserialize;

use crate::skills::build::{BuildOptions, build_single_provider};
use crate::skills::pin::{PinManifest, PinStore};
use crate::skills::{Catalog, Endpoint, Service};

/// Merge every pin in the overlay store into `catalog`. Same-FQN
/// providers in `catalog` are replaced, so the pin shadows the
/// canonical entry.
///
/// Returns the FQNs that were inserted (whether they shadowed
/// something or not) — useful for `pay skills list` to render the
/// overlay section.
pub fn merge_pins_into(catalog: &mut Catalog) -> Vec<String> {
    let store = PinStore::open_default();
    let pins = store.read_all();
    if pins.is_empty() {
        return Vec::new();
    }
    let mut inserted = Vec::with_capacity(pins.len());
    for (manifest, dir) in pins {
        match synthesize_pin_service(&manifest, &dir) {
            Ok(svc) => {
                let fqn = svc.fqn.clone();
                catalog.providers.retain(|p| p.fqn != fqn);
                catalog.providers.push(svc);
                inserted.push(fqn);
            }
            Err(e) => {
                tracing::warn!(fqn = %manifest.fqn, dir = ?dir, error = %e, "skipping pinned provider");
            }
        }
    }
    if !inserted.is_empty() {
        catalog.provider_count = catalog.providers.len() as u32;
    }
    inserted
}

/// Build a [`Service`] from a pin directory's `PAY.md`.
fn synthesize_pin_service(manifest: &PinManifest, dir: &Path) -> Result<Service, String> {
    let pay_md = dir.join("PAY.md");
    if !pay_md.is_file() {
        return Err(format!("missing PAY.md at {}", pay_md.display()));
    }
    let segments: Vec<&str> = manifest.fqn.split('/').collect();
    let (operator, origin, name) = match segments.as_slice() {
        [op, name] => (*op, *op, *name),
        [op, origin, name] => (*op, *origin, *name),
        _ => return Err(format!("unsupported fqn shape: {}", manifest.fqn)),
    };
    let options = BuildOptions {
        probe: false, // overlay is offline
        ..BuildOptions::default()
    };

    let result = build_single_provider(&pay_md, &manifest.fqn, name, operator, origin, &options);
    if !result.errors.is_empty() {
        return Err(format!(
            "build_single_provider rejected pin: {}",
            result.errors.join("; ")
        ));
    }
    let entry = result
        .index
        .providers
        .into_iter()
        .next()
        .ok_or_else(|| "build emitted no provider".to_string())?;
    let detail_json = result
        .detail_files
        .get(&format!("providers/{}.json", entry.fqn))
        .cloned()
        .unwrap_or_default();
    let endpoints = parse_endpoints(&detail_json);

    Ok(Service {
        fqn: entry.fqn,
        meta: entry.meta,
        endpoint_count: entry.endpoint_count as u32,
        has_metering: entry.has_metering,
        has_free_tier: entry.has_free_tier,
        min_price_usd: entry.min_price_usd,
        max_price_usd: entry.max_price_usd,
        sha: entry.sha,
        endpoints,
        content: None,
    })
}

/// Extract just the lightweight `Endpoint` view from the detail JSON
/// `build_single_provider` emits. Failures are non-fatal — the pin
/// still shows up in `list`/`search`, just without endpoint detail.
fn parse_endpoints(detail_json: &str) -> Vec<Endpoint> {
    if detail_json.is_empty() {
        return Vec::new();
    }
    #[derive(Deserialize)]
    struct Spec {
        method: String,
        path: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        resource: Option<String>,
        #[serde(default)]
        pricing: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct DetailEndpointShape {
        #[serde(flatten)]
        spec: Spec,
    }
    #[derive(Deserialize)]
    struct DetailShape {
        #[serde(default)]
        endpoints: Vec<DetailEndpointShape>,
    }
    let parsed: DetailShape = match serde_json::from_str(detail_json) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "could not parse pin detail json for endpoints");
            return Vec::new();
        }
    };
    parsed
        .endpoints
        .into_iter()
        .map(|e| Endpoint {
            method: e.spec.method,
            path: e.spec.path,
            full_path: String::new(),
            resource: e.spec.resource,
            description: e.spec.description,
            pricing: e.spec.pricing,
        })
        .collect()
}
