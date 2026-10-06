use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use mcp_wallet::driver::WalletDriver;
use mcp_wallet::privy::{PrivyConfig, PrivyWalletDriver};
use mcp_wallet::{DriverRegistry, server};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-wallet",
    about = "Provider-neutral programmatic wallet service"
)]
struct Args {
    #[arg(long, env = "WALLET_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    #[arg(long, env = "WALLET_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,
    #[arg(long, env = "WALLET_DISABLE_HOST_VALIDATION", default_value_t = false)]
    disable_host_validation: bool,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let driver =
        Arc::new(PrivyWalletDriver::new(PrivyConfig::from_env()?)?) as Arc<dyn WalletDriver>;
    let drivers = DriverRegistry::new([driver])?;
    let proof = std::env::var("WALLET_PROXY_PROOF")
        .map_err(|_| "WALLET_PROXY_PROOF must be set")?
        .into_bytes();
    if proof.len() < 32 {
        return Err("WALLET_PROXY_PROOF must contain at least 32 bytes".into());
    }
    let allowed = if args.disable_host_validation {
        vec![]
    } else if args.allowed_hosts.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    } else {
        args.allowed_hosts
    };
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(address=%args.bind,"wallet service listening");
    axum::serve(listener, server::router(drivers, proof, allowed)).await?;
    Ok(())
}
