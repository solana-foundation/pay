//! `pay inference` — manage remote inference gateways.
//!
//! `add <domain-or-ip>` fetches the gateway's discovery document
//! (`/__402/pdb/api/config`: title, network, providers, models, per-model
//! pricing) and caches it in `~/.config/pay/inference.yaml`; `ls` prints the
//! registry; `rm` drops an entry. Registered gateways appear in the
//! `pay claude` provider picker next to local servers and hosted catalog
//! entries — see `claude::discover_registry_gateways`.

pub mod registry;

use std::time::Duration;

use clap::Subcommand;
use owo_colors::OwoColorize;

use registry::{GatewayEntry, InferenceRegistry};

/// Remote gateways answer over the public internet (TLS handshake included)
/// — same budget as the hosted catalog probes.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Subcommand)]
pub enum InferenceCommand {
    /// Register a remote pay inference gateway by domain or IP (fetches and
    /// caches its providers, models, and pricing).
    Add(AddCommand),
    /// Remove a registered gateway.
    #[command(alias = "remove")]
    Rm(RmCommand),
    /// List registered gateways and their cached providers.
    #[command(alias = "list")]
    Ls,
}

#[derive(clap::Args)]
pub struct AddCommand {
    /// Gateway origin: `gateway.example.com`, `203.0.113.4:8080`, or a full
    /// `http(s)://` URL. Without a scheme, https is tried first, then http.
    pub origin: String,
}

#[derive(clap::Args)]
pub struct RmCommand {
    /// Origin to remove (scheme optional — matches how it was added).
    pub origin: String,
}

impl InferenceCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match self {
            Self::Add(cmd) => cmd.run(),
            Self::Rm(cmd) => cmd.run(),
            Self::Ls => run_ls(),
        }
    }
}

/// `pay inference` with no subcommand: list, then show the verbs.
pub fn run_default() -> pay_core::Result<()> {
    run_ls()?;
    eprintln!();
    eprintln!(
        "{}",
        "Subcommands: add <domain-or-ip> · rm <origin> · ls".dimmed()
    );
    Ok(())
}

impl AddCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let (origin, snapshot) = fetch_snapshot(&self.origin)?;

        let entry = GatewayEntry {
            origin: origin.clone(),
            title: snapshot.title.clone(),
            network: snapshot.network.clone(),
            refreshed_at: Some(chrono::Utc::now().to_rfc3339()),
            providers: snapshot.providers.clone(),
        };

        let mut registry = InferenceRegistry::load()?;
        let replaced = registry.upsert(entry);
        registry.save()?;

        let verb = if replaced { "updated" } else { "added" };
        eprintln!(
            "{} {} {}{}",
            "⏺".green(),
            verb,
            origin.bold(),
            snapshot
                .network
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default(),
        );
        for provider in &snapshot.providers {
            print_provider(provider);
        }
        if snapshot.providers.is_empty() {
            eprintln!("  {}", "no providers reported by the gateway".dimmed());
        }
        Ok(())
    }
}

impl RmCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let mut registry = InferenceRegistry::load()?;
        let Some(removed) = registry.remove(&self.origin) else {
            return Err(pay_core::Error::Config(format!(
                "no registered gateway matches `{}` — see `pay inference ls`",
                self.origin
            )));
        };
        registry.save()?;
        eprintln!("{} removed {}", "⏺".green(), removed.origin.bold());
        Ok(())
    }
}

fn run_ls() -> pay_core::Result<()> {
    let registry = InferenceRegistry::load()?;
    if registry.gateways.is_empty() {
        eprintln!(
            "{}",
            "No remote inference gateways registered. Add one with \
             `pay inference add <domain-or-ip>`."
                .dimmed()
        );
        return Ok(());
    }
    for gateway in &registry.gateways {
        eprintln!(
            "{}{}{}",
            gateway.origin.bold(),
            gateway
                .network
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default(),
            gateway
                .refreshed_at
                .as_deref()
                .map(|t| format!("  {}", format!("refreshed {t}").dimmed()))
                .unwrap_or_default(),
        );
        for provider in &gateway.providers {
            print_provider(provider);
        }
    }
    Ok(())
}

