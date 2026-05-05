//! Serve a filtered + URL-rewritten OpenAPI document at `/openapi.json`.
//!
//! The server loads an OpenAPI 3 or Google Discovery JSON document declared
//! by the operator (via `--openapi` on the CLI or an `openapi:` field in the
//! provider YAML — both reuse [`pay_types::registry::OpenapiSource`]).
//!
//! Two transforms are applied before the document is exposed to clients:
//!
//! 1. **Filter** — only the endpoints actually proxied by the YAML (`api.endpoints`)
//!    survive. Other paths/methods are stripped so agents see exactly what
//!    they can call through the gateway.
//!
//! 2. **URL rewrite** — `rootUrl` (Discovery), `baseUrl`/`mtlsRootUrl`
//!    (Discovery), and `servers[].url` (OpenAPI 3) are rewritten to point at
//!    the proxy itself, derived from the request `Host` header (with an
//!    optional `--public-url` override) so the gateway can be driven without
//!    knowing the upstream URL.

use std::collections::HashSet;
use std::path::Path;

use pay_types::metering::Endpoint;
use pay_types::registry::OpenapiSource;
use serde_json::{Map, Value};

use crate::{Error, Result};

/// Load the document referenced by `source` from disk, an HTTP URL, or
/// inline content.
///
/// `Path` is interpreted relative to `spec_dir` when not absolute — the
/// pay-server filesystem semantics differ from pay-skills (which resolves
/// `Path` against `service_url` over HTTP). Same enum, context-dependent
/// resolution.
pub fn load_document(source: &OpenapiSource, spec_dir: &Path) -> Result<Value> {
    let raw = match source {
        OpenapiSource::Path { path } => {
            let candidate = Path::new(path);
            let full = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                spec_dir.join(candidate)
            };
            std::fs::read_to_string(&full).map_err(|e| {
                Error::Config(format!("openapi: failed to read {}: {e}", full.display()))
            })?
        }
        OpenapiSource::Url { url } => {
            let resp = reqwest::blocking::get(url)
                .map_err(|e| Error::Config(format!("openapi fetch {url}: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Config(format!(
                    "openapi fetch {url} returned {}",
                    resp.status()
                )));
            }
            resp.text()
                .map_err(|e| Error::Config(format!("openapi fetch {url}: {e}")))?
        }
        OpenapiSource::Content { content } => content.clone(),
    };
    serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("openapi: document is not valid JSON: {e}")))
}

/// Strip every operation whose `(METHOD, path)` is not declared in
/// `endpoints`. Mutates the document in place.
///
/// Handles both schemas:
/// - **OpenAPI 3**: prunes methods inside each `paths.<path>` object; drops
///   path entries that end up with no methods.
/// - **Google Discovery**: prunes `methods.<name>` entries inside every
///   resource (recursing through nested `resources.<name>`); drops empty
///   `methods` / `resources` containers.
pub fn filter_to_endpoints(doc: &mut Value, endpoints: &[Endpoint]) {
    // Pre-canonicalize each YAML path so we can match it against openapi
    // paths regardless of placeholder spelling. `bigquery/v2/projects/{projectsId}/queries`
    // and `projects/{projectId}/queries` (after base-path strip) compare
    // equal because we collapse `{anything}` → `{*}`.
    let allowed: HashSet<(String, String)> = endpoints
        .iter()
        .map(|e| {
            (
                http_method_str(&e.method).to_string(),
                canonical_path(&e.path),
            )
        })
        .collect();

    if doc.get("openapi").is_some() || doc.get("swagger").is_some() {
        filter_openapi3(doc, &allowed);
    } else if doc
        .get("kind")
        .and_then(|v| v.as_str())
        .is_some_and(|k| k.starts_with("discovery#"))
    {
        filter_discovery(doc, &allowed);
    } else {
        // Unknown shape — best effort: try OpenAPI 3 if `paths` is present,
        // otherwise try Discovery if `resources` is present, else leave alone.
        if doc.get("paths").is_some() {
            filter_openapi3(doc, &allowed);
        } else if doc.get("resources").is_some() {
            filter_discovery(doc, &allowed);
        }
    }
}

