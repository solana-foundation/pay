//! `pay serve inference` — discover local AI inference servers (Ollama,
//! LM Studio, llama.cpp, vLLM, exo) and front them with the Pingora gateway,
//! tracking every request live in the TUI and the embedded web UI.
//!
//! v1 is free passthrough: synthesized specs have no metered endpoints, so
//! the payment gate forwards everything while `record_exchange` still feeds
//! the PDB correlation engine (`AllExchanges` mode). See
//! docs/serve-inference.md.

pub mod discovery;
pub mod spec;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use clap::Args;
use pay_core::PaymentState;
use pay_kit::mpp::server::Mpp;
use pay_pdb::PdbState;
use pay_pdb::correlation::CorrelationMode;
use pay_pdb::types::ProviderSummary;
use pay_types::metering::ApiSpec;

use discovery::{DiscoveredProvider, ProviderRegistry, load_registry};

#[derive(Debug, Args)]
pub struct InferenceCommand {
    /// Public bind for the gateway.
    #[arg(long, default_value = "127.0.0.1:1402")]
    pub bind: String,

    /// Only probe these providers (comma-separated slugs, e.g. `ollama,vllm`).
    #[arg(long, value_delimiter = ',')]
    pub providers: Vec<String>,

    /// Per-endpoint probe timeout in milliseconds.
    #[arg(long, default_value_t = 400)]
    pub probe_timeout: u64,

    /// Headless: log lines instead of the TUI.
    #[arg(long)]
    pub no_tui: bool,

    /// Don't serve the embedded web UI.
    #[arg(long)]
    pub no_web: bool,

    /// Seconds between provider re-probes (0 disables watching).
    #[arg(long, default_value_t = 10)]
    pub watch_interval: u64,

    /// Extra hand-written ApiSpec YAML(s) to serve alongside discovered
    /// providers (escape hatch).
    #[arg(long)]
    pub spec: Vec<String>,
}

/// Payment state for the inference gateway: no payment backends at all —
/// every request is `Passthrough` — plus the PDB hook for live tracking.
#[derive(Clone)]
pub struct InferenceState {
    apis: Arc<Vec<ApiSpec>>,
    pdb: PdbState,
}

impl PaymentState for InferenceState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        None
    }
    fn record_exchange(&self, exchange: pay_core::HttpExchange) {
        let entry = pay_pdb::types::LogEntry {
            id: self.pdb.next_log_id(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            method: exchange.method,
            path: exchange.path,
            status: exchange.status,
            ms: exchange.ms,
            req_headers: exchange.req_headers.into_iter().collect(),
            res_headers: exchange.res_headers.into_iter().collect(),
            res_body: None,
            client_ip: exchange.client_ip,
        };
        if let Ok(mut engine) = self.pdb.correlation.lock() {
            engine.ingest(entry);
        }
    }
}

