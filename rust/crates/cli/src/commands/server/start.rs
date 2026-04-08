//! `pay server start` — start a payment gateway proxy.

use std::sync::Arc;

use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
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

    /// Use the local wallet as fee-payer signer instead of the operator.signer backend (e.g. GCP KMS).
    #[arg(long)]
    pub local_signer: bool,

    /// Launch the Payment Debugger UI alongside the gateway.
    #[arg(long)]
    pub debugger: bool,
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
    pub fn run(self, keypair_source: Option<&str>, sandbox: bool) -> pay_core::Result<()> {
        let debugger = self.debugger || sandbox;
        let expanded = shellexpand::tilde(&self.spec);
        let contents = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| pay_core::Error::Config(format!("Failed to read {}: {e}", self.spec)))?;

        let api: ApiSpec = serde_yml::from_str(&contents)
            .map_err(|e| pay_core::Error::Config(format!("Invalid spec: {e}")))?;

        // Apply env vars from spec (static values or ${VAR} passthrough).
        // SAFETY: called before any threads are spawned.
        for (key, value) in &api.env {
            if value.starts_with("${") && value.ends_with('}') {
                let var_name = &value[2..value.len() - 1];
                if let Ok(v) = std::env::var(var_name) {
                    unsafe { std::env::set_var(key, v) };
                }
            } else {
                unsafe { std::env::set_var(key, value) };
            }
        }

        let op = api.operator.clone();
        let op = op.as_ref();

        #[cfg(not(feature = "gcp_kms"))]
        if !self.local_signer && op.and_then(|o| o.signer.as_ref()).is_some() {
            return Err(pay_core::Error::Config(
                "operator.signer requires the `gcp_kms` feature. Rebuild with `--features gcp_kms`, or use --local-signer."
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
            .unwrap_or_else(|| {
                if sandbox {
                    pay_core::config::SANDBOX_RPC_URL.to_string()
                } else {
                    pay_core::config::LOCAL_RPC_URL.to_string()
                }
            });

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
            let use_local = self.local_signer || sandbox;

            let fee_payer_signer: Option<Arc<dyn SolanaSigner>> = if use_local {
                if let Some(source) = keypair_source_owned.as_deref() {
                    let signer = pay_core::signer::load_signer(source)?;
                    Some(Arc::new(signer) as Arc<dyn SolanaSigner>)
                } else {
                    None
                }
            } else {
                #[cfg(feature = "gcp_kms")]
                {
                    if let Some(ref cfg) = signer_cfg {
                        Some(resolve_signer(cfg).await?)
                    } else {
                        None
                    }
                }
                #[cfg(not(feature = "gcp_kms"))]
                { None }
            };

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

            // ── Sandbox: bootstrap operator wallet (SOL + token account) ──
            if self.sandbox && let Some(ref signer) = fee_payer_signer {
                let pubkey = signer.pubkey().to_string();
                sandbox_airdrop(&rpc_url, &pubkey).await;
                sandbox_bootstrap_recipient(&rpc_url, &recipient, &currency).await;
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
                html: true,
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
            eprintln!("  {}   {}", "╔═╗ ╔═╗ ╦ ╦".bold(), "╔═╗ ╦ ╦".dimmed());
            eprintln!("  {}   {}", "╠═╝ ╠═╣ ╚╦╝".bold(), "╚═╗ ╠═╣".dimmed());
            eprintln!("  {}  {} {}", "╩   ╩ ╩  ╩".bold(), "○".dimmed(), "╚═╝ ╩ ╩".dimmed());
            eprintln!("  {}", "Developer Tools for Programmable Payments".dimmed());
            eprintln!();

            // Network link
            let network_label = if sandbox { "sandbox" } else { &network };
            let network_url = if sandbox {
                if rpc_url.contains("localhost") || rpc_url.contains("127.0.0.1") {
                    "http://localhost:18488".to_string()
                } else {
                    rpc_url.clone()
                }
            } else {
                "https://explorer.solana.com".to_string()
            };
            let network_link = terminal_link(network_label, &network_url);

            // Operator link (explorer token page)
            let short_recipient = if recipient.len() > 8 {
                format!("{}...{}", &recipient[..4], &recipient[recipient.len() - 4..])
            } else {
                recipient.clone()
            };
            let encoded_rpc = urlencoding::encode(&rpc_url);
            let operator_url = format!(
                "https://explorer.solana.com/address/{}/tokens?cluster=custom&customUrl={}",
                recipient, encoded_rpc
            );
            let operator_link = terminal_link(&short_recipient, &operator_url);

            eprintln!("  {}\t{}", "network".dimmed(), network_link);
            eprintln!(
                "  {}\t{} via {}",
                "currency".dimmed(),
                "$".green(),
                currency.green()
            );
            let balance_suffix = if sandbox {
                let bal = fetch_sol_balance(&rpc_url, &recipient).await;
                format!(" ({} SOL)", format_price(bal))
            } else {
                String::new()
            };
            eprintln!("  {}\t{}{}", "operator".dimmed(), operator_link, balance_suffix.dimmed());
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

            let max_path_len = api
                .endpoints
                .iter()
                .map(|e| e.path.len())
                .max()
                .unwrap_or(20);

            let rule = format!(
                "  {}{}{}", "─".repeat(9), "─".repeat(max_path_len + 2), "─".repeat(10)
            );
            eprintln!("{}", rule.dimmed());

            for ep in &api.endpoints {
                let method = format!("{:?}", ep.method).to_uppercase();
                let method_padded = format!("{:<7}", method);
                let method_colored = match method.as_str() {
                    "GET" => method_padded.green().to_string(),
                    "POST" => method_padded.blue().to_string(),
                    "PUT" => method_padded.yellow().to_string(),
                    "DELETE" => method_padded.red().to_string(),
                    "PATCH" => method_padded.cyan().to_string(),
                    _ => method_padded.dimmed().to_string(),
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
                    format!("{:>8}", format!("${}", format_price(price)))
                        .yellow()
                        .to_string()
                } else {
                    format!("{:>8}", "free").green().to_string()
                };

                eprintln!(
                    "  {} {:<width$} {}",
                    method_colored,
                    ep.path,
                    price_tag,
                    width = max_path_len,
                );
            }

            eprintln!("{}", rule.dimmed());

            eprintln!();

            // ── Build router ──
            let endpoints_json = build_endpoints_json(&api);

            let verify_mpp = mpp.clone();

            let state = AppState {
                apis: Arc::new(vec![api.clone()]),
                mpp: Some(mpp),
            };
            let mut app = axum::Router::new()
                .route("/__402/health", get(|| async { "ok" }))
                .route(
                    "/__402/endpoints",
                    get(move || async move { axum::Json(endpoints_json).into_response() }),
                )
                .route(
                    "/__402/verify",
                    post(move |body: axum::Json<GatewayVerifyRequest>| async move {
                        gateway_verify(verify_mpp, body.0).await
                    }),
                );

            let pdb_state = if debugger {
                let pdb_config = build_pdb_config(&api, &recipient, &network, &rpc_url);
                let pdb = pay_pdb::PdbState::new(pdb_config);
                pdb.spawn_cleanup();
                Some(pdb)
            } else {
                None
            };

            if let Some(ref pdb) = pdb_state {
                app = app
                    .route(
                        "/",
                        get(|headers: axum::http::HeaderMap| async move {
                            let accepts_html = headers
                                .get("accept")
                                .and_then(|v| v.to_str().ok())
                                .is_some_and(|v| v.contains("text/html"));
                            if accepts_html {
                                axum::response::Redirect::temporary("/__402/pdb/")
                                    .into_response()
                            } else {
                                axum::Json(serde_json::json!({"status": "ok"})).into_response()
                            }
                        }),
                    )
                    .nest_service(
                        "/__402/pdb",
                    pay_pdb::debugger_router(pdb.clone()),
                );
            }

            let app = app
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
                .with_state(state)
                // Logging layer (outermost — executes first).
                // Extension must be added AFTER the middleware layer (LIFO order)
                // so the extension is available when the middleware runs.
                .layer(middleware::from_fn(pay_pdb::logging::logging_middleware))
                .layer(axum::Extension(pdb_state));

            let listener = tokio::net::TcpListener::bind(&self.bind)
                .await
                .map_err(|e| {
                    pay_core::Error::Config(format!("Failed to bind {}: {e}", self.bind))
                })?;
            let display_addr = self.bind.replace("0.0.0.0", "127.0.0.1");
            if debugger {
                eprintln!(
                    "  {} {}",
                    "debugger".green().bold(),
                    format!("http://{}", display_addr).bold()
                );
            } else {
                eprintln!(
                    "  {} {}",
                    "listening".green().bold(),
                    format!("http://{}", display_addr).bold()
                );
            }
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
            "url": api.routing.display_url(),
        },
        "endpoints": endpoints,
    })
}

