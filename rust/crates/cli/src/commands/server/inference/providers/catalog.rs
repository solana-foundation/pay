//! Hosted providers resolved from the pay skills catalog.
//!
//! A [`CatalogProvider`] wraps one catalog entry (fqn + title + service_url +
//! endpoints/pricing) behind the same [`InferenceProvider`] trait the local
//! servers implement, so `pay claude` can list hosted gateways next to
//! Ollama & co. Endpoints, models, and pricing all come from catalog data —
//! nothing about the upstream API is hardcoded here.

use super::{Dialect, InferenceProvider, PaidEndpoint, PricingHint, get_json};

/// Hosted catalog providers appended to the `pay claude` picker by default.
/// An fqn that doesn't resolve (e.g. a skill still being authored) is
/// skipped silently, so it lights up as soon as it is published.
pub const DEFAULT_CATALOG_FQNS: &[&str] = &[
    "solana-foundation/alibaba/modelstudio",
    "solana-foundation/google/generativelanguage",
];

/// A hosted inference provider backed by a resolved catalog entry.
pub struct CatalogProvider {
    fqn: String,
    /// Short name derived from the fqn (last segment).
    slug: String,
    title: String,
    service_url: String,
    endpoints: Vec<pay_core::skills::Endpoint>,
}

impl CatalogProvider {
    /// Build from a catalog [`Service`](pay_core::skills::Service) whose
    /// endpoints have been loaded (see
    /// [`pay_core::skills::ensure_endpoints`]).
    pub fn from_service(svc: &pay_core::skills::Service) -> Self {
        let title = display_title(&svc.fqn, &svc.meta.title);
        Self {
            fqn: svc.fqn.clone(),
            slug: svc.name().to_string(),
            title,
            service_url: svc.meta.service_url.trim_end_matches('/').to_string(),
            endpoints: svc.endpoints.clone(),
        }
    }

    /// The gateway base URL this provider is served from — a valid payer
    /// proxy upstream as-is.
    pub fn service_url(&self) -> &str {
        &self.service_url
    }

    /// The entry's model-list endpoint, when it has one: an unmetered
    /// parameterless GET ending in `models` (e.g. Gemini's
    /// `GET v1beta/models`). Falls back to a metered one if that's all
    /// there is.
    fn model_list_endpoint(&self) -> Option<&pay_core::skills::Endpoint> {
        let is_model_list = |ep: &&pay_core::skills::Endpoint| {
            ep.method.eq_ignore_ascii_case("GET")
                && !ep.path.contains('{')
                && ep
                    .path
                    .to_ascii_lowercase()
                    .trim_end_matches('/')
                    .ends_with("models")
        };
        self.endpoints
            .iter()
            .find(|ep| is_model_list(ep) && ep.pricing.is_none())
            .or_else(|| self.endpoints.iter().find(is_model_list))
    }
}