fn print_provider(provider: &pay_pdb::types::ProviderSummary) {
    let models = if provider.models.is_empty() {
        "no models".dimmed().to_string()
    } else {
        provider.models.join(", ")
    };
    let price = provider
        .model_pricing
        .iter()
        .find_map(|p| p.price.clone())
        .map(|p| format!("  {}", p.dimmed()))
        .unwrap_or_default();
    eprintln!("  {} · {models}{price}", provider.title.bold());
}

/// The gateway discovery document (subset we cache). Mirrors what
/// `pay serve inference` exposes at [`registry::GATEWAY_CONFIG_PATH`].
#[derive(Debug, Clone, serde::Deserialize)]
struct GatewaySnapshot {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    providers: Vec<pay_pdb::types::ProviderSummary>,
}

/// Resolve `input` to a reachable gateway origin and fetch its discovery
/// document. Scheme-less inputs try https first, then http.
fn fetch_snapshot(input: &str) -> pay_core::Result<(String, GatewaySnapshot)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| pay_core::Error::Config(format!("http client: {e}")))?;

    let trimmed = input.trim().trim_end_matches('/');
    let candidates: Vec<String> = if trimmed.contains("://") {
        vec![trimmed.to_string()]
    } else {
        vec![format!("https://{trimmed}"), format!("http://{trimmed}")]
    };

    let mut last_err = String::new();
    for origin in &candidates {
        match fetch_snapshot_from(&client, origin) {
            Ok(snapshot) => return Ok((origin.clone(), snapshot)),
            Err(e) => last_err = e,
        }
    }
    Err(pay_core::Error::Config(format!(
        "no pay inference gateway at `{input}`: {last_err}"
    )))
}

fn fetch_snapshot_from(
    client: &reqwest::blocking::Client,
    origin: &str,
) -> Result<GatewaySnapshot, String> {
    let url = format!("{origin}{}", registry::GATEWAY_CONFIG_PATH);
    let response = client.get(&url).send().map_err(|e| format!("{e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    response
        .json::<GatewaySnapshot>()
        .map_err(|e| format!("{url} is not a gateway discovery document: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Serve `body` at the discovery path on an ephemeral port.
    async fn stub_gateway(body: &'static str) -> u16 {
        let router = Router::new().route(
            registry::GATEWAY_CONFIG_PATH,
            get(move || async move { body.to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        port
    }

    const CONFIG: &str = r#"{
        "mode": "inference",
        "network": "sandbox",
        "title": "Pay Inference",
        "providers": [{
            "slug": "llama-cpp",
            "title": "llama.cpp",
            "baseUrl": "http://127.0.0.1:8081",
            "up": true,
            "models": ["gpt-oss-120b"],
            "modelPricing": [{"model": "gpt-oss-120b", "price": "in $1.00 · out $8.00 /1M tok"}]
        }]
    }"#;

    #[test]
    fn add_fetch_parses_the_gateway_discovery_document() {
        let rt = rt();
        let port = rt.block_on(stub_gateway(CONFIG));
        // reqwest::blocking must run outside the async context.
        let (origin, snapshot) = fetch_snapshot(&format!("http://127.0.0.1:{port}")).unwrap();
        assert_eq!(origin, format!("http://127.0.0.1:{port}"));
        assert_eq!(snapshot.network.as_deref(), Some("sandbox"));
        assert_eq!(snapshot.providers.len(), 1);
        let provider = &snapshot.providers[0];
        assert_eq!(provider.slug, "llama-cpp");
        assert_eq!(provider.models, ["gpt-oss-120b"]);
        assert_eq!(
            provider.model_pricing[0].price.as_deref(),
            Some("in $1.00 · out $8.00 /1M tok")
        );
    }

    #[test]
    fn scheme_less_input_falls_back_to_http() {
        let rt = rt();
        let port = rt.block_on(stub_gateway(CONFIG));
        // https on the plain-HTTP stub fails; the http fallback lands.
        let (origin, _) = fetch_snapshot(&format!("127.0.0.1:{port}")).unwrap();
        assert_eq!(origin, format!("http://127.0.0.1:{port}"));
    }

    #[test]
    fn non_gateway_answers_are_rejected() {
        let rt = rt();
        let port = rt.block_on(stub_gateway("<html>not a gateway</html>"));
        let err = fetch_snapshot(&format!("http://127.0.0.1:{port}")).unwrap_err();
        assert!(
            err.to_string().contains("not a gateway discovery document"),
            "{err}"
        );
    }
}
