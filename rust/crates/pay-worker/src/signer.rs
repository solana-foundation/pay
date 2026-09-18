//! Fee-payer signer construction, replicated from
//! `pay-api/crates/api/src/signer.rs`.
//!
//! Two paths, env-var first:
//!
//! 1. **Local testing.** If `LOCAL_FEE_PAYER_PRIVATE_KEY` is set (a base58
//!    secret key or a `[1,2,3,...]` Solana CLI keypair JSON array), sign
//!    in-process with it. Handy for `sandbox`/surfnet without GCP.
//! 2. **Production.** GCP-KMS-backed signer built from the configured
//!    `key_name` + `pubkey`.

use std::sync::Arc;

use pay_kit::mpp::solana_keychain::{Signer, SolanaSigner, TransactionSigner};

use crate::config::FeePayerConfig;
use crate::error::JobError;

const LOCAL_PRIVATE_KEY_ENV: &str = "LOCAL_FEE_PAYER_PRIVATE_KEY";

/// Build a fee-payer `SolanaSigner` from config + env. See module docs.
pub async fn build_fee_payer_signer(
    fee_payer: &FeePayerConfig,
) -> Result<Arc<dyn TransactionSigner>, JobError> {
    if let Ok(key) = std::env::var(LOCAL_PRIVATE_KEY_ENV) {
        let key = key.trim();
        if !key.is_empty() {
            let signer = Signer::from_memory(key).map_err(|_| JobError::FeePayerSigner)?;
            let expected_pubkey = fee_payer
                .pubkey
                .as_deref()
                .filter(|pubkey| !pubkey.trim().is_empty())
                .ok_or_else(|| {
                    JobError::Config(
                        "fee-payer pubkey is required when LOCAL_FEE_PAYER_PRIVATE_KEY is set"
                            .into(),
                    )
                })?;
            if signer.pubkey().to_string() != expected_pubkey {
                tracing::warn!(
                    expected_pubkey,
                    actual_pubkey = %signer.pubkey(),
                    "rejecting local fee-payer key with unexpected pubkey"
                );
                return Err(JobError::FeePayerSigner);
            }
            tracing::warn!(pubkey = %signer.pubkey(), "using local fee-payer private key");
            return into_transaction_signer(signer);
        }
    }

    let key_name = fee_payer.key_name.as_deref().ok_or_else(|| {
        JobError::Config(
            "fee-payer key_name missing (set PAY_API_SEND__FEE_PAYER__KEY_NAME or \
             LOCAL_FEE_PAYER_PRIVATE_KEY)"
                .into(),
        )
    })?;
    let pubkey = fee_payer.pubkey.as_deref().ok_or_else(|| {
        JobError::Config(
            "fee-payer pubkey missing (set PAY_API_SEND__FEE_PAYER__PUBKEY or \
             LOCAL_FEE_PAYER_PRIVATE_KEY)"
                .into(),
        )
    })?;
    let signer = Signer::from_gcp_kms(key_name.to_string(), pubkey.to_string())
        .await
        .map_err(|_| JobError::FeePayerSigner)?;
    into_transaction_signer(signer)
}

/// The keychain `Signer` enum only implements message signing; transactions
/// are signed by the concrete backend. Both backends this service configures
/// implement `TransactionSigner`.
fn into_transaction_signer(signer: Signer) -> Result<Arc<dyn TransactionSigner>, JobError> {
    match signer {
        Signer::Memory(signer) => Ok(Arc::new(signer)),
        Signer::GcpKms(signer) => Ok(Arc::new(signer)),
        // Feature unification can add keychain variants this service never
        // configures (e.g. `openfort` enabled by the CLI); none of them is a
        // fee payer here.
        #[allow(unreachable_patterns)]
        _ => Err(JobError::FeePayerSigner),
    }
}
