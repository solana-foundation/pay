//! Resolve a [`OpenapiSource`] into a list of [`EndpointSpec`] entries.
//!
//! Provider specs can declare their endpoints in one of two mutually-exclusive
//! ways: an inline `endpoints:` list, or an `openapi:` source pointing to (or
//! inlining) an OpenAPI 3 document. When the latter is set the prober walks
//! `paths × methods` to synthesize the candidate endpoint list, which the
//! probe pipeline then hits to determine which are stablecoin-gated.
//!
//! Body generation: for POST/PUT/PATCH operations we also extract (or
//! synthesize) a request body so the probe doesn't get rejected with a 400
//! before reaching the paywall. Priority order:
//!   1. `requestBody.content."application/json".example`
//!   2. `requestBody.content."application/json".examples.<first>.value`
//!   3. `requestBody.content."application/json".schema.example`
//!   4. `requestBody.content."application/json".schema.examples[0]` (3.1)
//!   5. schema-derived dummy values (required fields only, `$ref`-resolved,
//!      format-aware: `email`, `uri`, `date-time`, `uuid`).

use std::time::Duration;

use pay_types::registry::{EndpointSpec, OpenapiSource, ProviderFrontmatter};
use reqwest::blocking::Client;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::{Error, Result};

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];
const FETCH_TIMEOUT_SECS: u64 = 15;
const MAX_SCHEMA_DEPTH: u32 = 6;

/// One endpoint resolved from an OpenAPI document — both the spec entry that
/// gets published to the index and the optional probe body extracted from
/// `requestBody`.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub spec: EndpointSpec,
    /// Serialized JSON body. `None` for GET/DELETE or when the OpenAPI doc
    /// does not declare a request body for the operation.
    pub body_example: Option<String>,
}

/// Fetch / read the document referenced by `source` and synthesize an
/// endpoint list from it.
///
/// `service_url` is used to resolve [`OpenapiSource::Path`] (treated as a path
/// relative to the provider's `service_url`).
pub fn resolve_endpoints(
    source: &OpenapiSource,
    service_url: &str,
) -> Result<Vec<ResolvedEndpoint>> {
    let body = load_document(source, service_url)?;
    parse_endpoints(&body)
}

/// Pure parser — no I/O. Synthesize endpoint specs from an OpenAPI JSON body.
///
/// Dispatches on the document's shape:
/// - **OpenAPI 3 / Swagger 2** (`openapi:` or `swagger:` key): walk
///   `paths.{path}.{method}`.
/// - **Google Discovery** (`kind: discovery#restDescription`): walk
///   `resources.*.methods.*` recursively (and any top-level `methods`).
///
/// Description is taken from the operation's `summary` first, then
/// `description`, falling back to `"<METHOD> <path>"`.
pub fn parse_endpoints(body: &str) -> Result<Vec<ResolvedEndpoint>> {
    let doc: Value = serde_json::from_str(body)
        .map_err(|e| Error::Mpp(format!("OpenAPI document is not valid JSON: {e}")))?;

    let mut endpoints = if doc.get("openapi").is_some() || doc.get("swagger").is_some() {
        parse_openapi3_endpoints(&doc)?
    } else if doc
        .get("kind")
        .and_then(|v| v.as_str())
        .is_some_and(|k| k.starts_with("discovery#"))
    {
        parse_discovery_endpoints(&doc)?
    } else if doc.get("paths").is_some() {
        // Best-effort fallback for OpenAPI-shaped docs missing the marker.
        parse_openapi3_endpoints(&doc)?
    } else if doc.get("resources").is_some() || doc.get("methods").is_some() {
        // Discovery-shaped doc that didn't ship the `kind` marker.
        parse_discovery_endpoints(&doc)?
    } else {
        return Err(Error::Mpp(
            "OpenAPI document has no `paths` (OpenAPI 3) or `resources`/`methods` (Discovery) entries".into(),
        ));
    };

    endpoints.sort_by(|a, b| {
        a.spec
            .path
            .cmp(&b.spec.path)
            .then_with(|| a.spec.method.cmp(&b.spec.method))
    });
    Ok(endpoints)
}

fn parse_openapi3_endpoints(doc: &Value) -> Result<Vec<ResolvedEndpoint>> {
    let paths = doc
        .get("paths")
        .and_then(|v| v.as_object())
        .ok_or_else(|| Error::Mpp("OpenAPI document has no `paths` object".to_string()))?;

    let mut endpoints = Vec::new();
    for (path, item) in paths {
        let item_obj = match item.as_object() {
            Some(obj) => obj,
            None => continue,
        };
        for &method in HTTP_METHODS {
            let Some(op) = item_obj.get(method) else {
                continue;
            };
            let description = op
                .get("summary")
                .and_then(|v| v.as_str())
                .or_else(|| op.get("description").and_then(|v| v.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{} {}", method.to_uppercase(), path));

            let spec = EndpointSpec {
                method: method.to_uppercase(),
                path: normalize_path(path),
                description,
                resource: op
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                pricing: None,
            };

            let body_example = if matches!(method, "post" | "put" | "patch") {
                extract_or_generate_body(op, doc).map(|v| v.to_string())
            } else {
                None
            };

            endpoints.push(ResolvedEndpoint { spec, body_example });
        }
    }
    Ok(endpoints)
}

fn parse_discovery_endpoints(doc: &Value) -> Result<Vec<ResolvedEndpoint>> {
    let mut endpoints = Vec::new();
    if let Some(resources) = doc.get("resources").and_then(|v| v.as_object()) {
        walk_discovery_resources(resources, doc, None, &mut endpoints);
    }
    if let Some(methods) = doc.get("methods").and_then(|v| v.as_object()) {
        emit_discovery_methods(methods, doc, None, &mut endpoints);
    }
    Ok(endpoints)
}

fn walk_discovery_resources(
    resources: &Map<String, Value>,
    root: &Value,
    parent_resource: Option<&str>,
    endpoints: &mut Vec<ResolvedEndpoint>,
) {
    for (name, resource) in resources {
        let resource_path = match parent_resource {
            Some(p) => format!("{p}.{name}"),
            None => name.clone(),
        };
        if let Some(methods) = resource.get("methods").and_then(|v| v.as_object()) {
            emit_discovery_methods(methods, root, Some(&resource_path), endpoints);
        }
        if let Some(nested) = resource.get("resources").and_then(|v| v.as_object()) {
            walk_discovery_resources(nested, root, Some(&resource_path), endpoints);
        }
    }
}

fn emit_discovery_methods(
    methods: &Map<String, Value>,
    root: &Value,
    resource_path: Option<&str>,
    endpoints: &mut Vec<ResolvedEndpoint>,
) {
    for (_, m) in methods {
        let http_method = m
            .get("httpMethod")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let path = m
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue;
        }
        let description = m
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} {}", http_method, path));

        let body_example = if matches!(http_method.as_str(), "POST" | "PUT" | "PATCH") {
            extract_or_generate_discovery_body(m, root).map(|v| v.to_string())
        } else {
            None
        };

        endpoints.push(ResolvedEndpoint {
            spec: EndpointSpec {
                method: http_method,
                path: normalize_path(&path),
                description,
                resource: resource_path.map(str::to_string),
                pricing: None,
            },
            body_example,
        });
    }
}