impl InferenceCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("tokio runtime: {e}")))?;

        let (internal_addr, state) = rt.block_on(self.setup())?;

        let cores = std::thread::available_parallelism().map(|n| n.get()).ok();
        // Pingora owns the public port; it must run without an ambient tokio
        // runtime. `rt` stays alive so the watch/cleanup/axum tasks keep
        // running for the life of the gateway.
        pay_proxy::run(state, &self.bind, internal_addr.to_string(), cores)
            .map_err(|e| pay_core::Error::Config(format!("gateway: {e}")))
    }

    async fn setup(&self) -> pay_core::Result<(std::net::SocketAddr, InferenceState)> {
        let registry =
            load_registry().map_err(|e| pay_core::Error::Config(format!("registry: {e}")))?;
        let restrict = (!self.providers.is_empty()).then_some(self.providers.as_slice());
        let timeout = Duration::from_millis(self.probe_timeout);

        eprintln!("⏺ probing local AI providers…");
        let discovered = discovery::discover(&registry, timeout, restrict).await;

        if discovered.is_empty() {
            for provider in registry_scope(&registry, restrict) {
                eprintln!(
                    "  {} not detected on port {} — is it running?",
                    provider.title,
                    provider
                        .ports
                        .first()
                        .map(u16::to_string)
                        .unwrap_or_default()
                );
            }
            return Err(pay_core::Error::Config(
                "no local inference providers found".into(),
            ));
        }
        for provider in &discovered {
            eprintln!(
                "  {} ✓ ({}, {} model{})",
                provider.spec.slug,
                provider.base_url,
                provider.models.len(),
                if provider.models.len() == 1 { "" } else { "s" },
            );
        }

        // Synthesized passthrough specs + any --spec extras.
        let mut specs: Vec<ApiSpec> = discovered.iter().map(spec::provider_spec).collect();
        for path in &self.spec {
            let contents = std::fs::read_to_string(shellexpand::tilde(path).as_ref())
                .map_err(|e| pay_core::Error::Config(format!("read {path}: {e}")))?;
            let mut api: ApiSpec = serde_yml::from_str(&contents)
                .map_err(|e| pay_core::Error::Config(format!("parse {path}: {e}")))?;
            api.apply_scheme_defaults();
            specs.push(api);
        }

        let summaries = provider_summaries(&registry, restrict, &discovered);
        let pdb = PdbState::with_mode(
            serde_json::json!({
                "mode": "inference",
                "title": "Pay Inference",
                "providers": summaries,
            }),
            CorrelationMode::AllExchanges,
        );
        pdb.set_providers(summaries);
        pdb.spawn_cleanup();

        if self.watch_interval > 0 {
            spawn_watch_task(
                registry.clone(),
                self.providers.clone(),
                timeout,
                Duration::from_secs(self.watch_interval),
                pdb.clone(),
            );
        }

        // Internal control plane: the gate forwards `/__402/*` and root here.
        let mut router = Router::new();
        if !self.no_web {
            router = router
                .nest(pay_pdb::PDB_PATH, pay_pdb::debugger_router(pdb.clone()))
                .route(
                    "/",
                    get(|| async { Redirect::temporary(&format!("{}/", pay_pdb::PDB_PATH)) }),
                );
        } else {
            let index = provider_index(&pdb);
            router = router.route("/", get(move || async move { index }));
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| pay_core::Error::Config(format!("bind control-plane: {e}")))?;
        let internal_addr = listener
            .local_addr()
            .map_err(|e| pay_core::Error::Config(format!("control-plane local_addr: {e}")))?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "control-plane axum server exited unexpectedly");
            }
        });

        let display_addr = self.bind.replace("0.0.0.0", "127.0.0.1");
        eprintln!("⏺ gateway on http://{display_addr}");
        if !self.no_web {
            eprintln!(
                "⏺ web UI http://{display_addr}{}/",
                pay_pdb::PDB_PATH
            );
        }
        for provider in &discovered {
            let host_port = display_addr
                .rsplit_once(':')
                .map(|(_, p)| p)
                .unwrap_or("1402");
            eprintln!(
                "  {} → http://{}.localhost:{}/",
                provider.spec.slug, provider.spec.slug, host_port
            );
        }

        let state = InferenceState {
            apis: Arc::new(specs),
            pdb,
        };
        Ok((internal_addr, state))
    }
}

/// Registry providers in scope for probing (`--providers` filter applied).
fn registry_scope<'a>(
    registry: &'a ProviderRegistry,
    restrict: Option<&[String]>,
) -> Vec<&'a discovery::ProviderSpec> {
    registry
        .providers
        .iter()
        .filter(|p| {
            restrict
                .map(|allowed| allowed.iter().any(|s| s == &p.slug))
                .unwrap_or(true)
        })
        .collect()
}

/// One summary per in-scope registry provider — discovered ones carry
/// models/version and `up: true`, the rest render as "not detected".
fn provider_summaries(
    registry: &ProviderRegistry,
    restrict: Option<&[String]>,
    discovered: &[DiscoveredProvider],
) -> Vec<ProviderSummary> {
    registry_scope(registry, restrict)
        .into_iter()
        .map(|spec| {
            match discovered.iter().find(|d| d.spec.slug == spec.slug) {
                Some(found) => found.summary(true),
                None => ProviderSummary {
                    slug: spec.slug.clone(),
                    title: spec.title.clone(),
                    base_url: format!(
                        "http://127.0.0.1:{}",
                        spec.ports.first().copied().unwrap_or_default()
                    ),
                    up: false,
                    models: Vec::new(),
                    version: None,
                    color: spec.color.clone(),
                },
            }
        })
        .collect()
}

/// Re-probe on an interval and broadcast provider status changes. Routes are
/// fixed at startup (subdomain → base_url), so a provider restarting on the
/// same port resumes seamlessly; a brand-new provider needs a gateway
/// restart, which we log once when first seen.
fn spawn_watch_task(
    registry: ProviderRegistry,
    restrict: Vec<String>,
    timeout: Duration,
    interval: Duration,
    pdb: PdbState,
) {
    tokio::spawn(async move {
        let restrict_ref = (!restrict.is_empty()).then_some(restrict.as_slice());
        let mut routed: Vec<String> = pdb
            .providers()
            .iter()
            .filter(|p| p.up)
            .map(|p| p.slug.clone())
            .collect();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            let discovered = discovery::discover(&registry, timeout, restrict_ref).await;
            for provider in &discovered {
                if !routed.contains(&provider.spec.slug) {
                    routed.push(provider.spec.slug.clone());
                    tracing::info!(
                        provider = %provider.spec.slug,
                        "new provider detected — restart `pay serve inference` to route it"
                    );
                }
            }
            pdb.set_providers(provider_summaries(&registry, restrict_ref, &discovered));
        }
    });
}

fn provider_index(pdb: &PdbState) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "service": "pay serve inference",
        "providers": pdb.providers(),
    }))
}