/// Build the sidebar config for the PDB frontend.
fn build_pdb_config(
    api: &ApiSpec,
    recipient: &str,
    network: &str,
    rpc_url: &str,
) -> serde_json::Value {
    let metered: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .filter(|e| e.metering.is_some())
        .map(|e| {
            let price = e
                .metering
                .as_ref()
                .and_then(|m| m.dimensions.first())
                .and_then(|d| d.tiers.first())
                .map(|t| format!("${}", format_price(t.price_usd)))
                .unwrap_or_else(|| "metered".into());
            serde_json::json!({
                "method": format!("{:?}", e.method).to_uppercase(),
                "path": e.path,
                "price": price,
                "description": e.description.as_deref().unwrap_or(""),
            })
        })
        .collect();

    let free: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .filter(|e| e.metering.is_none())
        .map(|e| {
            serde_json::json!({
                "method": format!("{:?}", e.method).to_uppercase(),
                "path": e.path,
                "price": "free",
                "description": e.description.as_deref().unwrap_or(""),
            })
        })
        .collect();

    serde_json::json!({
        "recipient": recipient,
        "network": network,
        "rpcUrl": rpc_url,
        "endpoints": {
            "mpp": metered,
            "x402": [],
            "oauth": free,
        }
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
    let balance_lamports = fetch_lamports(&client, rpc_url, pubkey).await;

    if balance_lamports < 1_000_000_000 {
        let _ = client
            .post(rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "requestAirdrop",
                "params": [pubkey, 2_000_000_000u64]
            }))
            .send()
            .await;
    }
}