/// Discovery `request` fields look like `{"$ref": "SchemaName"}` indexing
/// into the top-level `schemas` bucket. Walk that schema with the existing
/// schema-based generator (which tolerates Discovery's `$ref` form because
/// `generate_from_schema` handles unrecognized strings as opaque names).
fn extract_or_generate_discovery_body(method: &Value, root: &Value) -> Option<Value> {
    let request = method.get("request")?;
    let ref_name = request.get("$ref").and_then(|v| v.as_str())?;
    let schema = root.get("schemas").and_then(|s| s.get(ref_name))?.clone();
    // Convert Discovery's `$ref: "Foo"` (bare name) to the JSON-Pointer form
    // `generate_from_schema` understands when it recurses for nested refs.
    let normalized = rewrite_discovery_refs(schema);
    let example = generate_from_schema(&normalized, root, 0);
    if example.is_null() {
        None
    } else {
        Some(example)
    }
}

/// Rewrite every `$ref: "Name"` (Discovery form) inside `schema` to
/// `$ref: "#/schemas/Name"` so [`resolve_ref`] finds the target via
/// `doc.pointer`.
fn rewrite_discovery_refs(mut value: Value) -> Value {
    rewrite_refs_in_place(&mut value);
    value
}

fn rewrite_refs_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get_mut("$ref")
                && !s.starts_with('#')
                && !s.starts_with("http")
            {
                *s = format!("#/schemas/{s}");
            }
            for (_, v) in map.iter_mut() {
                rewrite_refs_in_place(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_refs_in_place(v);
            }
        }
        _ => {}
    }
}

/// Return the effective endpoint list for a provider spec.
///
/// If `spec.openapi` is set, fetch/parse the OpenAPI document and synthesize
/// endpoints from it. Otherwise return `spec.endpoints` as-is wrapped without
/// body examples.
///
/// Use [`effective_openapi`] when you also need the parsed OpenAPI document
/// itself (e.g. to embed it in the published index for offline consumers).
pub fn effective_endpoints(spec: &ProviderFrontmatter) -> Result<Vec<ResolvedEndpoint>> {
    Ok(effective_openapi(spec)?.endpoints)
}

/// Resolved openapi for one provider spec — both the synthesized endpoint
/// list and (when one was loaded) the parsed source document.
#[derive(Debug, Clone)]
pub struct ResolvedOpenapi {
    pub endpoints: Vec<ResolvedEndpoint>,
    /// The parsed OpenAPI / Discovery JSON. `None` when the spec uses
    /// inline `endpoints:` (no source document) — present whenever
    /// `openapi:` is set and the document fetched/parsed successfully.
    pub document: Option<Value>,
}

