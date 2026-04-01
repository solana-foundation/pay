//! `pay server start` — start a payment gateway proxy.

use std::sync::Arc;

use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use owo_colors::OwoColorize;
use pay_core::PaymentState;
use pay_types::metering::ApiSpec;
#[cfg(feature = "gcp_kms")]
use pay_types::metering::SignerConfig;
use solana_mpp::server::Mpp;
use solana_mpp::solana_keychain::SolanaSigner;

/// Start the payment gateway proxy.
///
/// Loads an API spec from a YAML file and starts an HTTP proxy that:
/// - Returns 402 with MPP challenge for metered endpoints
/// - Forwards to upstream on valid payment
/// - Passes through free endpoints directly
#[derive(clap::Args)]
pub struct StartCommand {
    /// Path to the provider YAML spec file.
    pub spec: String,

    /// Address to bind to.
    #[arg(long, default_value = "0.0.0.0:8402")]
    pub bind: String,

    /// Recipient wallet address for payments.
    #[arg(long)]
    pub recipient: Option<String>,

    /// Payment currency (SOL, USDC, etc.).
    #[arg(long, default_value = "USDC")]
    pub currency: String,

    /// RPC URL for payment verification.
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Sandbox mode — auto-airdrop SOL to the operator's fee payer wallet.
    #[arg(long)]
    pub sandbox: bool,
}

#[derive(Clone)]
struct AppState {
    apis: Arc<Vec<ApiSpec>>,
    mpp: Option<Mpp>,
}

impl PaymentState for AppState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpp.as_ref()
    }
}

impl StartCommand {
    pub fn run(self, keypair_source: Option<&str>) -> pay_core::Result<()> {
        let expanded = shellexpand::tilde(&self.spec);
        let contents = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| pay_core::Error::Config(format!("Failed to read {}: {e}", self.spec)))?;

        let api: ApiSpec = serde_yml::from_str(&contents)
            .map_err(|e| pay_core::Error::Config(format!("Invalid spec: {e}")))?;

        let op = api.operator.clone();
        let op = op.as_ref();

        #[cfg(not(feature = "gcp_kms"))]
        if op.and_then(|o| o.signer.as_ref()).is_some() {
            return Err(pay_core::Error::Config(
                "operator.signer requires the `gcp_kms` feature. Rebuild with `--features gcp_kms`."
                    .to_string(),
            ));
        }

        // Resolve config that doesn't need async.
        let currency = op
            .and_then(|o| o.currency.clone())
            .unwrap_or_else(|| self.currency.clone());

        let rpc_url = op
            .and_then(|o| o.rpc_url.clone())
            .or(self.rpc_url.clone())
            .or_else(|| std::env::var("PAY_RPC_URL").ok())
            .unwrap_or_else(|| pay_core::config::LOCAL_RPC_URL.to_string());

        let network = op
            .and_then(|o| o.network.clone())
            .unwrap_or_else(|| "mainnet-beta".to_string());

        let fee_payer = op.map(|o| o.fee_payer).unwrap_or(false);
        #[allow(unused_variables)]
        let signer_cfg = op.and_then(|o| o.signer.clone());
        let keypair_source_owned = keypair_source.map(|s| s.to_string());

        // Create the runtime first — everything async runs inside it so
        // background tasks (like GCP auth token refresh) stay alive.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("Failed to create runtime: {e}")))?;

