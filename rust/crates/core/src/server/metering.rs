use crate::server::accounting::{AccountingKey, AccountingStore};
use http::HeaderMap;
use pay_types::metering::{
    AccountingMode, ApiSpec, BillingUnit, CompareOp, Endpoint, MeterCondition, MeterDimension,
    MeterDirection, MeterVariant, Metering, MissingUsagePolicy, PriceTier, SplitRule, UsageMeter,
    UsageMeterSource,
};
use serde::{Deserialize, Serialize};

/// Properties extracted from an incoming request, used to evaluate metering conditions.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestProperties {
    pub input_tokens: Option<u64>,
    pub input_characters: Option<u64>,
    pub context_length: Option<u64>,
    pub body_size: Option<u64>,
    pub duration_seconds: Option<u64>,
    pub batch_size: Option<u64>,
    pub image_pixels: Option<u64>,
}

/// The resolved price for a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPrice {
    pub dimensions: Vec<ResolvedDimension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedDimension {
    pub direction: String,
    pub unit: String,
    pub scale: u64,
    pub price_usd: f64,
}

/// Settlement plan carried from x402-upto open verification to the post-response
/// settlement hook.
#[derive(Debug, Clone)]
pub struct UptoSettlementPlan {
    pub metering: Metering,
    pub variant_hint: Option<String>,
    pub request_properties: RequestProperties,
    pub ceiling_usd: f64,
    /// Token counts observed on the (possibly streamed) response body by the
    /// proxy's inference observer. Token and quota-unit dimensions settle from
    /// these counts — `Input → tokens_prompt`, `Output → tokens_completion` —
    /// instead of JSON-pointer extraction. `None` keeps the buffered-body path.
    pub inferred_usage: Option<crate::InferenceUsage>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UptoActualAmount {
    pub usd: f64,
    pub base_units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UptoUsageError {
    MissingUsage(String),
    InvalidUsage(String),
    InvalidJson(String),
    MissingUsagePolicyError,
}

impl std::fmt::Display for UptoUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingUsage(msg) => write!(f, "missing usage: {msg}"),
            Self::InvalidUsage(msg) => write!(f, "invalid usage: {msg}"),
            Self::InvalidJson(msg) => write!(f, "invalid response JSON: {msg}"),
            Self::MissingUsagePolicyError => {
                write!(f, "usage missing and missing_usage is set to error")
            }
        }
    }
}

impl std::error::Error for UptoUsageError {}

/// Find the matching endpoint for a request path and method.
pub fn find_endpoint<'a>(api: &'a ApiSpec, method: &str, path: &str) -> Option<&'a Endpoint> {
    // Exact match first
    if let Some(ep) = api
        .endpoints
        .iter()
        .find(|e| format!("{:?}", e.method).to_uppercase() == method && e.path == path)
    {
        return Some(ep);
    }

    // Pattern match: replace {param} segments with the actual values
    api.endpoints
        .iter()
        .find(|e| format!("{:?}", e.method).to_uppercase() == method && path_matches(&e.path, path))
}

/// Find an endpoint by path only (ignoring HTTP method).
/// Used for browser payment links where the browser sends GET to a POST endpoint.
/// Literal paths win over `{param}` / `{*rest}` patterns, as in [`find_endpoint`].
pub fn find_endpoint_by_path<'a>(api: &'a ApiSpec, path: &str) -> Option<&'a Endpoint> {
    api.endpoints
        .iter()
        .find(|e| e.path == path)
        .or_else(|| api.endpoints.iter().find(|e| path_matches(&e.path, path)))
}

/// Match a path pattern like "v1beta/models/{modelsId}:generateContent"
/// against a concrete path like "v1beta/models/gemini-2.0-flash:generateContent".
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    // A final `{*name}` segment is a recursive wildcard. It matches one or
    // more remaining segments while preserving the existing `{name}`
    // single-segment behavior used by provider API specs.
    if let Some(last) = pattern_parts.last()
        && last.starts_with("{*")
        && last.ends_with('}')
    {
        if path_parts.len() < pattern_parts.len() {
            return false;
        }
        return pattern_parts[..pattern_parts.len() - 1]
            .iter()
            .zip(path_parts.iter())
            .all(|(pat, actual)| path_segment_matches(pat, actual));
    }

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    pattern_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(pat, actual)| path_segment_matches(pat, actual))
}

fn path_segment_matches(pattern: &str, actual: &str) -> bool {
    if pattern.starts_with('{') && pattern.ends_with('}') {
        true
    } else if pattern.contains('{') {
        // Partial wildcard like "{modelsId}:generateContent".
        pattern
            .find('}')
            .is_some_and(|end| actual.ends_with(&pattern[end + 1..]))
    } else {
        pattern == actual
    }
}

/// Context for resolving a price — includes accounting state.
pub struct MeteringContext<'a> {
    pub api_name: &'a str,
    pub endpoint_path: &'a str,
    pub accounting_mode: &'a AccountingMode,
    pub store: &'a dyn AccountingStore,
    /// Wallet pubkey of the agent (from X-Payment or X-Wallet header). None for 402 quotes.
    pub wallet: Option<&'a str>,
}

/// Resolve the price for a metered endpoint given request properties.
/// Returns None if the endpoint is free (no metering).
pub fn resolve_price(
    metering: &Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
    ctx: Option<&MeteringContext>,
) -> Option<ResolvedPrice> {
    // Try variant matching first
    if !metering.variants.is_empty() {
        if let Some(variant) = resolve_variant(&metering.variants, variant_hint) {
            return Some(resolve_dimensions(&variant.dimensions, props, ctx));
        }
        if !metering.dimensions.is_empty() {
            return Some(resolve_dimensions(&metering.dimensions, props, ctx));
        }
        // Legacy specs without top-level dimensions use the first variant as
        // their implicit default.
        if let Some(first) = metering.variants.first() {
            return Some(resolve_dimensions(&first.dimensions, props, ctx));
        }
    }

    // Direct dimensions
    if !metering.dimensions.is_empty() {
        return Some(resolve_dimensions(&metering.dimensions, props, ctx));
    }

    // SKU-based — return a zero price (actual price resolved externally)
    if !metering.sku_tiers.is_empty() {
        return Some(ResolvedPrice {
            dimensions: vec![ResolvedDimension {
                direction: "usage".to_string(),
                unit: "requests".to_string(),
                scale: 1,
                price_usd: 0.0, // SKU pricing resolved externally
            }],
        });
    }

    None
}

/// Resolve the price a fixed per-call scheme (MPP charge, x402 exact, x402
/// batch) charges. Those schemes authorize before the request body is read,
/// so only a path-derived hint can select a variant; otherwise the body may
/// select any variant and the most expensive candidate is charged.
///
/// Errors when a dimension is aggregate-settled (tokens, quota units, or a
/// response meter): its quantity is unknown before authorization.
pub fn resolve_fixed_price(
    metering: &Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
) -> Result<Option<ResolvedPrice>, String> {
    if let Some(dim) = metering
        .dimensions
        .iter()
        .chain(metering.variants.iter().flat_map(|v| v.dimensions.iter()))
        .find(|dim| dim.is_aggregate_settled())
    {
        return Err(format!(
            "{:?} {:?} usage is metered from the response and cannot be charged per call",
            dim.direction, dim.unit
        ));
    }
    let dimensions = fixed_charge_dimensions(metering, props, variant_hint);
    if dimensions.is_empty() {
        return Ok(resolve_price(metering, props, variant_hint, None));
    }
    Ok(Some(resolve_dimensions_as(
        dimensions,
        props,
        None,
        Unmeasured::Costliest,
    )))
}

fn fixed_charge_dimensions<'a>(
    metering: &'a Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
) -> &'a [MeterDimension] {
    if metering.variants.is_empty() {
        return &metering.dimensions;
    }
    if variant_hint.is_some() {
        if let Some(variant) = resolve_variant(&metering.variants, variant_hint) {
            return &variant.dimensions;
        }
        if !metering.dimensions.is_empty() {
            return &metering.dimensions;
        }
    }
    metering
        .variants
        .iter()
        .map(|variant| variant.dimensions.as_slice())
        .chain(Some(metering.dimensions.as_slice()).filter(|dims| !dims.is_empty()))
        .max_by(|a, b| {
            let usd = |dims| {
                per_call_usd(&resolve_dimensions_as(
                    dims,
                    props,
                    None,
                    Unmeasured::Costliest,
                ))
            };
            usd(a).total_cmp(&usd(b))
        })
        .unwrap_or(&metering.dimensions)
}

/// Per-call USD amount charged for a resolved fixed price: every dimension
/// contributes its unit price.
pub fn per_call_usd(price: &ResolvedPrice) -> f64 {
    price
        .dimensions
        .iter()
        .map(|d| d.price_usd / d.scale.max(1) as f64)
        .sum()
}

pub fn effective_dimensions<'a>(
    metering: &'a Metering,
    variant_hint: Option<&str>,
) -> &'a [MeterDimension] {
    if !metering.variants.is_empty() {
        if let Some(variant) = resolve_variant(&metering.variants, variant_hint) {
            return &variant.dimensions;
        }
        if !metering.dimensions.is_empty() {
            return &metering.dimensions;
        }
        if let Some(first) = metering.variants.first() {
            return &first.dimensions;
        }
    }

    &metering.dimensions
}

/// The authorized `x402-upto` ceiling: `upto.max_usd`, or the legacy
/// single-dimension unit price. `None` without an explicit ceiling when
/// several dimensions are priced (no one unit price bounds their sum) or a
/// dimension is usage-metered (its price covers `scale` units, so one unit's
/// share would cap a multi-unit response far below its measured cost).
pub fn upto_max_usd(metering: &Metering, resolved_price: Option<&ResolvedPrice>) -> Option<f64> {
    if let Some(max_usd) = metering.upto.as_ref().and_then(|upto| upto.max_usd) {
        return Some(max_usd);
    }
    if metering.has_aggregate_dimensions() {
        return None;
    }
    match resolved_price.map(|p| p.dimensions.as_slice()) {
        Some([d]) => Some(d.price_usd / d.scale.max(1) as f64),
        Some([]) | None => Some(0.01),
        Some(_) => None,
    }
}

pub fn upto_min_usd(metering: &Metering) -> Option<f64> {
    metering
        .upto
        .as_ref()
        .and_then(|upto| upto.min_usd)
        .or(metering.min_usd)
}

pub fn upto_missing_usage_policy(metering: &Metering) -> MissingUsagePolicy {
    metering
        .upto
        .as_ref()
        .map(|upto| upto.missing_usage)
        .unwrap_or_default()
}

pub fn upto_uses_response_usage(metering: &Metering, variant_hint: Option<&str>) -> bool {
    metering
        .upto
        .as_ref()
        .and_then(|upto| upto.usage_preset.as_deref())
        .is_some()
        || effective_dimensions(metering, variant_hint)
            .iter()
            .any(|dim| dim.meter.is_some() || dim.unit == BillingUnit::QuotaUnits)
}

pub fn upto_requires_response_body(metering: &Metering, variant_hint: Option<&str>) -> bool {
    let preset = metering
        .upto
        .as_ref()
        .and_then(|upto| upto.usage_preset.as_deref());
    effective_dimensions(metering, variant_hint)
        .iter()
        .any(|dim| dimension_reads_response_body(dim, preset))
}

pub fn upto_response_body_limit(metering: &Metering) -> usize {
    const DEFAULT_LIMIT: usize = 1024 * 1024;
    metering
        .upto
        .as_ref()
        .and_then(|upto| upto.response_body.as_ref())
        .and_then(|body| body.max_bytes)
        .unwrap_or(DEFAULT_LIMIT)
}