async fn fetch_lamports(client: &reqwest::Client, rpc_url: &str, pubkey: &str) -> u64 {
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBalance",
            "params": [pubkey]
        }))
        .send()
        .await;
    match resp {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["result"]["value"].as_u64())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

async fn fetch_sol_balance(rpc_url: &str, pubkey: &str) -> f64 {
    let client = reqwest::Client::new();
    fetch_lamports(&client, rpc_url, pubkey).await as f64 / 1_000_000_000.0
}

/// Bootstrap the recipient's token account on the sandbox so SPL transfers succeed.
async fn sandbox_bootstrap_recipient(rpc_url: &str, recipient: &str, currency: &str) {
    let (mint, token_program) = match currency.to_uppercase().as_str() {
        "SOL" => return, // Native SOL doesn't need a token account
        "USDC" => (
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
        ),
        _ => return,
    };

    let client = reqwest::Client::new();

    // Set SOL balance so the account exists
    let _ = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "surfnet_setAccount",
            "params": [recipient, {
                "lamports": 1_000_000_000_u64,
                "data": "",
                "executable": false,
                "owner": "11111111111111111111111111111111",
            }]
        }))
        .send()
        .await;

    // Create token account with zero balance
    let _ = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "surfnet_setTokenAccount",
            "params": [recipient, mint, {
                "amount": 0,
            }, token_program]
        }))
        .send()
        .await;

}