/// Rewrite the document's base-URL fields to `public_url`, preserving the
/// upstream's path component so that `servers[0].url + paths[i]` still
/// resolves to a route the proxy actually accepts.
///
/// Why preserve the path: Google's BigQuery upstream advertises
/// `servers[0].url = https://bigquery.googleapis.com/bigquery/v2` with
/// `paths: { /projects/.../queries }`. The proxy's allowlist mirrors that —
/// it accepts `/bigquery/v2/projects/.../queries`. If we naively rewrite
/// `servers[0].url` to just `https://bigquery.google.gateway-402.com`, the
/// downstream consumer constructs `https://…/projects/…` (no `/bigquery/v2`)
/// and 404s. Keeping the upstream's `/bigquery/v2` suffix on the proxy URL
/// yields `https://…/bigquery/v2/projects/…` which routes correctly.
///
/// Behavior:
/// - **OpenAPI 3**: each `servers[].url` is replaced with
///   `public_url + <upstream path>`. Upstream-root URLs (no path component)
///   collapse to plain `public_url`.
/// - **Discovery**: `rootUrl`/`baseUrl`/`mtlsRootUrl` are rewritten to
///   `public_url` (with trailing `/`). Discovery composes as
///   `rootUrl + servicePath` so the upstream path is carried by
///   `servicePath`, not the root URL — no preservation needed here.
pub fn rewrite_urls(doc: &mut Value, public_url: &str) {
    let proxy_root = public_url.trim_end_matches('/').to_string();
    let with_slash = format!("{proxy_root}/");

    if let Some(servers) = doc.get_mut("servers").and_then(|v| v.as_array_mut()) {
        for entry in servers {
            let Some(obj) = entry.as_object_mut() else {
                continue;
            };
            let upstream_path = obj
                .get("url")
                .and_then(|v| v.as_str())
                .map(extract_path_component)
                .unwrap_or_default();
            let rewritten = if upstream_path.is_empty() {
                proxy_root.clone()
            } else {
                format!("{proxy_root}/{upstream_path}")
            };
            obj.insert("url".to_string(), Value::String(rewritten));
        }
    }

    let root_obj = match doc.as_object_mut() {
        Some(obj) => obj,
        None => return,
    };
    for key in ["rootUrl", "baseUrl", "mtlsRootUrl"] {
        if root_obj.contains_key(key) {
            root_obj.insert(key.to_string(), Value::String(with_slash.clone()));
        }
    }
}

/// Extract the path component of an absolute URL with no leading/trailing
/// slashes. Returns `""` for root-level URLs (no path or just `/`). Used by
/// `rewrite_urls` to carry the upstream base path onto the proxy URL.
fn extract_path_component(url: &str) -> String {
    let after_scheme = match url.split("://").nth(1) {
        Some(s) => s,
        None => url,
    };
    match after_scheme.find('/') {
        Some(i) => after_scheme[i..]
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string(),
        None => String::new(),
    }
}

/// Trim a leading `/` so YAML paths (`v1/foo`) compare equal to OpenAPI/
/// Discovery paths (`/v1/foo`).
fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

/// Canonicalize a path for cross-format matching:
/// - trim leading `/`
/// - collapse every `{placeholder}` → `{*}` so `/projects/{projectId}/queries`
///   matches the YAML's `/projects/{projectsId}/queries` (Google OpenAPI 3
///   uses singular, Google Discovery uses plural; we tolerate both).
fn canonical_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    // Hand-rolled `{...}` → `{*}` substitution; no regex dep needed.
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        if c == '{' {
            // skip until matching '}'
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    break;
                }
            }
            out.push_str("{*}");
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract the path component of `servers[0].url` in OpenAPI 3 docs.
/// Returns the prefix (without leading/trailing slash) so we can prepend it
/// to each `paths.<path>` key for matching against YAML allowlist entries.
/// Empty string when no servers, no path component, or `/`.
fn openapi3_base_path(doc: &Value) -> String {
    let url = match doc
        .get("servers")
        .and_then(|s| s.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.get("url"))
        .and_then(|v| v.as_str())
    {
        Some(u) => u,
        None => return String::new(),
    };
    let after_scheme = match url.split("://").nth(1) {
        Some(s) => s,
        None => url,
    };
    let path_start = match after_scheme.find('/') {
        Some(i) => i,
        None => return String::new(),
    };
    after_scheme[path_start..]
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

fn http_method_str(method: &pay_types::metering::HttpMethod) -> &'static str {
    use pay_types::metering::HttpMethod::*;
    match method {
        Get => "GET",
        Post => "POST",
        Put => "PUT",
        Patch => "PATCH",
        Delete => "DELETE",
    }
}

const HTTP_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

fn filter_openapi3(doc: &mut Value, allowed: &HashSet<(String, String)>) {
    // Compute the base path from servers[0].url so we can match the YAML's
    // proxy-relative paths (e.g. `bigquery/v2/projects/...`) against the
    // openapi's server-relative paths (e.g. `/projects/...`). For bigquery:
    // base_path = "bigquery/v2", openapi_path = "/projects/{projectId}/queries"
    // → combined "bigquery/v2/projects/{*}/queries" which matches the YAML's
    // canonicalized "bigquery/v2/projects/{*}/queries".
    let base_path = openapi3_base_path(doc);

    let Some(paths) = doc.get_mut("paths").and_then(|v| v.as_object_mut()) else {
        return;
    };
    let mut empty_paths: Vec<String> = Vec::new();
    for (path, item) in paths.iter_mut() {
        let combined = if base_path.is_empty() {
            normalize_path(path)
        } else {
            format!("{}/{}", base_path, path.trim_start_matches('/'))
        };
        let canon = canonical_path(&combined);
        let Some(item_obj) = item.as_object_mut() else {
            continue;
        };
        let methods_to_remove: Vec<String> = HTTP_METHODS
            .iter()
            .filter(|m| item_obj.contains_key(**m))
            .filter(|m| !allowed.contains(&(m.to_uppercase(), canon.clone())))
            .map(|m| (*m).to_string())
            .collect();
        for m in methods_to_remove {
            item_obj.remove(&m);
        }
        if !item_obj.keys().any(|k| HTTP_METHODS.contains(&k.as_str())) {
            empty_paths.push(path.clone());
        }
    }
    for p in empty_paths {
        paths.remove(&p);
    }
}