/// A request-only meter has an indivisible price, unlike consumable tokens.
/// Return it only when no response, variant, or accounting state is needed.
pub(super) fn flat_request_price(metering: &Metering) -> Option<f64> {
    if metering.dimensions.is_empty() || !metering.variants.is_empty() {
        return None;
    }
    let mut usd = 0.0;
    for dimension in &metering.dimensions {
        if dimension.direction != MeterDirection::Usage
            || dimension.unit != BillingUnit::Requests
            || dimension.meter.is_some()
            || dimension.period.is_some()
            || dimension.tiers.len() != 1
            || dimension.tiers[0].condition.is_some()
            || dimension.tiers[0].up_to.is_some()
        {
            return None;
        }
        usd += dimension.tiers[0].price_usd / dimension.scale.max(1) as f64;
    }
    Some(usd.max(upto_min_usd(metering).unwrap_or(0.0)))
}

pub fn upto_actual_amount_from_response(
    plan: &UptoSettlementPlan,
    max_amount: u64,
    headers: &HeaderMap,
    body: Option<&[u8]>,
) -> Result<UptoActualAmount, UptoUsageError> {
    if plan.ceiling_usd <= 0.0 || max_amount == 0 {
        return Ok(UptoActualAmount {
            usd: 0.0,
            base_units: 0,
        });
    }

    // Prefer the model the response actually reported for variant selection.
    // Inference APIs carry the model in the request BODY, so the path-derived
    // `variant_hint` is `None` and per-model rates would otherwise collapse to
    // the first variant. The observer parsed the real model from the response,
    // so use it (`resolve_variant` matches when the hint contains the variant
    // value, e.g. `gemma4:latest` ⊇ `gemma4`); fall back to the path hint.
    let observed_model = plan
        .inferred_usage
        .as_ref()
        .and_then(|u| u.model.as_deref());
    let variant_hint = observed_model.or(plan.variant_hint.as_deref());

    let result = extract_and_price_usage(
        &plan.metering,
        variant_hint,
        &plan.request_properties,
        headers,
        body,
        plan.inferred_usage.as_ref(),
    )
    .map(|actual_usd| clamp_actual_usd(&plan.metering, actual_usd, plan.ceiling_usd));

    let usd = match result {
        Ok(usd) => usd,
        Err(err) => match upto_missing_usage_policy(&plan.metering) {
            MissingUsagePolicy::Refund => 0.0,
            MissingUsagePolicy::Min => upto_min_usd(&plan.metering)
                .unwrap_or(0.0)
                .min(plan.ceiling_usd),
            MissingUsagePolicy::Ceiling => plan.ceiling_usd,
            MissingUsagePolicy::Error => {
                tracing::warn!(error = %err, "x402 upto response usage extraction failed");
                return Err(UptoUsageError::MissingUsagePolicyError);
            }
        },
    };

    let units_per_usd = max_amount as f64 / plan.ceiling_usd;
    Ok(UptoActualAmount {
        usd,
        base_units: usd_to_base_units(usd, units_per_usd, max_amount),
    })
}

/// Convert measured USD to base units, rounding to nearest like the session
/// meter, except that positive usage never rounds down to zero.
fn usd_to_base_units(usd: f64, units_per_usd: f64, max_amount: u64) -> u64 {
    if usd <= 0.0 || !usd.is_finite() {
        return 0;
    }
    ((usd * units_per_usd).round().max(1.0) as u64).min(max_amount)
}

fn clamp_actual_usd(metering: &Metering, actual_usd: f64, ceiling_usd: f64) -> f64 {
    let with_min = match upto_min_usd(metering) {
        Some(min_usd) if min_usd >= 0.0 => actual_usd.max(min_usd),
        _ => actual_usd,
    };
    with_min.clamp(0.0, ceiling_usd)
}

fn extract_and_price_usage(
    metering: &Metering,
    variant_hint: Option<&str>,
    props: &RequestProperties,
    headers: &HeaderMap,
    body: Option<&[u8]>,
    inferred_usage: Option<&crate::InferenceUsage>,
) -> Result<f64, UptoUsageError> {
    let dimensions = effective_dimensions(metering, variant_hint);
    if dimensions.is_empty() {
        return Err(UptoUsageError::MissingUsage(
            "no metering dimensions configured".to_string(),
        ));
    }

    // Observer-supplied token counts supersede body/meter extraction for token
    // dimensions. When every dimension that would otherwise read from the
    // response body is satisfiable from `inferred_usage`, we can skip parsing
    // the (possibly absent, because streamed) buffered body entirely. A
    // dimension reads from the body when it has a `ResponseJson` meter OR when
    // a preset supplies a JSON path for it.
    let preset = metering
        .upto
        .as_ref()
        .and_then(|upto| upto.usage_preset.as_deref());
    let body_still_needed = upto_requires_response_body(metering, variant_hint)
        && dimensions.iter().any(|dim| {
            let covered_by_inferred = inferred_usage
                .and_then(|usage| inferred_quantity_for_dim(usage, dim, props))
                .is_some();
            let reads_body = dimension_reads_response_body(dim, preset);
            !covered_by_inferred && reads_body
        });
    let json = if body_still_needed {
        let body =
            body.ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
        response_usage_documents(headers, body)?
    } else {
        vec![]
    };

    let mut priced_props = *props;
    if priced_props.context_length.is_none() {
        priced_props.context_length = inferred_usage
            .and_then(|usage| usage.tokens_prompt)
            .or_else(|| {
                json.iter()
                    .rev()
                    .find_map(|json| provider_token_quantity(json, MeterDirection::Input))
            });
    }
    let prices = resolve_price(metering, &priced_props, variant_hint, None)
        .ok_or_else(|| UptoUsageError::MissingUsage("no resolved price".to_string()))?;

    let mut total = 0.0;
    for (idx, dim) in dimensions.iter().enumerate() {
        let price = prices
            .dimensions
            .get(idx)
            .map(|d| d.price_usd)
            .unwrap_or(0.0);
        let extract = |json| {
            extract_dimension_quantity(dim, preset, &priced_props, headers, json, inferred_usage)
        };
        // Stream usage is cumulative, not additive. Providers may report input
        // and output in different events; a later lower record never lowers
        // the served quantity, so use the highest observation of each.
        let quantity = json
            .iter()
            .filter_map(|json| extract(Some(json)).ok())
            .max_by(f64::total_cmp)
            .map(Ok)
            .unwrap_or_else(|| extract(None))?;
        total += quantity / dim.scale.max(1) as f64 * price;
    }
    Ok(total)
}

