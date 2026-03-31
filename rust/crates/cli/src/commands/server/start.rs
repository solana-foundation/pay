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

        let op = api.operator.as_ref();

        // Resolve signer — operator.signer takes priority, then CLI/keystore fallback.
        #[cfg(feature = "gcp_kms")]
        let fee_payer_signer: Option<
            std::sync::Arc<dyn solana_mpp::solana_keychain::SolanaSigner>,
        > = if let Some(signer_cfg) = op.and_then(|o| o.signer.as_ref()) {
            Some(resolve_signer(signer_cfg)?)
        } else {
            None
        };
        #[cfg(not(feature = "gcp_kms"))]
        let fee_payer_signer: Option<
            std::sync::Arc<dyn solana_mpp::solana_keychain::SolanaSigner>,
        > = if op.and_then(|o| o.signer.as_ref()).is_some() {
            return Err(pay_core::Error::Config(
                    "operator.signer requires the `gcp_kms` feature. Rebuild with `--features gcp_kms`.".to_string(),
                ));
        } else {
            None
        };

        // Resolve recipient — operator.recipient > --recipient > signer pubkey > keystore.
        let recipient = if let Some(r) = op.and_then(|o| o.recipient.as_ref()) {
            r.clone()
        } else if let Some(r) = &self.recipient {
            r.clone()
        } else if let Some(ref signer) = fee_payer_signer {
            signer.pubkey().to_string()
        } else if let Ok(r) = std::env::var("PAY_PAYMENT_RECIPIENT") {
            r
        } else if let Some(source) = keypair_source {
            let signer = pay_core::signer::load_signer(source)?;
            signer.pubkey().to_string()
        } else {
            return Err(pay_core::Error::Config(
                "No recipient specified. Use operator.recipient in YAML, --recipient flag, PAY_PAYMENT_RECIPIENT env, or `pay setup`."
                    .to_string(),
            ));
        };

        // Resolve other config — operator overrides > CLI flags > env vars > defaults.
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

        let secret_key = std::env::var("PAY_MPP_CHALLENGE_SECRET")
            .unwrap_or_else(|_| bs58::encode(rand::random::<[u8; 32]>()).into_string());

        let mpp = Mpp::new(solana_mpp::server::Config {
            recipient: recipient.clone(),
            currency: currency.clone(),
            decimals: if currency.to_uppercase() == "SOL" {
                9
            } else {
                6
            },
            network: network.clone(),
            rpc_url: Some(rpc_url.clone()),
            secret_key: Some(secret_key),
            fee_payer,
            fee_payer_signer,
            ..Default::default()
        })
        .map_err(|e| pay_core::Error::Config(format!("Failed to create MPP server: {e}")))?;

        let metered_count = api
            .endpoints
            .iter()
            .filter(|e| e.metering.is_some())
            .count();
        let free_count = api.endpoints.len() - metered_count;

        // ── Banner ──
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

        // ── Endpoint table ──
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

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("Failed to create runtime: {e}")))?;

        rt.block_on(async {
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

/// Create a SolanaSigner from the operator.signer config.
#[cfg(feature = "gcp_kms")]
fn resolve_signer(config: &SignerConfig) -> pay_core::Result<Arc<dyn SolanaSigner>> {
    match config {
        SignerConfig::GcpKms { key_name, pubkey } => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| pay_core::Error::Config(format!("Runtime error: {e}")))?;

            let signer = rt
                .block_on(solana_mpp::solana_keychain::GcpKmsSigner::new(
                    key_name.clone(),
                    pubkey.clone(),
                ))
                .map_err(|e| {
                    pay_core::Error::Config(format!("Failed to create GCP KMS signer: {e}"))
                })?;

            Ok(Arc::new(signer))
        }
    }
}