fn filter_discovery(doc: &mut Value, allowed: &HashSet<(String, String)>) {
    if let Some(root_obj) = doc.as_object_mut() {
        prune_resources(root_obj, allowed);
    }
}

/// Strip upstream-auth metadata that doesn't apply to proxy callers. The
/// proxy handles upstream credentials internally (Google OAuth2, API keys,
/// etc.); leaving the auth schemes in the served doc misleads agents into
/// attaching tokens that the proxy won't honor anyway. Removes:
///
/// - `components.securitySchemes` (OpenAPI 3) — drops the bucket entirely.
/// - `security:` arrays at the root and on every operation (OpenAPI 3).
/// - `auth:` block (Google Discovery) at the root.
/// - `scopes:` array on every Discovery method, recursively through nested
///   resources.
pub fn strip_upstream_auth(doc: &mut Value) {
    if let Some(obj) = doc.as_object_mut() {
        // OpenAPI 3 root-level security and securitySchemes bucket.
        obj.remove("security");
        if let Some(components) = obj.get_mut("components").and_then(|v| v.as_object_mut()) {
            components.remove("securitySchemes");
            if components.is_empty() {
                obj.remove("components");
            }
        }
        // Discovery root-level auth block.
        obj.remove("auth");
    }

    // Per-operation security on OpenAPI 3 paths.
    if let Some(paths) = doc.get_mut("paths").and_then(|v| v.as_object_mut()) {
        for (_, item) in paths.iter_mut() {
            let Some(item_obj) = item.as_object_mut() else {
                continue;
            };
            for &method in HTTP_METHODS {
                if let Some(op) = item_obj.get_mut(method).and_then(|v| v.as_object_mut()) {
                    op.remove("security");
                }
            }
        }
    }

    // Per-method `scopes` arrays on Discovery resources, recursively.
    if let Some(resources) = doc.get_mut("resources").and_then(|v| v.as_object_mut()) {
        strip_discovery_method_scopes(resources);
    }
    if let Some(methods) = doc.get_mut("methods").and_then(|v| v.as_object_mut()) {
        for (_, m) in methods.iter_mut() {
            if let Some(mobj) = m.as_object_mut() {
                mobj.remove("scopes");
            }
        }
    }
}

fn strip_discovery_method_scopes(resources: &mut Map<String, Value>) {
    for (_, resource) in resources.iter_mut() {
        let Some(robj) = resource.as_object_mut() else {
            continue;
        };
        if let Some(methods) = robj.get_mut("methods").and_then(|v| v.as_object_mut()) {
            for (_, m) in methods.iter_mut() {
                if let Some(mobj) = m.as_object_mut() {
                    mobj.remove("scopes");
                }
            }
        }
        if let Some(nested) = robj.get_mut("resources").and_then(|v| v.as_object_mut()) {
            strip_discovery_method_scopes(nested);
        }
    }
}

/// Drop schemas / parameters / requestBodies / responses that no surviving
/// operation transitively references. Run *after* [`filter_to_endpoints`]
/// so the reachability seed only includes kept operations — the upstream's
/// dead schema baggage gets cut along with the methods that referenced it.
///
/// Handles both shapes:
/// - **OpenAPI 3**: walks `paths.<path>.<method>` (plus `security:` + the
///   root `tags:`) for `$ref` strings, then BFS-expands through
///   `components.{schemas,parameters,requestBodies,responses,headers,examples,
///   links,callbacks,pathItems}`. Unreferenced sub-entries are removed.
/// - **Google Discovery**: walks `resources.*.methods.*.{request,response,
///   parameters}` for `$ref` strings (each pointing into the top-level
///   `schemas` bucket), BFS-expands through `schemas`, and removes
///   unreferenced top-level schemas.
pub fn prune_unused_components(doc: &mut Value) {
    if doc.get("openapi").is_some() || doc.get("swagger").is_some() {
        prune_openapi3_components(doc);
    } else if doc
        .get("kind")
        .and_then(|v| v.as_str())
        .is_some_and(|k| k.starts_with("discovery#"))
    {
        prune_discovery_schemas(doc);
    } else if doc.get("paths").is_some() {
        // Best-effort fallback for OpenAPI-shaped docs missing the marker.
        prune_openapi3_components(doc);
    } else if doc.get("schemas").is_some() && doc.get("resources").is_some() {
        prune_discovery_schemas(doc);
    }
}

