use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use mcp_compute::binding::DataBindingClient;
use mcp_compute::driver::ComputeDriver;
use mcp_compute::google::{GoogleCloudFunctionsDriver, GoogleConfig};
use mcp_compute::trigger_driver::TriggerDriver;
use mcp_compute::trigger_google::{GoogleTriggerConfig, GoogleTriggerDriver};
use mcp_compute::{DriverRegistry, TriggerDriverRegistry, server};

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
    let trigger_driver = Arc::new(GoogleTriggerDriver::new(GoogleTriggerConfig::from_env()?).await?)
        as Arc<dyn TriggerDriver>;
    let trigger_drivers = TriggerDriverRegistry::new([trigger_driver])?;
    let executor_proof = std::env::var("COMPUTE_TRIGGER_EXECUTOR_PROOF")
        .map_err(|_| "COMPUTE_TRIGGER_EXECUTOR_PROOF must be set")?
        .into_bytes();
    if executor_proof.len() < 32 {
        return Err("COMPUTE_TRIGGER_EXECUTOR_PROOF must contain at least 32 bytes".into());
    }
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
        server::router(
            drivers,
            trigger_drivers,
            args.gateway_domain,
            allowed_hosts,
            bindings,
            executor_proof,
        ),
    )
    .await?;
    Ok(())
}
