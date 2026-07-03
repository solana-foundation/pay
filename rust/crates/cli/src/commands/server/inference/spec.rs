//! Synthesize an [`ApiSpec`] per discovered provider.
//!
//! Without pricing, specs have no metered endpoints, so every request is
//! `GateDecision::Passthrough`: forwarded upstream unmetered but still
//! captured by the proxy's `record_exchange` hook into PDB.
//!
//! With pricing (`--price-usd`, sandbox), the registry's `paid` endpoints
//! are emitted as metered endpoints (flat USD per request, `mpp-charge`) and
//! the spec carries a localnet operator, so the gate 402s them and verifies
//! retries in-gate against the sandbox MPP backend. Everything not listed
//! stays passthrough.

use pay_types::metering::{
    ApiCategory, ApiSpec, BillingUnit, Endpoint, MeterDimension, MeterDirection, Metering,
    OperatorConfig, PriceTier, RoutingConfig,
};

use super::discovery::{DiscoveredProvider, PaidEndpoint};

/// Pricing input for monetized spec synthesis.
pub struct SpecPricing {
    /// Flat USD price per paid request.
    pub price_usd: f64,
    /// Payout wallet — the gateway's own sandbox wallet.
    pub recipient: String,
}

/// Build the proxy spec for one discovered provider. Routed by subdomain:
/// `http://{slug}.localhost:{bind_port}/…`. With `pricing`, the provider's
/// registry `paid` endpoints become metered charge endpoints on a localnet
/// operator; without it (`None`), the spec is pure free passthrough.
pub fn provider_spec(provider: &DiscoveredProvider, pricing: Option<&SpecPricing>) -> ApiSpec {
    let endpoints = pricing
        .map(|p| {
            provider
                .spec
                .paid
                .iter()
                .map(|endpoint| paid_endpoint(endpoint, p.price_usd))
                .collect()
        })
        .unwrap_or_default();

    let mut api = ApiSpec {
        name: provider.spec.slug.clone(),
        subdomain: provider.spec.slug.clone(),
        title: provider.spec.title.clone(),
        description: format!(
            "Local {} inference proxied by pay serve inference",
            provider.spec.title
        ),
        category: ApiCategory::AiMl,
        version: provider.version.clone().unwrap_or_else(|| "v1".into()),
        env: Default::default(),
        routing: RoutingConfig::Proxy {
            url: provider.base_url.clone(),
            path_rewrites: Vec::new(),
            auth: None,
        },
        accounting: Default::default(),
        endpoints,
        free_tier: None,
        quotas: None,
        notes: None,
        operator: pricing.map(|p| sandbox_operator(&p.recipient)),
        recipients: Default::default(),
        session: None,
    };
    // Resolve per-endpoint scheme defaults (→ `[mpp-charge]`) so the gate,
    // challenge builder, and verifier all read the same scheme set — same as
    // `server start` does right after loading a YAML spec.
    api.apply_scheme_defaults();
    api
}

/// One flat-priced charge endpoint: `{direction: usage, unit: requests,
/// scale: 1}` with a single catch-all tier.
fn paid_endpoint(paid: &PaidEndpoint, price_usd: f64) -> Endpoint {
    Endpoint {
        method: paid.method.clone(),
        path: paid.path.clone(),
        description: None,
        resource: None,
        routing: None,
        metering: Some(Metering {
            dimensions: vec![MeterDimension {
                direction: MeterDirection::Usage,
                unit: BillingUnit::Requests,
                scale: 1,
                period: None,
                tiers: vec![PriceTier {
                    up_to: None,
                    price_usd,
                    condition: None,
                    notes: None,
                    splits: Vec::new(),
                }],
                meter: None,
            }],
            ..Default::default()
        }),
        subscription: None,
    }
}

