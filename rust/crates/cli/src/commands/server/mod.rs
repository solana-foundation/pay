pub mod demo;
pub mod inference;
pub mod local_registration;
pub(crate) mod payments;
pub(crate) mod provider_registration;
pub mod scaffold;
pub mod start;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum GateCommand {
    /// Start a local demo with a dashboard for tracing payments.
    Demo(demo::DemoCommand),
    /// Start a proxy that enables stablecoin payments for your API.
    Api(start::StartCommand),
    /// Discover local AI inference servers (Ollama, LM Studio, llama.cpp,
    /// vLLM, exo) and proxy them with live request tracking.
    Inference(inference::InferenceCommand),
    /// Create a paywall YAML file that defines endpoints and payment requirements.
    Scaffold(scaffold::ScaffoldCommand),
    /// Legacy alias for `pay gate api`.
    #[command(hide = true)]
    Start(start::StartCommand),
}

impl GateCommand {
    pub fn otlp_sidecar(&self) -> Option<&str> {
        match self {
            Self::Demo(cmd) => cmd.otlp_sidecar.as_deref(),
            Self::Api(cmd) => cmd.otlp_sidecar.as_deref(),
            Self::Inference(_) => None,
            Self::Scaffold(_) => None,
            Self::Start(cmd) => cmd.otlp_sidecar.as_deref(),
        }
    }

    pub fn run(
        self,
        legacy_signer_source: Option<&str>,
        account_override: Option<&str>,
        sandbox: bool,
    ) -> pay_core::Result<()> {
        match self {
            Self::Demo(cmd) => cmd.run(legacy_signer_source, account_override, sandbox),
            Self::Api(cmd) => cmd.run(legacy_signer_source, account_override, sandbox),
            Self::Inference(cmd) => cmd.run(legacy_signer_source, account_override, sandbox),
            Self::Scaffold(cmd) => cmd.run(),
            Self::Start(cmd) => cmd.run(legacy_signer_source, account_override, sandbox),
        }
    }
}

/// Load the account selected for `network` without flattening its auth policy.
///
/// Named accounts (`--account`, then `PAY_ACTIVE_ACCOUNT`) and the network's
/// active account go through the existing account-aware loader. A raw
/// `pay.toml`/legacy keystore source is used only when no account is selected.
pub(crate) fn load_account_or_legacy_signer(
    network: &str,
    cli_account: Option<&str>,
    legacy_source: Option<&str>,
    intent: &pay_core::keystore::AuthIntent,
) -> pay_core::Result<Option<pay_core::signer::ResolvedSigner>> {
    let env_account = std::env::var("PAY_ACTIVE_ACCOUNT")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let account_override = cli_account.or(env_account.as_deref());
    let accounts = pay_core::accounts::AccountsFile::load()?;
    let has_network_account = accounts.account_for_network(network).is_some();

    if account_override.is_some() || has_network_account {
        let store = pay_core::accounts::FileAccountsStore::default_path();
        let (signer, _) = pay_core::signer::load_signer_for_network_with_intent(
            network,
            &store,
            account_override,
            intent,
        )?;
        return Ok(Some(signer));
    }

    legacy_source
        .map(|source| pay_core::signer::load_resolved_signer_with_intent(source, intent))
        .transpose()
}
