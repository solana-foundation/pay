//! Inference provider implementations for `pay serve inference`.
//!
//! Each built-in local inference server (Ollama, LM Studio, llama.cpp, vLLM,
//! exo) implements [`InferenceProvider`] in its own file with its constants
//! and tests. Users extend or override the built-ins (matched by slug) via
//! `~/.config/pay/inference-providers.yml`, which loads as
//! [`CustomProvider`]s implementing the same trait.

pub mod custom;
pub mod exo;
pub mod llama_cpp;
pub mod lm_studio;
pub mod ollama;
pub mod vllm;

use std::sync::Arc;

use serde::Deserialize;

pub use custom::CustomProvider;

/// One monetizable endpoint of a provider's API surface.
#[derive(Debug, Clone, Deserialize)]
pub struct PaidEndpoint {
    pub method: pay_types::metering::HttpMethod,
    pub path: String,
}

/// A local inference server the gateway can discover, front, and meter.
#[async_trait::async_trait]
pub trait InferenceProvider: Send + Sync {
    fn slug(&self) -> &str;
    fn title(&self) -> &str;
    /// Well-known ports probed during discovery, in order.
    fn ports(&self) -> &[u16];
    /// Brand color hex for UI badges.
    fn color(&self) -> Option<&str>;
    /// Provider-specific positive identification (bare 200s never match).
    /// Returns the server version when the response carries one:
    /// `Some(version)` on a positive match (`Some(None)` when the response
    /// has no version), `None` when nothing matched.
    async fn identify(&self, client: &reqwest::Client, base_url: &str) -> Option<Option<String>>;
    async fn list_models(&self, client: &reqwest::Client, base_url: &str) -> Vec<String>;
    /// Endpoints that get metered when the gateway runs with `--price-usd`
    /// (method + path, no leading slash — gate convention). Anything not
    /// listed stays free passthrough.
    fn paid_endpoints(&self) -> Vec<PaidEndpoint>;
    /// `chat` | `completion` | `embeddings` | `other` for a request path.
    fn endpoint_kind(&self, path: &str) -> &'static str {
        default_endpoint_kind(path)
    }
}

/// Built-in providers in probe priority order. Order matters: when two
/// providers share a port (8080 is contested in the wild), the first entry
/// whose identify probe passes claims it.
pub fn builtin_providers() -> Vec<Arc<dyn InferenceProvider>> {
    vec![
        Arc::new(ollama::Ollama),
        Arc::new(lm_studio::LmStudio),
        Arc::new(llama_cpp::LlamaCpp),
        Arc::new(vllm::Vllm),
        Arc::new(exo::Exo),
    ]
}

/// Default endpoint-kind mapping shared by the OpenAI-compatible surface.
/// `chat` is checked first — `/v1/chat/completions` contains both markers —
/// and `/v1/messages` (Anthropic-compat) is a chat endpoint too. Covers
/// Ollama's native paths as well (`/api/chat`, `/api/generate`,
/// `/api/embed`) and llama.cpp's (`/completion`, `/infill`, `/embedding`).
pub fn default_endpoint_kind(path: &str) -> &'static str {
    let path = path.to_ascii_lowercase();
    if path.contains("chat") || path.contains("messages") {
        "chat"
    } else if path.contains("embed") {
        "embeddings"
    } else if path.contains("completion") || path.contains("generate") || path.contains("infill") {
        "completion"
    } else {
        "other"
    }
}

/// A POST paid endpoint (all inference calls are POSTs).
pub(crate) fn post(path: &str) -> PaidEndpoint {
    PaidEndpoint {
        method: pay_types::metering::HttpMethod::Post,
        path: path.to_string(),
    }
}

/// The three OpenAI-compatible paid endpoints every provider serves.
pub(crate) fn openai_paid_endpoints() -> Vec<PaidEndpoint> {
    vec![
        post("v1/chat/completions"),
        post("v1/completions"),
        post("v1/embeddings"),
    ]
}

/// GET `base_url + path`, returning the body only on a 2xx response.
pub(crate) async fn get_body(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
) -> Option<String> {
    let resp = client.get(format!("{base_url}{path}")).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

/// GET `base_url + path` and parse the body as JSON.
pub(crate) async fn get_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
) -> Option<serde_json::Value> {
    let body = get_body(client, base_url, path).await?;
    serde_json::from_str(&body).ok()
}

/// Identify by JSON key: passes when GET `path` returns JSON with `key` at
/// the top level. The response's `version` field (when present) is reported
/// as the server version. A JSON-key match keeps generic dev servers on
/// contested ports from false-positiving on bare 200s.
pub(crate) async fn identify_json_key(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    key: &str,
) -> Option<Option<String>> {
    let json = get_json(client, base_url, path).await?;
    json.get(key)?;
    Some(
        json.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    )
}

/// Model names from a JSON endpoint: the array at `json_pointer` (e.g.
/// `/models`, `/data`), taking `name_key` (e.g. `name`, `id`) of each item.
pub(crate) async fn models_from_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    json_pointer: &str,
    name_key: &str,
) -> Vec<String> {
    let Some(json) = get_json(client, base_url, path).await else {
        return Vec::new();
    };
    json.pointer(json_pointer)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get(name_key)?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// OpenAI-compatible model listing: `/v1/models` → `data[].id`.
pub(crate) async fn openai_models(client: &reqwest::Client, base_url: &str) -> Vec<String> {
    models_from_json(client, base_url, "/v1/models", "/data", "id").await
}

#[cfg(test)]
pub(crate) mod test_support {
    use axum::Router;
    use axum::routing::get;

    pub(crate) fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Serve `routes` on an ephemeral port; returns the port.
    pub(crate) async fn stub(routes: Vec<(&'static str, &'static str)>) -> u16 {
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

    pub(crate) fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(400))
            .build()
            .unwrap()
    }

    pub(crate) fn base_url(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_registered_in_probe_priority_order() {
        let providers = builtin_providers();
        let slugs: Vec<&str> = providers.iter().map(|p| p.slug()).collect();
        assert_eq!(
            slugs,
            vec!["ollama", "lm-studio", "llama-cpp", "vllm", "exo"]
        );
    }

    #[test]
    fn builtin_paid_endpoints_follow_gate_conventions() {
        for provider in builtin_providers() {
            for endpoint in provider.paid_endpoints() {
                assert!(
                    matches!(endpoint.method, pay_types::metering::HttpMethod::Post),
                    "{}: inference calls are POSTs",
                    provider.slug()
                );
                assert!(
                    !endpoint.path.starts_with('/'),
                    "{}: paid paths follow the gate's no-leading-slash convention",
                    provider.slug()
                );
            }
        }
    }

    #[test]
    fn default_endpoint_kind_mapping() {
        assert_eq!(default_endpoint_kind("/v1/chat/completions"), "chat");
        assert_eq!(default_endpoint_kind("/v1/messages"), "chat");
        assert_eq!(default_endpoint_kind("/v1/completions"), "completion");
        assert_eq!(default_endpoint_kind("/v1/embeddings"), "embeddings");
        assert_eq!(default_endpoint_kind("/api/tags"), "other");
    }
}
