//! `pay server demo` — start the gateway with a bundled demo paywall.
//!
//! Extracts the embedded playground API spec to `./pay-demo.yaml` in the
//! current working directory, then invokes `pay gate api` with sandbox and
//! debugger implied.

use crate::commands::server::start::StartCommand;

const DEMO_PAYWALL: &str = include_str!("../../../../../playground-api.yaml");

#[derive(clap::Args)]
pub struct DemoCommand {
    /// Address to bind to.
    #[arg(long, default_value = "0.0.0.0:1402")]
    pub bind: String,

    /// Recipient wallet address for payments.
    #[arg(long)]
    pub recipient: Option<String>,

    /// Payment currency (SOL, USDC, etc.).
    #[arg(long, default_value = "USDC")]
    pub currency: String,

    /// Use local Surfpool (http://localhost:8899) instead of hosted sandbox.
    #[arg(long)]
    pub local: bool,

    /// Export traces and metrics to an OTLP HTTP sidecar at HOST:PORT.
    #[arg(long, value_name = "HOST:PORT")]
    pub otlp_sidecar: Option<String>,
}

impl DemoCommand {
    pub fn run(
        self,
        legacy_signer_source: Option<&str>,
        account_override: Option<&str>,
        _sandbox: bool,
    ) -> pay_core::Result<()> {
        // Keep the generated file after first launch: subscription Plan
        // publication writes its PDA and immutable terms back into this YAML,
        // and the challenge-binding secret must remain stable for bearer reuse.
        let paywall_path = std::path::PathBuf::from("pay-demo.yaml");
        if !paywall_path.exists() {
            let challenge_secret = bs58::encode(rand::random::<[u8; 32]>()).into_string();
            let rendered = DEMO_PAYWALL.replace("${MPP_SECRET_KEY}", &challenge_secret);
            std::fs::write(&paywall_path, rendered).map_err(|e| {
                pay_core::Error::Config(format!("Failed to write pay-demo.yaml: {e}"))
            })?;
        }

        // Demo mode always runs on sandbox. Default to hosted Surfpool;
        // --local overrides to localhost.
        let rpc_url = if self.local {
            Some(pay_core::config::LOCAL_RPC_URL.to_string())
        } else {
            Some(pay_core::config::SANDBOX_RPC_URL.to_string())
        };

        let cmd = StartCommand {
            paywall: paywall_path.to_string_lossy().into_owned(),
            bind: self.bind,
            tls_cert: None,
            tls_key: None,
            recipient: self.recipient,
            currency: self.currency,
            rpc_url,
            debugger: true,
            otlp_sidecar: self.otlp_sidecar,
            openapi: None,
            public_url: None,
            no_register: false,
            scaffolded_paywall: Some("./pay-demo.yaml".to_string()),
        };
        cmd.run(legacy_signer_source, account_override, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_demo_exposes_all_playground_payment_patterns() {
        let api: pay_types::metering::ApiSpec = serde_yml::from_str(DEMO_PAYWALL).unwrap();
        let paths: Vec<&str> = api
            .endpoints
            .iter()
            .map(|endpoint| endpoint.path.as_str())
            .collect();

        assert_eq!(paths.len(), 6);
        assert!(paths.contains(&"api/v1/quote/{symbol}"));
        assert!(paths.contains(&"api/v1/fortune"));
        assert!(paths.contains(&"api/v1/joke"));
        assert!(paths.contains(&"api/v1/summarize"));
        assert!(paths.contains(&"api/v1/feed"));
        assert!(paths.contains(&"api/v1/stream"));
        assert!(
            api.endpoints
                .iter()
                .find(|endpoint| endpoint.path == "api/v1/feed")
                .unwrap()
                .subscription
                .is_some()
        );
    }
}
