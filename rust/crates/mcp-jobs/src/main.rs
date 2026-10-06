use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use mcp_jobs::driver::JobDriver;
use mcp_jobs::google::{GoogleJobsConfig, GoogleJobsDriver};
use mcp_jobs::{DriverRegistry, server};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-jobs",
    about = "Provider-neutral paid background jobs service"
)]
struct Args {
    #[arg(long, env = "JOBS_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    #[arg(long, env = "JOBS_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,
    #[arg(long, env = "JOBS_DISABLE_HOST_VALIDATION", default_value_t = false)]
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
        Arc::new(GoogleJobsDriver::new(GoogleJobsConfig::from_env()?)?) as Arc<dyn JobDriver>;
    let drivers = DriverRegistry::new([driver])?;
    let proof = std::env::var("JOBS_PROXY_PROOF")
        .map_err(|_| "JOBS_PROXY_PROOF must be set")?
        .into_bytes();
    if proof.len() < 32 {
        return Err("JOBS_PROXY_PROOF must contain at least 32 bytes".into());
    }
    let allowed_hosts = if args.disable_host_validation {
        vec![]
    } else if args.allowed_hosts.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    } else {
        args.allowed_hosts
    };
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(address=%args.bind,"jobs service listening");
    axum::serve(listener, server::router(drivers, proof, allowed_hosts)).await?;
    Ok(())
}