fn response_usage_documents(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Vec<serde_json::Value>, UptoUsageError> {
    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    if !is_sse {
        return serde_json::from_slice(body)
            .map(|json| vec![json])
            .map_err(|error| UptoUsageError::InvalidJson(error.to_string()));
    }
    crate::server::session_stream::sse_json_documents(body)
        .map_err(|error| UptoUsageError::InvalidJson(error.to_string()))
}

/// The observer count for a token or quota-unit dimension, if any.
fn inferred_quantity_for_dim(
    usage: &crate::InferenceUsage,
    dim: &MeterDimension,
    props: &RequestProperties,
) -> Option<u64> {
    let tokens = match dim.direction {
        MeterDirection::Input => usage.tokens_prompt,
        // A stream-event approximation can undercount a multi-token delta.
        MeterDirection::Output => usage
            .tokens_completion
            .filter(|_| !usage.tokens_completion_estimated),
        _ => None,
    }?;
    match dim.unit {
        BillingUnit::Tokens => Some(tokens),
        BillingUnit::QuotaUnits => {
            quota_tokens_per_unit(dim, props).map(|per_unit| ceil_div_u64(tokens, per_unit))
        }
        _ => None,
    }
}

pub(crate) fn quota_tokens_per_unit(
    dim: &MeterDimension,
    props: &RequestProperties,
) -> Option<u64> {
    select_price_tier(dim, props)
        .and_then(|tier| tier.notes.as_deref())
        .and_then(parse_tokens_per_quota_unit)
}

pub(crate) fn parse_tokens_per_quota_unit(notes: &str) -> Option<u64> {
    let lower = notes.to_ascii_lowercase();
    let index = lower.find(" tokens per quota unit")?;
    lower[..index].split_whitespace().rev().find_map(|part| {
        part.replace(',', "")
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
    })
}

pub(crate) fn provider_token_quantity(
    json: &serde_json::Value,
    direction: MeterDirection,
) -> Option<u64> {
    let usage_objects = [
        json.get("usage"),
        json.pointer("/response/usage"),
        json.pointer("/message/usage"),
    ];
    match direction {
        MeterDirection::Input => usage_objects
            .into_iter()
            .flatten()
            .filter_map(|usage| {
                usage
                    .get("prompt_tokens")
                    .or_else(|| usage.get("input_tokens"))
                    .and_then(serde_json::Value::as_u64)
            })
            .max()
            .or_else(|| {
                json.pointer("/usageMetadata/promptTokenCount")
                    .and_then(serde_json::Value::as_u64)
            }),
        MeterDirection::Output => usage_objects
            .into_iter()
            .flatten()
            .filter_map(|usage| {
                usage
                    .get("completion_tokens")
                    .or_else(|| usage.get("output_tokens"))
                    .and_then(serde_json::Value::as_u64)
            })
            .max()
            .or_else(|| {
                let usage = json.get("usageMetadata")?;
                let input = usage.get("promptTokenCount")?.as_u64()?;
                let candidates = usage
                    .get("candidatesTokenCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let thoughts = usage
                    .get("thoughtsTokenCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let tool_prompt = usage
                    .get("toolUsePromptTokenCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let derived = usage
                    .get("totalTokenCount")
                    .and_then(serde_json::Value::as_u64)
                    .map(|total| total.saturating_sub(input).saturating_sub(tool_prompt))
                    .unwrap_or(0);
                Some(candidates.saturating_add(thoughts).max(derived))
            }),
        _ => None,
    }
}

fn ceil_div_u64(value: u64, divisor: u64) -> u64 {
    value.saturating_add(divisor.saturating_sub(1)) / divisor.max(1)
}

/// Units billed in whole counts; a fractional quantity for these is invalid
/// rather than rounded (a `0.49` request must not price as zero).
fn is_count_unit(unit: BillingUnit) -> bool {
    !matches!(
        unit,
        BillingUnit::Minutes
            | BillingUnit::Hours
            | BillingUnit::Seconds
            | BillingUnit::Gibibytes
            | BillingUnit::Tebibytes
            | BillingUnit::Vcpu
    )
}

fn extract_dimension_quantity(
    dim: &MeterDimension,
    preset: Option<&str>,
    props: &RequestProperties,
    headers: &HeaderMap,
    json: Option<&serde_json::Value>,
    inferred_usage: Option<&crate::InferenceUsage>,
) -> Result<f64, UptoUsageError> {
    let quantity = extract_raw_quantity(dim, preset, props, headers, json, inferred_usage)?;
    if is_count_unit(dim.unit) && quantity.fract() != 0.0 {
        return Err(UptoUsageError::InvalidUsage(format!(
            "fractional usage {quantity} for {:?} {:?}",
            dim.direction, dim.unit
        )));
    }
    Ok(quantity)
}

fn extract_raw_quantity(
    dim: &MeterDimension,
    preset: Option<&str>,
    props: &RequestProperties,
    headers: &HeaderMap,
    json: Option<&serde_json::Value>,
    inferred_usage: Option<&crate::InferenceUsage>,
) -> Result<f64, UptoUsageError> {
    // Observer token counts take precedence for token dimensions.
    if let Some(usage) = inferred_usage
        && let Some(quantity) = inferred_quantity_for_dim(usage, dim, props)
    {
        return Ok(quantity as f64);
    }
    // A response-header meter is already explicit and does not require
    // buffering a JSON body. Endpoint presets only disambiguate inherited
    // JSON meters whose field names differ by API dialect.
    if let Some(meter) = &dim.meter
        && matches!(meter.source, UsageMeterSource::ResponseHeader)
    {
        return extract_from_meter(meter, headers, json);
    }
    // Responses and Chat Completions use different names for the same token
    // dimensions. Let the endpoint-level Responses preset override meters
    // inherited from a shared model-pricing table.
    if preset.is_some_and(|preset| preset.eq_ignore_ascii_case("openai-responses"))
        && let Some(path) = preset_json_path(preset, dim)
    {
        let json =
            json.ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
        return extract_from_json_pointer(json, path);
    }
    if let Some(meter) = &dim.meter {
        return extract_from_meter(meter, headers, json);
    }

    if dim.unit == BillingUnit::QuotaUnits {
        let json =
            json.ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
        let tokens = provider_token_quantity(json, dim.direction).ok_or_else(|| {
            UptoUsageError::MissingUsage(format!(
                "no token usage for {:?} quota units",
                dim.direction
            ))
        })?;
        let per_unit = quota_tokens_per_unit(dim, props).ok_or_else(|| {
            UptoUsageError::MissingUsage(format!(
                "no tokens-per-quota-unit hint for {:?}",
                dim.direction
            ))
        })?;
        return Ok(ceil_div_u64(tokens, per_unit) as f64);
    }

    if dim.unit == BillingUnit::Tokens {
        let json =
            json.ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
        return provider_token_quantity(json, dim.direction)
            .map(|tokens| tokens as f64)
            .ok_or_else(|| {
                UptoUsageError::MissingUsage(format!(
                    "no token usage for {:?} tokens",
                    dim.direction
                ))
            });
    }

    if let Some(path) = preset_json_path(preset, dim) {
        let json =
            json.ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
        return extract_from_json_pointer(json, path);
    }

    if matches!(dim.direction, MeterDirection::Usage) && matches!(dim.unit, BillingUnit::Requests) {
        return Ok(1.0);
    }

    Err(UptoUsageError::MissingUsage(format!(
        "no usage meter for {:?} {:?}",
        dim.direction, dim.unit
    )))
}

fn dimension_reads_response_body(dim: &MeterDimension, preset: Option<&str>) -> bool {
    match dim.meter.as_ref().map(|meter| &meter.source) {
        Some(UsageMeterSource::ResponseHeader) => false,
        Some(UsageMeterSource::ResponseJson) => true,
        None => {
            matches!(dim.unit, BillingUnit::Tokens | BillingUnit::QuotaUnits)
                || preset_json_path(preset, dim).is_some()
        }
    }
}

fn extract_from_meter(
    meter: &UsageMeter,
    headers: &HeaderMap,
    json: Option<&serde_json::Value>,
) -> Result<f64, UptoUsageError> {
    match meter.source {
        UsageMeterSource::ResponseJson => {
            let json = json
                .ok_or_else(|| UptoUsageError::MissingUsage("response body unavailable".into()))?;
            let path = meter.path.as_deref().ok_or_else(|| {
                UptoUsageError::MissingUsage("response_json meter missing path".into())
            })?;
            extract_from_json_pointer(json, path)
        }
        UsageMeterSource::ResponseHeader => {
            let name = meter.header.as_deref().ok_or_else(|| {
                UptoUsageError::MissingUsage("response_header meter missing header".into())
            })?;
            let value = headers
                .get(name)
                .ok_or_else(|| UptoUsageError::MissingUsage(format!("header `{name}`")))?;
            let value = value
                .to_str()
                .map_err(|e| UptoUsageError::InvalidUsage(e.to_string()))?;
            parse_usage_quantity(value)
        }
    }
}

fn preset_json_path(preset: Option<&str>, dim: &MeterDimension) -> Option<&'static str> {
    let preset = preset?;
    if preset.eq_ignore_ascii_case("google-generativelanguage") {
        return match (dim.direction, dim.unit) {
            (MeterDirection::Input, BillingUnit::Tokens) => Some("/usageMetadata/promptTokenCount"),
            (MeterDirection::Output, BillingUnit::Tokens) => {
                Some("/usageMetadata/candidatesTokenCount")
            }
            (MeterDirection::Usage, BillingUnit::Tokens) => Some("/usageMetadata/totalTokenCount"),
            _ => None,
        };
    }
    if preset.eq_ignore_ascii_case("openai-compatible") {
        return match (dim.direction, dim.unit) {
            (MeterDirection::Input, BillingUnit::Tokens) => Some("/usage/prompt_tokens"),
            (MeterDirection::Output, BillingUnit::Tokens) => Some("/usage/completion_tokens"),
            (MeterDirection::Usage, BillingUnit::Tokens) => Some("/usage/total_tokens"),
            _ => None,
        };
    }
    if preset.eq_ignore_ascii_case("openai-responses") {
        return match (dim.direction, dim.unit) {
            (MeterDirection::Input, BillingUnit::Tokens) => Some("/usage/input_tokens"),
            (MeterDirection::Output, BillingUnit::Tokens) => Some("/usage/output_tokens"),
            (MeterDirection::Usage, BillingUnit::Tokens) => Some("/usage/total_tokens"),
            _ => None,
        };
    }
    None
}

fn extract_from_json_pointer(json: &serde_json::Value, path: &str) -> Result<f64, UptoUsageError> {
    let value = json
        .pointer(path)
        .ok_or_else(|| UptoUsageError::MissingUsage(format!("json path `{path}`")))?;
    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_u64() {
                Ok(v as f64)
            } else if let Some(v) = n.as_f64() {
                if v.is_finite() && v >= 0.0 {
                    Ok(v)
                } else {
                    Err(UptoUsageError::InvalidUsage(format!(
                        "json path `{path}` is negative or non-finite"
                    )))
                }
            } else {
                Err(UptoUsageError::InvalidUsage(format!(
                    "json path `{path}` is not a supported number"
                )))
            }
        }
        serde_json::Value::String(s) => parse_usage_quantity(s),
        _ => Err(UptoUsageError::InvalidUsage(format!(
            "json path `{path}` is not numeric"
        ))),
    }
}

fn parse_usage_quantity(value: &str) -> Result<f64, UptoUsageError> {
    let trimmed = value.trim();
    if let Ok(v) = trimmed.parse::<u64>() {
        return Ok(v as f64);
    }
    let v = trimmed
        .parse::<f64>()
        .map_err(|e| UptoUsageError::InvalidUsage(e.to_string()))?;
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err(UptoUsageError::InvalidUsage(
            "usage value is negative or non-finite".to_string(),
        ))
    }
}

/// Split rules for the tier a fixed per-call charge is priced at: the
/// selected tier's override, else the metering-level splits. Uses the same
/// variant and tier selection as [`resolve_fixed_price`].
pub fn resolve_split_rules<'a>(
    metering: &'a Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
) -> &'a [SplitRule] {
    fixed_charge_dimensions(metering, props, variant_hint)
        .iter()
        .filter_map(|dimension| select_tier(dimension, props, Unmeasured::Costliest))
        .map(|tier| tier.splits.as_slice())
        .find(|splits| !splits.is_empty())
        .unwrap_or(&metering.splits)
}

/// Every split rule a metering config can apply, across tiers and variants.
pub fn all_split_rules(metering: &Metering) -> impl Iterator<Item = &SplitRule> {
    fn tier_splits(dimensions: &[MeterDimension]) -> impl Iterator<Item = &SplitRule> {
        dimensions
            .iter()
            .flat_map(|dimension| dimension.tiers.iter())
            .flat_map(|tier| tier.splits.iter())
    }
    metering
        .splits
        .iter()
        .chain(tier_splits(&metering.dimensions))
        .chain(
            metering
                .variants
                .iter()
                .flat_map(|variant| tier_splits(&variant.dimensions)),
        )
}

/// After a request is forwarded, record the usage and return the actual price charged.
pub fn record_usage(
    metering: &Metering,
    props: &RequestProperties,
    variant_hint: Option<&str>,
    ctx: &MeteringContext,
    units_consumed: u64,
) -> Option<ResolvedPrice> {
    let scope = match ctx.accounting_mode {
        AccountingMode::Pooled => "pool".to_string(),
        AccountingMode::PerAgent => ctx.wallet.unwrap_or("unknown").to_string(),
    };

    let key = AccountingKey {
        api: ctx.api_name.to_string(),
        endpoint: ctx.endpoint_path.to_string(),
        period: crate::server::accounting::current_period(),
        scope,
    };

    // Increment the counter
    let _new_total = ctx.store.increment(&key, units_consumed);

    // Resolve price at the new usage level
    resolve_price(metering, props, variant_hint, Some(ctx))
}

pub(crate) fn resolve_variant<'a>(
    variants: &'a [MeterVariant],
    hint: Option<&str>,
) -> Option<&'a MeterVariant> {
    // The most specific (longest) contained value wins, so an exact value
    // beats its prefixes and `gpt-4o` is never priced as `gpt-4`. Ties keep
    // config order.
    let mut default = None;
    let mut best: Option<&MeterVariant> = None;
    for variant in variants {
        if variant.value.eq_ignore_ascii_case("default") {
            default.get_or_insert(variant);
        } else if hint.is_some_and(|hint| hint.contains(&variant.value))
            && best.is_none_or(|best| variant.value.len() > best.value.len())
        {
            best = Some(variant);
        }
    }
    best.or(default)
}

fn resolve_dimensions(
    dimensions: &[MeterDimension],
    props: &RequestProperties,
    ctx: Option<&MeteringContext>,
) -> ResolvedPrice {
    resolve_dimensions_as(dimensions, props, ctx, Unmeasured::Provisional)
}

/// How a tier condition on a request property that was not measured resolves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unmeasured {
    /// Request-time estimate later corrected by response metering; only an
    /// unknown `body_size`, which nothing measures later, is priced
    /// conservatively.
    Provisional,
    /// Fixed per-call authorization: never repriced, so any unmeasured
    /// condition prices the costliest tier.
    Costliest,
}

fn resolve_dimensions_as(
    dimensions: &[MeterDimension],
    props: &RequestProperties,
    ctx: Option<&MeteringContext>,
    unmeasured: Unmeasured,
) -> ResolvedPrice {
    let resolved = dimensions
        .iter()
        .map(|dim| {
            let price = resolve_tier(&dim.tiers, props, ctx, unmeasured);
            ResolvedDimension {
                direction: format!("{:?}", dim.direction).to_lowercase(),
                unit: format!("{:?}", dim.unit).to_lowercase(),
                scale: dim.scale,
                price_usd: price,
            }
        })
        .collect();

    ResolvedPrice {
        dimensions: resolved,
    }
}

fn resolve_tier(
    tiers: &[PriceTier],
    props: &RequestProperties,
    ctx: Option<&MeteringContext>,
    unmeasured: Unmeasured,
) -> f64 {
    // If we have accounting context and tiers have up_to, resolve by cumulative usage
    let has_volume_tiers = tiers.iter().any(|t| t.up_to.is_some());

    if has_volume_tiers {
        if let Some(ctx) = ctx {
            let scope = match ctx.accounting_mode {
                AccountingMode::Pooled => "pool".to_string(),
                AccountingMode::PerAgent => ctx.wallet.unwrap_or("unknown").to_string(),
            };
            let key = AccountingKey {
                api: ctx.api_name.to_string(),
                endpoint: ctx.endpoint_path.to_string(),
                period: crate::server::accounting::current_period(),
                scope,
            };
            let usage = ctx.store.get_usage(&key);
            return resolve_tier_by_volume(tiers, usage);
        }
        // No accounting context (402 quote) — use first non-free tier
        return first_non_free_price(tiers);
    }

    select_price_tier_from_tiers(tiers, props, unmeasured)
        .map(|tier| tier.price_usd)
        .unwrap_or(0.0)
}

pub(crate) fn select_price_tier<'a>(
    dimension: &'a MeterDimension,
    props: &RequestProperties,
) -> Option<&'a PriceTier> {
    select_tier(dimension, props, Unmeasured::Provisional)
}

fn select_tier<'a>(
    dimension: &'a MeterDimension,
    props: &RequestProperties,
    unmeasured: Unmeasured,
) -> Option<&'a PriceTier> {
    if dimension.tiers.iter().any(|tier| tier.up_to.is_some()) {
        return dimension
            .tiers
            .iter()
            .find(|tier| tier.price_usd > 0.0)
            .or_else(|| dimension.tiers.first());
    }
    select_price_tier_from_tiers(&dimension.tiers, props, unmeasured)
}

