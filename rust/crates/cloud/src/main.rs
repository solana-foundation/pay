use clap::Parser;
use pay_cloud::AppState;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// pay-cloud v0: browser onboarding for the pay CLI.
#[derive(Parser)]
#[command(name = "pay-cloud", version, about)]
struct Args {
    /// Interface to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// TCP port to listen on.
    #[arg(long, default_value_t = 8402)]
    port: u16,

    /// Base URL browsers reach this server at, used as the consent redirect
    /// target. Defaults to `http://<bind>:<port>`.
    #[arg(long)]
    public_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let addr = format!("{}:{}", args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local = listener.local_addr()?;
    info!(url = %format!("http://{local}"), "pay-cloud listening");

    let public_url = args
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", args.bind, args.port));
    let state = AppState::new(public_url.clone());
    let state = match std::env::var("PAY_CLOUD_PAGES_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(pages) => {
            info!(%pages, "consent page served by the pages app");
            state.with_pages_url(pages)
        }
        None => state,
    };
    info!(public_url, drivers = ?state.driver_ids(), "wallet drivers");
    #[cfg(feature = "coinflow")]
    let state = match pay_cloud::funding::Config::from_env()? {
        Some(cfg) => {
            info!(
                env = ?cfg.env,
                merchant = %cfg.merchant_id,
                settle_to_customer = cfg.settle_to_customer,
                webhooks = cfg.webhook_key.is_some(),
                "card purchases enabled (Coinflow)"
            );
            state.with_funding(pay_cloud::funding::Funding::new(cfg))
        }
        None => {
            info!(
                "card purchases disabled: set {} to enable",
                pay_cloud::funding::API_KEY_ENV
            );
            state
        }
    };
    #[cfg(feature = "mcp")]
    let state = match pay_cloud::mcp::Config::from_env(&public_url) {
        Some(cfg) => {
            info!(
                static_tokens = cfg.token_count(),
                allowed_hosts = ?cfg.allowed_hosts,
                "MCP connector enabled at /mcp with its OAuth server"
            );
            if let Some(tenant) = cfg.anonymous_tenant() {
                tracing::warn!(
                    subject = %tenant.id,
                    "DEV ONLY: requests with no Authorization act as this tenant; never deploy like this"
                );
            }
            let state = state.with_mcp(cfg.clone());
            dev_mock_tenants(&state, &cfg).await?;
            state
        }
        None => {
            info!(
                "MCP connector disabled: set {}=1 to enable",
                pay_cloud::mcp::ENABLE_ENV
            );
            state
        }
    };
    #[cfg(feature = "privy")]
    let state = match pay_cloud::privy::Config::from_env()? {
        Some(cfg) => {
            info!(
                app_id = %cfg.app_id,
                signer_id = %cfg.signer_id,
                policy = cfg.policy_id.as_deref().unwrap_or("none"),
                api = %cfg.api_base,
                jwks = %cfg.jwks_url,
                "Privy login enabled on the consent page"
            );
            let privy = pay_cloud::privy::Privy::connect(cfg).await?;
            info!(keys = privy.key_count(), "Privy verification keys loaded");
            state.with_privy(privy)
        }
        None => {
            info!(
                "Privy login disabled: set {} (and the other PRIVY_* variables) to enable",
                pay_cloud::privy::APP_ID_ENV
            );
            state
        }
    };
    // One line per request at INFO: a host's failed handshake is
    // diagnosed from these, so they must not hide behind a debug filter.
    let trace = TraceLayer::new_for_http()
        .make_span_with(tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO))
        .on_request(tower_http::trace::DefaultOnRequest::new().level(tracing::Level::INFO))
        .on_response(
            tower_http::trace::DefaultOnResponse::new()
                .level(tracing::Level::INFO)
                .latency_unit(tower_http::LatencyUnit::Millis),
        );
    let app = pay_cloud::router(state).layer(trace);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// `PAY_CLOUD_DEV_MOCK_TENANTS=1`: give every static token a wallet from
/// the first driver using a mock grant, so a host connected with a header
/// can use every tool. Only meaningful against the mock Openfort; a real
/// provider refuses the grant, which is the point.
#[cfg(feature = "mcp")]
async fn dev_mock_tenants(
    state: &AppState,
    cfg: &pay_cloud::mcp::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let enabled = std::env::var("PAY_CLOUD_DEV_MOCK_TENANTS")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true"));
    if !enabled || cfg.tokens().is_empty() {
        return Ok(());
    }
    let Some(driver_id) = state.driver_ids().first().copied() else {
        return Err("PAY_CLOUD_DEV_MOCK_TENANTS needs a wallet driver".into());
    };
    let driver = state.driver(driver_id).expect("listed driver exists");
    let grant = pay_cloud::drivers::ConsentGrant {
        api_key: "sk_test_mock".to_string(),
        publishable_key: Some("pk_test_mock".to_string()),
        project_id: Some("pro_mock".to_string()),
        project: Some("Mock project".to_string()),
    };
    for token in cfg.tokens() {
        let wallet = driver
            .provision(&grant)
            .await
            .map_err(|e| format!("dev tenant provisioning failed: {e}"))?;
        let subject = pay_cloud::mcp::token_fingerprint(token);
        info!(%subject, address = %wallet.address, "DEV ONLY: static token bound to a mock wallet");
        state
            .tenants()
            .bind(pay_cloud::tenants::TenantRecord::from_wallet(
                &subject, &wallet,
            ));
    }
    Ok(())
}

#[cfg(not(feature = "mcp"))]
#[allow(dead_code)]
async fn dev_mock_tenants(_state: &AppState, _cfg: &()) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}
