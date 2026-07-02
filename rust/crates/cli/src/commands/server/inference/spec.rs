//! Synthesize an [`ApiSpec`] per discovered provider.
//!
//! Specs have no metered endpoints, so every request is
//! `GateDecision::Passthrough`: forwarded upstream unmetered but still
//! captured by the proxy's `record_exchange` hook into PDB. Monetization
//! (token-priced metering) later flips this synthesis, nothing downstream.

use pay_types::metering::{ApiCategory, ApiSpec, RoutingConfig};

use super::discovery::DiscoveredProvider;

/// Build the passthrough proxy spec for one discovered provider. Routed by
/// subdomain: `http://{slug}.localhost:{bind_port}/…`.
pub fn provider_spec(provider: &DiscoveredProvider) -> ApiSpec {
    ApiSpec {
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
        endpoints: Vec::new(),
        free_tier: None,
        quotas: None,
        notes: None,
        operator: None,
        recipients: Default::default(),
        session: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::server::inference::discovery::{IdentifyProbe, ProviderSpec};

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
            },
            base_url: "http://127.0.0.1:11434".into(),
            models: vec!["llama3.2:3b".into()],
            version: Some("0.9.1".into()),
        }
    }

    #[test]
    fn synthesized_spec_is_passthrough_proxy() {
        let spec = provider_spec(&discovered());

        assert_eq!(spec.name, "ollama");
        assert_eq!(spec.subdomain, "ollama");
        assert!(
            spec.endpoints.is_empty(),
            "no metered endpoints — everything must be Passthrough"
        );
        assert!(spec.operator.is_none(), "no payments in v1");
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
    fn synthesized_spec_survives_yaml_roundtrip() {
        // `server start` loads specs through serde_yml; the synthesized spec
        // must stay loadable if a user dumps and edits it.
        let spec = provider_spec(&discovered());
        let yaml = serde_yml::to_string(&spec).unwrap();
        let reloaded: ApiSpec = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(reloaded.name, spec.name);
        assert!(matches!(reloaded.routing, RoutingConfig::Proxy { .. }));
    }
}
