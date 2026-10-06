use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use mcp_data::binding::BindingIssuer;
use mcp_data::driver::DataDriver;
use mcp_data::firestore::{FirestoreConfig, FirestoreDriver};
use mcp_data::{DriverRegistry, server};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-data",
    about = "Provider-neutral paid managed data service"
)]
struct Args {
    #[arg(long, env = "DATA_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    #[arg(
        long,
        env = "DATA_GATEWAY_DOMAIN",
        default_value = "data.gcp.gateway-402.com"
    )]
    gateway_domain: String,
    #[arg(long, env = "DATA_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,
    #[arg(long, env = "DATA_DISABLE_HOST_VALIDATION", default_value_t = false)]
    disable_host_validation: bool,
    /// Serve only capability-authenticated workload data routes. This mode is
    /// safe to expose without Cloud Run IAM because it has no MCP or binding
    /// issuance endpoint.
    #[arg(long, env = "DATA_RUNTIME_ONLY", default_value_t = false)]
    runtime_only: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let firestore =
        Arc::new(FirestoreDriver::new(FirestoreConfig::from_env()?)?) as Arc<dyn DataDriver>;
    let drivers = DriverRegistry::new([firestore])?;
    let binding_key = std::env::var("DATA_BINDING_SIGNING_KEY")
        .map_err(|_| "DATA_BINDING_SIGNING_KEY must be set")?
        .into_bytes();
    let internal_proof = std::env::var("DATA_BINDING_INTERNAL_PROOF")
        .map_err(|_| "DATA_BINDING_INTERNAL_PROOF must be set")?
        .into_bytes();
    let bindings = BindingIssuer::new(binding_key, internal_proof)?;
    let allowed_hosts = if args.disable_host_validation {
        Vec::new()
    } else if args.allowed_hosts.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    } else {
        args.allowed_hosts
    };
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(address = %args.bind, runtime_only = args.runtime_only, "data service listening");
    let app = if args.runtime_only {
        server::runtime_router(drivers, bindings)
    } else {
        server::router(drivers, args.gateway_domain, allowed_hosts, bindings)
    };
    axum::serve(listener, app).await?;
    Ok(())
}