        rt.block_on(async {
            // ── Resolve signer (async — needs the runtime) ──
            #[cfg(feature = "gcp_kms")]
            let fee_payer_signer: Option<Arc<dyn SolanaSigner>> =
                if let Some(ref cfg) = signer_cfg {
                    Some(resolve_signer(cfg).await?)
                } else {
                    None
                };
            #[cfg(not(feature = "gcp_kms"))]
            let fee_payer_signer: Option<Arc<dyn SolanaSigner>> = None;

            // ── Resolve recipient ──
            let recipient = if let Some(r) = op.and_then(|o| o.recipient.as_ref()) {
                r.clone()
            } else if let Some(r) = &self.recipient {
                r.clone()
            } else if let Some(ref signer) = fee_payer_signer {
                signer.pubkey().to_string()
            } else if let Ok(r) = std::env::var("PAY_PAYMENT_RECIPIENT") {
                r
            } else if let Some(ref source) = keypair_source_owned {
                let signer = pay_core::signer::load_signer(source)?;
                signer.pubkey().to_string()
            } else {
                return Err(pay_core::Error::Config(
                    "No recipient specified. Use operator.recipient in YAML, --recipient flag, PAY_PAYMENT_RECIPIENT env, or `pay setup`."
                        .to_string(),
                ));
            };

            // ── Sandbox: auto-airdrop SOL to operator wallet ──
            if self.sandbox {
                if let Some(ref signer) = fee_payer_signer {
                    let pubkey = signer.pubkey().to_string();
                    eprintln!(
                        "  {} checking balance for {}…",
                        "sandbox".yellow().bold(),
                        &pubkey[..8]
                    );
                    sandbox_airdrop(&rpc_url, &pubkey).await;
                } else {
                    eprintln!(
                        "  {} no fee payer signer — skipping airdrop",
                        "sandbox".yellow().bold()
                    );
                }
            }

            // ── Create MPP server ──
            let secret_key = std::env::var("PAY_MPP_CHALLENGE_SECRET")
                .unwrap_or_else(|_| bs58::encode(rand::random::<[u8; 32]>()).into_string());

            // Resolve currency label to mint address for the challenge.
            let (mpp_currency, decimals) = resolve_currency(&currency, &network);

            let mpp = Mpp::new(solana_mpp::server::Config {
                recipient: recipient.clone(),
                currency: mpp_currency,
                decimals,
                network: network.clone(),
                rpc_url: Some(rpc_url.clone()),
                secret_key: Some(secret_key),
                fee_payer,
                fee_payer_signer,
                ..Default::default()
            })
            .map_err(|e| pay_core::Error::Config(format!("Failed to create MPP server: {e}")))?;

            // ── Banner ──
            let metered_count = api
                .endpoints
                .iter()
                .filter(|e| e.metering.is_some())
                .count();
            let free_count = api.endpoints.len() - metered_count;

            eprintln!();
            eprintln!("  {} {}", "pay server".bold(), api.title.bold());
            eprintln!();
            eprintln!("  {}  {}", "upstream".dimmed(), api.forward.url);
            eprintln!(
                "  {}  {}",
                "wallet  ".dimmed(),
                recipient.chars().take(8).collect::<String>().dimmed()
            );
            eprintln!("  {} {}", "currency".dimmed(), self.currency.green());
            eprintln!("  {}      {}", "rpc".dimmed(), rpc_url.dimmed());
            eprintln!();

            eprintln!(
                "  {}",
                format!(
                    "{} endpoints ({} metered, {} free)",
                    api.endpoints.len(),
                    metered_count,
                    free_count
                )
                .dimmed()
            );
            eprintln!();

            for ep in &api.endpoints {
                let method = format!("{:?}", ep.method).to_uppercase();
                let method_colored = match method.as_str() {
                    "GET" => method.green().to_string(),
                    "POST" => method.blue().to_string(),
                    "PUT" => method.yellow().to_string(),
                    "DELETE" => method.red().to_string(),
                    "PATCH" => method.cyan().to_string(),
                    _ => method.dimmed().to_string(),
                };
                let price_tag = if let Some(ref m) = ep.metering {
                    let price = m
                        .dimensions
                        .first()
                        .map(|d| d.tiers.first().map(|t| t.price_usd).unwrap_or(0.0))
                        .or_else(|| {
                            m.variants
                                .first()
                                .and_then(|v| v.dimensions.first())
                                .and_then(|d| d.tiers.first())
                                .map(|t| t.price_usd)
                        })
                        .unwrap_or(0.0);
                    format!("${:.4}", price).yellow().to_string()
                } else {
                    "free".green().to_string()
                };

                eprintln!("  {:<7} {:<40} {}", method_colored, ep.path, price_tag,);
            }

            eprintln!();
            eprintln!("  {}  /__gateway/health", "GET".green());
            eprintln!("  {}  /__gateway/endpoints", "GET".green());
            eprintln!();

            // ── Build router ──
            let endpoints_json = build_endpoints_json(&api);

            let state = AppState {
                apis: Arc::new(vec![api.clone()]),
                mpp: Some(mpp),
            };

            let app = axum::Router::new()
                .route("/__gateway/health", get(|| async { "ok" }))
                .route(
                    "/__gateway/endpoints",
                    get(move || async move { axum::Json(endpoints_json).into_response() }),
                )
                .fallback(any(move |req: axum::http::Request<axum::body::Body>| {
                    let api = api.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                            .await
                            .unwrap_or_default();
                        pay_core::server::proxy::forward_request(
                            &api,
                            parts.method,
                            &parts.uri,
                            &parts.headers,
                            bytes,
                        )
                        .await
                        .unwrap_or_else(|e| e)
                    }
                }))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    pay_core::server::payment::payment_middleware::<AppState>,
                ))
                .with_state(state);

            let listener = tokio::net::TcpListener::bind(&self.bind)
                .await
                .map_err(|e| {
                    pay_core::Error::Config(format!("Failed to bind {}: {e}", self.bind))
                })?;
            eprintln!(
                "  {} {}",
                "listening".green().bold(),
                format!("http://{}", self.bind).bold()
            );
            eprintln!();
            axum::serve(listener, app)
                .await
                .map_err(|e| pay_core::Error::Config(format!("Server error: {e}")))
        })
    }
}