fn select_price_tier_from_tiers<'a>(
    tiers: &'a [PriceTier],
    props: &RequestProperties,
    unmeasured: Unmeasured,
) -> Option<&'a PriceTier> {
    // A condition on a property that was not measured (e.g. a chunked body
    // without Content-Length) must not silently select a cheaper tier.
    let conservative = |condition: &MeterCondition| {
        condition_property(condition, props).is_none()
            && (unmeasured == Unmeasured::Costliest
                || matches!(condition, MeterCondition::BodySize { .. }))
    };
    if tiers
        .iter()
        .any(|tier| tier.condition.as_ref().is_some_and(conservative))
    {
        return tiers
            .iter()
            .max_by(|a, b| a.price_usd.total_cmp(&b.price_usd));
    }
    tiers
        .iter()
        .find(|tier| {
            tier.condition
                .as_ref()
                .is_none_or(|condition| evaluate_condition(condition, props))
        })
        .or_else(|| tiers.last())
}

/// Resolve tier based on cumulative volume usage.
fn resolve_tier_by_volume(tiers: &[PriceTier], current_usage: u64) -> f64 {
    for tier in tiers {
        if let Some(up_to) = tier.up_to {
            if current_usage <= up_to {
                return tier.price_usd;
            }
        } else {
            return tier.price_usd;
        }
    }
    tiers.last().map(|t| t.price_usd).unwrap_or(0.0)
}

/// For 402 quotes without accounting context: return the first non-free tier price.
/// This is the most expensive paid tier — safe for the Foundation.
fn first_non_free_price(tiers: &[PriceTier]) -> f64 {
    tiers
        .iter()
        .find(|t| t.price_usd > 0.0)
        .map(|t| t.price_usd)
        .unwrap_or(0.0)
}

fn condition_property(condition: &MeterCondition, props: &RequestProperties) -> Option<u64> {
    match condition {
        MeterCondition::InputTokens { .. } => props.input_tokens,
        MeterCondition::InputCharacters { .. } => props.input_characters,
        MeterCondition::ContextLength { .. } => props.context_length,
        MeterCondition::BodySize { .. } => props.body_size,
        MeterCondition::DurationSeconds { .. } => props.duration_seconds,
        MeterCondition::BatchSize { .. } => props.batch_size,
        MeterCondition::ImagePixels { .. } => props.image_pixels,
    }
}

