//! User-defined providers from `~/.config/pay/inference-providers.yml`.
//!
//! The file keeps its original schema (`slug`/`title`/`ports`/`color`/
//! `identify`/`models`/`paid`); each entry deserializes into a
//! [`CustomProvider`] implementing [`InferenceProvider`] with generic
//! JSON-key / body-contains identification and json-pointer model parsing.
//! User entries replace built-ins with the same slug and otherwise append at
//! higher probe priority.

use serde::Deserialize;

use super::{InferenceProvider, PaidEndpoint, default_endpoint_kind, get_body, models_from_json};

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

/// A data-driven provider defined by the user registry file.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomProvider {
    pub slug: String,
    pub title: String,
    pub ports: Vec<u16>,
    /// Brand color hex for UI badges.
    #[serde(default)]
    pub color: Option<String>,
    /// Probes tried in order; the first that passes identifies the provider.
    pub identify: Vec<IdentifyProbe>,
    #[serde(default)]
    pub models: Option<ModelsProbe>,
    /// Endpoints that get metered when the gateway runs with `--price-usd`.
    /// Paths carry no leading slash (gate convention). Anything not listed
    /// stays free passthrough.
    #[serde(default)]
    pub paid: Vec<PaidEndpoint>,
}

#[async_trait::async_trait]
impl InferenceProvider for CustomProvider {
    fn slug(&self) -> &str {
        &self.slug
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn ports(&self) -> &[u16] {
        &self.ports
    }
    fn color(&self) -> Option<&str> {
        self.color.as_deref()
    }
    async fn identify(&self, client: &reqwest::Client, base_url: &str) -> Option<Option<String>> {
        for probe in &self.identify {
            let Some(body) = get_body(client, base_url, &probe.path).await else {
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
    async fn list_models(&self, client: &reqwest::Client, base_url: &str) -> Vec<String> {
        match &self.models {
            Some(probe) => {
                models_from_json(
                    client,
                    base_url,
                    &probe.path,
                    &probe.json_pointer,
                    &probe.name_key,
                )
                .await
            }
            None => Vec::new(),
        }
    }
    fn paid_endpoints(&self) -> Vec<PaidEndpoint> {
        self.paid.clone()
    }
    fn endpoint_kind(&self, path: &str) -> &'static str {
        default_endpoint_kind(path)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{base_url, client, rt, stub};
    use super::*;

    fn jan() -> CustomProvider {
        serde_yml::from_str(
            r##"
slug: jan
title: Jan
ports: [1337]
color: "#94a3b8"
identify:
  - { path: /healthz, expect_body_contains: jan-server }
  - { path: /v1/models, expect_json_key: data }
models: { path: /v1/models, json_pointer: /data, name_key: id }
paid:
  - { method: POST, path: v1/chat/completions }
"##,
        )
        .unwrap()
    }

    #[test]
    fn identify_matches_json_key_and_reads_version() {
        rt().block_on(async {
            let port = stub(vec![("/v1/models", r#"{"data":[],"version":"1.2.3"}"#)]).await;
            let version = jan().identify(&client(), &base_url(port)).await;
            assert_eq!(version, Some(Some("1.2.3".to_string())));
        });
    }

    #[test]
    fn identify_matches_body_contains_without_version() {
        rt().block_on(async {
            let port = stub(vec![("/healthz", "jan-server ok")]).await;
            assert_eq!(jan().identify(&client(), &base_url(port)).await, Some(None));
        });
    }

    #[test]
    fn identify_rejects_generic_200() {
        rt().block_on(async {
            let port = stub(vec![
                ("/healthz", "<html>hello</html>"),
                ("/v1/models", "<html>hello</html>"),
            ])
            .await;
            assert_eq!(jan().identify(&client(), &base_url(port)).await, None);
        });
    }

    #[test]
    fn probe_without_expectation_never_matches() {
        rt().block_on(async {
            let provider: CustomProvider = serde_yml::from_str(
                r#"
slug: bad
title: Bad
ports: [9999]
identify:
  - { path: /anything }
"#,
            )
            .unwrap();
            let port = stub(vec![("/anything", r#"{"ok":true}"#)]).await;
            assert_eq!(provider.identify(&client(), &base_url(port)).await, None);
        });
    }

    #[test]
    fn list_models_uses_json_pointer_and_name_key() {
        rt().block_on(async {
            let port = stub(vec![(
                "/v1/models",
                r#"{"data":[{"id":"jan-nano"},{"id":"jan-large"}]}"#,
            )])
            .await;
            let models = jan().list_models(&client(), &base_url(port)).await;
            assert_eq!(models, vec!["jan-nano", "jan-large"]);
        });
    }

    #[test]
    fn missing_models_probe_yields_no_models() {
        rt().block_on(async {
            let mut provider = jan();
            provider.models = None;
            let port = stub(vec![("/v1/models", r#"{"data":[{"id":"jan-nano"}]}"#)]).await;
            assert!(
                provider
                    .list_models(&client(), &base_url(port))
                    .await
                    .is_empty()
            );
        });
    }

    #[test]
    fn paid_endpoints_and_endpoint_kind_come_from_the_file_and_default_map() {
        let provider = jan();
        let paths: Vec<String> = provider
            .paid_endpoints()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, ["v1/chat/completions"]);
        assert_eq!(provider.endpoint_kind("/v1/chat/completions"), "chat");
        assert_eq!(provider.endpoint_kind("/v1/models"), "other");
    }
}