/// Recursively walk a JSON value collecting every `$ref` string.
fn collect_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "$ref" {
                    if let Some(s) = v.as_str() {
                        refs.insert(s.to_string());
                    }
                } else {
                    collect_refs(v, refs);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, refs);
            }
        }
        _ => {}
    }
}

const OPENAPI3_COMPONENT_SUBKEYS: &[&str] = &[
    "schemas",
    "parameters",
    "requestBodies",
    "responses",
    "examples",
    "headers",
    "links",
    "callbacks",
    "pathItems",
];

fn prune_openapi3_components(doc: &mut Value) {
    let mut reachable: HashSet<String> = HashSet::new();

    // Seed: every $ref under the kept paths and root-level fields that may
    // legitimately reference components (`security` is name-based not $ref,
    // skip it; `tags` is name-based too).
    if let Some(paths) = doc.get("paths") {
        collect_refs(paths, &mut reachable);
    }
    // BFS through components: each ref's target may itself reference more.
    let mut frontier: Vec<String> = reachable.iter().cloned().collect();
    while let Some(r) = frontier.pop() {
        let Some(pointer) = r.strip_prefix('#') else {
            continue; // external/file refs not supported
        };
        if let Some(target) = doc.pointer(pointer) {
            let mut new_refs = HashSet::new();
            collect_refs(target, &mut new_refs);
            for nr in new_refs {
                if reachable.insert(nr.clone()) {
                    frontier.push(nr);
                }
            }
        }
    }

    // Prune unreferenced sub-entries from `components.<sub>`.
    let mut drop_components = false;
    if let Some(components) = doc.get_mut("components").and_then(|v| v.as_object_mut()) {
        for sub_key in OPENAPI3_COMPONENT_SUBKEYS {
            let to_remove: Vec<String> = match components.get(*sub_key).and_then(|v| v.as_object())
            {
                Some(sub) => sub
                    .keys()
                    .filter(|k| {
                        let full_ref = format!("#/components/{sub_key}/{k}");
                        !reachable.contains(&full_ref)
                    })
                    .cloned()
                    .collect(),
                None => continue,
            };
            if let Some(sub) = components.get_mut(*sub_key).and_then(|v| v.as_object_mut()) {
                for k in to_remove {
                    sub.remove(&k);
                }
                if sub.is_empty() {
                    components.remove(*sub_key);
                }
            }
        }
        drop_components = components.is_empty();
    }
    if drop_components && let Some(root) = doc.as_object_mut() {
        root.remove("components");
    }
}

fn prune_discovery_schemas(doc: &mut Value) {
    let mut reachable: HashSet<String> = HashSet::new();

    // Seed from kept resources/methods. Discovery `$ref` values are bare
    // schema names (no `#/...` prefix); they index into the top-level
    // `schemas` bucket.
    if let Some(resources) = doc.get("resources") {
        collect_refs(resources, &mut reachable);
    }
    // Top-level `methods` (some discovery docs put methods at the root).
    if let Some(methods) = doc.get("methods") {
        collect_refs(methods, &mut reachable);
    }

    // BFS expand through schemas — each schema may reference others.
    let mut frontier: Vec<String> = reachable.iter().cloned().collect();
    while let Some(name) = frontier.pop() {
        if let Some(schema) = doc.pointer(&format!("/schemas/{name}")) {
            let mut new_refs = HashSet::new();
            collect_refs(schema, &mut new_refs);
            for nr in new_refs {
                if reachable.insert(nr.clone()) {
                    frontier.push(nr);
                }
            }
        }
    }

    // Drop unreferenced schemas; drop the bucket entirely if empty.
    let mut drop_bucket = false;
    if let Some(schemas) = doc.get_mut("schemas").and_then(|v| v.as_object_mut()) {
        let to_remove: Vec<String> = schemas
            .keys()
            .filter(|k| !reachable.contains(*k))
            .cloned()
            .collect();
        for k in to_remove {
            schemas.remove(&k);
        }
        drop_bucket = schemas.is_empty();
    }
    if drop_bucket && let Some(root) = doc.as_object_mut() {
        root.remove("schemas");
    }
}