/// Like [`effective_endpoints`] but also returns the parsed source document
/// when one was loaded. Used by `pay skills build` to inline the full
/// OpenAPI doc in each provider's detail JSON so consumers get schemas and
/// types without a follow-up HTTP round-trip after `pay skills update`.
pub fn effective_openapi(spec: &ProviderFrontmatter) -> Result<ResolvedOpenapi> {
    match &spec.openapi {
        Some(source) => {
            let body = load_document(source, &spec.meta.service_url)?;
            let endpoints = parse_endpoints(&body)?;
            let document = serde_json::from_str::<Value>(&body).ok();
            Ok(ResolvedOpenapi {
                endpoints,
                document,
            })
        }
        None => Ok(ResolvedOpenapi {
            endpoints: spec
                .endpoints
                .iter()
                .cloned()
                .map(|spec| ResolvedEndpoint {
                    spec,
                    body_example: None,
                })
                .collect(),
            document: None,
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestValidationOutcome {
    Valid,
    NotInSpec,
    Invalid(RequestValidationFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestValidationFailure {
    pub method: String,
    pub path: String,
    pub problems: Vec<String>,
    pub example: Option<String>,
    pub allowed_methods: Vec<String>,
}

/// Validate a request body against a cached OpenAPI / Discovery document.
///
/// Returns [`RequestValidationOutcome::NotInSpec`] when the path cannot be
/// matched to the document; callers should treat that as "no local validation"
/// rather than as a request error.
pub fn validate_request(
    doc: &Value,
    method: &str,
    relative_path: &str,
    query_params: &[(String, String)],
    body: Option<&str>,
) -> RequestValidationOutcome {
    let method = method.trim().to_ascii_uppercase();
    let relative_path = normalize_path(relative_path);
    if doc.get("openapi").is_some() || doc.get("swagger").is_some() || doc.get("paths").is_some() {
        return validate_openapi3_request(doc, &method, &relative_path, query_params, body);
    }
    if doc.get("resources").is_some() || doc.get("methods").is_some() {
        return validate_discovery_request(doc, &method, &relative_path, body);
    }
    RequestValidationOutcome::NotInSpec
}

pub fn validate_request_body(
    doc: &Value,
    method: &str,
    relative_path: &str,
    body: Option<&str>,
) -> RequestValidationOutcome {
    validate_request(doc, method, relative_path, &[], body)
}

fn validate_openapi3_request(
    doc: &Value,
    method: &str,
    relative_path: &str,
    query_params: &[(String, String)],
    body: Option<&str>,
) -> RequestValidationOutcome {
    let Some(paths) = doc.get("paths").and_then(|v| v.as_object()) else {
        return RequestValidationOutcome::NotInSpec;
    };
    let Some((spec_path, item)) = paths
        .iter()
        .find(|(path, _)| path_template_matches(&normalize_path(path), relative_path))
    else {
        return RequestValidationOutcome::NotInSpec;
    };

    let Some(item_obj) = item.as_object() else {
        return RequestValidationOutcome::NotInSpec;
    };
    let allowed_methods = allowed_methods(item_obj);
    let method_key = method.to_ascii_lowercase();
    let Some(operation) = item_obj.get(&method_key) else {
        return RequestValidationOutcome::Invalid(RequestValidationFailure {
            method: method.to_string(),
            path: normalize_path(spec_path),
            problems: vec![format!(
                "method `{method}` is not declared for this endpoint; allowed methods: {}",
                if allowed_methods.is_empty() {
                    "(none)".to_string()
                } else {
                    allowed_methods.join(", ")
                }
            )],
            example: None,
            allowed_methods,
        });
    };

    let spec_path = normalize_path(spec_path);
    let mut query_problems =
        validate_openapi_query_parameters(doc, item_obj, operation, query_params);
    match validate_openapi_operation_body(doc, method, &spec_path, operation, body) {
        RequestValidationOutcome::Valid if query_problems.is_empty() => {
            RequestValidationOutcome::Valid
        }
        RequestValidationOutcome::Valid => {
            RequestValidationOutcome::Invalid(RequestValidationFailure {
                method: method.to_string(),
                path: spec_path,
                problems: query_problems,
                example: None,
                allowed_methods: Vec::new(),
            })
        }
        RequestValidationOutcome::Invalid(mut failure) => {
            query_problems.append(&mut failure.problems);
            failure.problems = query_problems;
            RequestValidationOutcome::Invalid(failure)
        }
        RequestValidationOutcome::NotInSpec => RequestValidationOutcome::NotInSpec,
    }
}

fn validate_discovery_request(
    doc: &Value,
    method: &str,
    relative_path: &str,
    body: Option<&str>,
) -> RequestValidationOutcome {
    let mut methods = Vec::new();
    if let Some(resources) = doc.get("resources").and_then(|v| v.as_object()) {
        collect_discovery_methods(resources, &mut methods);
    }
    if let Some(top_level_methods) = doc.get("methods").and_then(|v| v.as_object()) {
        for method_doc in top_level_methods.values() {
            methods.push(method_doc);
        }
    }

    let path_matches: Vec<_> = methods
        .into_iter()
        .filter_map(|method_doc| {
            let path = method_doc.get("path").and_then(|v| v.as_str())?;
            path_template_matches(&normalize_path(path), relative_path)
                .then_some((normalize_path(path), method_doc))
        })
        .collect();
    let Some((spec_path, method_doc)) = path_matches
        .iter()
        .find(|(_, method_doc)| {
            method_doc
                .get("httpMethod")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .eq_ignore_ascii_case(method)
        })
        .map(|(path, method_doc)| (path.as_str(), *method_doc))
    else {
        if path_matches.is_empty() {
            return RequestValidationOutcome::NotInSpec;
        }
        let mut allowed: Vec<String> = path_matches
            .iter()
            .filter_map(|(_, method_doc)| method_doc.get("httpMethod").and_then(|v| v.as_str()))
            .map(|method| method.to_ascii_uppercase())
            .collect();
        allowed.sort();
        allowed.dedup();
        return RequestValidationOutcome::Invalid(RequestValidationFailure {
            method: method.to_string(),
            path: path_matches[0].0.clone(),
            problems: vec![format!(
                "method `{method}` is not declared for this endpoint; allowed methods: {}",
                allowed.join(", ")
            )],
            example: None,
            allowed_methods: allowed,
        });
    };

    validate_discovery_operation_body(doc, method, spec_path, method_doc, body)
}

fn validate_openapi_query_parameters(
    doc: &Value,
    path_item: &Map<String, Value>,
    operation: &Value,
    query_params: &[(String, String)],
) -> Vec<String> {
    let parameters = collect_openapi_query_parameters(
        doc,
        path_item.get("parameters"),
        operation.get("parameters"),
    );
    let mut problems = Vec::new();
    for parameter in parameters {
        let name = match parameter.get("name").and_then(|v| v.as_str()) {
            Some(name) if !name.is_empty() => name,
            _ => continue,
        };
        let values = query_param_values(query_params, name);
        let required = parameter
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let schema = parameter.get("schema");
        if values.is_empty() {
            if required {
                let provided = provided_query_params(query_params);
                let suffix = if provided.is_empty() {
                    String::new()
                } else {
                    format!("; provided query parameters: {provided}")
                };
                problems.push(format!("query.{name} is required{suffix}"));
            }
            continue;
        }
        let value = query_value_for_schema(&values, schema);
        if let Some(schema) = schema {
            validate_schema_value(
                &format!("query.{name}"),
                &value,
                schema,
                doc,
                &mut problems,
                0,
            );
        }
    }
    problems
}

fn collect_openapi_query_parameters(
    doc: &Value,
    path_parameters: Option<&Value>,
    operation_parameters: Option<&Value>,
) -> Vec<Value> {
    let mut by_name = Map::new();
    for parameter in path_parameters
        .into_iter()
        .chain(operation_parameters)
        .filter_map(|parameters| parameters.as_array())
        .flatten()
        .filter_map(|parameter| resolve_parameter(parameter, doc))
    {
        if parameter.get("in").and_then(|v| v.as_str()) != Some("query") {
            continue;
        }
        if let Some(name) = parameter.get("name").and_then(|v| v.as_str()) {
            by_name.insert(name.to_string(), parameter);
        }
    }
    by_name.into_values().collect()
}

fn resolve_parameter(parameter: &Value, doc: &Value) -> Option<Value> {
    if let Some(ref_str) = parameter.get("$ref").and_then(|v| v.as_str()) {
        return resolve_ref(ref_str, doc);
    }
    Some(parameter.clone())
}

fn query_param_values<'a>(query_params: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    query_params
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .collect()
}

fn provided_query_params(query_params: &[(String, String)]) -> String {
    let mut names: Vec<&str> = query_params.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

fn query_value_for_schema(values: &[&str], schema: Option<&Value>) -> Value {
    if schema.is_some_and(|schema| schema_type_names(schema).iter().any(|ty| ty == "array")) {
        let item_schema = schema.and_then(|schema| schema.get("items"));
        return Value::Array(
            values
                .iter()
                .map(|value| coerce_query_scalar(value, item_schema))
                .collect(),
        );
    }
    coerce_query_scalar(values.first().copied().unwrap_or_default(), schema)
}

fn coerce_query_scalar(value: &str, schema: Option<&Value>) -> Value {
    let types = schema.map(schema_type_names).unwrap_or_default();
    if types.iter().any(|ty| ty == "integer")
        && let Ok(n) = value.parse::<i64>()
    {
        return json!(n);
    }
    if types.iter().any(|ty| ty == "number")
        && let Ok(n) = value.parse::<f64>()
    {
        return json!(n);
    }
    if types.iter().any(|ty| ty == "boolean")
        && let Ok(b) = value.parse::<bool>()
    {
        return json!(b);
    }
    json!(value)
}

fn collect_discovery_methods<'a>(resources: &'a Map<String, Value>, methods: &mut Vec<&'a Value>) {
    for resource in resources.values() {
        if let Some(resource_methods) = resource.get("methods").and_then(|v| v.as_object()) {
            methods.extend(resource_methods.values());
        }
        if let Some(nested) = resource.get("resources").and_then(|v| v.as_object()) {
            collect_discovery_methods(nested, methods);
        }
    }
}

fn validate_openapi_operation_body(
    doc: &Value,
    method: &str,
    spec_path: &str,
    operation: &Value,
    body: Option<&str>,
) -> RequestValidationOutcome {
    let body = non_empty_body(body);
    let request_body = operation
        .get("requestBody")
        .and_then(|request_body| resolve_request_body(request_body, doc));

    let Some(request_body) = request_body else {
        if body.is_some() {
            return RequestValidationOutcome::Invalid(RequestValidationFailure {
                method: method.to_string(),
                path: spec_path.to_string(),
                problems: vec![
                    "OpenAPI operation does not declare a request body; remove the body or select an endpoint that accepts one".to_string(),
                ],
                example: None,
                allowed_methods: Vec::new(),
            });
        }
        return RequestValidationOutcome::Valid;
    };

    let required = request_body
        .get("required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let content = request_body.get("content").and_then(|v| v.as_object());
    let Some(json_media) = content.and_then(select_json_media) else {
        let content_types = content
            .map(|content| content.keys().cloned().collect::<Vec<_>>().join(", "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(none declared)".to_string());
        if required || body.is_some() {
            return RequestValidationOutcome::Invalid(RequestValidationFailure {
                method: method.to_string(),
                path: spec_path.to_string(),
                problems: vec![format!(
                    "OpenAPI requestBody does not declare an application/json schema; declared content types: {content_types}"
                )],
                example: None,
                allowed_methods: Vec::new(),
            });
        }
        return RequestValidationOutcome::Valid;
    };

    let schema = json_media.get("schema");
    validate_body_against_schema(
        doc,
        method,
        spec_path,
        body,
        required,
        schema,
        extract_or_generate_body(operation, doc).map(|value| value.to_string()),
    )
}

fn validate_discovery_operation_body(
    doc: &Value,
    method: &str,
    spec_path: &str,
    operation: &Value,
    body: Option<&str>,
) -> RequestValidationOutcome {
    let body = non_empty_body(body);
    let request_ref = operation
        .get("request")
        .and_then(|request| request.get("$ref"))
        .and_then(|v| v.as_str());
    let schema = request_ref
        .and_then(|name| doc.get("schemas").and_then(|schemas| schemas.get(name)))
        .cloned()
        .map(rewrite_discovery_refs);
    let example = extract_or_generate_discovery_body(operation, doc).map(|value| value.to_string());

    if schema.is_none() && body.is_some() {
        return RequestValidationOutcome::Invalid(RequestValidationFailure {
            method: method.to_string(),
            path: spec_path.to_string(),
            problems: vec![
                "Discovery operation does not declare a request schema; remove the body or select an endpoint that accepts one".to_string(),
            ],
            example: None,
            allowed_methods: Vec::new(),
        });
    }

    validate_body_against_schema(
        doc,
        method,
        spec_path,
        body,
        false,
        schema.as_ref(),
        example,
    )
}

fn validate_body_against_schema(
    doc: &Value,
    method: &str,
    spec_path: &str,
    body: Option<&str>,
    request_body_required: bool,
    schema: Option<&Value>,
    example: Option<String>,
) -> RequestValidationOutcome {
    let schema_requires_body = schema.is_some_and(schema_has_required_fields);
    let Some(body) = body else {
        if request_body_required || schema_requires_body {
            return RequestValidationOutcome::Invalid(RequestValidationFailure {
                method: method.to_string(),
                path: spec_path.to_string(),
                problems: vec![required_body_message(schema)],
                example,
                allowed_methods: Vec::new(),
            });
        }
        return RequestValidationOutcome::Valid;
    };

    let Some(schema) = schema else {
        return RequestValidationOutcome::Valid;
    };
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(err) => {
            return RequestValidationOutcome::Invalid(RequestValidationFailure {
                method: method.to_string(),
                path: spec_path.to_string(),
                problems: vec![format!(
                    "body must be valid JSON for this catalog endpoint: {err}"
                )],
                example,
                allowed_methods: Vec::new(),
            });
        }
    };

    let mut problems = Vec::new();
    validate_schema_value("body", &value, schema, doc, &mut problems, 0);
    if problems.is_empty() {
        RequestValidationOutcome::Valid
    } else {
        RequestValidationOutcome::Invalid(RequestValidationFailure {
            method: method.to_string(),
            path: spec_path.to_string(),
            problems,
            example,
            allowed_methods: Vec::new(),
        })
    }
}

fn non_empty_body(body: Option<&str>) -> Option<&str> {
    body.map(str::trim).filter(|body| !body.is_empty())
}

fn resolve_request_body(request_body: &Value, doc: &Value) -> Option<Value> {
    if let Some(ref_str) = request_body.get("$ref").and_then(|v| v.as_str()) {
        return resolve_ref(ref_str, doc);
    }
    Some(request_body.clone())
}

fn select_json_media(content: &Map<String, Value>) -> Option<&Value> {
    content.get("application/json").or_else(|| {
        content
            .iter()
            .find(|(content_type, _)| {
                let content_type = content_type
                    .split(';')
                    .next()
                    .unwrap_or(content_type)
                    .trim()
                    .to_ascii_lowercase();
                content_type.ends_with("+json") || content_type.ends_with("/json")
            })
            .map(|(_, media)| media)
    })
}

fn allowed_methods(item: &Map<String, Value>) -> Vec<String> {
    let mut methods: Vec<String> = HTTP_METHODS
        .iter()
        .filter(|method| item.contains_key(**method))
        .map(|method| method.to_ascii_uppercase())
        .collect();
    methods.sort();
    methods
}

fn required_body_message(schema: Option<&Value>) -> String {
    let required = schema
        .and_then(required_fields)
        .filter(|fields| !fields.is_empty())
        .map(|fields| format!(" Required fields: {}.", fields.join(", ")))
        .unwrap_or_default();
    format!("request body is required by the catalog OpenAPI schema.{required}")
}

fn schema_has_required_fields(schema: &Value) -> bool {
    required_fields(schema).is_some_and(|fields| !fields.is_empty())
}

fn required_fields(schema: &Value) -> Option<Vec<String>> {
    schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field.as_str().map(str::to_string))
                .collect()
        })
}

fn validate_schema_value(
    path: &str,
    value: &Value,
    schema: &Value,
    doc: &Value,
    problems: &mut Vec<String>,
    depth: u32,
) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    if value.is_null() && schema_allows_null(schema) {
        return;
    }
    if let Some(ref_str) = schema.get("$ref").and_then(|v| v.as_str()) {
        if let Some(resolved) = resolve_ref(ref_str, doc) {
            validate_schema_value(path, value, &resolved, doc, problems, depth + 1);
        }
        return;
    }

    if let Some(arr) = schema.get("allOf").and_then(|v| v.as_array()) {
        for item in arr {
            validate_schema_value(path, value, item, doc, problems, depth + 1);
        }
    }
    for combinator in ["anyOf", "oneOf"] {
        if let Some(arr) = schema.get(combinator).and_then(|v| v.as_array()) {
            let matched = arr.iter().any(|item| {
                let mut nested = Vec::new();
                validate_schema_value(path, value, item, doc, &mut nested, depth + 1);
                nested.is_empty()
            });
            if !matched {
                problems.push(format!(
                    "{path} must match one of the `{combinator}` schema variants"
                ));
            }
            return;
        }
    }

    if let Some(expected) = schema.get("const")
        && expected != value
    {
        problems.push(format!("{path} must equal `{expected}`"));
    }
    if let Some(enums) = schema.get("enum").and_then(|v| v.as_array())
        && !enums.iter().any(|candidate| candidate == value)
    {
        let values = enums
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        problems.push(format!("{path} must be one of: {values}"));
    }

    let expected_types = schema_type_names(schema);
    if !expected_types.is_empty()
        && !expected_types
            .iter()
            .any(|ty| value_matches_type(value, ty))
    {
        problems.push(format!(
            "{path} must be {}, got {}",
            expected_types.join(" or "),
            value_type_name(value)
        ));
        return;
    }

    if value.is_object() || schema.get("properties").is_some() {
        validate_object(path, value, schema, doc, problems, depth);
    } else if let Some(arr) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (idx, item) in arr.iter().enumerate() {
            validate_schema_value(
                &format!("{path}[{idx}]"),
                item,
                items,
                doc,
                problems,
                depth + 1,
            );
        }
    }
    validate_scalar_constraints(path, value, schema, problems);
}

fn validate_object(
    path: &str,
    value: &Value,
    schema: &Value,
    doc: &Value,
    problems: &mut Vec<String>,
    depth: u32,
) {
    let Some(obj) = value.as_object() else {
        return;
    };
    if let Some(required) = required_fields(schema) {
        for field in required {
            if !obj.contains_key(&field) {
                problems.push(format!("{path}.{field} is required"));
            }
        }
    }
    let props = schema.get("properties").and_then(|v| v.as_object());
    if let Some(props) = props {
        for (field, field_schema) in props {
            if let Some(field_value) = obj.get(field) {
                validate_schema_value(
                    &format!("{path}.{field}"),
                    field_value,
                    field_schema,
                    doc,
                    problems,
                    depth + 1,
                );
            }
        }
    }
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            if let Some(props) = props {
                for field in obj.keys() {
                    if !props.contains_key(field) {
                        problems.push(format!("{path}.{field} is not allowed"));
                    }
                }
            }
        }
        Some(schema @ Value::Object(_)) => {
            if let Some(props) = props {
                for (field, field_value) in obj {
                    if !props.contains_key(field) {
                        validate_schema_value(
                            &format!("{path}.{field}"),
                            field_value,
                            schema,
                            doc,
                            problems,
                            depth + 1,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn validate_scalar_constraints(
    path: &str,
    value: &Value,
    schema: &Value,
    problems: &mut Vec<String>,
) {
    if let Some(min_len) = schema.get("minLength").and_then(|v| v.as_u64())
        && let Some(value) = value.as_str()
        && value.chars().count() < min_len as usize
    {
        problems.push(format!("{path} must have at least {min_len} characters"));
    }
    if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64())
        && let Some(value) = value.as_f64()
        && value < min
    {
        problems.push(format!("{path} must be >= {min}"));
    }
}

fn schema_type_names(schema: &Value) -> Vec<String> {
    match schema.get("type") {
        Some(Value::String(ty)) if ty != "null" => vec![ty.clone()],
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(|ty| ty.as_str())
            .filter(|ty| *ty != "null")
            .map(str::to_string)
            .collect(),
        _ if schema.get("properties").is_some() => vec!["object".to_string()],
        _ if schema.get("items").is_some() => vec!["array".to_string()],
        _ => Vec::new(),
    }
}

fn schema_allows_null(schema: &Value) -> bool {
    schema
        .get("nullable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || matches!(
            schema.get("type"),
            Some(Value::String(ty)) if ty == "null"
        )
        || schema
            .get("type")
            .and_then(|v| v.as_array())
            .is_some_and(|types| types.iter().any(|ty| ty.as_str() == Some("null")))
}

fn value_matches_type(value: &Value, ty: &str) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
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

fn load_document(source: &OpenapiSource, _service_url: &str) -> Result<String> {
    match source {
        OpenapiSource::Url { url } => {
            // Registry providers must use a fully-qualified https:// URL —
            // validation rejects anything else. We don't accept relative URLs
            // because the registry is consumed remotely and resolving against
            // `service_url` would be fragile/ambiguous.
            fetch(url)
        }
        OpenapiSource::Path { path } => Err(Error::Mpp(format!(
            "openapi.path ({path}) is filesystem-only (used by `pay server start --openapi`); registry providers must use `openapi: {{ url: <https URL> }}`"
        ))),
        OpenapiSource::Content { content } => Ok(content.clone()),
    }
}

fn fetch(url: &str) -> Result<String> {
    debug!(%url, "Fetching OpenAPI document");
    let client = Client::builder()
        .user_agent(format!("pay-skills/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create HTTP client: {e}")))?;

    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::Mpp(format!("OpenAPI fetch failed for {url}: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| Error::Mpp(format!("OpenAPI fetch read body failed for {url}: {e}")))?;
    if !status.is_success() {
        return Err(Error::Mpp(format!(
            "OpenAPI fetch returned {status} for {url}"
        )));
    }
    Ok(body)
}

#[cfg(test)]
fn join_url(service_url: &str, path: &str) -> String {
    let base = service_url.trim_end_matches('/');
    let suffix = path.trim_start_matches('/');
    format!("{base}/{suffix}")
}

/// OpenAPI paths are absolute (`/foo/bar`); the registry stores them
/// relative to `service_url` (`foo/bar`). Trim the leading `/` to match.
fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

// ── Body example extraction & schema-driven generation ──────────────────────

/// Extract a JSON request body example for an operation. Returns `None` when
/// no `application/json` requestBody is declared (and `None` when the body
/// would be a useless `null`).
fn extract_or_generate_body(op: &Value, doc: &Value) -> Option<Value> {
    let content = op.get("requestBody")?.get("content")?;
    // Pick application/json if present, else first content type.
    let json_media = content
        .get("application/json")
        .or_else(|| content.as_object().and_then(|m| m.values().next()))?;

    // 1. Operation-level example
    if let Some(ex) = json_media.get("example") {
        return Some(ex.clone());
    }
    // 2. Operation-level examples map (named) — pick the first
    if let Some(ex) = json_media
        .get("examples")
        .and_then(|v| v.as_object())
        .and_then(|m| m.values().next())
        .and_then(|v| v.get("value"))
    {
        return Some(ex.clone());
    }
    // 3-4. Schema-level example/examples
    let schema = json_media.get("schema")?;
    if let Some(ex) = schema.get("example") {
        return Some(ex.clone());
    }
    if let Some(first) = schema
        .get("examples")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
    {
        return Some(first.clone());
    }
    // 5. Generate from schema (with $ref resolution + required-only fields).
    let generated = generate_from_schema(schema, doc, 0);
    if generated.is_null() {
        None
    } else {
        Some(generated)
    }
}

/// Walk a JSON Schema object and produce a minimal example value.
///
/// - resolves `$ref` (only `#/components/schemas/...` form)
/// - fills `required` fields, omits optional ones
/// - format-aware string values (`email`, `uri`, `uuid`, `date-time`, `date`)
/// - depth-limited to avoid infinite recursion through self-referential
///   schemas (e.g. tree-like structures)
fn generate_from_schema(schema: &Value, doc: &Value, depth: u32) -> Value {
    if depth > MAX_SCHEMA_DEPTH {
        return Value::Null;
    }

    // $ref — resolve once and recurse on the target.
    if let Some(ref_str) = schema.get("$ref").and_then(|v| v.as_str()) {
        if let Some(resolved) = resolve_ref(ref_str, doc) {
            return generate_from_schema(&resolved, doc, depth + 1);
        }
        return Value::Null;
    }

    // anyOf/oneOf/allOf — pick the first variant to get *something*.
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(arr) = schema.get(combinator).and_then(|v| v.as_array())
            && let Some(first) = arr.first()
        {
            return generate_from_schema(first, doc, depth + 1);
        }
    }

    // Enum first — gives the most realistic example.
    if let Some(first) = schema
        .get("enum")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    // const
    if let Some(c) = schema.get("const") {
        return c.clone();
    }

    let ty = schema.get("type").and_then(|v| v.as_str());
    match ty {
        Some("string") => string_example(schema),
        Some("integer") => integer_example(schema),
        Some("number") => number_example(schema),
        Some("boolean") => json!(false),
        Some("array") => array_example(schema, doc, depth),
        Some("object") | None => object_example(schema, doc, depth),
        Some(other) => {
            debug!(unknown_type = other, "openapi schema: unknown type");
            Value::Null
        }
    }
}

fn string_example(schema: &Value) -> Value {
    let format = schema.get("format").and_then(|v| v.as_str()).unwrap_or("");
    let value = match format {
        "email" => "test@example.com",
        "uri" | "url" | "uri-reference" => "https://example.com",
        "uuid" => "00000000-0000-0000-0000-000000000000",
        "date-time" => "2026-01-01T00:00:00Z",
        "date" => "2026-01-01",
        "ipv4" => "127.0.0.1",
        "ipv6" => "::1",
        "hostname" => "example.com",
        "byte" => "dGVzdA==",
        "binary" => "test",
        _ => "test",
    };
    json!(value)
}

fn integer_example(schema: &Value) -> Value {
    if let Some(min) = schema.get("minimum").and_then(|v| v.as_i64()) {
        return json!(min);
    }
    if let Some(min) = schema.get("exclusiveMinimum").and_then(|v| v.as_i64()) {
        return json!(min + 1);
    }
    json!(1)
}

fn number_example(schema: &Value) -> Value {
    if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
        return json!(min);
    }
    json!(1)
}

fn array_example(schema: &Value, doc: &Value, depth: u32) -> Value {
    let Some(items) = schema.get("items") else {
        return json!([]);
    };
    let item = generate_from_schema(items, doc, depth + 1);
    if item.is_null() {
        json!([])
    } else {
        json!([item])
    }
}

fn object_example(schema: &Value, doc: &Value, depth: u32) -> Value {
    let mut obj = Map::new();
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let props = schema.get("properties").and_then(|v| v.as_object());

    // 1. Required properties — must be present for validation to pass.
    if let Some(props) = props {
        for key in &required {
            if let Some(prop_schema) = props.get(key) {
                let value = generate_from_schema(prop_schema, doc, depth + 1);
                obj.insert(key.clone(), value);
            }
        }
    }

    // 2. If no required fields and no properties, return an empty object.
    if obj.is_empty() && required.is_empty() && props.is_none_or(|p| p.is_empty()) {
        return json!({});
    }

    Value::Object(obj)
}

/// Resolve a `$ref` like `"#/components/schemas/Foo"` against the root doc.
/// Returns `None` for external refs or malformed pointers.
fn resolve_ref(ref_str: &str, doc: &Value) -> Option<Value> {
    let pointer = ref_str.strip_prefix('#')?;
    doc.pointer(pointer).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoints_walks_paths_and_methods() {
        let doc = r#"{
            "openapi": "3.1.0",
            "paths": {
                "/api/register": {
                    "post": {
                        "summary": "Register a new domain",
                        "tags": ["domains"]
                    }
                },
                "/api/domain/dns": {
                    "get": { "summary": "Read DNS records" },
                    "post": { "summary": "Update DNS records" }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert_eq!(endpoints.len(), 3);

        let by_path: std::collections::HashMap<_, _> = endpoints
            .iter()
            .map(|e| ((e.spec.method.as_str(), e.spec.path.as_str()), e))
            .collect();
        assert_eq!(
            by_path[&("POST", "api/register")].spec.description,
            "Register a new domain"
        );
        assert_eq!(
            by_path[&("POST", "api/register")].spec.resource.as_deref(),
            Some("domains")
        );
        assert!(by_path.contains_key(&("GET", "api/domain/dns")));
        assert!(by_path.contains_key(&("POST", "api/domain/dns")));
    }

    #[test]
    fn parse_endpoints_falls_back_to_method_path_when_no_description() {
        let doc = r#"{ "paths": { "/x": { "get": {} } } }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].spec.description, "GET /x");
    }

    #[test]
    fn parse_endpoints_prefers_summary_over_description() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "get": {
                        "summary": "short summary",
                        "description": "long description"
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert_eq!(endpoints[0].spec.description, "short summary");
    }

    #[test]
    fn parse_endpoints_skips_non_method_keys() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "summary": "common summary",
                    "parameters": [],
                    "get": { "summary": "g" }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].spec.method, "GET");
    }

    #[test]
    fn validate_request_body_accepts_matching_json() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/v1/search": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["query"],
                                        "properties": {
                                            "query": { "type": "string", "minLength": 1 },
                                            "limit": { "type": "integer", "minimum": 1 }
                                        },
                                        "additionalProperties": false
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        assert_eq!(
            validate_request_body(
                &doc,
                "POST",
                "v1/search",
                Some(r#"{"query":"solana","limit":1}"#)
            ),
            RequestValidationOutcome::Valid
        );
    }

    #[test]
    fn validate_request_body_reports_required_fields_and_type_errors() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/v1/search": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["query"],
                                        "properties": {
                                            "query": { "type": "string" },
                                            "limit": { "type": "integer" }
                                        },
                                        "additionalProperties": false
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        let outcome = validate_request_body(
            &doc,
            "POST",
            "v1/search",
            Some(r#"{"limit":"many","extra":true}"#),
        );
        let RequestValidationOutcome::Invalid(failure) = outcome else {
            panic!("expected invalid body");
        };
        assert!(
            failure
                .problems
                .iter()
                .any(|problem| problem == "body.query is required")
        );
        assert!(
            failure
                .problems
                .iter()
                .any(|problem| problem == "body.limit must be integer, got string")
        );
        assert!(
            failure
                .problems
                .iter()
                .any(|problem| problem == "body.extra is not allowed")
        );
    }

    #[test]
    fn validate_request_body_reports_missing_body_with_example() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/v1/search": {
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["query"],
                                        "properties": {
                                            "query": { "type": "string" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        let outcome = validate_request_body(&doc, "POST", "v1/search", None);
        let RequestValidationOutcome::Invalid(failure) = outcome else {
            panic!("expected invalid body");
        };
        assert_eq!(
            failure.problems,
            vec!["request body is required by the catalog OpenAPI schema. Required fields: query."]
        );
        assert_eq!(failure.example.as_deref(), Some(r#"{"query":"test"}"#));
    }

    #[test]
    fn validate_request_body_rejects_wrong_method_for_known_path() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/v1/search": {
                    "post": {}
                }
            }
        });

        let outcome = validate_request_body(&doc, "GET", "v1/search", None);
        let RequestValidationOutcome::Invalid(failure) = outcome else {
            panic!("expected invalid method");
        };
        assert_eq!(failure.allowed_methods, vec!["POST"]);
        assert!(failure.problems[0].contains("method `GET` is not declared for this endpoint"));
    }

    #[test]
    fn validate_request_reports_missing_required_query_parameter() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/x402/search": {
                    "get": {
                        "parameters": [
                            {
                                "in": "query",
                                "name": "q",
                                "required": true,
                                "schema": {
                                    "type": "string",
                                    "minLength": 1
                                }
                            },
                            {
                                "in": "query",
                                "name": "page",
                                "schema": {
                                    "type": "integer",
                                    "minimum": 1
                                }
                            }
                        ]
                    }
                }
            }
        });

        let outcome = validate_request(
            &doc,
            "GET",
            "x402/search",
            &[
                ("query".to_string(), "bottled water".to_string()),
                ("limit".to_string(), "5".to_string()),
            ],
            None,
        );
        let RequestValidationOutcome::Invalid(failure) = outcome else {
            panic!("expected invalid query");
        };
        assert_eq!(failure.path, "x402/search");
        assert!(failure.problems.iter().any(
            |problem| problem == "query.q is required; provided query parameters: limit, query"
        ));
    }

    #[test]
    fn validate_request_accepts_and_type_checks_query_parameters() {
        let doc = json!({
            "openapi": "3.1.0",
            "paths": {
                "/x402/search": {
                    "get": {
                        "parameters": [
                            {
                                "in": "query",
                                "name": "q",
                                "required": true,
                                "schema": { "type": "string", "minLength": 1 }
                            },
                            {
                                "in": "query",
                                "name": "page",
                                "schema": { "type": "integer", "minimum": 1 }
                            }
                        ]
                    }
                }
            }
        });

        assert_eq!(
            validate_request(
                &doc,
                "GET",
                "x402/search",
                &[
                    ("q".to_string(), "bottled water".to_string()),
                    ("page".to_string(), "2".to_string()),
                ],
                None,
            ),
            RequestValidationOutcome::Valid
        );

        let outcome = validate_request(
            &doc,
            "GET",
            "x402/search",
            &[
                ("q".to_string(), "bottled water".to_string()),
                ("page".to_string(), "zero".to_string()),
            ],
            None,
        );
        let RequestValidationOutcome::Invalid(failure) = outcome else {
            panic!("expected invalid query type");
        };
        assert!(
            failure
                .problems
                .iter()
                .any(|problem| problem == "query.page must be integer, got string")
        );
    }

    #[test]
    fn parse_endpoints_rejects_missing_paths() {
        let doc = r#"{ "openapi": "3.1.0" }"#;
        let err = parse_endpoints(doc).unwrap_err();
        assert!(format!("{err:?}").contains("`paths`"));
    }

    #[test]
    fn parse_endpoints_rejects_invalid_json() {
        let err = parse_endpoints("{not json").unwrap_err();
        assert!(format!("{err:?}").contains("not valid JSON"));
    }

    #[test]
    fn parse_endpoints_emits_stable_ordering() {
        let doc = r#"{
            "paths": {
                "/b": { "post": {} },
                "/a": { "get": {} },
                "/a/sub": { "get": {} }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let order: Vec<_> = endpoints
            .iter()
            .map(|e| (e.spec.method.as_str(), e.spec.path.as_str()))
            .collect();
        assert_eq!(order, vec![("GET", "a"), ("GET", "a/sub"), ("POST", "b"),]);
    }

    #[test]
    fn join_url_handles_trailing_and_leading_slashes() {
        assert_eq!(
            join_url("https://api.example.com/", "/openapi.json"),
            "https://api.example.com/openapi.json"
        );
        assert_eq!(
            join_url("https://api.example.com", "openapi.json"),
            "https://api.example.com/openapi.json"
        );
    }

    #[test]
    fn load_document_returns_inline_content() {
        let content = "{\"paths\": {}}".to_string();
        let src = OpenapiSource::Content {
            content: content.clone(),
        };
        let body = load_document(&src, "https://api.example.com").unwrap();
        assert_eq!(body, content);
    }

    // ── Body example tests ──

    #[test]
    fn body_example_uses_operation_level_example() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "example": { "domain": "example.com" }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"domain": "example.com"})
        );
    }

    #[test]
    fn body_example_uses_named_examples_first_value() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "examples": {
                                        "default": { "value": { "name": "alice" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"name": "alice"})
        );
    }

    #[test]
    fn body_example_falls_back_to_schema_example() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": { "example": { "k": 1 } }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            json!({"k": 1})
        );
    }

    #[test]
    fn body_example_generates_from_required_schema_fields() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["domain", "tld"],
                                        "properties": {
                                            "domain": { "type": "string" },
                                            "tld": { "type": "string", "enum": ["com", "org"] },
                                            "optional_field": { "type": "string" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["domain"], json!("test"));
        assert_eq!(parsed["tld"], json!("com"));
        // Optional field is not included.
        assert!(parsed.get("optional_field").is_none());
    }

    #[test]
    fn body_example_uses_format_hints() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["email", "url", "id"],
                                        "properties": {
                                            "email": { "type": "string", "format": "email" },
                                            "url": { "type": "string", "format": "uri" },
                                            "id": { "type": "string", "format": "uuid" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["email"], json!("test@example.com"));
        assert_eq!(parsed["url"], json!("https://example.com"));
        assert_eq!(parsed["id"], json!("00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn body_example_resolves_refs() {
        let doc = r##"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/Foo" }
                                }
                            }
                        }
                    }
                }
            },
            "components": {
                "schemas": {
                    "Foo": {
                        "type": "object",
                        "required": ["bar"],
                        "properties": {
                            "bar": { "type": "integer", "minimum": 5 }
                        }
                    }
                }
            }
        }"##;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["bar"], json!(5));
    }

    #[test]
    fn body_example_handles_arrays_and_nested_objects() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "post": {
                        "requestBody": {
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "required": ["items"],
                                        "properties": {
                                            "items": {
                                                "type": "array",
                                                "items": {
                                                    "type": "object",
                                                    "required": ["name"],
                                                    "properties": {
                                                        "name": { "type": "string" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        let body = endpoints[0].body_example.as_deref().unwrap();
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["items"], json!([{"name": "test"}]));
    }

    #[test]
    fn body_example_is_none_for_get() {
        let doc = r#"{
            "paths": {
                "/x": {
                    "get": {
                        "requestBody": {
                            "content": {
                                "application/json": { "example": { "k": 1 } }
                            }
                        }
                    }
                }
            }
        }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert!(endpoints[0].body_example.is_none());
    }

    #[test]
    fn body_example_is_none_when_no_request_body() {
        let doc = r#"{ "paths": { "/x": { "post": {} } } }"#;
        let endpoints = parse_endpoints(doc).unwrap();
        assert!(endpoints[0].body_example.is_none());
    }
}
