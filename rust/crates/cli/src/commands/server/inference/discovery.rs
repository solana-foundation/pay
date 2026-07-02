//! Local inference provider discovery — probes well-known ports with
//! provider-specific identify endpoints and reads model lists.

use std::time::Duration;

use serde::Deserialize;

/// Embedded registry of known providers (top 5).
const EMBEDDED_REGISTRY: &str = include_str!("providers.yml");

/// User override/extension file, merged over the embedded registry by slug.
const USER_REGISTRY_PATH: &str = "~/.config/pay/inference-providers.yml";

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderRegistry {
    pub providers: Vec<ProviderSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderSpec {
    pub slug: String,
    pub title: String,
    pub ports: Vec<u16>,
    /// Probes tried in order; the first that passes identifies the provider.
    pub identify: Vec<IdentifyProbe>,
    #[serde(default)]
    pub models: Option<ModelsProbe>,
    /// Brand color hex for UI badges.
    #[serde(default)]
    pub color: Option<String>,
}

/// A probe passes only on a provider-specific positive signal — a bare
/// `200 OK` never counts, so generic dev servers on contested ports (8080)
/// don't false-positive.
#[derive(Debug, Clone, Deserialize)]
pub struct IdentifyProbe {
    pub path: String,
    /// Passes if the response is JSON with this top-level key.
    #[serde(default)]
    pub expect_json_key: Option<String>,
    /// Passes if the response body contains this substring.
    #[serde(default)]
    pub expect_body_contains: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsProbe {
    pub path: String,
    /// JSON pointer to the model array (e.g. `/models`, `/data`).
    pub json_pointer: String,
    /// Key of the model name within each array item (e.g. `name`, `id`).
    pub name_key: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveredProvider {
    pub spec: ProviderSpec,
    /// e.g. `http://127.0.0.1:11434`.
    pub base_url: String,
    pub models: Vec<String>,
    /// Server-reported version when the identify response carries one.
    pub version: Option<String>,
}

/// Load the embedded registry, merged with the user's override file when
/// present. User entries replace embedded entries with the same slug and
/// otherwise append (at higher priority for contested ports).
pub fn load_registry() -> pay_core::Result<ProviderRegistry> {
    let mut registry: ProviderRegistry = serde_yml::from_str(EMBEDDED_REGISTRY)
        .map_err(|e| pay_core::Error::Config(format!("embedded providers.yml invalid: {e}")))?;

    let user_path = shellexpand::tilde(USER_REGISTRY_PATH).to_string();
    if let Ok(contents) = std::fs::read_to_string(&user_path) {
        let user: ProviderRegistry = serde_yml::from_str(&contents)
            .map_err(|e| pay_core::Error::Config(format!("{user_path} invalid: {e}")))?;
        registry = merge_registries(registry, user);
    }

    Ok(registry)
}

fn merge_registries(embedded: ProviderRegistry, user: ProviderRegistry) -> ProviderRegistry {
    let user_slugs: Vec<&str> = user.providers.iter().map(|p| p.slug.as_str()).collect();
    let mut providers = user.providers.clone();
    providers.extend(
        embedded
            .providers
            .into_iter()
            .filter(|p| !user_slugs.contains(&p.slug.as_str())),
    );
    ProviderRegistry { providers }
}

/// Hard cap on one provider's whole probe pass (all ports + model list) so a
/// slow-to-accept server can't stall discovery.
const PROVIDER_PROBE_BUDGET: Duration = Duration::from_secs(1);

/// Progress events emitted by [`discover_with`] as each provider is probed.
pub enum ProbeEvent<'a> {
    Started(&'a ProviderSpec),
    Found(&'a DiscoveredProvider),
    Missed(&'a ProviderSpec),
}

/// Probe registry providers in order and return those that identified
/// positively. A port is claimed by the first provider that identifies on
/// it, so contested ports (8080) resolve deterministically by registry
/// order. Each provider gets at most [`PROVIDER_PROBE_BUDGET`].
pub async fn discover(
    registry: &ProviderRegistry,
    timeout: Duration,
    restrict: Option<&[String]>,
) -> Vec<DiscoveredProvider> {
    discover_with(registry, timeout, restrict, |_| {}).await
}

/// [`discover`] with a per-provider progress callback (drives the startup
/// spinner).
pub async fn discover_with(
    registry: &ProviderRegistry,
    timeout: Duration,
    restrict: Option<&[String]>,
    mut on_event: impl FnMut(ProbeEvent<'_>),
) -> Vec<DiscoveredProvider> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client");

    let mut claimed_ports: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut discovered = Vec::new();

    for spec in &registry.providers {
        if let Some(allowed) = restrict
            && !allowed.iter().any(|s| s == &spec.slug)
        {
            continue;
        }
        on_event(ProbeEvent::Started(spec));

        let found = tokio::time::timeout(
            PROVIDER_PROBE_BUDGET,
            probe_provider(&client, spec, &claimed_ports),
        )
        .await
        .ok()
        .flatten();

        match found {
            Some((provider, port)) => {
                claimed_ports.insert(port);
                on_event(ProbeEvent::Found(&provider));
                discovered.push(provider);
            }
            None => on_event(ProbeEvent::Missed(spec)),
        }
    }

    discovered
}

/// Try each of the provider's ports (skipping ones already claimed by an
/// earlier provider); first identify hit wins.
async fn probe_provider(
    client: &reqwest::Client,
    spec: &ProviderSpec,
    claimed_ports: &std::collections::HashSet<u16>,
) -> Option<(DiscoveredProvider, u16)> {
    for port in &spec.ports {
        if claimed_ports.contains(port) {
            continue;
        }
        let base_url = format!("http://127.0.0.1:{port}");
        if let Some(version) = identify(client, &base_url, spec).await {
            let models = match &spec.models {
                Some(probe) => fetch_models(client, &base_url, probe).await,
                None => Vec::new(),
            };
            return Some((
                DiscoveredProvider {
                    spec: spec.clone(),
                    base_url,
                    models,
                    version,
                },
                *port,
            ));
        }
    }
    None
}

/// Run a provider's identify probes against `base_url`. Returns
/// `Some(version)` on a positive match (`Some(None)` when the response
/// carries no version), `None` when nothing matched.
async fn identify(
    client: &reqwest::Client,
    base_url: &str,
    spec: &ProviderSpec,
) -> Option<Option<String>> {
    for probe in &spec.identify {
        let url = format!("{base_url}{}", probe.path);
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(body) = resp.text().await else {
            continue;
        };

        if let Some(key) = &probe.expect_json_key {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                && json.get(key).is_some()
            {
                let version = json
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                return Some(version);
            }
            continue;
        }
        if let Some(needle) = &probe.expect_body_contains {
            if body.contains(needle.as_str()) {
                return Some(None);
            }
            continue;
        }
        // A probe with no expectation is a config error; never match on it.
    }
    None
}

async fn fetch_models(
    client: &reqwest::Client,
    base_url: &str,
    probe: &ModelsProbe,
) -> Vec<String> {
    let url = format!("{base_url}{}", probe.path);
    let Ok(resp) = client.get(&url).send().await else {
        return Vec::new();
    };
    let Ok(body) = resp.text().await else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Vec::new();
    };
    json.pointer(&probe.json_pointer)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get(&probe.name_key)?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl DiscoveredProvider {
    pub fn summary(&self, up: bool) -> pay_pdb::types::ProviderSummary {
        pay_pdb::types::ProviderSummary {
            slug: self.spec.slug.clone(),
            title: self.spec.title.clone(),
            base_url: self.base_url.clone(),
            up,
            models: self.models.clone(),
            version: self.version.clone(),
            color: self.spec.color.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Serve `routes` on an ephemeral port; returns the port.
    async fn stub(routes: Vec<(&'static str, &'static str)>) -> u16 {
        let mut router = Router::new();
        for (path, body) in routes {
            router = router.route(path, get(move || async move { body.to_string() }));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        port
    }

    fn registry_with_ports(ports: &[(&str, u16)]) -> ProviderRegistry {
        let mut registry: ProviderRegistry = serde_yml::from_str(EMBEDDED_REGISTRY).unwrap();
        for provider in &mut registry.providers {
            provider.ports = ports
                .iter()
                .filter(|(slug, _)| *slug == provider.slug)
                .map(|(_, port)| *port)
                .collect();
        }
        registry
    }

    #[test]
    fn embedded_registry_parses_with_five_providers() {
        let registry: ProviderRegistry = serde_yml::from_str(EMBEDDED_REGISTRY).unwrap();
        let slugs: Vec<_> = registry.providers.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["ollama", "lm-studio", "llama-cpp", "vllm", "exo"]
        );
        for provider in &registry.providers {
            assert!(
                !provider.identify.is_empty(),
                "{} needs probes",
                provider.slug
            );
            assert!(
                provider
                    .identify
                    .iter()
                    .all(|p| p.expect_json_key.is_some() || p.expect_body_contains.is_some()),
                "{} has a probe with no positive expectation",
                provider.slug
            );
        }
    }

    #[test]
    fn discovers_ollama_with_models_and_version() {
        rt().block_on(async {
            let port = stub(vec![
                ("/api/version", r#"{"version":"0.9.1"}"#),
                (
                    "/api/tags",
                    r#"{"models":[{"name":"llama3.2:3b"},{"name":"nomic-embed-text"}]}"#,
                ),
            ])
            .await;

            let registry = registry_with_ports(&[("ollama", port)]);
            let found = discover(&registry, Duration::from_millis(400), None).await;

            assert_eq!(found.len(), 1);
            assert_eq!(found[0].spec.slug, "ollama");
            assert_eq!(found[0].base_url, format!("http://127.0.0.1:{port}"));
            assert_eq!(found[0].version.as_deref(), Some("0.9.1"));
            assert_eq!(found[0].models, vec!["llama3.2:3b", "nomic-embed-text"]);
        });
    }

    #[test]
    fn generic_200_server_is_not_identified() {
        rt().block_on(async {
            // A dev server answering 200 HTML on every llama.cpp probe path.
            let port = stub(vec![
                ("/props", "<html>hello</html>"),
                ("/v1/models", "<html>hello</html>"),
            ])
            .await;

            let registry = registry_with_ports(&[("llama-cpp", port)]);
            let found = discover(&registry, Duration::from_millis(400), None).await;
            assert!(found.is_empty(), "bare 200s must not identify a provider");
        });
    }

    #[test]
    fn contested_port_resolves_by_registry_order() {
        rt().block_on(async {
            // A llama.cpp server: /props matches; exo's /v1/models probe would
            // also match (llama.cpp serves OpenAI-compat /v1/models with data).
            let port = stub(vec![
                ("/props", r#"{"default_generation_settings":{}}"#),
                ("/v1/models", r#"{"data":[{"id":"qwen2.5-7b"}]}"#),
            ])
            .await;

            // Both candidates on the same port; llama-cpp comes first in the
            // registry so it must win.
            let registry = registry_with_ports(&[("llama-cpp", port), ("exo", port)]);
            let found = discover(&registry, Duration::from_millis(400), None).await;

            assert_eq!(found.len(), 1);
            assert_eq!(found[0].spec.slug, "llama-cpp");
            assert_eq!(found[0].models, vec!["qwen2.5-7b"]);
        });
    }

    #[test]
    fn down_provider_is_absent() {
        rt().block_on(async {
            // Nothing listens on this port (bind + drop to reserve a dead one).
            let dead = {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                listener.local_addr().unwrap().port()
            };
            let registry = registry_with_ports(&[("ollama", dead)]);
            let found = discover(&registry, Duration::from_millis(200), None).await;
            assert!(found.is_empty());
        });
    }

    #[test]
    fn restrict_filters_providers() {
        rt().block_on(async {
            let port = stub(vec![("/api/version", r#"{"version":"0.9.1"}"#)]).await;
            let registry = registry_with_ports(&[("ollama", port)]);

            let found = discover(
                &registry,
                Duration::from_millis(400),
                Some(&["vllm".to_string()]),
            )
            .await;
            assert!(found.is_empty(), "--providers vllm must skip ollama probes");
        });
    }

    #[test]
    fn probe_events_fire_in_registry_order() {
        rt().block_on(async {
            let port = stub(vec![("/api/version", r#"{"version":"0.9.1"}"#)]).await;
            let registry = registry_with_ports(&[("ollama", port)]);

            let mut events: Vec<String> = Vec::new();
            let found = discover_with(&registry, Duration::from_millis(400), None, |event| {
                events.push(match event {
                    ProbeEvent::Started(spec) => format!("started:{}", spec.slug),
                    ProbeEvent::Found(provider) => format!("found:{}", provider.spec.slug),
                    ProbeEvent::Missed(spec) => format!("missed:{}", spec.slug),
                });
            })
            .await;

            assert_eq!(found.len(), 1);
            assert_eq!(
                events,
                vec![
                    "started:ollama",
                    "found:ollama",
                    "started:lm-studio",
                    "missed:lm-studio",
                    "started:llama-cpp",
                    "missed:llama-cpp",
                    "started:vllm",
                    "missed:vllm",
                    "started:exo",
                    "missed:exo",
                ]
            );
        });
    }

    #[test]
    fn user_registry_overrides_by_slug_and_appends() {
        let embedded: ProviderRegistry = serde_yml::from_str(EMBEDDED_REGISTRY).unwrap();
        let user: ProviderRegistry = serde_yml::from_str(
            r#"
providers:
  - slug: ollama
    title: Ollama (custom port)
    ports: [11500]
    identify:
      - { path: /api/version, expect_json_key: version }
  - slug: jan
    title: Jan
    ports: [1337]
    identify:
      - { path: /v1/models, expect_json_key: data }
"#,
        )
        .unwrap();

        let merged = merge_registries(embedded, user);
        let ollama = merged
            .providers
            .iter()
            .find(|p| p.slug == "ollama")
            .unwrap();
        assert_eq!(ollama.ports, vec![11500]);
        assert!(merged.providers.iter().any(|p| p.slug == "jan"));
        // No duplicate ollama entry from the embedded registry.
        assert_eq!(
            merged
                .providers
                .iter()
                .filter(|p| p.slug == "ollama")
                .count(),
            1
        );
    }
}
