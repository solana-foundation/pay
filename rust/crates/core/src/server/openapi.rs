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
    let allowed: HashSet<(String, String)> = endpoints
        .iter()
        .map(|e| {
            (
                http_method_str(&e.method).to_string(),
                normalize_path(&e.path),
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

/// Rewrite the document's base-URL fields to `public_url`.
///
/// - OpenAPI 3: every `servers[].url` is replaced with `public_url`.
/// - Discovery: `rootUrl` is set to `public_url` (with trailing `/` so
///   `rootUrl + servicePath` still composes correctly); `baseUrl` and
///   `mtlsRootUrl` are likewise rewritten when present.
pub fn rewrite_urls(doc: &mut Value, public_url: &str) {
    let trimmed = public_url.trim_end_matches('/').to_string();
    let with_slash = format!("{trimmed}/");

    if let Some(servers) = doc.get_mut("servers").and_then(|v| v.as_array_mut()) {
        for entry in servers {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("url".to_string(), Value::String(trimmed.clone()));
            }
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

/// Trim a leading `/` so YAML paths (`v1/foo`) compare equal to OpenAPI/
/// Discovery paths (`/v1/foo`).
fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_string()
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
    let Some(paths) = doc.get_mut("paths").and_then(|v| v.as_object_mut()) else {
        return;
    };
    let mut empty_paths: Vec<String> = Vec::new();
    for (path, item) in paths.iter_mut() {
        let normalized = normalize_path(path);
        let Some(item_obj) = item.as_object_mut() else {
            continue;
        };
        let methods_to_remove: Vec<String> = HTTP_METHODS
            .iter()
            .filter(|m| item_obj.contains_key(**m))
            .filter(|m| !allowed.contains(&(m.to_uppercase(), normalized.clone())))
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
                if allowed.contains(&(http_method, normalize_path(path))) {
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
    fn rewrite_urls_updates_openapi3_servers() {
        let mut doc = json!({
            "openapi": "3.1.0",
            "servers": [
                {"url": "https://upstream.example.com/foo"},
                {"url": "https://other.example.com/"}
            ]
        });
        rewrite_urls(&mut doc, "https://proxy.example.com");
        let servers = doc["servers"].as_array().unwrap();
        assert_eq!(servers[0]["url"], json!("https://proxy.example.com"));
        assert_eq!(servers[1]["url"], json!("https://proxy.example.com"));
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