/// Walk a discovery container (root or nested resource) and prune `methods`
/// and nested `resources` that don't survive the allowlist. Returns `true` if
/// the container has any surviving methods or resources after pruning.
fn prune_resources(
    container: &mut Map<String, Value>,
    allowed: &HashSet<(String, String)>,
) -> bool {
    // Prune methods.
    if let Some(methods) = container.get_mut("methods").and_then(|v| v.as_object_mut()) {
        let to_remove: Vec<String> = methods
            .iter()
            .filter_map(|(name, m)| {
                let http_method = m
                    .get("httpMethod")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let path = m.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if allowed.contains(&(http_method, canonical_path(path))) {
                    None
                } else {
                    Some(name.clone())
                }
            })
            .collect();
        for name in to_remove {
            methods.remove(&name);
        }
        if methods.is_empty() {
            container.remove("methods");
        }
    }

    // Recurse into nested resources.
    if let Some(resources) = container
        .get_mut("resources")
        .and_then(|v| v.as_object_mut())
    {
        let names: Vec<String> = resources.keys().cloned().collect();
        for name in names {
            let keep = if let Some(r) = resources.get_mut(&name).and_then(|v| v.as_object_mut()) {
                prune_resources(r, allowed)
            } else {
                false
            };
            if !keep {
                resources.remove(&name);
            }
        }
        if resources.is_empty() {
            container.remove("resources");
        }
    }

    container.contains_key("methods") || container.contains_key("resources")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep(method: pay_types::metering::HttpMethod, path: &str) -> Endpoint {
        Endpoint {
            method,
            path: path.to_string(),
            description: Some("test endpoint".to_string()),
            resource: None,
            metering: None,
            routing: None,
        }
    }

    use pay_types::metering::HttpMethod::{Get, Post};

    #[test]
    fn filter_openapi3_keeps_only_allowed_methods() {
        let mut doc = json!({
            "openapi": "3.1.0",
            "servers": [{"url": "https://upstream.example.com/"}],
            "paths": {
                "/v1/keep": { "post": {"summary": "kept"}, "get": {"summary": "removed"} },
                "/v1/drop": { "post": {"summary": "removed-entirely"} }
            }
        });
        let endpoints = vec![ep(Post, "v1/keep")];
        filter_to_endpoints(&mut doc, &endpoints);

        let paths = doc["paths"].as_object().unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths.contains_key("/v1/keep"));
        let kept = paths["/v1/keep"].as_object().unwrap();
        assert!(kept.contains_key("post"));
        assert!(!kept.contains_key("get"));
    }

    #[test]
    fn filter_discovery_walks_nested_resources_and_methods() {
        let mut doc = json!({
            "kind": "discovery#restDescription",
            "rootUrl": "https://upstream.example.com/",
            "resources": {
                "currentConditions": {
                    "methods": {
                        "lookup": {
                            "httpMethod": "POST",
                            "path": "v1/currentConditions:lookup"
                        }
                    }
                },
                "history": {
                    "methods": {
                        "lookup": {
                            "httpMethod": "POST",
                            "path": "v1/history:lookup"
                        }
                    }
                },
                "mapTypes": {
                    "resources": {
                        "heatmapTiles": {
                            "methods": {
                                "lookup": {
                                    "httpMethod": "GET",
                                    "path": "v1/mapTypes/{mapType}/heatmapTiles/{zoom}/{x}/{y}"
                                }
                            }
                        }
                    }
                }
            }
        });

        let endpoints = vec![
            ep(Post, "v1/currentConditions:lookup"),
            ep(Get, "v1/mapTypes/{mapType}/heatmapTiles/{zoom}/{x}/{y}"),
        ];
        filter_to_endpoints(&mut doc, &endpoints);

        let resources = doc["resources"].as_object().unwrap();
        // history resource should be gone (its only method wasn't allowlisted).
        assert!(!resources.contains_key("history"));
        // currentConditions kept.
        assert!(
            resources["currentConditions"]["methods"]
                .as_object()
                .unwrap()
                .contains_key("lookup")
        );
        // mapTypes nested resource kept (heatmapTiles.lookup was allowlisted).
        assert!(
            resources["mapTypes"]["resources"]["heatmapTiles"]["methods"]
                .as_object()
                .unwrap()
                .contains_key("lookup")
        );
    }

    #[test]
    fn rewrite_urls_preserves_upstream_path_component() {
        // BigQuery shape: upstream advertises a /bigquery/v2 base in
        // servers[0].url and bare `/projects/...` paths. The proxy's
        // allowlist accepts `/bigquery/v2/...`, so the rewritten server
        // must keep the path component or downstream consumers 404.
        let mut doc = json!({
            "openapi": "3.1.0",
            "servers": [
                {"url": "https://bigquery.googleapis.com/bigquery/v2"},
                {"url": "https://other.example.com/"}
            ]
        });
        rewrite_urls(&mut doc, "https://bigquery.proxy.example.com");
        let servers = doc["servers"].as_array().unwrap();
        assert_eq!(
            servers[0]["url"],
            json!("https://bigquery.proxy.example.com/bigquery/v2")
        );
        // Root-level upstream collapses to plain proxy URL.
        assert_eq!(
            servers[1]["url"],
            json!("https://bigquery.proxy.example.com")
        );
    }

    #[test]
    fn rewrite_urls_strips_trailing_slash_on_proxy_url() {
        let mut doc = json!({
            "openapi": "3.1.0",
            "servers": [{"url": "https://upstream.example.com/v1/"}]
        });
        // Trailing slash on public_url should not double up.
        rewrite_urls(&mut doc, "https://proxy.example.com/");
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://proxy.example.com/v1")
        );
    }

    #[test]
    fn rewrite_urls_updates_discovery_root_and_base_urls() {
        let mut doc = json!({
            "kind": "discovery#restDescription",
            "rootUrl": "https://upstream.example.com/",
            "baseUrl": "https://upstream.example.com/v1/",
            "mtlsRootUrl": "https://upstream.mtls.example.com/"
        });
        rewrite_urls(&mut doc, "https://proxy.example.com/");
        // Trailing slash preserved on rewrite.
        assert_eq!(doc["rootUrl"], json!("https://proxy.example.com/"));
        assert_eq!(doc["baseUrl"], json!("https://proxy.example.com/"));
        assert_eq!(doc["mtlsRootUrl"], json!("https://proxy.example.com/"));
    }

    #[test]
    fn rewrite_urls_is_noop_for_missing_fields() {
        let mut doc = json!({"foo": "bar"});
        rewrite_urls(&mut doc, "https://proxy.example.com");
        assert_eq!(doc, json!({"foo": "bar"}));
    }

    #[test]
    fn rewrite_urls_handles_root_level_upstream_unchanged() {
        // Civicinfo / language / speech / etc. shape: upstream root-level
        // with a trailing slash. Path collapses to empty → bare proxy URL.
        // This is the pre-fix behavior, kept stable for the majority case.
        for upstream in [
            "https://civicinfo.googleapis.com/",
            "https://civicinfo.googleapis.com",
            "https://language.googleapis.com/",
            "https://speech.googleapis.com/",
        ] {
            let mut doc = json!({
                "openapi": "3.0.0",
                "servers": [{"url": upstream}]
            });
            rewrite_urls(&mut doc, "https://proxy.example.com");
            assert_eq!(
                doc["servers"][0]["url"],
                json!("https://proxy.example.com"),
                "root-level upstream `{upstream}` should produce bare proxy URL"
            );
        }
    }

    #[test]
    fn rewrite_urls_handles_multi_segment_upstream_path() {
        // Hypothetical upstream that nests deeper than bigquery.
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://api.example.com/v3/foo/bar"}]
        });
        rewrite_urls(&mut doc, "https://proxy.example.com");
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://proxy.example.com/v3/foo/bar")
        );
    }

    #[test]
    fn rewrite_urls_handles_trailing_slash_on_upstream() {
        // `https://x.com/v2/` should produce `proxy/v2`, not `proxy/v2/`.
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://upstream.example.com/v2/"}]
        });
        rewrite_urls(&mut doc, "https://proxy.example.com");
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://proxy.example.com/v2")
        );
    }

    #[test]
    fn rewrite_urls_handles_url_with_port() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://localhost:8443/api/v2"}]
        });
        rewrite_urls(&mut doc, "https://proxy.example.com:9443");
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://proxy.example.com:9443/api/v2")
        );
    }

    #[test]
    fn rewrite_urls_mixed_servers_each_keep_their_own_path() {
        // A server array with one path-bearing entry and one root entry —
        // each is rewritten independently, preserving its own path.
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [
                {"url": "https://upstream.example.com/api/v2"},
                {"url": "https://upstream.example.com/"}
            ]
        });
        rewrite_urls(&mut doc, "https://proxy.example.com");
        let servers = doc["servers"].as_array().unwrap();
        assert_eq!(servers[0]["url"], json!("https://proxy.example.com/api/v2"));
        assert_eq!(servers[1]["url"], json!("https://proxy.example.com"));
    }

    /// End-to-end flow modeled on bigquery: load an upstream-shaped doc,
    /// filter it down to a YAML's allowlist, rewrite the URLs, then
    /// reconstruct `servers[0].url + paths[i]` and assert that's the URL
    /// the proxy actually accepts (`/bigquery/v2/projects/.../queries`).
    ///
    /// This is the regression test for the audit failure in pay-skills:
    /// catalog consumers were constructing bare URLs that 404'd because
    /// `rewrite_urls` was stripping the upstream base path.
    #[test]
    fn pipeline_bigquery_shape_constructs_correct_proxy_url() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://bigquery.googleapis.com/bigquery/v2"}],
            "paths": {
                "/projects/{projectId}/queries": {
                    "post": {"summary": "kept"},
                    "get":  {"summary": "drop me"}
                },
                "/projects/{projectId}/datasets": {
                    "get": {"summary": "drop me too"}
                }
            }
        });
        // Mirrors the bigquery.yml allowlist (POST queries only).
        let endpoints = vec![ep(Post, "bigquery/v2/projects/{projectsId}/queries")];
        filter_to_endpoints(&mut doc, &endpoints);
        rewrite_urls(&mut doc, "https://bigquery.proxy.example.com");

        // Server keeps the upstream base path on the proxy URL.
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://bigquery.proxy.example.com/bigquery/v2")
        );
        // Only the allow-listed POST survives.
        let paths = doc["paths"].as_object().unwrap();
        assert_eq!(paths.len(), 1);
        let queries = paths["/projects/{projectId}/queries"].as_object().unwrap();
        assert!(queries.contains_key("post"));
        assert!(!queries.contains_key("get"));

        // A consumer constructing `servers[0].url + path` lands on the URL
        // the proxy actually accepts.
        let server = doc["servers"][0]["url"].as_str().unwrap();
        let path = paths.keys().next().unwrap();
        assert_eq!(
            format!("{server}{path}"),
            "https://bigquery.proxy.example.com/bigquery/v2/projects/{projectId}/queries"
        );
    }

    /// End-to-end flow modeled on civicinfo: upstream root server, paths
    /// already include the version prefix. The constructed URL must keep
    /// that prefix and not have it duplicated.
    #[test]
    fn pipeline_civicinfo_shape_constructs_correct_proxy_url() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://civicinfo.googleapis.com/"}],
            "paths": {
                "/civicinfo/v2/divisions": {"get": {"summary": "kept"}},
                "/civicinfo/v2/elections": {"get": {"summary": "drop me"}}
            }
        });
        let endpoints = vec![ep(Get, "civicinfo/v2/divisions")];
        filter_to_endpoints(&mut doc, &endpoints);
        rewrite_urls(&mut doc, "https://civicinfo.proxy.example.com");

        // Root-level upstream → bare proxy URL.
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://civicinfo.proxy.example.com")
        );
        let paths = doc["paths"].as_object().unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths.contains_key("/civicinfo/v2/divisions"));

        let server = doc["servers"][0]["url"].as_str().unwrap();
        let path = paths.keys().next().unwrap();
        assert_eq!(
            format!("{server}{path}"),
            "https://civicinfo.proxy.example.com/civicinfo/v2/divisions"
        );
    }

    /// End-to-end flow on a service that emits an empty `paths: {}` after
    /// filtering (e.g. the YAML's allowlist doesn't match anything in the
    /// upstream). `rewrite_urls` should still run and produce a sane
    /// servers entry — no panic, no malformed output.
    #[test]
    fn pipeline_handles_empty_paths_after_filter() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "servers": [{"url": "https://documentai.googleapis.com/"}],
            "paths": {
                "/v1/{name}:reviewDocument": {"post": {}}
            }
        });
        // Allowlist that doesn't match anything (different path shape).
        let endpoints = vec![ep(
            Post,
            "v1/projects/{*}/locations/{*}/processors/{*}:process",
        )];
        filter_to_endpoints(&mut doc, &endpoints);
        rewrite_urls(&mut doc, "https://documentai.proxy.example.com");
        assert_eq!(
            doc["servers"][0]["url"],
            json!("https://documentai.proxy.example.com")
        );
        assert!(doc["paths"].as_object().unwrap().is_empty());
    }

    #[test]
    fn filter_drops_paths_with_no_surviving_methods() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "paths": {
                "/v1/a": { "get": {} },
                "/v1/b": { "get": {} }
            }
        });
        let endpoints = vec![ep(Get, "v1/a")];
        filter_to_endpoints(&mut doc, &endpoints);
        let paths = doc["paths"].as_object().unwrap();
        assert!(paths.contains_key("/v1/a"));
        assert!(!paths.contains_key("/v1/b"));
    }

    #[test]
    fn prune_openapi3_drops_unreferenced_schemas() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "paths": {
                "/v1/keep": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "#/components/schemas/Used"}
                                }
                            }
                        },
                        "responses": {
                            "200": {"$ref": "#/components/responses/Ok"}
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Used":      {"type": "object", "properties": {"nested": {"$ref": "#/components/schemas/Nested"}}},
                    "Nested":    {"type": "string"},
                    "Orphan":    {"type": "object"},
                    "AlsoOrphan":{"type": "object"}
                },
                "responses": {
                    "Ok":         {"description": "ok"},
                    "Unused":     {"description": "unused"}
                },
                "requestBodies": {"DeadBody": {"description": "x"}},
                "parameters":    {"DeadParam": {"name": "p"}}
            }
        });
        prune_unused_components(&mut doc);

        let schemas = doc["components"]["schemas"].as_object().unwrap();
        assert!(schemas.contains_key("Used"));
        assert!(schemas.contains_key("Nested")); // transitive
        assert!(!schemas.contains_key("Orphan"));
        assert!(!schemas.contains_key("AlsoOrphan"));

        let responses = doc["components"]["responses"].as_object().unwrap();
        assert!(responses.contains_key("Ok"));
        assert!(!responses.contains_key("Unused"));

        // Unused buckets dropped entirely.
        assert!(
            !doc["components"]
                .as_object()
                .unwrap()
                .contains_key("requestBodies")
        );
        assert!(
            !doc["components"]
                .as_object()
                .unwrap()
                .contains_key("parameters")
        );
    }

    #[test]
    fn prune_openapi3_drops_components_object_when_empty() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "paths": {"/v1/x": {"get": {}}},
            "components": {
                "schemas": {"Orphan": {"type": "object"}},
                "parameters": {"DeadParam": {"name": "p"}}
            }
        });
        prune_unused_components(&mut doc);
        assert!(!doc.as_object().unwrap().contains_key("components"));
    }

    #[test]
    fn prune_discovery_drops_unreferenced_schemas() {
        let mut doc = json!({
            "kind": "discovery#restDescription",
            "schemas": {
                "Used":     {"type": "object", "properties": {"x": {"$ref": "Nested"}}},
                "Nested":   {"type": "string"},
                "Orphan":   {"type": "object"},
                "Disjoint": {"type": "object", "properties": {"y": {"$ref": "OtherOrphan"}}},
                "OtherOrphan": {"type": "object"}
            },
            "resources": {
                "things": {
                    "methods": {
                        "lookup": {
                            "httpMethod": "POST",
                            "path": "v1/things:lookup",
                            "request":  {"$ref": "Used"},
                            "response": {"$ref": "Used"}
                        }
                    }
                }
            }
        });
        prune_unused_components(&mut doc);
        let schemas = doc["schemas"].as_object().unwrap();
        assert!(schemas.contains_key("Used"));
        assert!(schemas.contains_key("Nested")); // transitive
        assert!(!schemas.contains_key("Orphan"));
        assert!(!schemas.contains_key("Disjoint"));
        assert!(!schemas.contains_key("OtherOrphan"));
    }

    #[test]
    fn strip_auth_drops_openapi3_security_and_scheme_bucket() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "security": [{"oauth2": ["scope.a"]}],
            "paths": {
                "/v1/x": {
                    "post": {
                        "summary": "x",
                        "security": [{"oauth2": ["scope.b"]}]
                    }
                }
            },
            "components": {
                "schemas": {"Foo": {"type": "object"}},
                "securitySchemes": {
                    "oauth2": {"type": "oauth2", "flows": {}}
                }
            }
        });
        strip_upstream_auth(&mut doc);

        // Root-level + per-operation security gone.
        assert!(!doc.as_object().unwrap().contains_key("security"));
        assert!(
            !doc["paths"]["/v1/x"]["post"]
                .as_object()
                .unwrap()
                .contains_key("security")
        );
        // securitySchemes bucket gone (other components survive).
        let comp = doc["components"].as_object().unwrap();
        assert!(!comp.contains_key("securitySchemes"));
        assert!(comp.contains_key("schemas"));
    }

    #[test]
    fn strip_auth_drops_components_when_only_security_schemes_were_left() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "components": {
                "securitySchemes": {"oauth2": {"type": "oauth2"}}
            }
        });
        strip_upstream_auth(&mut doc);
        assert!(!doc.as_object().unwrap().contains_key("components"));
    }

    #[test]
    fn strip_auth_drops_discovery_auth_and_method_scopes() {
        let mut doc = json!({
            "kind": "discovery#restDescription",
            "auth": {
                "oauth2": {"scopes": {"https://example.com/auth/x": {"description": "x"}}}
            },
            "resources": {
                "things": {
                    "methods": {
                        "lookup": {
                            "httpMethod": "POST",
                            "path": "v1/things:lookup",
                            "scopes": ["https://example.com/auth/x"]
                        }
                    },
                    "resources": {
                        "nested": {
                            "methods": {
                                "get": {
                                    "httpMethod": "GET",
                                    "path": "v1/things/nested",
                                    "scopes": ["https://example.com/auth/y"]
                                }
                            }
                        }
                    }
                }
            }
        });
        strip_upstream_auth(&mut doc);

        assert!(!doc.as_object().unwrap().contains_key("auth"));
        // Scopes removed from every method, including nested resources.
        assert!(
            !doc["resources"]["things"]["methods"]["lookup"]
                .as_object()
                .unwrap()
                .contains_key("scopes")
        );
        assert!(
            !doc["resources"]["things"]["resources"]["nested"]["methods"]["get"]
                .as_object()
                .unwrap()
                .contains_key("scopes")
        );
        // Methods themselves are still there (we only stripped scopes, not the methods).
        assert_eq!(
            doc["resources"]["things"]["methods"]["lookup"]["httpMethod"],
            json!("POST")
        );
    }

    #[test]
    fn filter_normalizes_leading_slash_for_path_match() {
        let mut doc = json!({
            "openapi": "3.0.0",
            "paths": { "/v1/x": { "get": {} } }
        });
        // YAML path without leading slash should still match.
        let endpoints = vec![ep(Get, "v1/x")];
        filter_to_endpoints(&mut doc, &endpoints);
        assert!(doc["paths"].as_object().unwrap().contains_key("/v1/x"));
    }
}