/// Whether `condition` holds. An unmeasured property passes (provisional).
pub(crate) fn evaluate_condition(condition: &MeterCondition, props: &RequestProperties) -> bool {
    let (op, threshold) = match condition {
        MeterCondition::InputTokens { op, value }
        | MeterCondition::InputCharacters { op, value }
        | MeterCondition::ContextLength { op, value }
        | MeterCondition::BodySize { op, value }
        | MeterCondition::DurationSeconds { op, value }
        | MeterCondition::BatchSize { op, value }
        | MeterCondition::ImagePixels { op, value } => (op, *value),
    };
    let Some(actual) = condition_property(condition, props) else {
        return true;
    };

    match op {
        CompareOp::Lte => actual <= threshold,
        CompareOp::Lt => actual < threshold,
        CompareOp::Gte => actual >= threshold,
        CompareOp::Gt => actual > threshold,
        CompareOp::Eq => actual == threshold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn test_path_matches_exact() {
        assert!(path_matches("v1/models", "v1/models"));
        assert!(!path_matches("v1/models", "v1/other"));
    }

    #[test]
    fn test_path_matches_wildcard() {
        assert!(path_matches(
            "v1beta/models/{modelsId}:generateContent",
            "v1beta/models/gemini-2.0-flash:generateContent"
        ));
        assert!(!path_matches(
            "v1beta/models/{modelsId}:generateContent",
            "v1beta/models/gemini-2.0-flash:streamGenerateContent"
        ));
    }

    #[test]
    fn test_path_matches_full_wildcard_segment() {
        assert!(path_matches(
            "v1/projects/{projectsId}/locations/{locationsId}",
            "v1/projects/my-project/locations/us-central1"
        ));
    }

    #[test]
    fn test_evaluate_condition() {
        let props = RequestProperties {
            context_length: Some(100_000),
            ..Default::default()
        };

        let cond_lte = MeterCondition::ContextLength {
            op: CompareOp::Lte,
            value: 200_000,
        };
        assert!(evaluate_condition(&cond_lte, &props));

        let cond_gt = MeterCondition::ContextLength {
            op: CompareOp::Gt,
            value: 200_000,
        };
        assert!(!evaluate_condition(&cond_gt, &props));
    }

    #[test]
    fn test_evaluate_condition_missing_prop() {
        let props = RequestProperties::default();
        let cond = MeterCondition::ContextLength {
            op: CompareOp::Lte,
            value: 200_000,
        };
        // Missing prop → condition passes (permissive)
        assert!(evaluate_condition(&cond, &props));
    }

    #[test]
    fn test_evaluate_all_compare_ops() {
        let props = RequestProperties {
            body_size: Some(100),
            ..Default::default()
        };

        assert!(evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Eq,
                value: 100
            },
            &props
        ));
        assert!(!evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Eq,
                value: 50
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Lt,
                value: 200
            },
            &props
        ));
        assert!(!evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Lt,
                value: 100
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Gte,
                value: 100
            },
            &props
        ));
        assert!(!evaluate_condition(
            &MeterCondition::BodySize {
                op: CompareOp::Gte,
                value: 200
            },
            &props
        ));
    }

    #[test]
    fn test_evaluate_all_condition_fields() {
        let props = RequestProperties {
            input_tokens: Some(100),
            input_characters: Some(200),
            context_length: Some(300),
            body_size: Some(400),
            duration_seconds: Some(500),
            batch_size: Some(600),
            image_pixels: Some(700),
        };

        assert!(evaluate_condition(
            &MeterCondition::InputTokens {
                op: CompareOp::Eq,
                value: 100
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::InputCharacters {
                op: CompareOp::Eq,
                value: 200
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::DurationSeconds {
                op: CompareOp::Eq,
                value: 500
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::BatchSize {
                op: CompareOp::Eq,
                value: 600
            },
            &props
        ));
        assert!(evaluate_condition(
            &MeterCondition::ImagePixels {
                op: CompareOp::Eq,
                value: 700
            },
            &props
        ));
    }

    #[test]
    fn test_path_matches_different_lengths() {
        assert!(!path_matches("v1/a/b", "v1/a"));
        assert!(!path_matches("v1/a", "v1/a/b"));
    }

    #[test]
    fn test_path_matches_recursive_final_wildcard() {
        assert!(path_matches("{*path}", "v1/jobs/42"));
        assert!(path_matches("v1/{*path}", "v1/jobs/42"));
        assert!(!path_matches("v1/{*path}", "v1"));
        assert!(!path_matches("v1/{*path}", "v2/jobs/42"));
    }

    #[test]
    fn test_resolve_tier_by_volume() {
        let tiers = vec![
            PriceTier {
                up_to: Some(100),
                price_usd: 0.0,
                condition: None,
                notes: None,
                splits: vec![],
            },
            PriceTier {
                up_to: Some(1000),
                price_usd: 0.01,
                condition: None,
                notes: None,
                splits: vec![],
            },
            PriceTier {
                up_to: None,
                price_usd: 0.005,
                condition: None,
                notes: None,
                splits: vec![],
            },
        ];

        // Free tier
        assert_eq!(resolve_tier_by_volume(&tiers, 50), 0.0);
        assert_eq!(resolve_tier_by_volume(&tiers, 100), 0.0);
        // Second tier
        assert_eq!(resolve_tier_by_volume(&tiers, 101), 0.01);
        assert_eq!(resolve_tier_by_volume(&tiers, 1000), 0.01);
        // Final tier (no cap)
        assert_eq!(resolve_tier_by_volume(&tiers, 1001), 0.005);
        assert_eq!(resolve_tier_by_volume(&tiers, 999_999), 0.005);
    }

    #[test]
    fn test_first_non_free_price() {
        let tiers = vec![
            PriceTier {
                up_to: Some(100),
                price_usd: 0.0,
                condition: None,
                notes: None,
                splits: vec![],
            },
            PriceTier {
                up_to: None,
                price_usd: 0.05,
                condition: None,
                notes: None,
                splits: vec![],
            },
        ];
        assert_eq!(first_non_free_price(&tiers), 0.05);
    }

    #[test]
    fn test_first_non_free_price_all_free() {
        let tiers = vec![PriceTier {
            up_to: None,
            price_usd: 0.0,
            condition: None,
            notes: None,
            splits: vec![],
        }];
        assert_eq!(first_non_free_price(&tiers), 0.0);
    }

    fn make_api(subdomain: &str, endpoints: Vec<Endpoint>) -> ApiSpec {
        ApiSpec {
            name: "test".to_string(),
            subdomain: subdomain.to_string(),
            title: "Test API".to_string(),
            description: "".to_string(),
            category: pay_types::metering::ApiCategory::AiMl,
            version: "1.0".to_string(),
            env: std::collections::HashMap::new(),
            routing: pay_types::metering::RoutingConfig::Proxy {
                url: "https://api.example.com".to_string(),
                path_rewrites: vec![],
                auth: None,
            },
            accounting: AccountingMode::Pooled,
            endpoints,
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
            session: None,
            batch_settlement: None,
            recipients: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_find_endpoint_exact_match() {
        let api = make_api(
            "test",
            vec![Endpoint {
                method: pay_types::metering::HttpMethod::Get,
                path: "v1/models".to_string(),
                description: None,
                resource: None,
                routing: None,
                metering: None,
                subscription: None,
            }],
        );
        let ep = find_endpoint(&api, "GET", "v1/models");
        assert!(ep.is_some());
        assert_eq!(ep.unwrap().path, "v1/models");
    }

    #[test]
    fn test_find_endpoint_pattern_match() {
        let api = make_api(
            "test",
            vec![Endpoint {
                method: pay_types::metering::HttpMethod::Post,
                path: "v1/models/{modelId}:generate".to_string(),
                description: None,
                resource: None,
                routing: None,
                metering: None,
                subscription: None,
            }],
        );
        let ep = find_endpoint(&api, "POST", "v1/models/gpt-4:generate");
        assert!(ep.is_some());
    }

    #[test]
    fn test_find_endpoint_no_match() {
        let api = make_api(
            "test",
            vec![Endpoint {
                method: pay_types::metering::HttpMethod::Get,
                path: "v1/models".to_string(),
                description: None,
                resource: None,
                routing: None,
                metering: None,
                subscription: None,
            }],
        );
        assert!(find_endpoint(&api, "POST", "v1/models").is_none());
        assert!(find_endpoint(&api, "GET", "v2/models").is_none());
    }

    #[test]
    fn test_resolve_price_no_metering() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };
        assert!(resolve_price(&metering, &RequestProperties::default(), None, None).is_none());
    }

    #[test]
    fn test_resolve_price_with_dimensions() {
        let metering = Metering {
            dimensions: vec![MeterDimension {
                direction: pay_types::metering::MeterDirection::Input,
                unit: pay_types::metering::BillingUnit::Tokens,
                scale: 1_000_000,
                period: None,
                tiers: vec![PriceTier {
                    up_to: None,
                    price_usd: 0.01,
                    condition: None,
                    notes: None,
                    splits: vec![],
                }],
                meter: None,
            }],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };
        let price = resolve_price(&metering, &RequestProperties::default(), None, None);
        assert!(price.is_some());
        let p = price.unwrap();
        assert_eq!(p.dimensions.len(), 1);
        assert_eq!(p.dimensions[0].price_usd, 0.01);
    }

    #[test]
    fn test_resolve_price_with_sku_tiers() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![],
            sku_tiers: vec![pay_types::metering::SkuTier {
                sku: "essentials".to_string(),
                level: pay_types::metering::SkuLevel::Essentials,
            }],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };
        let price = resolve_price(&metering, &RequestProperties::default(), None, None);
        assert!(price.is_some());
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.0);
    }

    #[test]
    fn test_resolve_price_variant_match() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![
                MeterVariant {
                    param: "model".to_string(),
                    value: "gemini-pro".to_string(),
                    description: None,
                    dimensions: vec![MeterDimension {
                        direction: pay_types::metering::MeterDirection::Input,
                        unit: pay_types::metering::BillingUnit::Tokens,
                        scale: 1_000_000,
                        period: None,
                        tiers: vec![PriceTier {
                            up_to: None,
                            price_usd: 0.05,
                            condition: None,
                            notes: None,
                            splits: vec![],
                        }],
                        meter: None,
                    }],
                },
                MeterVariant {
                    param: "model".to_string(),
                    value: "gemini-flash".to_string(),
                    description: None,
                    dimensions: vec![MeterDimension {
                        direction: pay_types::metering::MeterDirection::Input,
                        unit: pay_types::metering::BillingUnit::Tokens,
                        scale: 1_000_000,
                        period: None,
                        tiers: vec![PriceTier {
                            up_to: None,
                            price_usd: 0.01,
                            condition: None,
                            notes: None,
                            splits: vec![],
                        }],
                        meter: None,
                    }],
                },
            ],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };
        // Match second variant
        let price = resolve_price(
            &metering,
            &RequestProperties::default(),
            Some("gemini-flash-001"),
            None,
        );
        assert!(price.is_some());
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.01);
    }

    #[test]
    fn test_resolve_price_uses_explicit_default_variant_for_unknown_hint() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![
                MeterVariant {
                    param: "model".to_string(),
                    value: "qwen-turbo".to_string(),
                    description: None,
                    dimensions: vec![usage_dim(
                        MeterDirection::Usage,
                        BillingUnit::Requests,
                        1,
                        0.001,
                        None,
                    )],
                },
                MeterVariant {
                    param: "model".to_string(),
                    value: "default".to_string(),
                    description: None,
                    dimensions: vec![usage_dim(
                        MeterDirection::Usage,
                        BillingUnit::Requests,
                        1,
                        0.100,
                        None,
                    )],
                },
            ],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };

        let price = resolve_price(
            &metering,
            &RequestProperties::default(),
            Some("unknown-model"),
            None,
        )
        .unwrap();

        assert_eq!(price.dimensions[0].price_usd, 0.100);
    }

    #[test]
    fn test_resolve_price_variant_no_match_uses_first() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![MeterVariant {
                param: "model".to_string(),
                value: "gemini-pro".to_string(),
                description: None,
                dimensions: vec![MeterDimension {
                    direction: pay_types::metering::MeterDirection::Input,
                    unit: pay_types::metering::BillingUnit::Tokens,
                    scale: 1_000_000,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.05,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                    meter: None,
                }],
            }],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };
        // No variant hint match → uses first variant as default
        let price = resolve_price(
            &metering,
            &RequestProperties::default(),
            Some("unknown-model"),
            None,
        );
        assert!(price.is_some());
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.05);
    }

    #[test]
    fn test_resolve_price_variant_no_match_uses_default_dimensions() {
        let default_dim = MeterDimension {
            direction: pay_types::metering::MeterDirection::Input,
            unit: pay_types::metering::BillingUnit::Tokens,
            scale: 1_000_000,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.02,
                condition: None,
                notes: None,
                splits: vec![],
            }],
            meter: None,
        };
        let variant_dim = MeterDimension {
            direction: pay_types::metering::MeterDirection::Input,
            unit: pay_types::metering::BillingUnit::Tokens,
            scale: 1_000_000,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.50,
                condition: None,
                notes: None,
                splits: vec![],
            }],
            meter: None,
        };
        let metering = Metering {
            dimensions: vec![default_dim],
            variants: vec![MeterVariant {
                param: "model".to_string(),
                value: "gemini-pro".to_string(),
                description: None,
                dimensions: vec![variant_dim],
            }],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };

        let price = resolve_price(
            &metering,
            &RequestProperties::default(),
            Some("unknown-model"),
            None,
        )
        .expect("default dimensions should price unmatched variants");

        assert_eq!(price.dimensions[0].price_usd, 0.02);
        assert_eq!(
            effective_dimensions(&metering, Some("unknown-model"))[0].tiers[0].price_usd,
            0.02
        );
    }

    #[test]
    fn test_resolve_price_conditional_tiers() {
        let metering = Metering {
            dimensions: vec![MeterDimension {
                direction: pay_types::metering::MeterDirection::Input,
                unit: pay_types::metering::BillingUnit::Tokens,
                scale: 1_000_000,
                period: None,
                tiers: vec![
                    PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: Some(MeterCondition::ContextLength {
                            op: CompareOp::Lte,
                            value: 128_000,
                        }),
                        notes: None,
                        splits: vec![],
                    },
                    PriceTier {
                        up_to: None,
                        price_usd: 0.02,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    },
                ],
                meter: None,
            }],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };

        // Within condition
        let props = RequestProperties {
            context_length: Some(64_000),
            ..Default::default()
        };
        let price = resolve_price(&metering, &props, None, None);
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.01);

        // Exceeds condition — falls to second tier
        let props = RequestProperties {
            context_length: Some(256_000),
            ..Default::default()
        };
        let price = resolve_price(&metering, &props, None, None);
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.02);
    }

    #[test]
    fn test_record_usage() {
        use crate::server::accounting::InMemoryStore;

        let store = InMemoryStore::new();
        let metering = Metering {
            dimensions: vec![MeterDimension {
                direction: pay_types::metering::MeterDirection::Usage,
                unit: pay_types::metering::BillingUnit::Requests,
                scale: 1,
                period: None,
                tiers: vec![
                    PriceTier {
                        up_to: Some(100),
                        price_usd: 0.0,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    },
                    PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    },
                ],
                meter: None,
            }],
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: None,
        };

        let ctx = MeteringContext {
            api_name: "test",
            endpoint_path: "v1/test",
            accounting_mode: &AccountingMode::Pooled,
            store: &store,
            wallet: None,
        };

        // Record usage — should be in free tier
        let price = record_usage(&metering, &RequestProperties::default(), None, &ctx, 50);
        assert!(price.is_some());
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.0);

        // Record more — should push into paid tier
        let price = record_usage(&metering, &RequestProperties::default(), None, &ctx, 60);
        assert!(price.is_some());
        assert_eq!(price.unwrap().dimensions[0].price_usd, 0.01);
    }

    fn price_tier(price_usd: f64) -> PriceTier {
        PriceTier {
            up_to: None,
            price_usd,
            condition: None,
            notes: None,
            splits: vec![],
        }
    }

    fn usage_dim(
        direction: MeterDirection,
        unit: BillingUnit,
        scale: u64,
        price_usd: f64,
        meter: Option<UsageMeter>,
    ) -> MeterDimension {
        MeterDimension {
            direction,
            unit,
            scale,
            period: None,
            tiers: vec![price_tier(price_usd)],
            meter,
        }
    }

    fn upto_metering(
        dimensions: Vec<MeterDimension>,
        upto: Option<pay_types::metering::UptoMetering>,
    ) -> Metering {
        Metering {
            dimensions,
            variants: vec![],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto,
        }
    }

    fn response_json(path: &str) -> UsageMeter {
        UsageMeter {
            source: UsageMeterSource::ResponseJson,
            path: Some(path.to_string()),
            header: None,
        }
    }

    fn response_header(header: &str) -> UsageMeter {
        UsageMeter {
            source: UsageMeterSource::ResponseHeader,
            path: None,
            header: Some(header.to_string()),
        }
    }

    fn settlement_plan(metering: Metering, ceiling_usd: f64) -> UptoSettlementPlan {
        UptoSettlementPlan {
            metering,
            variant_hint: None,
            request_properties: RequestProperties::default(),
            ceiling_usd,
            inferred_usage: None,
        }
    }

    #[test]
    fn upto_max_usd_prefers_explicit_ceiling() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Requests,
                1,
                0.01,
                None,
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );
        let price = resolve_price(&metering, &RequestProperties::default(), None, None);

        assert_eq!(upto_max_usd(&metering, price.as_ref()), Some(0.10));
    }

    #[test]
    fn upto_max_usd_keeps_legacy_first_dimension_fallback() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Requests,
                1_000,
                0.02,
                None,
            )],
            None,
        );
        let price = resolve_price(&metering, &RequestProperties::default(), None, None);

        assert_eq!(upto_max_usd(&metering, price.as_ref()), Some(0.00002));
    }

    #[test]
    fn upto_max_usd_never_derives_a_ceiling_for_usage_metered_dimensions() {
        // $1 per 1M tokens: one token's share would cap a 100k-token
        // response worth $0.10 at $0.000001.
        let tokens = upto_metering(
            vec![usage_dim(
                MeterDirection::Output,
                BillingUnit::Tokens,
                1_000_000,
                1.0,
                None,
            )],
            None,
        );
        let price = resolve_price(&tokens, &RequestProperties::default(), None, None);
        assert_eq!(upto_max_usd(&tokens, price.as_ref()), None);

        let explicit = Metering {
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
            ..tokens
        };
        assert_eq!(upto_max_usd(&explicit, price.as_ref()), Some(0.50));
    }

    #[test]
    fn upto_actual_amount_prices_response_json_dimensions() {
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000,
                    0.01,
                    Some(response_json("/usage/input_tokens")),
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000,
                    0.03,
                    Some(response_json("/usage/output_tokens")),
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                response_body: Some(pay_types::metering::UptoResponseBody {
                    mode: pay_types::metering::UptoResponseBodyMode::Buffer,
                    max_bytes: Some(4096),
                }),
                ..Default::default()
            }),
        );
        let body = br#"{"usage":{"input_tokens":1000,"output_tokens":500}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.025);
        assert_eq!(actual.base_units, 25_000);
    }

    #[test]
    fn upto_actual_amount_supports_google_generativelanguage_preset() {
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000,
                    0.01,
                    None,
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000,
                    0.03,
                    None,
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                usage_preset: Some("google-generativelanguage".to_string()),
                ..Default::default()
            }),
        );
        let body = br#"{"usageMetadata":{"promptTokenCount":2000,"candidatesTokenCount":1000}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn upto_sse_usage_matches_json_and_selects_request_variant() {
        let mut metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000_000,
                    1.0,
                    None,
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000_000,
                    1.0,
                    None,
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(1.0),
                missing_usage: MissingUsagePolicy::Refund,
                usage_preset: Some("openai-compatible".into()),
                ..Default::default()
            }),
        );
        metering.variants.push(MeterVariant {
            param: "model".into(),
            value: "premium".into(),
            description: None,
            dimensions: vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000_000,
                    10.0,
                    None,
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000_000,
                    10.0,
                    None,
                ),
            ],
        });
        let mut plan = settlement_plan(metering, 1.0);
        plan.variant_hint = Some("premium".into());
        let json = br#"{"usage":{"prompt_tokens":1000,"completion_tokens":1000}}"#;
        let expected =
            upto_actual_amount_from_response(&plan, 1_000_000, &HeaderMap::new(), Some(json))
                .unwrap();
        assert_eq!(expected.base_units, 20_000);
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        let body = b": keepalive\r\n\r\ndata: {\"choices\":[]}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":500}}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":1000}}\r\n\r\ndata: [DONE]\r\n\r\n";
        assert_eq!(
            upto_actual_amount_from_response(&plan, 1_000_000, &headers, Some(body)).unwrap(),
            expected
        );
        assert_eq!(
            upto_actual_amount_from_response(
                &plan,
                1_000_000,
                &headers,
                Some(b"data: {\"error\":\"worker_failed\"}\n\n")
            )
            .unwrap()
            .base_units,
            0
        );
    }

    #[test]
    fn flat_request_meter_has_an_indivisible_admission_price() {
        let mut meter = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Requests,
                1,
                0.75,
                None,
            )],
            None,
        );
        assert_eq!(flat_request_price(&meter), Some(0.75));
        meter.dimensions[0].unit = BillingUnit::Tokens;
        assert_eq!(flat_request_price(&meter), None);
    }

    #[test]
    fn upto_actual_amount_supports_openai_compatible_preset() {
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000,
                    0.01,
                    None,
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000,
                    0.03,
                    None,
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                usage_preset: Some("openai-compatible".to_string()),
                ..Default::default()
            }),
        );
        let body =
            br#"{"usage":{"prompt_tokens":2000,"completion_tokens":1000,"total_tokens":3000}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn quota_units_support_openai_compatible_usage() {
        let mut input = usage_dim(
            MeterDirection::Input,
            BillingUnit::QuotaUnits,
            1,
            0.000001,
            None,
        );
        input.tiers[0].notes = Some("4 input tokens per quota unit".to_string());
        let mut output = usage_dim(
            MeterDirection::Output,
            BillingUnit::QuotaUnits,
            1,
            0.000003,
            None,
        );
        output.tiers[0].notes = Some("2 output tokens per quota unit".to_string());
        let metering = upto_metering(vec![input, output], None);
        let body = br#"{"usage":{"prompt_tokens":8,"completion_tokens":5,"total_tokens":13}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert!((actual.usd - 0.000011).abs() < f64::EPSILON);
        assert_eq!(actual.base_units, 11);
    }

    #[test]
    fn quota_units_use_notes_from_the_selected_conditional_tier() {
        let mut input = usage_dim(
            MeterDirection::Input,
            BillingUnit::QuotaUnits,
            1,
            0.000005,
            None,
        );
        input.tiers = vec![
            PriceTier {
                up_to: None,
                price_usd: 0.000005,
                condition: Some(MeterCondition::ContextLength {
                    op: CompareOp::Lte,
                    value: 200_000,
                }),
                notes: Some("4 input tokens per quota unit".to_string()),
                splits: vec![],
            },
            PriceTier {
                up_to: None,
                price_usd: 0.000005,
                condition: Some(MeterCondition::ContextLength {
                    op: CompareOp::Gt,
                    value: 200_000,
                }),
                notes: Some("2 input tokens per quota unit".to_string()),
                splits: vec![],
            },
        ];
        let mut plan = settlement_plan(upto_metering(vec![input], None), 0.10);
        plan.request_properties.context_length = Some(250_000);
        plan.inferred_usage = Some(crate::InferenceUsage {
            tokens_prompt: Some(5),
            ..Default::default()
        });

        let actual =
            upto_actual_amount_from_response(&plan, 100_000, &HeaderMap::new(), None).unwrap();

        assert!((actual.usd - 0.000015).abs() < f64::EPSILON);
        assert_eq!(actual.base_units, 15);
    }

    #[test]
    fn gemini_tool_prompt_tokens_are_not_billed_as_output() {
        let mut output = usage_dim(
            MeterDirection::Output,
            BillingUnit::QuotaUnits,
            1,
            0.000001,
            None,
        );
        output.tiers[0].notes = Some("1 output tokens per quota unit".to_string());
        let metering = upto_metering(vec![output], None);
        let body = br#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":2,"toolUsePromptTokenCount":20,"totalTokenCount":35}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert!((actual.usd - 0.000005).abs() < f64::EPSILON);
        assert_eq!(actual.base_units, 5);
    }

    #[test]
    fn openai_responses_preset_overrides_shared_chat_usage_meters() {
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000,
                    0.01,
                    Some(response_json("/usage/prompt_tokens")),
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000,
                    0.03,
                    Some(response_json("/usage/completion_tokens")),
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                usage_preset: Some("openai-responses".to_string()),
                ..Default::default()
            }),
        );
        let body = br#"{"usage":{"input_tokens":2000,"output_tokens":1000,"total_tokens":3000}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn upto_actual_amount_prices_response_header_meter() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_header("x-usage-tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-usage-tokens", HeaderValue::from_static("2500"));

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &headers,
            None,
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn openai_responses_preset_honors_explicit_response_header_meter() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Input,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_header("x-usage-tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                usage_preset: Some("openai-responses".to_string()),
                ..Default::default()
            }),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-usage-tokens", HeaderValue::from_static("2500"));

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &headers,
            None,
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn upto_actual_amount_applies_minimum_and_ceiling_clamps() {
        let low = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.001,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                min_usd: Some(0.01),
                ..Default::default()
            }),
        );
        let low_actual = upto_actual_amount_from_response(
            &settlement_plan(low, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"tokens":100}}"#),
        )
        .unwrap();
        assert_eq!(low_actual.usd, 0.01);
        assert_eq!(low_actual.base_units, 10_000);

        let high = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                1.0,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );
        let high_actual = upto_actual_amount_from_response(
            &settlement_plan(high, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"tokens":1000}}"#),
        )
        .unwrap();
        assert_eq!(high_actual.usd, 0.10);
        assert_eq!(high_actual.base_units, 100_000);
    }

    fn per_million_token_plan() -> UptoSettlementPlan {
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000_000,
                    0.10,
                    Some(response_json("/usage/prompt_tokens")),
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000_000,
                    0.30,
                    Some(response_json("/usage/completion_tokens")),
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.02),
                ..Default::default()
            }),
        );
        settlement_plan(metering, 0.02)
    }

    fn per_million_amount(body: &[u8]) -> u64 {
        upto_actual_amount_from_response(
            &per_million_token_plan(),
            20_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap()
        .base_units
    }

    #[test]
    fn positive_sub_base_unit_usage_charges_at_least_one_base_unit() {
        // 1 + 1 tokens = $0.0000004, i.e. 0.4 base units at six decimals.
        assert_eq!(
            per_million_amount(br#"{"usage":{"prompt_tokens":1,"completion_tokens":1}}"#),
            1
        );
        // 1.5 base units rounds half up.
        assert_eq!(
            per_million_amount(br#"{"usage":{"prompt_tokens":3,"completion_tokens":4}}"#),
            2
        );
    }

    #[test]
    fn exact_and_zero_usage_convert_without_rounding_up() {
        assert_eq!(
            per_million_amount(br#"{"usage":{"prompt_tokens":1000,"completion_tokens":1000}}"#),
            400
        );
        assert_eq!(
            per_million_amount(br#"{"usage":{"prompt_tokens":0,"completion_tokens":0}}"#),
            0
        );
    }

    #[test]
    fn usd_to_base_units_never_exceeds_the_authorized_amount() {
        assert_eq!(usd_to_base_units(1.0, 1_000_000.0, 20_000), 20_000);
        assert_eq!(usd_to_base_units(f64::NAN, 1_000_000.0, 20_000), 0);
        assert_eq!(usd_to_base_units(-1.0, 1_000_000.0, 20_000), 0);
    }

    fn sse_output_amount(body: &[u8]) -> u64 {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Output,
                BillingUnit::Tokens,
                1_000_000,
                1.0,
                Some(response_json("/usage/completion_tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(1.0),
                missing_usage: MissingUsagePolicy::Error,
                ..Default::default()
            }),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            "text/event-stream".parse().unwrap(),
        );
        upto_actual_amount_from_response(
            &settlement_plan(metering, 1.0),
            1_000_000,
            &headers,
            Some(body),
        )
        .unwrap()
        .base_units
    }

    #[test]
    fn decreasing_cumulative_sse_usage_never_lowers_settlement() {
        assert_eq!(
            sse_output_amount(
                b"data: {\"usage\":{\"completion_tokens\":1000}}\n\ndata: {\"usage\":{\"completion_tokens\":1}}\n\n"
            ),
            1_000
        );
    }

    #[test]
    fn increasing_cumulative_sse_usage_settles_the_final_total() {
        assert_eq!(
            sse_output_amount(
                b"data: {\"usage\":{\"completion_tokens\":1}}\n\ndata: {\"usage\":{\"completion_tokens\":1000}}\n\n"
            ),
            1_000
        );
        assert_eq!(
            sse_output_amount(b"data: {\"usage\":{\"completion_tokens\":1000}}\n\n"),
            1_000
        );
    }

    #[test]
    fn upto_missing_usage_policy_refunds_by_default() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{}}"#),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.0);
        assert_eq!(actual.base_units, 0);
    }

    #[test]
    fn upto_missing_usage_policy_can_use_min_or_ceiling_or_error() {
        let base = vec![usage_dim(
            MeterDirection::Usage,
            BillingUnit::Tokens,
            1_000,
            0.02,
            Some(response_json("/usage/tokens")),
        )];

        let min = upto_metering(
            base.clone(),
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                min_usd: Some(0.01),
                missing_usage: MissingUsagePolicy::Min,
                ..Default::default()
            }),
        );
        let min_actual = upto_actual_amount_from_response(
            &settlement_plan(min, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{}}"#),
        )
        .unwrap();
        assert_eq!(min_actual.usd, 0.01);
        assert_eq!(min_actual.base_units, 10_000);

        let ceiling = upto_metering(
            base.clone(),
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                missing_usage: MissingUsagePolicy::Ceiling,
                ..Default::default()
            }),
        );
        let ceiling_actual = upto_actual_amount_from_response(
            &settlement_plan(ceiling, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{}}"#),
        )
        .unwrap();
        assert_eq!(ceiling_actual.usd, 0.10);
        assert_eq!(ceiling_actual.base_units, 100_000);

        let error = upto_metering(
            base,
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                missing_usage: MissingUsagePolicy::Error,
                ..Default::default()
            }),
        );
        let err = upto_actual_amount_from_response(
            &settlement_plan(error, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{}}"#),
        )
        .unwrap_err();
        assert_eq!(err, UptoUsageError::MissingUsagePolicyError);
    }

    #[test]
    fn upto_response_usage_detection_distinguishes_body_and_header_meters() {
        let json = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering::default()),
        );
        assert!(upto_uses_response_usage(&json, None));
        assert!(upto_requires_response_body(&json, None));

        let header = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_header("x-usage-tokens")),
            )],
            Some(pay_types::metering::UptoMetering::default()),
        );
        assert!(upto_uses_response_usage(&header, None));
        assert!(!upto_requires_response_body(&header, None));

        let preset = upto_metering(
            vec![usage_dim(
                MeterDirection::Input,
                BillingUnit::Tokens,
                1_000,
                0.02,
                None,
            )],
            Some(pay_types::metering::UptoMetering {
                usage_preset: Some("google-generativelanguage".to_string()),
                ..Default::default()
            }),
        );
        assert!(upto_requires_response_body(&preset, None));
    }

    #[test]
    fn upto_actual_amount_supports_request_units_without_meter() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Requests,
                1,
                0.02,
                None,
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            None,
        )
        .unwrap();

        assert_eq!(actual.usd, 0.02);
        assert_eq!(actual.base_units, 20_000);
    }

    #[test]
    fn upto_actual_amount_accepts_numeric_strings() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"tokens":"2500"}}"#),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.05);
        assert_eq!(actual.base_units, 50_000);
    }

    #[test]
    fn upto_actual_amount_errors_on_bad_json_when_policy_is_error() {
        let metering = upto_metering(
            vec![usage_dim(
                MeterDirection::Usage,
                BillingUnit::Tokens,
                1_000,
                0.02,
                Some(response_json("/usage/tokens")),
            )],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                missing_usage: MissingUsagePolicy::Error,
                ..Default::default()
            }),
        );

        let err = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.10),
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":"#),
        )
        .unwrap_err();

        assert_eq!(err, UptoUsageError::MissingUsagePolicyError);
    }

    #[test]
    fn upto_actual_amount_uses_variant_specific_dimensions() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![
                MeterVariant {
                    param: "model".to_string(),
                    value: "pro".to_string(),
                    description: None,
                    dimensions: vec![usage_dim(
                        MeterDirection::Usage,
                        BillingUnit::Tokens,
                        1_000,
                        0.08,
                        Some(response_json("/usage/tokens")),
                    )],
                },
                MeterVariant {
                    param: "model".to_string(),
                    value: "flash".to_string(),
                    description: None,
                    dimensions: vec![usage_dim(
                        MeterDirection::Usage,
                        BillingUnit::Tokens,
                        1_000,
                        0.02,
                        Some(response_json("/usage/tokens")),
                    )],
                },
            ],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        };
        let plan = UptoSettlementPlan {
            metering,
            variant_hint: Some("models/gemini-flash".to_string()),
            request_properties: RequestProperties::default(),
            ceiling_usd: 0.10,
            inferred_usage: None,
        };

        let actual = upto_actual_amount_from_response(
            &plan,
            100_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"tokens":1000}}"#),
        )
        .unwrap();

        assert_eq!(actual.usd, 0.02);
        assert_eq!(actual.base_units, 20_000);
    }

    #[test]
    fn upto_actual_amount_selects_variant_from_observed_model() {
        // Body-model API: the path hint is None, so without the observed model
        // the price would collapse to the FIRST variant (gemma4). The observer
        // reports the served model (`qwen3:8b`), which must select the qwen
        // variant. Rates: gemma4 in 0.10/out 0.30, qwen3:8b in 0.50/out 1.50.
        let metering = Metering {
            dimensions: vec![],
            variants: vec![
                MeterVariant {
                    param: "model".to_string(),
                    value: "gemma4".to_string(),
                    description: None,
                    dimensions: vec![
                        usage_dim(
                            MeterDirection::Input,
                            BillingUnit::Tokens,
                            1_000_000,
                            0.10,
                            Some(response_json("/usage/prompt_tokens")),
                        ),
                        usage_dim(
                            MeterDirection::Output,
                            BillingUnit::Tokens,
                            1_000_000,
                            0.30,
                            Some(response_json("/usage/completion_tokens")),
                        ),
                    ],
                },
                MeterVariant {
                    param: "model".to_string(),
                    value: "qwen3:8b".to_string(),
                    description: None,
                    dimensions: vec![
                        usage_dim(
                            MeterDirection::Input,
                            BillingUnit::Tokens,
                            1_000_000,
                            0.50,
                            Some(response_json("/usage/prompt_tokens")),
                        ),
                        usage_dim(
                            MeterDirection::Output,
                            BillingUnit::Tokens,
                            1_000_000,
                            1.50,
                            Some(response_json("/usage/completion_tokens")),
                        ),
                    ],
                },
            ],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
        };
        // Path hint is None (model is in the body); observer reports qwen3:8b
        // plus token counts {prompt: 100, completion: 200}.
        let plan = UptoSettlementPlan {
            metering,
            variant_hint: None,
            request_properties: RequestProperties::default(),
            ceiling_usd: 0.50,
            inferred_usage: Some(crate::InferenceUsage {
                model: Some("qwen3:8b".to_string()),
                tokens_prompt: Some(100),
                tokens_completion: Some(200),
                ..Default::default()
            }),
        };

        // qwen rates: 100/1e6*0.50 + 200/1e6*1.50 = 0.00005 + 0.0003 = 0.00035.
        let actual =
            upto_actual_amount_from_response(&plan, 500_000, &HeaderMap::new(), None).unwrap();
        let qwen_expected = 100.0 / 1_000_000.0 * 0.50 + 200.0 / 1_000_000.0 * 1.50;
        assert!(
            (actual.usd - qwen_expected).abs() < 1e-12,
            "expected qwen rate {qwen_expected}, got {}",
            actual.usd
        );

        // Sanity: the gemma4 (first-variant) rate would be cheaper — prove we
        // did NOT fall back to it.
        let gemma_wrong = 100.0 / 1_000_000.0 * 0.10 + 200.0 / 1_000_000.0 * 0.30;
        assert!(
            (actual.usd - gemma_wrong).abs() > 1e-9,
            "must not fall back to the first variant's rate"
        );
    }

    #[test]
    fn upto_never_settles_an_estimated_completion_count() {
        // Output $1/1M tokens, read from the response JSON.
        let metering = Metering {
            dimensions: vec![usage_dim(
                MeterDirection::Output,
                BillingUnit::Tokens,
                1_000_000,
                1.0,
                Some(response_json("/usage/completion_tokens")),
            )],
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plan = |estimated| UptoSettlementPlan {
            metering: metering.clone(),
            variant_hint: None,
            request_properties: RequestProperties::default(),
            ceiling_usd: 0.50,
            // Three content deltas, however many tokens each carried.
            inferred_usage: Some(crate::InferenceUsage {
                tokens_completion: Some(3),
                tokens_completion_estimated: estimated,
                ..Default::default()
            }),
        };
        let body = br#"{"usage":{"completion_tokens":400}}"#;

        // The provider's count in the body is what settles.
        let actual =
            upto_actual_amount_from_response(&plan(true), 500_000, &HeaderMap::new(), Some(body))
                .unwrap();
        assert_eq!(actual.base_units, 400);
        // Without it the missing-usage policy applies, not the estimate.
        let actual = upto_actual_amount_from_response(
            &plan(true),
            500_000,
            &HeaderMap::new(),
            Some(br#"{"text":"no usage"}"#),
        );
        assert!(
            actual
                .as_ref()
                .map_or(true, |actual| actual.base_units != 3),
            "{actual:?}"
        );

        // Positive control: an authoritative observer count still supersedes
        // the body.
        let actual =
            upto_actual_amount_from_response(&plan(false), 500_000, &HeaderMap::new(), Some(body))
                .unwrap();
        assert_eq!(actual.base_units, 3);
    }

    #[test]
    fn upto_actual_amount_prefers_inferred_usage_over_json() {
        // Per-model variant: input $0.10/1M, output $0.30/1M. inferred_usage
        // {prompt: 12, completion: 214} → 12/1e6*0.10 + 214/1e6*0.30
        // = 0.0000012 + 0.0000642 = 0.0000654 USD.
        let metering = Metering {
            dimensions: vec![],
            variants: vec![MeterVariant {
                param: "model".to_string(),
                value: "gemma4".to_string(),
                description: None,
                dimensions: vec![
                    usage_dim(
                        MeterDirection::Input,
                        BillingUnit::Tokens,
                        1_000_000,
                        0.10,
                        // A JSON meter is present as the fallback; the observer
                        // path must supersede it (and the body is absent here).
                        Some(response_json("/usage/input_tokens")),
                    ),
                    usage_dim(
                        MeterDirection::Output,
                        BillingUnit::Tokens,
                        1_000_000,
                        0.30,
                        Some(response_json("/usage/output_tokens")),
                    ),
                ],
            }],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
        };
        let plan = UptoSettlementPlan {
            metering,
            variant_hint: Some("gemma4:latest".to_string()),
            request_properties: RequestProperties::default(),
            ceiling_usd: 0.50,
            inferred_usage: Some(crate::InferenceUsage {
                tokens_prompt: Some(12),
                tokens_completion: Some(214),
                ..Default::default()
            }),
        };

        // No response body: settlement must still succeed from inferred_usage.
        let actual =
            upto_actual_amount_from_response(&plan, 100_000, &HeaderMap::new(), None).unwrap();

        let expected = 12.0 / 1_000_000.0 * 0.10 + 214.0 / 1_000_000.0 * 0.30;
        assert!((actual.usd - expected).abs() < 1e-12, "usd={}", actual.usd);
    }

    #[test]
    fn buffered_usage_selects_context_tier_from_provider_prompt_tokens() {
        let metering = upto_metering(
            vec![MeterDimension {
                direction: MeterDirection::Output,
                unit: BillingUnit::Tokens,
                scale: 1,
                period: None,
                tiers: vec![
                    PriceTier {
                        up_to: None,
                        price_usd: 0.000001,
                        condition: Some(MeterCondition::ContextLength {
                            op: CompareOp::Lte,
                            value: 2,
                        }),
                        notes: None,
                        splits: vec![],
                    },
                    PriceTier {
                        up_to: None,
                        price_usd: 0.000005,
                        condition: Some(MeterCondition::ContextLength {
                            op: CompareOp::Gt,
                            value: 2,
                        }),
                        notes: None,
                        splits: vec![],
                    },
                ],
                meter: None,
            }],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.10),
                ..Default::default()
            }),
        );

        for body in [
            br#"{"usage":{"prompt_tokens":3,"completion_tokens":2}}"#.as_slice(),
            br#"{"usage":{"input_tokens":3,"output_tokens":2}}"#.as_slice(),
            br#"{"response":{"usage":{"input_tokens":3,"output_tokens":2}}}"#.as_slice(),
            br#"{"message":{"usage":{"input_tokens":3,"output_tokens":2}}}"#.as_slice(),
            br#"{"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#.as_slice(),
        ] {
            let actual = upto_actual_amount_from_response(
                &settlement_plan(metering.clone(), 0.10),
                100_000,
                &HeaderMap::new(),
                Some(body),
            )
            .unwrap();

            assert_eq!(actual.usd, 0.000010);
            assert_eq!(actual.base_units, 10);
        }
    }

    #[test]
    fn upto_settles_at_the_observed_models_variant_not_the_first() {
        // Two per-model variants; the body-model API leaves variant_hint None,
        // so without the observed-model fix this would settle at the first
        // variant (`cheap`). The observer reports `pricey`, which must be the
        // rate charged.
        let variant = |value: &str, in_rate: f64, out_rate: f64| MeterVariant {
            param: "model".to_string(),
            value: value.to_string(),
            description: None,
            dimensions: vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000_000,
                    in_rate,
                    None,
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000_000,
                    out_rate,
                    None,
                ),
            ],
        };
        let metering = Metering {
            dimensions: vec![],
            variants: vec![
                variant("cheap-model", 0.10, 0.30),
                variant("pricey-model", 1.00, 3.00),
            ],
            sku_tiers: vec![],
            splits: vec![],
            schemes: None,
            min_usd: None,
            upto: Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
        };
        let plan = UptoSettlementPlan {
            metering,
            variant_hint: None, // path carries no model (inference API)
            request_properties: RequestProperties::default(),
            ceiling_usd: 0.50,
            inferred_usage: Some(crate::InferenceUsage {
                model: Some("pricey-model".to_string()),
                tokens_prompt: Some(1_000),
                tokens_completion: Some(1_000),
                ..Default::default()
            }),
        };

        let actual =
            upto_actual_amount_from_response(&plan, 100_000, &HeaderMap::new(), None).unwrap();

        // pricey rates: 1000/1e6*1.00 + 1000/1e6*3.00 = 0.004, NOT the cheap
        // variant's 1000/1e6*0.10 + 1000/1e6*0.30 = 0.0004.
        let pricey = 1_000.0 / 1_000_000.0 * 1.00 + 1_000.0 / 1_000_000.0 * 3.00;
        assert!(
            (actual.usd - pricey).abs() < 1e-12,
            "settled at the observed model's rate: usd={}",
            actual.usd
        );
    }

    #[test]
    fn upto_actual_amount_falls_back_to_json_without_inferred_usage() {
        // Same shape, but inferred_usage None → the JSON pointers drive it.
        let metering = upto_metering(
            vec![
                usage_dim(
                    MeterDirection::Input,
                    BillingUnit::Tokens,
                    1_000_000,
                    0.10,
                    Some(response_json("/usage/input_tokens")),
                ),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Tokens,
                    1_000_000,
                    0.30,
                    Some(response_json("/usage/output_tokens")),
                ),
            ],
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(0.50),
                ..Default::default()
            }),
        );
        let body = br#"{"usage":{"input_tokens":12,"output_tokens":214}}"#;

        let actual = upto_actual_amount_from_response(
            &settlement_plan(metering, 0.50),
            100_000,
            &HeaderMap::new(),
            Some(body),
        )
        .unwrap();

        let expected = 12.0 / 1_000_000.0 * 0.10 + 214.0 / 1_000_000.0 * 0.30;
        assert!((actual.usd - expected).abs() < 1e-12, "usd={}", actual.usd);
    }

    fn yaml_metering(yaml: &str) -> Metering {
        serde_yml::from_str(yaml).expect("metering parses")
    }

    fn request_variant(value: &str, price_usd: f64) -> String {
        format!(
            "  - param: model\n    value: {value}\n    dimensions:\n      - direction: usage\n        unit: requests\n        scale: 1\n        tiers: [{{price_usd: {price_usd}}}]\n"
        )
    }

    #[test]
    fn fixed_price_without_path_hint_charges_most_expensive_variant() {
        let metering = yaml_metering(&format!(
            "variants:\n{}{}{}",
            request_variant("cheap", 0.01),
            request_variant("premium", 0.10),
            request_variant("default", 0.05),
        ));
        let props = RequestProperties::default();

        let unhinted = resolve_fixed_price(&metering, &props, None)
            .unwrap()
            .unwrap();
        assert_eq!(per_call_usd(&unhinted), 0.10);
        // Request-time pricing (upto/session) still uses the default variant.
        let legacy = resolve_price(&metering, &props, None, None).unwrap();
        assert_eq!(per_call_usd(&legacy), 0.05);

        let cheap = resolve_fixed_price(&metering, &props, Some("cheap"))
            .unwrap()
            .unwrap();
        assert_eq!(per_call_usd(&cheap), 0.01);
        let other = resolve_fixed_price(&metering, &props, Some("unlisted"))
            .unwrap()
            .unwrap();
        assert_eq!(per_call_usd(&other), 0.05);
    }

    #[test]
    fn fixed_price_includes_top_level_dimensions_as_candidates() {
        let metering = yaml_metering(&format!(
            "dimensions:\n  - direction: usage\n    unit: requests\n    scale: 1\n    tiers: [{{price_usd: 0.25}}]\nvariants:\n{}",
            request_variant("cheap", 0.01),
        ));
        let props = RequestProperties::default();

        assert_eq!(
            per_call_usd(
                &resolve_fixed_price(&metering, &props, None)
                    .unwrap()
                    .unwrap()
            ),
            0.25
        );
        // A path hint that matches nothing uses the top-level backstop.
        assert_eq!(
            per_call_usd(
                &resolve_fixed_price(&metering, &props, Some("x"))
                    .unwrap()
                    .unwrap()
            ),
            0.25
        );
        assert_eq!(
            per_call_usd(
                &resolve_fixed_price(&metering, &props, Some("cheap"))
                    .unwrap()
                    .unwrap()
            ),
            0.01
        );
    }

    #[test]
    fn fixed_price_unmatched_hint_without_fallback_charges_most_expensive_variant() {
        let metering = yaml_metering(&format!(
            "variants:\n{}{}",
            request_variant("cheap", 0.01),
            request_variant("premium", 0.10),
        ));
        let price = resolve_fixed_price(&metering, &RequestProperties::default(), Some("unlisted"))
            .unwrap()
            .unwrap();
        assert_eq!(per_call_usd(&price), 0.10);
    }

    #[test]
    fn variant_selection_prefers_the_most_specific_value() {
        for variants in [
            vec![("gpt-4", 0.01), ("gpt-4o", 1.00)],
            vec![("gpt-4o", 1.00), ("gpt-4", 0.01)],
        ] {
            let yaml = variants
                .iter()
                .map(|(value, price)| request_variant(value, *price))
                .collect::<String>();
            let metering = yaml_metering(&format!("variants:\n{yaml}"));
            let price = |hint: &str| {
                per_call_usd(
                    &resolve_price(&metering, &RequestProperties::default(), Some(hint), None)
                        .unwrap(),
                )
            };
            assert_eq!(price("gpt-4o"), 1.00);
            assert_eq!(price("gpt-4"), 0.01);
            // Family matching still applies to dated or tagged aliases.
            assert_eq!(price("gpt-4o-2024-08-06"), 1.00);
            assert_eq!(price("gpt-4-turbo"), 0.01);
            assert_eq!(price("v1/models/gpt-4o:generateContent"), 1.00);
        }
    }

    #[test]
    fn unmeasured_condition_property_prices_the_costliest_tier() {
        let metering = yaml_metering(
            "dimensions:\n  - direction: usage\n    unit: requests\n    scale: 1\n    tiers:\n      - price_usd: 0.01\n        condition: {field: body_size, op: \"<=\", value: 1048576}\n      - price_usd: 0.10\n        condition: {field: body_size, op: \">\", value: 1048576}\n",
        );
        let price = |body_size| {
            let props = RequestProperties {
                body_size,
                ..Default::default()
            };
            per_call_usd(
                &resolve_fixed_price(&metering, &props, None)
                    .unwrap()
                    .unwrap(),
            )
        };
        assert_eq!(price(None), 0.10);
        assert_eq!(price(Some(1_048_577)), 0.10);
        assert_eq!(price(Some(1_048_576)), 0.01);
        assert_eq!(price(Some(0)), 0.01);
    }

    #[test]
    fn fixed_price_does_not_assume_unmeasured_conditions_pass() {
        let metering = yaml_metering(
            "dimensions:\n  - direction: usage\n    unit: requests\n    scale: 1\n    tiers:\n      - price_usd: 0.01\n        condition: {field: context_length, op: \"<=\", value: 1000}\n      - price_usd: 0.05\n        condition: {field: context_length, op: \">\", value: 1000}\n",
        );
        let props = RequestProperties::default();
        assert_eq!(
            per_call_usd(
                &resolve_fixed_price(&metering, &props, None)
                    .unwrap()
                    .unwrap()
            ),
            0.05
        );
        // Response-repriced paths keep their provisional estimate.
        assert_eq!(
            per_call_usd(&resolve_price(&metering, &props, None, None).unwrap()),
            0.01
        );
    }

    #[test]
    fn upto_max_usd_requires_explicit_ceiling_for_multiple_dimensions() {
        let dims = || {
            vec![
                usage_dim(MeterDirection::Usage, BillingUnit::Requests, 1, 0.01, None),
                usage_dim(
                    MeterDirection::Output,
                    BillingUnit::Bytes,
                    1,
                    1.00,
                    Some(UsageMeter {
                        source: UsageMeterSource::ResponseHeader,
                        path: None,
                        header: Some("x-usage-bytes".into()),
                    }),
                ),
            ]
        };
        let implicit = upto_metering(dims(), None);
        let price = resolve_price(&implicit, &RequestProperties::default(), None, None);
        assert_eq!(upto_max_usd(&implicit, price.as_ref()), None);

        let explicit = upto_metering(
            dims(),
            Some(pay_types::metering::UptoMetering {
                max_usd: Some(2.0),
                ..Default::default()
            }),
        );
        let price = resolve_price(&explicit, &RequestProperties::default(), None, None);
        assert_eq!(upto_max_usd(&explicit, price.as_ref()), Some(2.0));
    }

    #[test]
    fn fixed_price_sums_every_dimension() {
        let metering = yaml_metering(
            "dimensions:\n  - direction: usage\n    unit: requests\n    scale: 1\n    tiers: [{price_usd: 0.001}]\n  - direction: usage\n    unit: requests\n    scale: 1\n    tiers: [{price_usd: 0.5}]\n",
        );
        let price = resolve_fixed_price(&metering, &RequestProperties::default(), None)
            .unwrap()
            .unwrap();
        assert!((per_call_usd(&price) - 0.501).abs() < 1e-12);
    }

    #[test]
    fn fixed_price_rejects_aggregate_settled_dimensions() {
        for dims in [
            "  - direction: input\n    unit: tokens\n    scale: 1000000\n    tiers: [{price_usd: 1.0}]\n",
            "  - direction: input\n    unit: quota_units\n    scale: 1\n    tiers: [{price_usd: 1.0}]\n",
            "  - direction: output\n    unit: bytes\n    scale: 1\n    meter: {source: response_header, header: x-bytes}\n    tiers: [{price_usd: 1.0}]\n",
        ] {
            let top = yaml_metering(&format!("dimensions:\n{dims}"));
            assert!(resolve_fixed_price(&top, &RequestProperties::default(), None).is_err());
            let nested = dims
                .replace("\n  ", "\n        ")
                .replacen("  -", "        -", 1);
            let variant = yaml_metering(&format!(
                "variants:\n{}  - param: model\n    value: big\n    dimensions:\n{nested}",
                request_variant("small", 0.01)
            ));
            assert!(
                resolve_fixed_price(&variant, &RequestProperties::default(), Some("small"))
                    .is_err()
            );
        }
        let flat = yaml_metering(&format!("variants:\n{}", request_variant("small", 0.01)));
        assert!(resolve_fixed_price(&flat, &RequestProperties::default(), None).is_ok());
    }

    fn fractional_plan(
        unit: BillingUnit,
        meter: UsageMeter,
        policy: MissingUsagePolicy,
    ) -> UptoSettlementPlan {
        settlement_plan(
            upto_metering(
                vec![usage_dim(MeterDirection::Usage, unit, 1, 1.0, Some(meter))],
                Some(pay_types::metering::UptoMetering {
                    max_usd: Some(1.0),
                    missing_usage: policy,
                    ..Default::default()
                }),
            ),
            1.0,
        )
    }

    #[test]
    fn fractional_count_usage_is_invalid_not_rounded_to_zero() {
        let mut headers = HeaderMap::new();
        headers.insert("x-usage-units", HeaderValue::from_static("0.49"));
        for (meter, headers, body) in [
            (
                response_json("/usage/units"),
                HeaderMap::new(),
                Some(br#"{"usage":{"units":0.49}}"#.as_slice()),
            ),
            (
                response_json("/usage/units"),
                HeaderMap::new(),
                Some(br#"{"usage":{"units":"0.49"}}"#.as_slice()),
            ),
            (response_header("x-usage-units"), headers, None),
        ] {
            let error = upto_actual_amount_from_response(
                &fractional_plan(
                    BillingUnit::Requests,
                    meter.clone(),
                    MissingUsagePolicy::Error,
                ),
                1_000_000,
                &headers,
                body,
            );
            assert_eq!(error, Err(UptoUsageError::MissingUsagePolicyError));
            // Other policies apply instead of a silent zero.
            let ceiling = upto_actual_amount_from_response(
                &fractional_plan(BillingUnit::Requests, meter, MissingUsagePolicy::Ceiling),
                1_000_000,
                &headers,
                body,
            )
            .unwrap();
            assert_eq!(ceiling.base_units, 1_000_000);
        }
    }

    #[test]
    fn fractional_continuous_usage_is_priced_exactly() {
        let actual = upto_actual_amount_from_response(
            &fractional_plan(
                BillingUnit::Seconds,
                response_json("/usage/seconds"),
                MissingUsagePolicy::Error,
            ),
            1_000_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"seconds":0.49}}"#),
        )
        .unwrap();
        assert!((actual.usd - 0.49).abs() < 1e-12);
        assert_eq!(actual.base_units, 490_000);

        // Whole counts still settle normally.
        let whole = upto_actual_amount_from_response(
            &fractional_plan(
                BillingUnit::Requests,
                response_json("/usage/units"),
                MissingUsagePolicy::Error,
            ),
            1_000_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"units":2.0}}"#),
        )
        .unwrap();
        assert_eq!(whole.usd, 1.0);
    }

    #[test]
    fn positive_usage_settles_at_least_one_base_unit() {
        let actual = upto_actual_amount_from_response(
            &fractional_plan(
                BillingUnit::Seconds,
                response_json("/usage/seconds"),
                MissingUsagePolicy::Error,
            ),
            1_000_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"seconds":0.0000001}}"#),
        )
        .unwrap();
        assert_eq!(actual.base_units, 1);
        let zero = upto_actual_amount_from_response(
            &fractional_plan(
                BillingUnit::Seconds,
                response_json("/usage/seconds"),
                MissingUsagePolicy::Error,
            ),
            1_000_000,
            &HeaderMap::new(),
            Some(br#"{"usage":{"seconds":0}}"#),
        )
        .unwrap();
        assert_eq!(zero.base_units, 0);
    }
}
