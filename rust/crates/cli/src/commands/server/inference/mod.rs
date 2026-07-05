//! `pay serve inference` — discover local AI inference servers (Ollama,
//! LM Studio, llama.cpp, vLLM, exo) and front them with the Pingora gateway,
//! tracking every request live in the TUI and the embedded web UI.
//!
//! By default this is free passthrough: synthesized specs have no metered
//! endpoints, so the payment gate forwards everything while
//! `record_exchange` still feeds the PDB correlation engine (`AllExchanges`
//! mode). With `--sandbox --price-usd <USD>`, the registry's `paid`
//! endpoints are synthesized as metered charge endpoints and the command
//! builds the same sandbox charge stack as `pay --sandbox server start`
//! (localnet + Surfpool, ephemeral fee-payer signer, USDC), so priced
//! endpoints 402 and verified retries move localnet stablecoins — entirely
//! in-gate, no extra control-plane routes. See docs/serve-inference.md.

pub mod discovery;
pub mod spec;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use clap::Args;
use pay_core::PaymentState;
use pay_core::server::telemetry::FeePayerWallet;
use pay_kit::mpp::server::Mpp;
use pay_kit::mpp::solana_keychain::SolanaSigner;
use pay_pdb::PdbState;
use pay_pdb::correlation::CorrelationMode;
use pay_pdb::types::{InferenceInfo, ProviderSummary};
use pay_types::metering::ApiSpec;

use super::payments;
use discovery::{DiscoveredProvider, ProviderRegistry, load_registry};

/// Canonical (user-visible) mount of the web UI in inference mode. Must live
/// under `/__402/` — the gate only forwards that prefix (plus root) to the
/// control plane. The SPA also stays mounted at `pay_pdb::PDB_PATH` because
/// its API/SSE calls are absolute paths there.
const UI_PATH: &str = "/__402/ui";

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

    /// Sandbox guard: refuse any spec whose operator is not explicitly
    /// `network: localnet`, so no mainnet stablecoins can move through this
    /// gateway. (Also honored when the global `pay --sandbox` flag is set.)
    #[arg(short = 's', long)]
    pub sandbox: bool,

    /// Charge this flat USD price per paid inference request (the registry's
    /// `paid` endpoints; model-listing/health stay free). Sandbox only —
    /// requires `--sandbox` (or global `pay --sandbox`).
    #[arg(long, value_name = "USD")]
    pub price_usd: Option<f64>,
}

/// Payment state for the inference gateway: the PDB hook for live tracking,
/// plus — only when `--price-usd` is set — the sandbox MPP charge backend.
/// Unpriced, the backend fields stay empty and every request is
/// `Passthrough`, exactly as before.
#[derive(Clone)]
pub struct InferenceState {
    apis: Arc<Vec<ApiSpec>>,
    pdb: PdbState,
    mpps: Vec<Mpp>,
    fee_payer_signer: Option<Arc<dyn SolanaSigner>>,
    fee_payer_wallet: Option<FeePayerWallet>,
}