#[async_trait::async_trait]
impl InferenceProvider for CatalogProvider {
    fn slug(&self) -> &str {
        &self.slug
    }
    fn title(&self) -> &str {
        &self.title
    }
    /// Hosted — no local ports to probe.
    fn ports(&self) -> &[u16] {
        &[]
    }
    fn color(&self) -> Option<&str> {
        if self.fqn.contains("/google/") {
            Some("#4285f4")
        } else if self.fqn.contains("/alibaba/") {
            Some("#ff6a00")
        } else {
            Some("#94a3b8")
        }
    }
    /// Reachability, not identification: the entry came from the trusted
    /// catalog, so any HTTP response (even 4xx) from its gateway means up.
    async fn identify(&self, client: &reqwest::Client, base_url: &str) -> Option<Option<String>> {
        let base = base_url.trim_end_matches('/');
        let url = match self.model_list_endpoint() {
            Some(ep) => format!("{base}/{}", ep.path.trim_start_matches('/')),
            None => base.to_string(),
        };
        if client.get(&url).send().await.is_ok() {
            return Some(None);
        }
        if url != base && client.get(base).send().await.is_ok() {
            return Some(None);
        }
        None
    }
    async fn list_models(&self, client: &reqwest::Client, base_url: &str) -> Vec<String> {
        let Some(ep) = self.model_list_endpoint() else {
            return Vec::new();
        };
        let path = format!("/{}", ep.path.trim_start_matches('/'));
        match get_json(client, base_url.trim_end_matches('/'), &path).await {
            Some(json) => parse_model_names(&json),
            None => Vec::new(),
        }
    }
    /// Metered catalog endpoints, in gate convention (no leading slash).
    fn paid_endpoints(&self) -> Vec<PaidEndpoint> {
        self.endpoints
            .iter()
            .filter(|ep| ep.pricing.is_some())
            .filter_map(|ep| {
                Some(PaidEndpoint {
                    method: parse_method(&ep.method)?,
                    path: ep.path.trim_start_matches('/').to_string(),
                })
            })
            .collect()
    }
    fn dialect(&self) -> Dialect {
        if self.fqn.contains("google/generativelanguage") {
            Dialect::GeminiNative
        } else if self.fqn.contains("alibaba/modelstudio") {
            Dialect::OpenAiCompat
        } else {
            Dialect::Unknown
        }
    }
    /// Min/max `price_usd` across every metered endpoint (all variants and
    /// dimensions folded in), with the unit of the first priced dimension.
    /// This is the model-agnostic aggregate; the picker prefers
    /// [`Self::pricing_hint_for_model`] once a model is chosen.
    fn pricing_hint(&self) -> Option<PricingHint> {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut unit: Option<String> = None;
        for ep in self.endpoints.iter().filter(|ep| ep.pricing.is_some()) {
            if let Some((lo, hi)) = pay_core::skills::price_range_usd(&ep.pricing) {
                min = min.min(lo);
                max = max.max(hi);
            }
            if unit.is_none() {
                unit = first_unit(ep.pricing.as_ref());
            }
        }
        if !min.is_finite() {
            return None;
        }
        Some(PricingHint {
            min_usd: min,
            max_usd: max,
            unit: unit.unwrap_or_else(|| "requests".to_string()),
            // Model-agnostic aggregate: no single input/output pair to show.
            io: None,
        })
    }

    /// Price for `model`, resolved from the catalog `variants[]` when the
    /// provider prices per model (e.g. Gemini's per-model token tiers).
    ///
    /// Matching mirrors the runtime gateway: substring, first match wins,
    /// with a `default` sentinel fallback. Across the matched variant's
    /// dimensions (e.g. input + output token tiers) the min/max `price_usd`
    /// is reported so a single chip conveys the spread. Falls back to the
    /// model-agnostic [`Self::pricing_hint`] when there are no variants or
    /// no model is given.
    fn pricing_hint_for_model(&self, model: Option<&str>) -> Option<PricingHint> {
        let Some(model) = model else {
            return self.pricing_hint();
        };
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut unit: Option<String> = None;
        let mut io: Option<(f64, f64)> = None;
        let mut matched = false;
        for ep in self.endpoints.iter().filter(|ep| ep.pricing.is_some()) {
            let Some(variants) = ep
                .pricing
                .as_ref()
                .and_then(|p| p.get("variants"))
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            let Some(variant) = match_variant(variants, model) else {
                continue;
            };
            matched = true;
            for (lo, hi) in dimension_prices(variant) {
                min = min.min(lo);
                max = max.max(hi);
            }
            // Real input/output token rates for this model (the
            // consolidation win): hosted per-model rows now show the same
            // `in $X · out $Y` chip as local ones.
            if io.is_none() {
                io = directional_io(variant);
            }
            if unit.is_none() {
                unit = variant
                    .get("dimensions")
                    .and_then(|d| d.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|d| d.get("unit"))
                    .and_then(|u| u.as_str())
                    .map(str::to_string);
            }
        }
        if !matched || !min.is_finite() {
            return self.pricing_hint();
        }
        Some(PricingHint {
            min_usd: min,
            max_usd: max,
            unit: unit.unwrap_or_else(|| "tokens".to_string()),
            io,
        })
    }
}

