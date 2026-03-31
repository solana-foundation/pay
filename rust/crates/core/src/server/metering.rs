use crate::server::accounting::{AccountingKey, AccountingStore};
use pay_types::metering::{
    AccountingMode, ApiSpec, CompareOp, Endpoint, MeterCondition, MeterDimension, MeterVariant,
    Metering, PriceTier,
};
use serde::{Deserialize, Serialize};

/// Properties extracted from an incoming request, used to evaluate metering conditions.
#[derive(Debug, Default)]
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

/// Match a path pattern like "v1beta/models/{modelsId}:generateContent"
/// against a concrete path like "v1beta/models/gemini-2.0-flash:generateContent".
fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    pattern_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(pat, actual)| {
            if pat.starts_with('{') && pat.ends_with('}') {
                // Wildcard segment — matches anything
                true
            } else if pat.contains('{') {
                // Partial wildcard like "{modelsId}:generateContent"
                // Split on the first '{' and match the suffix
                if let Some(suffix_start) = pat.find('}') {
                    let suffix = &pat[suffix_start + 1..];
                    actual.ends_with(suffix)
                } else {
                    false
                }
            } else {
                pat == actual
            }
        })
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
        // If no variant matched, use the first one as default
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

fn resolve_variant<'a>(
    variants: &'a [MeterVariant],
    hint: Option<&str>,
) -> Option<&'a MeterVariant> {
    let hint = hint?;
    variants.iter().find(|v| hint.contains(&v.value))
}

fn resolve_dimensions(
    dimensions: &[MeterDimension],
    props: &RequestProperties,
    ctx: Option<&MeteringContext>,
) -> ResolvedPrice {
    let resolved = dimensions
        .iter()
        .map(|dim| {
            let price = resolve_tier(&dim.tiers, props, ctx, dim);
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
    _dim: &MeterDimension,
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

    // No volume tiers — resolve by condition
    for tier in tiers {
        if let Some(ref condition) = tier.condition
            && !evaluate_condition(condition, props)
        {
            continue;
        }
        return tier.price_usd;
    }

    tiers.last().map(|t| t.price_usd).unwrap_or(0.0)
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

fn evaluate_condition(condition: &MeterCondition, props: &RequestProperties) -> bool {
    let (actual, op, threshold) = match condition {
        MeterCondition::InputTokens { op, value } => (props.input_tokens, op, *value),
        MeterCondition::InputCharacters { op, value } => (props.input_characters, op, *value),
        MeterCondition::ContextLength { op, value } => (props.context_length, op, *value),
        MeterCondition::BodySize { op, value } => (props.body_size, op, *value),
        MeterCondition::DurationSeconds { op, value } => (props.duration_seconds, op, *value),
        MeterCondition::BatchSize { op, value } => (props.batch_size, op, *value),
        MeterCondition::ImagePixels { op, value } => (props.image_pixels, op, *value),
    };

    let actual = match actual {
        Some(v) => v,
        // If we don't have the property, assume the condition doesn't apply (pass)
        None => return true,
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
    fn test_resolve_tier_by_volume() {
        let tiers = vec![
            PriceTier {
                up_to: Some(100),
                price_usd: 0.0,
                condition: None,
                notes: None,
            },
            PriceTier {
                up_to: Some(1000),
                price_usd: 0.01,
                condition: None,
                notes: None,
            },
            PriceTier {
                up_to: None,
                price_usd: 0.005,
                condition: None,
                notes: None,
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
            },
            PriceTier {
                up_to: None,
                price_usd: 0.05,
                condition: None,
                notes: None,
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
            forward: pay_types::metering::ForwardConfig {
                url: "https://api.example.com".to_string(),
                auth: None,
            },
            accounting: AccountingMode::Pooled,
            endpoints,
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
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
                metering: None,
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
                metering: None,
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
                metering: None,
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
                }],
            }],
            variants: vec![],
            sku_tiers: vec![],
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
                        }],
                    }],
                },
                MeterVariant {
                    param: "model".to_string(),
                    value: "gemini-flash".to_string(),
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
                        }],
                    }],
                },
            ],
            sku_tiers: vec![],
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
    fn test_resolve_price_variant_no_match_uses_first() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![MeterVariant {
                param: "model".to_string(),
                value: "gemini-pro".to_string(),
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
                    }],
                }],
            }],
            sku_tiers: vec![],
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
                    },
                    PriceTier {
                        up_to: None,
                        price_usd: 0.02,
                        condition: None,
                        notes: None,
                    },
                ],
            }],
            variants: vec![],
            sku_tiers: vec![],
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
                    },
                    PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                    },
                ],
            }],
            variants: vec![],
            sku_tiers: vec![],
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
}