impl PaymentState for InferenceState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpps.first()
    }
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpps.iter().collect()
    }
    fn fee_payer_signer(&self) -> Option<Arc<dyn SolanaSigner>> {
        self.fee_payer_signer.clone()
    }
    fn fee_payer_wallet(&self) -> Option<&FeePayerWallet> {
        self.fee_payer_wallet.as_ref()
    }
    fn record_request_start(&self, start: &pay_core::RequestStart) -> Option<u64> {
        // Same host→spec resolution the gate uses; provider slug == spec name.
        let subdomain = start.host.as_deref()?.split('.').next().unwrap_or("");
        let provider = self
            .apis
            .iter()
            .find(|a| a.subdomain == subdomain)
            .or_else(|| (self.apis.len() == 1).then(|| self.apis.first()).flatten())?
            .name
            .clone();
        let info = InferenceInfo {
            provider,
            endpoint_kind: Some(endpoint_kind(&start.path).to_string()),
            ..Default::default()
        };
        Some(
            self.pdb
                .begin_exchange(&start.method, &start.path, &start.client_ip, Some(info)),
        )
    }
    fn record_exchange_update(&self, log_id: u64, usage: &pay_core::InferenceUsage) {
        self.pdb.update_exchange(log_id, usage_to_info(usage));
    }
    fn record_exchange(&self, exchange: pay_core::HttpExchange) {
        // Fold the final observer telemetry in while the exchange is still
        // open, then close it (or create a completed flow if no start was
        // tracked — id continuity is what ties the two together).
        if let (Some(log_id), Some(usage)) = (exchange.log_id, exchange.usage.as_ref()) {
            self.pdb.update_exchange(log_id, usage_to_info(usage));
        }
        let entry = pay_pdb::types::LogEntry {
            id: exchange.log_id.unwrap_or_else(|| self.pdb.next_log_id()),
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

/// The `--sandbox` guard: a spec may only carry an operator that explicitly
/// declares `network: localnet`. Anything else — mainnet, devnet, or an
/// unset network (which would fall back to other resolution rules) — is
/// refused loudly rather than silently rewritten, so no mainnet stablecoins
/// can move through this gateway.
fn enforce_sandbox(spec: &ApiSpec, path: &str) -> pay_core::Result<()> {
    let Some(operator) = &spec.operator else {
        return Ok(()); // no operator ⇒ no payment backend ⇒ nothing can move
    };
    match operator.network.as_deref() {
        Some("localnet") => Ok(()),
        Some(other) => Err(pay_core::Error::Config(format!(
            "--sandbox: spec {path} declares operator network \"{other}\" — only \
             \"localnet\" is allowed in sandbox mode"
        ))),
        None => Err(pay_core::Error::Config(format!(
            "--sandbox: spec {path} has an operator without an explicit network — \
             set `operator.network: localnet` to run it in sandbox mode"
        ))),
    }
}

/// `--price-usd` gate: monetization is sandbox-only for now (localnet
/// stablecoins via the Surfpool sandbox). No flag ⇒ free passthrough — there
/// is no default price. A price without sandbox is refused loudly rather
/// than silently ignored or silently pointed at a real cluster.
fn validate_pricing(price_usd: Option<f64>, sandbox: bool) -> pay_core::Result<Option<f64>> {
    match price_usd {
        None => Ok(None),
        Some(_) if !sandbox => Err(pay_core::Error::Config(
            "--price-usd: mainnet monetization is not wired yet — run with --sandbox".into(),
        )),
        Some(price) if !price.is_finite() || price <= 0.0 => Err(pay_core::Error::Config(format!(
            "--price-usd must be a positive USD amount, got {price}"
        ))),
        Some(price) => Ok(Some(price)),
    }
}

/// Endpoint kind from the request path. `chat` is checked first —
/// `/v1/chat/completions` contains both markers.
fn endpoint_kind(path: &str) -> &'static str {
    let path = path.to_ascii_lowercase();
    if path.contains("chat") {
        "chat"
    } else if path.contains("embed") {
        "embeddings"
    } else if path.contains("completion") || path.contains("generate") || path.contains("infill") {
        "completion"
    } else {
        "other"
    }
}

/// Observer telemetry → PDB wire type. Provider is left empty — the
/// correlation engine merges field-wise onto the request-time info.
fn usage_to_info(usage: &pay_core::InferenceUsage) -> InferenceInfo {
    InferenceInfo {
        provider: String::new(),
        model: usage.model.clone(),
        endpoint_kind: None,
        streamed: usage.streamed,
        tokens_prompt: usage.tokens_prompt,
        tokens_completion: usage.tokens_completion,
        ttft_ms: usage.ttft_ms,
        tokens_per_sec: usage.tokens_per_sec,
    }
}

impl InferenceCommand {
    pub fn run(self, global_sandbox: bool) -> pay_core::Result<()> {
        let sandbox = self.sandbox || global_sandbox;
        let price_usd = validate_pricing(self.price_usd, sandbox)?;

        // Probe the public bind up front: Pingora only discovers a taken port
        // deep inside its service thread, where the bind failure is a panic.
        // Fail here with an actionable message instead (a second gateway is
        // usually another `pay serve inference` or a `pay claude` session).
        match std::net::TcpListener::bind(&self.bind) {
            Ok(probe) => drop(probe),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(pay_core::Error::Config(format!(
                    "{} is already in use — another gateway is running (pay serve \
                     inference or a pay claude session). Stop it or pass --bind \
                     with a free port.",
                    self.bind
                )));
            }
            Err(e) => {
                return Err(pay_core::Error::Config(format!(
                    "cannot bind {}: {e}",
                    self.bind
                )));
            }
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("tokio runtime: {e}")))?;

        let (internal_addr, state) = rt.block_on(self.setup(sandbox, price_usd))?;

        let cores = std::thread::available_parallelism().map(|n| n.get()).ok();
        // `rt` stays alive so the watch/cleanup/axum/bridge tasks keep
        // running for the life of the gateway.

        let use_tui = !self.no_tui && std::io::IsTerminal::is_terminal(&std::io::stderr());
        if !use_tui {
            // Headless: Pingora owns the main thread (it must run without an
            // ambient tokio runtime, which is true here after block_on).
            return pay_proxy::run(state, &self.bind, internal_addr.to_string(), cores)
                .map_err(|e| pay_core::Error::Config(format!("gateway: {e}")));
        }

        // TUI mode: the terminal needs the main thread, so Pingora runs on a
        // spawned thread with a caller-owned shutdown (returns instead of
        // exiting the process, so we can restore the terminal after quit).
        //
        // Subscribe the event bridge BEFORE snapshotting so no flow event
        // falls between the two.
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut broadcast_rx = state.pdb.tx.subscribe();
        rt.spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(msg) => {
                        if event_tx.send(msg).is_err() {
                            break; // TUI gone
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let initial_providers = state.pdb.providers();
        let initial_flows = state.pdb.correlation.lock().unwrap().snapshot();
        let initial_connections = state.pdb.connections();

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let proxy_thread = {
            let state = state.clone();
            let bind = self.bind.clone();
            let control_plane = internal_addr.to_string();
            std::thread::spawn(move || {
                if let Err(e) =
                    pay_proxy::run_with_shutdown(state, &bind, control_plane, cores, shutdown_rx)
                {
                    tracing::error!(error = %e, "gateway exited with error");
                }
            })
        };

        let display_addr = self.bind.replace("0.0.0.0", "127.0.0.1");
        let tui_result = crate::tui::run_inference_tui(crate::tui::InferenceTuiArgs {
            gateway_url: format!("http://{display_addr}"),
            web_url: (!self.no_web).then(|| format!("http://{display_addr}{UI_PATH}/")),
            initial_providers,
            initial_flows,
            initial_connections,
            events: event_rx,
        });

        let _ = shutdown_tx.send(true);
        let _ = proxy_thread.join();
        tui_result.map_err(|e| pay_core::Error::Config(format!("tui: {e}")))
    }

    async fn setup(
        &self,
        sandbox: bool,
        price_usd: Option<f64>,
    ) -> pay_core::Result<(std::net::SocketAddr, InferenceState)> {
        let registry =
            load_registry().map_err(|e| pay_core::Error::Config(format!("registry: {e}")))?;
        let restrict = (!self.providers.is_empty()).then_some(self.providers.as_slice());
        let timeout = Duration::from_millis(self.probe_timeout);

        // Single-line probe progress: the spinner narrates the provider being
        // probed while results print above it. Hidden automatically when
        // stderr isn't a terminal.
        let spinner = indicatif::ProgressBar::new_spinner().with_style(
            indicatif::ProgressStyle::with_template("{spinner:.green} {msg}")
                .expect("static template")
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⏺"]),
        );
        spinner.enable_steady_tick(Duration::from_millis(80));
        // On a hidden draw target (non-TTY stderr) `ProgressBar::println` is
        // dropped — fall back to plain eprintln so headless logs keep the
        // probe results.
        let emit = |line: String| {
            if spinner.is_hidden() {
                eprintln!("{line}");
            } else {
                spinner.println(line);
            }
        };
        let discovered =
            discovery::discover_with(&registry, timeout, restrict, |event| match event {
                discovery::ProbeEvent::Started(spec) => {
                    let port = spec.ports.first().copied().unwrap_or_default();
                    spinner.set_message(format!("probing {} (:{port})…", spec.title));
                }
                discovery::ProbeEvent::Found(provider) => {
                    let version = provider
                        .version
                        .as_deref()
                        .map(|v| format!(" ({v})"))
                        .unwrap_or_default();
                    emit(format!(
                        "  {} ✓  {} · {} model{}{version}",
                        provider.spec.slug,
                        provider.base_url,
                        provider.models.len(),
                        if provider.models.len() == 1 { "" } else { "s" },
                    ));
                }
                discovery::ProbeEvent::Missed(spec) => {
                    emit(format!("  {} — not detected", spec.title));
                }
            })
            .await;
        spinner.finish_and_clear();

        if discovered.is_empty() {
            return Err(pay_core::Error::Config(
                "no local inference providers found — start one (e.g. `ollama serve`) and retry"
                    .into(),
            ));
        }

        // Sandbox charge stack — only when priced. `validate_pricing`
        // guarantees `price_usd` implies sandbox, so this is localnet-only.
        let payment = match price_usd {
            Some(price) => Some(build_sandbox_payments(price).await?),
            None => None,
        };

        // Synthesized specs (+ pricing when monetized) + any --spec extras.
        let pricing = payment.as_ref().map(|p| &p.pricing);
        let mut specs: Vec<ApiSpec> = discovered
            .iter()
            .map(|provider| spec::provider_spec(provider, pricing))
            .collect();
        for path in &self.spec {
            let contents = std::fs::read_to_string(shellexpand::tilde(path).as_ref())
                .map_err(|e| pay_core::Error::Config(format!("read {path}: {e}")))?;
            let mut api: ApiSpec = serde_yml::from_str(&contents)
                .map_err(|e| pay_core::Error::Config(format!("parse {path}: {e}")))?;
            if sandbox {
                enforce_sandbox(&api, path)?;
            }
            api.apply_scheme_defaults();
            specs.push(api);
        }
        if sandbox {
            eprintln!("⏺ sandbox — localnet only; mainnet stablecoins cannot move");
        }

        let summaries = provider_summaries(&registry, restrict, &discovered);
        let pdb = PdbState::with_mode(
            serde_json::json!({
                "mode": "inference",
                "title": "Pay Inference",
                "providers": summaries,
                "network": if sandbox { "sandbox" } else { "local" },
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
        // The UI's canonical URL in inference mode is /__402/ui/ (users see
        // it in the address bar); the SPA's API calls are absolute
        // `/__402/pdb/*` paths, so the router stays mounted there too.
        // `nest_service` (not `nest`) so the nested root `/…/` resolves —
        // same as `server start`.
        let mut router = Router::new();
        if !self.no_web {
            router = router
                .nest_service(UI_PATH, pay_pdb::debugger_router(pdb.clone()))
                .nest_service(pay_pdb::PDB_PATH, pay_pdb::debugger_router(pdb.clone()))
                .route(
                    "/",
                    get(|| async { Redirect::temporary(&format!("{UI_PATH}/")) }),
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
            eprintln!("⏺ web UI http://{display_addr}{UI_PATH}/");
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
        if let Some(payment) = &payment {
            eprintln!(
                "⏺ charging ${:.4}/request → {} (localnet USDC)",
                payment.pricing.price_usd, payment.pricing.recipient
            );
        }

        let (mpps, fee_payer_signer, fee_payer_wallet) = match payment {
            Some(payment) => (
                payment.mpps,
                Some(payment.fee_payer_signer),
                Some(payment.fee_payer_wallet),
            ),
            None => (Vec::new(), None, None),
        };
        let state = InferenceState {
            apis: Arc::new(specs),
            pdb,
            mpps,
            fee_payer_signer,
            fee_payer_wallet,
        };
        Ok((internal_addr, state))
    }
}

/// Sandbox charge backends for `--price-usd`: everything the gate needs to
/// 402 the paid endpoints and verify retried payments in-gate.
struct SandboxPayments {
    pricing: spec::SpecPricing,
    mpps: Vec<Mpp>,
    fee_payer_signer: Arc<dyn SolanaSigner>,
    fee_payer_wallet: FeePayerWallet,
}

/// Build the minimal sandbox charge stack, reusing the `server start`
/// machinery (`super::payments`): localnet + sandbox RPC, the ephemeral
/// auto fee-payer signer (payout recipient = the signer's own wallet), USDC
/// only, Surfpool funding + recipient ATA preparation, the blockhash cache,
/// the charge HMAC secret env mirror, and a single MPP charge server.
async fn build_sandbox_payments(price_usd: f64) -> pay_core::Result<SandboxPayments> {
    let network = crate::network::SolanaNetwork::Localnet;
    let rpc_url = payments::resolve_sandbox_rpc_url(None);

    let (fee_payer_signer, generated) = payments::load_auto_fee_payer_signer(&network)?;
    if let Some((account_name, pubkey)) = &generated {
        eprintln!("⏺ generated gateway account {account_name} ({pubkey}) on localnet");
    }
    // Funds land in the gateway's own sandbox wallet — no separate
    // recipient flag in sandbox monetization.
    let recipient = fee_payer_signer.pubkey().to_string();
    let recipient_pubkey = solana_pubkey::Pubkey::from_str(&recipient)
        .map_err(|e| pay_core::Error::Config(format!("gateway wallet pubkey: {e}")))?;

    let currency_configs = vec![{
        let (mint, decimals) = payments::resolve_currency("USDC", network.slug());
        ("USDC".to_string(), mint, decimals)
    }];
    let stable_requirements =
        payments::stable_token_account_requirements(&currency_configs, network.slug())?;

    let surfpool_targets = payments::surfpool_funding_targets(&recipient, Some(&recipient));
    let payout_targets = vec![payments::PayoutRecipientTarget {
        label: "gateway wallet".to_string(),
        pubkey: recipient_pubkey,
    }];
    let (should_fund, _balances) = payments::prepare_funding_targets(
        true,
        &network,
        &rpc_url,
        &surfpool_targets,
        &payout_targets,
        &stable_requirements,
    )
    .await?;

    let challenge_binding_secret = payments::init_challenge_binding_secret();

    payments::ensure_payout_recipient_token_accounts(
        &[recipient_pubkey],
        &stable_requirements,
        network.slug(),
        &rpc_url,
        should_fund,
        Some(fee_payer_signer.clone()),
    )
    .await?;

    let blockhash_cache = payments::spawn_blockhash_cache(&rpc_url);
    let mpps = payments::build_charge_mpps(
        &currency_configs,
        &recipient,
        network.slug(),
        &rpc_url,
        &challenge_binding_secret,
        true,
        Some(fee_payer_signer.clone()),
        &blockhash_cache,
    )?;
    let fee_payer_wallet = FeePayerWallet::new(rpc_url, recipient.clone());

    Ok(SandboxPayments {
        pricing: spec::SpecPricing {
            price_usd,
            recipient,
        },
        mpps,
        fee_payer_signer,
        fee_payer_wallet,
    })
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
        .map(
            |spec| match discovered.iter().find(|d| d.spec.slug == spec.slug) {
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
            },
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use pay_pdb::types::FlowStatus;

    fn state() -> InferenceState {
        let provider = discovery::DiscoveredProvider {
            spec: serde_yml::from_str(
                r#"{ slug: ollama, title: Ollama, ports: [11434],
                     identify: [{ path: /api/version, expect_json_key: version }] }"#,
            )
            .unwrap(),
            base_url: "http://127.0.0.1:11434".into(),
            models: vec![],
            version: None,
        };
        InferenceState {
            apis: Arc::new(vec![spec::provider_spec(&provider, None)]),
            pdb: PdbState::with_mode(serde_json::json!({}), CorrelationMode::AllExchanges),
            mpps: Vec::new(),
            fee_payer_signer: None,
            fee_payer_wallet: None,
        }
    }

    #[test]
    fn price_without_sandbox_is_refused() {
        let err = validate_pricing(Some(0.001), false).expect_err("must refuse");
        assert!(
            err.to_string()
                .contains("mainnet monetization is not wired yet — run with --sandbox"),
            "unexpected message: {err}"
        );

        // Non-positive / non-finite prices are refused even in sandbox.
        assert!(validate_pricing(Some(0.0), true).is_err());
        assert!(validate_pricing(Some(-1.0), true).is_err());
        assert!(validate_pricing(Some(f64::NAN), true).is_err());

        // No flag = free passthrough, sandbox or not; valid price + sandbox ok.
        assert_eq!(validate_pricing(None, false).unwrap(), None);
        assert_eq!(validate_pricing(None, true).unwrap(), None);
        assert_eq!(validate_pricing(Some(0.001), true).unwrap(), Some(0.001));
    }

    #[test]
    fn sandbox_guard_rejects_non_localnet_operators() {
        let base = spec::provider_spec(
            &discovery::DiscoveredProvider {
                spec: serde_yml::from_str(
                    r#"{ slug: ollama, title: Ollama, ports: [11434],
                     identify: [{ path: /api/version, expect_json_key: version }] }"#,
                )
                .unwrap(),
                base_url: "http://127.0.0.1:11434".into(),
                models: vec![],
                version: None,
            },
            None,
        );

        // No operator: nothing can move, allowed.
        assert!(enforce_sandbox(&base, "spec.yml").is_ok());

        // OperatorConfig has no Default; build via YAML like real specs do.
        let with_network = |network: Option<&str>| {
            let mut spec = base.clone();
            let yaml = match network {
                Some(n) => format!("network: \"{n}\""),
                None => "fee_payer: false".to_string(),
            };
            spec.operator = Some(serde_yml::from_str(&yaml).unwrap());
            spec
        };

        assert!(enforce_sandbox(&with_network(Some("localnet")), "spec.yml").is_ok());
        let mainnet_err = enforce_sandbox(&with_network(Some("mainnet")), "spec.yml")
            .expect_err("mainnet must be refused");
        assert!(mainnet_err.to_string().contains("mainnet"));
        assert!(
            enforce_sandbox(&with_network(None), "spec.yml").is_err(),
            "unset network must be refused, not defaulted"
        );
    }

    #[test]
    fn endpoint_kind_mapping() {
        assert_eq!(endpoint_kind("/v1/chat/completions"), "chat");
        assert_eq!(endpoint_kind("/api/chat"), "chat");
        assert_eq!(endpoint_kind("/v1/completions"), "completion");
        assert_eq!(endpoint_kind("/api/generate"), "completion");
        assert_eq!(endpoint_kind("/infill"), "completion");
        assert_eq!(endpoint_kind("/v1/embeddings"), "embeddings");
        assert_eq!(endpoint_kind("/api/tags"), "other");
    }

    #[test]
    fn request_start_to_exchange_lifecycle() {
        let state = state();

        let log_id = state.record_request_start(&pay_core::RequestStart {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            host: Some("ollama.localhost:1402".into()),
            client_ip: "127.0.0.1".into(),
        });
        let log_id = log_id.expect("provider-matched request must be tracked");

        // In-flight with provider + endpoint kind from request time.
        let flows = state.pdb.correlation.lock().unwrap().snapshot();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].status, FlowStatus::InProgress);
        let inf = flows[0].inference.as_ref().unwrap();
        assert_eq!(inf.provider, "ollama");
        assert_eq!(inf.endpoint_kind.as_deref(), Some("chat"));

        // Live observer update merges usage without losing provider.
        state.record_exchange_update(
            log_id,
            &pay_core::InferenceUsage {
                model: Some("llama3.2:3b".into()),
                streamed: true,
                ttft_ms: Some(180),
                tokens_completion: Some(20),
                ..Default::default()
            },
        );

        // Completion closes the same flow (id continuity) with final usage.
        state.record_exchange(pay_core::HttpExchange {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            status: 200,
            ms: 2300,
            req_headers: vec![],
            res_headers: vec![],
            client_ip: "127.0.0.1".into(),
            log_id: Some(log_id),
            usage: Some(pay_core::InferenceUsage {
                model: Some("llama3.2:3b".into()),
                streamed: true,
                ttft_ms: Some(180),
                tokens_prompt: Some(12),
                tokens_completion: Some(214),
                tokens_per_sec: Some(41.2),
            }),
        });

        let flows = state.pdb.correlation.lock().unwrap().snapshot();
        assert_eq!(flows.len(), 1, "completion must close the in-flight flow");
        assert_eq!(flows[0].status, FlowStatus::ResourceDelivered);
        let inf = flows[0].inference.as_ref().unwrap();
        assert_eq!(inf.provider, "ollama");
        assert_eq!(inf.tokens_prompt, Some(12));
        assert_eq!(inf.tokens_completion, Some(214));
        assert_eq!(inf.tokens_per_sec, Some(41.2));
    }

    #[test]
    fn unknown_host_is_not_tracked_in_flight() {
        let state = state();
        // Two specs would disable the single-API fallback; with one spec any
        // host matches, so test with an explicit second spec.
        let mut apis = (*state.apis).clone();
        apis.push({
            let mut second = apis[0].clone();
            second.name = "vllm".into();
            second.subdomain = "vllm".into();
            second
        });
        let state = InferenceState {
            apis: Arc::new(apis),
            pdb: state.pdb.clone(),
            mpps: Vec::new(),
            fee_payer_signer: None,
            fee_payer_wallet: None,
        };

        let log_id = state.record_request_start(&pay_core::RequestStart {
            method: "GET".into(),
            path: "/whatever".into(),
            host: Some("unknown.localhost:1402".into()),
            client_ip: "127.0.0.1".into(),
        });
        assert!(log_id.is_none());
    }
}