/// The `(input, output)` per-scale token rates for a variant, read from its
/// `direction: input` / `direction: output` dimensions (first tier of each).
/// `None` when the variant doesn't split by direction (so the row falls back
/// to the min/max range display).
fn directional_io(variant: &serde_json::Value) -> Option<(f64, f64)> {
    let dims = variant.get("dimensions").and_then(|d| d.as_array())?;
    let rate_for = |direction: &str| {
        dims.iter()
            .find(|d| d.get("direction").and_then(|v| v.as_str()) == Some(direction))
            .and_then(|d| d.get("tiers"))
            .and_then(|t| t.as_array())
            .and_then(|tiers| tiers.first())
            .and_then(|tier| tier.get("price_usd"))
            .and_then(|p| p.as_f64())
    };
    Some((rate_for("input")?, rate_for("output")?))
}

/// Unit of the first dimension — either a top-level `dimensions[0]` or the
/// first variant's `dimensions[0]`.
fn first_unit(pricing: Option<&serde_json::Value>) -> Option<String> {
    let pricing = pricing?;
    pricing
        .pointer("/dimensions/0/unit")
        .or_else(|| pricing.pointer("/variants/0/dimensions/0/unit"))
        .and_then(|u| u.as_str())
        .map(str::to_string)
}

/// Match a request `model` against catalog `variants[]` the way the runtime
/// gateway does: substring (`variant.value` in `model`), first match wins,
/// so specific variants must precede broader prefixes in the catalog. A
/// variant whose `value` is `default` matches only when nothing else does.
fn match_variant<'a>(
    variants: &'a [serde_json::Value],
    model: &str,
) -> Option<&'a serde_json::Value> {
    let mut default = None;
    for variant in variants {
        let Some(value) = variant.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        if value == "default" {
            default = default.or(Some(variant));
        } else if model.contains(value) {
            return Some(variant);
        }
    }
    default
}

/// Min/max `price_usd` across every tier of a variant's dimensions.
fn dimension_prices(variant: &serde_json::Value) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let Some(dims) = variant.get("dimensions").and_then(|d| d.as_array()) else {
        return out;
    };
    for dim in dims {
        let Some(tiers) = dim.get("tiers").and_then(|t| t.as_array()) else {
            continue;
        };
        let prices: Vec<f64> = tiers
            .iter()
            .filter_map(|t| t.get("price_usd").and_then(|p| p.as_f64()))
            .collect();
        if let (Some(lo), Some(hi)) = (
            prices.iter().copied().reduce(f64::min),
            prices.iter().copied().reduce(f64::max),
        ) {
            out.push((lo, hi));
        }
    }
    out
}

/// Model names from a model-list response body. Understands Gemini's
/// `{"models":[{"name":"models/gemini-…"}]}` (the `models/` prefix is
/// stripped) and the OpenAI-compatible `{"data":[{"id":"…"}]}`.
fn parse_model_names(json: &serde_json::Value) -> Vec<String> {
    if let Some(items) = json.get("models").and_then(|v| v.as_array()) {
        return items
            .iter()
            .filter_map(|item| item.get("name")?.as_str())
            .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
            .collect();
    }
    if let Some(items) = json.get("data").and_then(|v| v.as_array()) {
        return items
            .iter()
            .filter_map(|item| item.get("id")?.as_str().map(str::to_string))
            .collect();
    }
    Vec::new()
}

/// Picker title for a catalog entry. Known default fqns get a short brand
/// name (catalog titles like "Generative Language API (Gemini)" are too
/// verbose for a picker row); everything else uses the catalog title,
/// falling back to the fqn.
fn display_title(fqn: &str, catalog_title: &str) -> String {
    if fqn.contains("google/generativelanguage") {
        return "Google Gemini".to_string();
    }
    if fqn.contains("alibaba/modelstudio") {
        return "Alibaba Model Studio".to_string();
    }
    if catalog_title.trim().is_empty() {
        fqn.to_string()
    } else {
        catalog_title.to_string()
    }
}

fn parse_method(method: &str) -> Option<pay_types::metering::HttpMethod> {
    use pay_types::metering::HttpMethod;
    Some(match method.to_ascii_uppercase().as_str() {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        "PUT" => HttpMethod::Put,
        "PATCH" => HttpMethod::Patch,
        "DELETE" => HttpMethod::Delete,
        _ => return None,
    })
}

