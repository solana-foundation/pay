use clap::Parser;
use pay_connect::AppState;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Privy-backed hosted wallets for MCP clients and the pay CLI.
#[derive(Parser)]
#[command(name = "pay-connect", version, about)]
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
    info!(url = %format!("http://{local}"), "pay-connect listening");

    let public_url = args
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", args.bind, args.port));
    let state = AppState::new(public_url.clone());
    let state = match std::env::var("PAY_CONNECT_PAGES_URL")
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
    info!(public_url, "pay-connect configured");
    #[cfg(feature = "mcp")]
    let state = match pay_connect::mcp::Config::from_env(&public_url) {
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
            state.with_mcp(cfg)
        }
        None => {
            info!(
                "MCP connector disabled: set {}=1 to enable",
                pay_connect::mcp::ENABLE_ENV
            );
            state
        }
    };
    #[cfg(feature = "privy")]
    let state = match pay_connect::privy::Config::from_env()? {
        Some(cfg) => {
            info!(
                app_id = %cfg.app_id,
                signer_id = %cfg.signer_id,
                policy = cfg.policy_id.as_deref().unwrap_or("none"),
                api = %cfg.api_base,
                jwks = %cfg.jwks_url,
                "Privy login enabled on the consent page"
            );
            let privy = pay_connect::privy::Privy::connect(cfg).await?;
            info!(keys = privy.key_count(), "Privy verification keys loaded");
            state.with_privy(privy)
        }
        None => {
            info!(
                "Privy login disabled: set {} (and the other PRIVY_* variables) to enable",
                pay_connect::privy::APP_ID_ENV
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
    let app = pay_connect::router(state).layer(trace);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