/// Sandbox operator: explicitly `network: localnet` (which is also what the
/// `--sandbox` guard demands), paying out to the gateway's own wallet, with
/// the gateway sponsoring transaction fees. USDC is spelled out rather than
/// defaulted so a dumped spec stays self-describing.
fn sandbox_operator(recipient: &str) -> OperatorConfig {
    OperatorConfig {
        signer: None,
        recipient: Some(recipient.to_string()),
        currencies: [("usd".to_string(), vec!["USDC".to_string()])]
            .into_iter()
            .collect(),
        rpc_url: None,
        network: Some("localnet".to_string()),
        fee_payer: true,
        challenge_binding_secret: None,
        realm: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::server::inference::discovery::{IdentifyProbe, ProviderSpec};
    use pay_types::metering::{HttpMethod, Scheme};

    fn discovered() -> DiscoveredProvider {
        DiscoveredProvider {
            spec: ProviderSpec {
                slug: "ollama".into(),
                title: "Ollama".into(),
                ports: vec![11434],
                identify: vec![IdentifyProbe {
                    path: "/api/version".into(),
                    expect_json_key: Some("version".into()),
                    expect_body_contains: None,
                }],
                models: None,
                color: Some("#22c55e".into()),
                paid: vec![
                    PaidEndpoint {
                        method: HttpMethod::Post,
                        path: "api/chat".into(),
                    },
                    PaidEndpoint {
                        method: HttpMethod::Post,
                        path: "v1/chat/completions".into(),
                    },
                ],
            },
            base_url: "http://127.0.0.1:11434".into(),
            models: vec!["llama3.2:3b".into()],
            version: Some("0.9.1".into()),
        }
    }

    fn pricing() -> SpecPricing {
        SpecPricing {
            price_usd: 0.001,
            recipient: "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY".into(),
        }
    }

    #[test]
    fn synthesized_spec_is_passthrough_proxy() {
        let spec = provider_spec(&discovered(), None);

        assert_eq!(spec.name, "ollama");
        assert_eq!(spec.subdomain, "ollama");
        assert!(
            spec.endpoints.is_empty(),
            "no metered endpoints — everything must be Passthrough"
        );
        assert!(spec.operator.is_none(), "no payments without --price-usd");
        match &spec.routing {
            RoutingConfig::Proxy {
                url,
                path_rewrites,
                auth,
            } => {
                assert_eq!(url, "http://127.0.0.1:11434");
                assert!(path_rewrites.is_empty());
                assert!(auth.is_none());
            }
            other => panic!("expected proxy routing, got {other:?}"),
        }
    }

    #[test]
    fn priced_spec_meters_paid_endpoints_only() {
        let spec = provider_spec(&discovered(), Some(&pricing()));

        // Exactly the registry's `paid` list — nothing else gets an endpoint
        // entry, so unlisted paths (e.g. /api/tags) stay passthrough.
        let paths: Vec<&str> = spec.endpoints.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["api/chat", "v1/chat/completions"]);

        for endpoint in &spec.endpoints {
            assert!(matches!(endpoint.method, HttpMethod::Post));
            let meter = endpoint.metering.as_ref().expect("paid endpoint metered");
            assert_eq!(meter.dimensions.len(), 1);
            let dim = &meter.dimensions[0];
            assert_eq!(dim.direction, MeterDirection::Usage);
            assert_eq!(dim.unit, BillingUnit::Requests);
            assert_eq!(dim.scale, 1);
            assert_eq!(dim.tiers.len(), 1);
            assert_eq!(dim.tiers[0].price_usd, 0.001);
            assert_eq!(dim.tiers[0].up_to, None);
            // apply_scheme_defaults ran: charge scheme resolved, not None.
            assert_eq!(meter.accepted_schemes(), [Scheme::MppCharge]);
            assert!(meter.schemes.is_some(), "scheme defaults must be resolved");
        }

        let operator = spec.operator.as_ref().expect("priced spec has operator");
        assert_eq!(operator.network.as_deref(), Some("localnet"));
        assert_eq!(
            operator.recipient.as_deref(),
            Some("CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY")
        );
        assert!(operator.fee_payer);
        assert_eq!(
            operator.currencies.get("usd").map(Vec::as_slice),
            Some(["USDC".to_string()].as_slice())
        );

        // The full spec passes the same validation `server start` runs.
        assert_eq!(
            pay_types::metering::validate_api_spec(&spec),
            Vec::<String>::new()
        );
    }

    #[test]
    fn priced_spec_passes_the_sandbox_guard() {
        // The synthesized operator must satisfy `enforce_sandbox` (explicit
        // localnet) — the gateway refuses anything else in sandbox mode.
        let spec = provider_spec(&discovered(), Some(&pricing()));
        let network = spec.operator.as_ref().and_then(|o| o.network.as_deref());
        assert_eq!(network, Some("localnet"));
    }

    #[test]
    fn synthesized_spec_survives_yaml_roundtrip() {
        // `server start` loads specs through serde_yml; the synthesized spec
        // must stay loadable if a user dumps and edits it.
        for pricing in [None, Some(pricing())] {
            let spec = provider_spec(&discovered(), pricing.as_ref());
            let yaml = serde_yml::to_string(&spec).unwrap();
            let reloaded: ApiSpec = serde_yml::from_str(&yaml).unwrap();
            assert_eq!(reloaded.name, spec.name);
            assert_eq!(reloaded.endpoints.len(), spec.endpoints.len());
            assert!(matches!(reloaded.routing, RoutingConfig::Proxy { .. }));
        }
    }
}