fn format_price(price: f64) -> String {
    if price.fract() == 0.0 {
        format!("{}", price as u64)
    } else {
        let s = format!("{:.4}", price);
        s.trim_end_matches('0').to_string()
    }
}

/// Emit an OSC 8 clickable hyperlink for terminals that support it.
fn terminal_link(text: &str, url: &str) -> String {
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text)
}

// ── Gateway verify endpoint ──

#[derive(serde::Deserialize)]
struct GatewayVerifyRequest {
    method: String,
    path: String,
    price: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    authorization: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    request_id: Option<String>,
    #[serde(default)]
    external_id: Option<String>,
    /// JSON-encoded splits array from the gateway (assembled by JS policy).
    #[serde(default)]
    splits_json: Option<String>,
}

#[derive(serde::Serialize)]
struct GatewayVerifyResponse {
    decision: String,
    status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    www_authenticate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    challenge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
}

async fn gateway_verify(
    mpp: Mpp,
    req: GatewayVerifyRequest,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use solana_mpp::{format_receipt, format_www_authenticate, parse_authorization};

    let auth = req
        .authorization
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    // Parse splits from JSON string (assembled by Apigee JS policy).
    let splits: Vec<solana_mpp::protocol::solana::Split> = req
        .splits_json
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "[]")
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    match auth {
        None => {
            let challenge = match mpp.charge_with_options(
                &req.price,
                solana_mpp::server::ChargeOptions {
                    description: req.description.as_deref(),
                    external_id: req.external_id.as_deref(),
                    splits: splits.clone(),
                    ..Default::default()
                },
            ) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            let www_auth = format_www_authenticate(&challenge).unwrap_or_default();
            axum::Json(GatewayVerifyResponse {
                decision: "payment_required".to_string(),
                status_code: 402,
                www_authenticate: Some(www_auth),
                body: Some(serde_json::json!({
                    "error": "payment_required",
                    "endpoint": { "method": req.method, "path": req.path },
                })),
                challenge_id: Some(challenge.id),
                external_id: req.external_id,
                receipt: None,
                receipt_status: None,
                receipt_reference: None,
            })
            .into_response()
        }
        Some(auth_value) => {
            let credential = match parse_authorization(auth_value) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            match mpp.verify_credential(&credential).await {
                Ok(receipt) => {
                    let encoded = format_receipt(&receipt).unwrap_or_default();
                    axum::Json(GatewayVerifyResponse {
                        decision: "allow".to_string(),
                        status_code: 200,
                        receipt: Some(encoded),
                        receipt_status: Some(receipt.status.to_string()),
                        receipt_reference: Some(receipt.reference),
                        challenge_id: Some(receipt.challenge_id),
                        external_id: req.external_id,
                        www_authenticate: None,
                        body: None,
                    })
                    .into_response()
                }
                Err(error) => {
                    // Re-issue challenge on failure
                    let challenge = mpp
                        .charge_with_options(
                            &req.price,
                            solana_mpp::server::ChargeOptions {
                                description: req.description.as_deref(),
                                external_id: req.external_id.as_deref(),
                                splits,
                                ..Default::default()
                            },
                        )
                        .ok();
                    let www_auth = challenge
                        .as_ref()
                        .and_then(|c| format_www_authenticate(c).ok());
                    axum::Json(GatewayVerifyResponse {
                        decision: "payment_required".to_string(),
                        status_code: 402,
                        www_authenticate: www_auth,
                        body: Some(serde_json::json!({
                            "error": "verification_failed",
                            "message": error.to_string(),
                            "retryable": error.retryable,
                        })),
                        challenge_id: challenge.map(|c| c.id),
                        external_id: req.external_id,
                        receipt: None,
                        receipt_status: Some("failed".to_string()),
                        receipt_reference: None,
                    })
                    .into_response()
                }
            }
        }
    }
}
