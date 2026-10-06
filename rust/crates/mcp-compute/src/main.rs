use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use mcp_compute::binding::DataBindingClient;
use mcp_compute::driver::ComputeDriver;
use mcp_compute::google::{GoogleCloudFunctionsDriver, GoogleConfig};
use mcp_compute::{DriverRegistry, server};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-compute",
    about = "Provider-neutral paid serverless compute service"
)]
struct Args {
    #[arg(long, env = "COMPUTE_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    #[arg(
        long,
        env = "COMPUTE_GATEWAY_DOMAIN",
        default_value = "cpu.gcp.gateway-402.com"
    )]
    gateway_domain: String,
    #[arg(long, env = "COMPUTE_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,
    /// Disable MCP Host-header validation. This is safe for the private Cloud
    /// Run origin because IAM and the verified-payer header remain mandatory.
    #[arg(long, env = "COMPUTE_DISABLE_HOST_VALIDATION", default_value_t = false)]
    disable_host_validation: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let google = GoogleCloudFunctionsDriver::new(GoogleConfig::from_env()?)?;
    let bindings = DataBindingClient::from_env(google.clone())?;
    let drivers = DriverRegistry::new([Arc::new(google) as Arc<dyn ComputeDriver>])?;
    let allowed_hosts = if args.disable_host_validation {
        Vec::new()
    } else if args.allowed_hosts.is_empty() {
        vec!["localhost".into(), "127.0.0.1".into(), "::1".into()]
    } else {
        args.allowed_hosts
    };
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(address = %args.bind, "compute MCP listening");
    axum::serve(
        listener,
        server::router(drivers, args.gateway_domain, allowed_hosts, bindings),
    )
    .await?;
    Ok(())
}
