//! Remote pay inference gateways registered via `pay inference add`.
//!
//! A [`RemoteGateway`] wraps one provider advertised by a remote gateway's
//! discovery document behind the same [`InferenceProvider`] trait as the
//! local servers and hosted catalog entries. Like catalog providers it
//! probes no local ports (`hosted()`), so a pick routes the payer proxy
//! straight at the gateway origin, which answers its own 402 challenges.

use super::{
    Dialect, InferenceProvider, PaidEndpoint, identify_json_key, openai_paid_endpoints, post,
};

pub struct RemoteGateway {
    slug: String,
    title: String,
    color: Option<String>,
}

impl RemoteGateway {
    pub fn new(slug: String, title: String, color: Option<String>) -> Self {
        Self { slug, title, color }
    }
}

#[async_trait::async_trait]
impl InferenceProvider for RemoteGateway {
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
        self.color.as_deref()
    }
    async fn identify(&self, client: &reqwest::Client, base_url: &str) -> Option<Option<String>> {
        // The gateway discovery document is provider-agnostic positive
        // identification: a generic web server on the origin won't have it.
        identify_json_key(client, base_url, "/__402/pdb/api/config", "providers").await
    }
    async fn list_models(&self, _client: &reqwest::Client, _base_url: &str) -> Vec<String> {
        // Models come from the discovery document at registration/probe
        // time; the gateway meters the model-list route per provider.
        Vec::new()
    }
    fn paid_endpoints(&self) -> Vec<PaidEndpoint> {
        // Gateways front llama.cpp/Ollama-class servers: the OpenAI trio
        // plus llama.cpp's native paths. Only the chat path is consulted on
        // the client side (payer-proxy routing).
        let mut paid = openai_paid_endpoints();
        paid.extend([post("completion"), post("infill"), post("embedding")]);
        paid
    }
    fn dialect(&self) -> Dialect {
        Dialect::OpenAiCompat
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{base_url, client, rt, stub};
    use super::*;

    fn gateway() -> RemoteGateway {
        RemoteGateway::new(
            "llama-cpp".to_string(),
            "llama.cpp".to_string(),
            Some("#f59e0b".to_string()),
        )
    }

    #[test]
    fn is_hosted_and_openai_compatible() {
        let g = gateway();
        assert!(g.ports().is_empty(), "remote gateways probe no local ports");
        assert_eq!(g.dialect(), Dialect::OpenAiCompat);
        assert!(
            g.paid_endpoints()
                .iter()
                .any(|ep| ep.path == "v1/chat/completions")
        );
    }

    #[test]
    fn identify_matches_the_discovery_document() {
        rt().block_on(async {
            let port = stub(vec![(
                "/__402/pdb/api/config",
                r#"{"mode":"inference","providers":[]}"#,
            )])
            .await;
            assert_eq!(
                gateway().identify(&client(), &base_url(port)).await,
                Some(None)
            );
        });
    }

    #[test]
    fn identify_rejects_generic_200() {
        rt().block_on(async {
            let port = stub(vec![("/__402/pdb/api/config", "<html>hello</html>")]).await;
            assert_eq!(gateway().identify(&client(), &base_url(port)).await, None);
        });
    }
}