/// Resolve `fqns` against the skills catalog into [`CatalogProvider`]s.
///
/// Uses [`pay_core::skills::ensure_endpoints`] — the same lazy detail-fetch
/// path `pay skills show` uses (CDN + `~/.config/pay/skills/detail` cache).
/// Any fqn that fails to resolve (not yet published, detail fetch failed,
/// no `service_url`) is skipped with a debug log, never an error.
pub async fn resolve_catalog_providers(
    catalog: &mut pay_core::skills::Catalog,
    fqns: &[&str],
) -> Vec<CatalogProvider> {
    let mut providers = Vec::new();
    for fqn in fqns {
        if let Err(e) = pay_core::skills::ensure_endpoints(catalog, fqn).await {
            tracing::debug!(fqn, error = %e, "catalog provider unresolved — skipping");
            continue;
        }
        let Some(svc) = catalog
            .providers
            .iter()
            .find(|s| s.fqn.eq_ignore_ascii_case(fqn))
        else {
            tracing::debug!(fqn, "catalog provider missing after resolution — skipping");
            continue;
        };
        if svc.meta.service_url.trim().is_empty() {
            tracing::debug!(fqn, "catalog provider has no service_url — skipping");
            continue;
        }
        providers.push(CatalogProvider::from_service(svc));
    }
    providers
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{base_url, client, rt, stub};
    use super::*;

    /// A mock catalog entry mirroring the live
    /// `solana-foundation/google/generativelanguage` shape.
    fn gemini_service(service_url: &str) -> pay_core::skills::Service {
        serde_json::from_value(serde_json::json!({
            "fqn": "solana-foundation/google/generativelanguage",
            "title": "Generative Language API (Gemini)",
            "category": "ai_ml",
            "service_url": service_url,
            "sha": "7909acc608de86fb",
            "endpoints": [
                {
                    "method": "GET",
                    "path": "v1beta/models",
                    "description": "List available Gemini models."
                },
                {
                    "method": "POST",
                    "path": "v1beta/models/{modelsId}:generateContent",
                    "description": "Generate a model response.",
                    "pricing": {
                        "mode": "flat",
                        "dimensions": [
                            { "unit": "requests", "scale": 1, "tiers": [{ "price_usd": 0.01 }] }
                        ]
                    }
                },
                {
                    "method": "POST",
                    "path": "v1beta/models/{modelsId}:embedContent",
                    "description": "Generate an embedding.",
                    "pricing": {
                        "mode": "flat",
                        "dimensions": [
                            { "unit": "requests", "scale": 1, "tiers": [{ "price_usd": 0.0 }] }
                        ]
                    }
                }
            ]
        }))
        .unwrap()
    }

    fn gemini(service_url: &str) -> CatalogProvider {
        CatalogProvider::from_service(&gemini_service(service_url))
    }

    /// A Gemini entry whose generateContent carries per-model `variants[]`
    /// (the `x-pay-metering` shape), mirroring the agent-gateway YAML:
    /// per-model input/output token tiers plus a `default` fallback.
    fn gemini_variant_priced() -> CatalogProvider {
        let svc: pay_core::skills::Service = serde_json::from_value(serde_json::json!({
            "fqn": "solana-foundation/google/generativelanguage",
            "service_url": "https://generativelanguage.google.gateway-402.com",
            "endpoints": [{
                "method": "POST",
                "path": "v1beta/models/{modelsId}:generateContent",
                "pricing": {
                    "variants": [
                        {
                            "param": "model",
                            "value": "gemini-2.5-flash-lite",
                            "dimensions": [
                                { "direction": "input", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 0.115 }] },
                                { "direction": "output", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 0.46 }] }
                            ]
                        },
                        {
                            "param": "model",
                            "value": "gemini-2.5-flash",
                            "dimensions": [
                                { "direction": "input", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 0.345 }] },
                                { "direction": "output", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 2.875 }] }
                            ]
                        },
                        {
                            "param": "model",
                            "value": "default",
                            "dimensions": [
                                { "direction": "input", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 1.0 }] },
                                { "direction": "output", "unit": "tokens", "scale": 1000000, "tiers": [{ "price_usd": 8.0 }] }
                            ]
                        }
                    ]
                }
            }]
        }))
        .unwrap();
        CatalogProvider::from_service(&svc)
    }

    const GEMINI_MODELS_JSON: &str = r#"{
        "models": [
            { "name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash" },
            { "name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro" }
        ]
    }"#;

    #[test]
    fn identity_comes_from_the_catalog_entry() {
        let provider = gemini("https://generativelanguage.google.gateway-402.com/");
        assert_eq!(provider.slug(), "generativelanguage");
        // Known default fqns get a short brand title for the picker.
        assert_eq!(provider.title(), "Google Gemini");
        assert_eq!(
            provider.service_url(),
            "https://generativelanguage.google.gateway-402.com"
        );
        assert!(provider.ports().is_empty(), "hosted — no local ports");
        assert_eq!(provider.color(), Some("#4285f4"));
    }

    #[test]
    fn display_title_maps_known_fqns_and_falls_back_to_catalog_title() {
        assert_eq!(
            display_title("solana-foundation/google/generativelanguage", "whatever"),
            "Google Gemini"
        );
        assert_eq!(
            display_title("solana-foundation/alibaba/modelstudio", ""),
            "Alibaba Model Studio"
        );
        assert_eq!(display_title("op/other", "Catalog Title"), "Catalog Title");
        assert_eq!(display_title("op/other", "  "), "op/other");
    }

    #[test]
    fn paid_endpoints_are_the_metered_catalog_endpoints() {
        let paid = gemini("https://example.com").paid_endpoints();
        let paths: Vec<&str> = paid.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "v1beta/models/{modelsId}:generateContent",
                "v1beta/models/{modelsId}:embedContent",
            ],
            "only metered endpoints, no leading slash"
        );
        assert!(
            paid.iter()
                .all(|e| matches!(e.method, pay_types::metering::HttpMethod::Post))
        );
    }

    #[test]
    fn pricing_hint_spans_metered_price_range() {
        let hint = gemini("https://example.com").pricing_hint().unwrap();
        assert_eq!(hint.min_usd, 0.0);
        assert_eq!(hint.max_usd, 0.01);
        assert_eq!(hint.unit, "requests");
        assert_eq!(hint.to_string(), "$0.0000–0.0100/req");
    }

    #[test]
    fn pricing_hint_for_model_resolves_the_matching_variant() {
        let provider = gemini_variant_priced();

        // Exact substring match → that model's input/output token spread.
        let flash = provider
            .pricing_hint_for_model(Some("gemini-2.5-flash"))
            .unwrap();
        assert_eq!(flash.min_usd, 0.345);
        assert_eq!(flash.max_usd, 2.875);
        assert_eq!(flash.unit, "tokens");
        // Directional dims populate `io`, so the chip shows real in/out rates.
        assert_eq!(flash.io, Some((0.345, 2.875)));
        assert_eq!(flash.to_string(), "in $0.34 · out $2.88 /1M tok");

        // First-match-wins: the flash-lite variant precedes the broader
        // flash prefix, so a lite id resolves to lite pricing.
        let lite = provider
            .pricing_hint_for_model(Some("gemini-2.5-flash-lite"))
            .unwrap();
        assert_eq!((lite.min_usd, lite.max_usd), (0.115, 0.46));

        // Unknown model falls back to the `default` sentinel variant.
        let unknown = provider
            .pricing_hint_for_model(Some("some-future-model"))
            .unwrap();
        assert_eq!((unknown.min_usd, unknown.max_usd), (1.0, 8.0));

        // No model given → the model-agnostic aggregate (spans all variants).
        let agg = provider.pricing_hint_for_model(None).unwrap();
        assert_eq!(agg.min_usd, 0.115);
        assert_eq!(agg.max_usd, 8.0);
    }

    #[test]
    fn pricing_hint_for_model_falls_back_to_aggregate_without_variants() {
        // Flat (no-variant) provider: the model arg is ignored, aggregate
        // pricing is returned so the chip still shows a price.
        let provider = gemini("https://example.com");
        let hint = provider
            .pricing_hint_for_model(Some("gemini-2.5-flash"))
            .unwrap();
        assert_eq!(hint, provider.pricing_hint().unwrap());
    }

    #[test]
    fn pricing_hint_is_none_without_metered_endpoints() {
        let svc: pay_core::skills::Service = serde_json::from_value(serde_json::json!({
            "fqn": "op/free",
            "service_url": "https://example.com",
            "endpoints": [{ "method": "GET", "path": "v1/models" }]
        }))
        .unwrap();
        assert_eq!(CatalogProvider::from_service(&svc).pricing_hint(), None);
    }

    #[test]
    fn dialect_maps_known_fqns() {
        assert_eq!(
            gemini("https://example.com").dialect(),
            Dialect::GeminiNative
        );

        let alibaba: pay_core::skills::Service = serde_json::from_value(serde_json::json!({
            "fqn": "solana-foundation/alibaba/modelstudio",
            "service_url": "https://modelstudio.alibaba.gateway-402.com"
        }))
        .unwrap();
        assert_eq!(
            CatalogProvider::from_service(&alibaba).dialect(),
            Dialect::OpenAiCompat
        );

        let other: pay_core::skills::Service = serde_json::from_value(serde_json::json!({
            "fqn": "op/other",
            "service_url": "https://example.com"
        }))
        .unwrap();
        assert_eq!(
            CatalogProvider::from_service(&other).dialect(),
            Dialect::Unknown
        );
    }

    #[test]
    fn list_models_parses_v1beta_models() {
        rt().block_on(async {
            let port = stub(vec![("/v1beta/models", GEMINI_MODELS_JSON)]).await;
            let models = gemini(&base_url(port))
                .list_models(&client(), &base_url(port))
                .await;
            assert_eq!(models, vec!["gemini-2.5-flash", "gemini-2.5-pro"]);
        });
    }

    #[test]
    fn parse_model_names_handles_openai_compat_data_ids() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"data":[{"id":"qwen-max"},{"id":"qwen-plus"}]}"#).unwrap();
        assert_eq!(parse_model_names(&json), vec!["qwen-max", "qwen-plus"]);
        assert!(parse_model_names(&serde_json::json!({"ok": true})).is_empty());
    }

    #[test]
    fn identify_is_up_on_any_http_response() {
        rt().block_on(async {
            // 200 on the model-list endpoint.
            let port = stub(vec![("/v1beta/models", GEMINI_MODELS_JSON)]).await;
            let provider = gemini(&base_url(port));
            assert_eq!(
                provider.identify(&client(), &base_url(port)).await,
                Some(None)
            );

            // Even a 404 (no matching route) counts — the entry came from
            // the catalog, reachability is all identify checks.
            let bare = stub(vec![("/unrelated", "ok")]).await;
            assert_eq!(
                provider.identify(&client(), &base_url(bare)).await,
                Some(None)
            );
        });
    }

    #[test]
    fn identify_is_down_when_nothing_listens() {
        rt().block_on(async {
            let dead = {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                listener.local_addr().unwrap().port()
            };
            let provider = gemini(&base_url(dead));
            assert_eq!(provider.identify(&client(), &base_url(dead)).await, None);
        });
    }

    #[test]
    fn resolve_skips_fqns_missing_from_the_catalog() {
        rt().block_on(async {
            // Catalog with the google entry only (endpoints preloaded so no
            // detail fetch happens) — the alibaba default must be skipped.
            let mut catalog: pay_core::skills::Catalog =
                serde_json::from_value(serde_json::json!({
                    "version": "1",
                    "providers": [gemini_service("https://generativelanguage.google.gateway-402.com")]
                }))
                .unwrap();

            let providers = resolve_catalog_providers(&mut catalog, DEFAULT_CATALOG_FQNS).await;
            let slugs: Vec<&str> = providers.iter().map(|p| p.slug()).collect();
            assert_eq!(slugs, vec!["generativelanguage"]);
        });
    }

    #[test]
    fn resolve_skips_entries_without_a_service_url() {
        rt().block_on(async {
            let mut catalog: pay_core::skills::Catalog =
                serde_json::from_value(serde_json::json!({
                    "version": "1",
                    "providers": [{
                        "fqn": "op/no-url",
                        "endpoints": [{ "method": "GET", "path": "v1/models" }]
                    }]
                }))
                .unwrap();

            let providers = resolve_catalog_providers(&mut catalog, &["op/no-url"]).await;
            assert!(providers.is_empty());
        });
    }
}