fn build_endpoints_json(api: &ApiSpec) -> serde_json::Value {
    let endpoints: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .map(|ep| {
            let mut obj = serde_json::json!({
                "method": format!("{:?}", ep.method).to_uppercase(),
                "path": ep.path,
                "metered": ep.metering.is_some(),
            });
            if let Some(desc) = &ep.description {
                obj["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref m) = ep.metering {
                let price = m
                    .dimensions
                    .first()
                    .map(|d| d.tiers.first().map(|t| t.price_usd).unwrap_or(0.0))
                    .unwrap_or(0.0);
                obj["price_usd"] = serde_json::json!(price);
            }
            obj
        })
        .collect();

    serde_json::json!({
        "name": api.name,
        "title": api.title,
        "forward": {
            "url": api.forward.url,
        },
        "endpoints": endpoints,
    })
}

/// Resolve a currency label to the value used in the MPP challenge.
/// SPL tokens use their mint address; SOL uses "sol".
fn resolve_currency(currency: &str, network: &str) -> (String, u8) {
    match currency.to_uppercase().as_str() {
        "SOL" => ("sol".to_string(), 9),
        "USDC" => {
            let mint = match network {
                "devnet" => "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
                _ => "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            };
            (mint.to_string(), 6)
        }
        other => (other.to_string(), 6),
    }
}

/// Create a SolanaSigner from the operator.signer config.
/// Must be called from within the main async runtime so the GCP auth
/// token cache's background refresh tasks stay alive.
#[cfg(feature = "gcp_kms")]
async fn resolve_signer(config: &SignerConfig) -> pay_core::Result<Arc<dyn SolanaSigner>> {
    match config {
        SignerConfig::GcpKms { key_name, pubkey } => {
            let signer =
                solana_mpp::solana_keychain::GcpKmsSigner::new(key_name.clone(), pubkey.clone())
                    .await
                    .map_err(|e| {
                        pay_core::Error::Config(format!("Failed to create GCP KMS signer: {e}"))
                    })?;

            Ok(Arc::new(signer))
        }
    }
}

/// Check SOL balance and request airdrop if below 1 SOL.
async fn sandbox_airdrop(rpc_url: &str, pubkey: &str) {
    let client = reqwest::Client::new();
    let balance_resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBalance",
            "params": [pubkey]
        }))
        .send()
        .await;
    let balance_lamports = match balance_resp {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["result"]["value"].as_u64())
            .unwrap_or(0),
        Err(_) => 0,
    };
    let balance_sol = balance_lamports as f64 / 1_000_000_000.0;
    eprintln!("  {}  {:.4} SOL", "balance".dimmed(), balance_sol);

    if balance_lamports < 1_000_000_000 {
        eprintln!("  {} requesting airdrop…", "sandbox".yellow().bold());
        let airdrop_resp = client
            .post(rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "requestAirdrop",
                "params": [pubkey, 2_000_000_000u64]
            }))
            .send()
            .await;
        match airdrop_resp {
            Ok(r) => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                if body.get("error").is_some() {
                    eprintln!(
                        "  {} airdrop failed: {}",
                        "sandbox".yellow().bold(),
                        body["error"]["message"]
                    );
                } else {
                    eprintln!("  {} +2 SOL airdropped", "sandbox".green().bold());
                }
            }
            Err(e) => eprintln!(
                "  {} airdrop request failed: {e}",
                "sandbox".yellow().bold()
            ),
        }
    }
}
