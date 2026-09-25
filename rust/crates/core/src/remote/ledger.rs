//! Ledger hardware wallets, through solana-keychain's `ledger` backend.
//!
//! The key never leaves the device and every signature is confirmed on its
//! screen, so there are no credentials to store: `accounts.yml` carries only
//! the derivation path (as the wallet id) and the cached address.
//!
//! What a Ledger can and cannot do for pay:
//!
//! - Transactions sign natively for versions 0 and legacy; the Solana app
//!   does not sign version 1 yet, so [`SigningBackend::max_tx_version`] caps
//!   negotiation at V0.
//! - `sign_message` wraps the payload in an off-chain envelope, so the
//!   signature does not verify over the raw bytes. Payment paths that need a
//!   raw signature (SIWMPP authenticate, subscription bearer proofs,
//!   operator-signed session proofs, batch-settlement vouchers) refuse with
//!   a clear message; see [`crate::signer::ResolvedSigner::require_raw_message_signing`].
//!
//! Connecting blocks the calling thread until the device answers, exactly
//! like the platform keystores block on a biometric prompt.

use std::time::{Duration, Instant};

use pay_kit::core::tx::TxVersion;
use pay_kit::solana_keychain::{
    DEFAULT_DERIVATION_PATH, LedgerConfig, LedgerSigner, SignerError, SolanaSigner,
    TransactionSigner,
};

use crate::backend::{Approval, Custody, SigningBackend};
use crate::remote::{CredentialField, Credentials, RemoteProvider, RemoteWallet};
use crate::{Error, Result};

pub struct Ledger;

/// Derivation paths offered at setup, in order. The first is the Solana
/// app's default and what Ledger Live uses for its first account; the second
/// is the older Solana CLI convention.
pub const CANDIDATE_PATHS: &[&str] = &[DEFAULT_DERIVATION_PATH, "m/44'/501'/0'/0'"];

/// Solana's BIP-44 prefix; anything else is not a Solana key.
const SOLANA_PATH_PREFIX: &str = "m/44'/501'";
const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(500);

fn config(derivation_path: &str, confirm_on_device: bool) -> LedgerConfig {
    LedgerConfig {
        derivation_path: Some(derivation_path.to_string()),
        confirm_pubkey_on_device: confirm_on_device,
        // Launch the Solana app after the user unlocks the device. The
        // dependency's expected discovery failures are suppressed by the CLI
        // while this interactive retry runs.
        auto_open_app: true,
        ..LedgerConfig::default()
    }
}

fn config_with_timeout(
    derivation_path: &str,
    confirm_on_device: bool,
    timeout: Duration,
) -> LedgerConfig {
    LedgerConfig {
        signing_timeout: timeout,
        ..config(derivation_path, confirm_on_device)
    }
}

fn explain(err: SignerError, what: &str) -> Error {
    if let SignerError::NotAvailable(detail) = &err
        && app_configuration_is_incompatible(detail)
    {
        return Error::Config(format!(
            "Could not {what} on the Ledger: this pay build cannot parse the Solana app's configuration. \
             The device is fine; unlocking or reconnecting it will not help. Update pay when \
             Solana app 1.16 support is available, or use a compatible Ledger app or device."
        ));
    }
    Error::Config(format!(
        "Could not {what} on the Ledger: {err}\n\
         Plug the device in and unlock it, then retry."
    ))
}

fn discover_wallets() -> std::result::Result<Vec<RemoteWallet>, SignerError> {
    let mut wallets = Vec::with_capacity(CANDIDATE_PATHS.len());
    for (i, path) in CANDIDATE_PATHS.iter().enumerate() {
        match LedgerSigner::connect_with(config(path, false)) {
            Ok(signer) => wallets.push(RemoteWallet {
                id: path.to_string(),
                address: signer.pubkey().to_string(),
            }),
            Err(err) if i == 0 => return Err(err),
            Err(err) => {
                tracing::debug!(path, %err, "skipping Ledger derivation path");
            }
        }
    }
    Ok(wallets)
}

fn app_configuration_is_incompatible(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("configuration format this build")
        || detail.contains("newer solana-remote-wallet")
}

fn discovery_error_is_retryable(err: &SignerError) -> bool {
    let SignerError::NotAvailable(detail) = err else {
        return false;
    };
    let detail = detail.to_ascii_lowercase();

    // Keep this as an allowlist of the transient states emitted by
    // solana-keychain. NotAvailable also covers permanent configuration
    // failures (no udev access, multiple devices, unsupported app versions),
    // which must be returned instead of turning into an endless poll.
    detail == "no ledger device found"
        || detail.contains("ledger is busy with another operation")
        || detail.contains("the ledger is locked")
        || detail.starts_with("ledger did not answer.")
        || detail.starts_with("ledger is not reachable.")
}

impl SigningBackend for Ledger {
    fn id(&self) -> &'static str {
        "ledger"
    }
    fn display_name(&self) -> &'static str {
        "Ledger"
    }
    fn description(&self) -> &'static str {
        "hardware wallet, confirm each payment on the device"
    }
    fn custody(&self) -> Custody {
        Custody::Hardware
    }
    /// The key is generated on the device and cannot be read out.
    fn is_exportable(&self) -> bool {
        false
    }
    /// The Solana app signs an off-chain envelope, not the raw bytes.
    fn signs_raw_messages(&self) -> bool {
        false
    }
    fn approval(&self) -> Approval {
        Approval::DeviceConfirmation
    }
    /// A device is attached, whether or not it is unlocked yet.
    fn is_available(&self) -> bool {
        LedgerSigner::is_attached()
    }
    /// The Solana app signs v0 but not v1 yet (LedgerHQ/app-solana#248).
    fn max_tx_version(&self) -> Option<TxVersion> {
        Some(TxVersion::V0)
    }
}

impl RemoteProvider for Ledger {
    /// Nothing to store: the device is the credential.
    fn credential_fields(&self) -> &'static [CredentialField] {
        &[]
    }

    fn credentials_hint(&self) -> &'static str {
        "Plug in your Ledger and unlock it."
    }

    fn validate_wallet_id(&self, id: &str) -> Result<()> {
        if !id.starts_with(SOLANA_PATH_PREFIX) {
            return Err(Error::Config(format!(
                "`{id}` is not a Solana derivation path; expected one starting with `{SOLANA_PATH_PREFIX}` \
                 (default `{DEFAULT_DERIVATION_PATH}`)."
            )));
        }
        Ok(())
    }

    /// Read the address at each candidate path. The first read also proves
    /// the device is reachable; a failure there is reported with the fix.
    fn discover(&self, _credentials: &Credentials) -> Result<Vec<RemoteWallet>> {
        discover_wallets().map_err(|err| explain(err, "read the first account"))
    }

    fn discover_interactive(&self, _credentials: &Credentials) -> Result<Vec<RemoteWallet>> {
        loop {
            if !LedgerSigner::is_attached() {
                std::thread::sleep(DISCOVERY_POLL_INTERVAL);
                continue;
            }

            match discover_wallets() {
                Ok(wallets) => return Ok(wallets),
                Err(err) if discovery_error_is_retryable(&err) => {
                    std::thread::sleep(DISCOVERY_POLL_INTERVAL);
                }
                Err(err) => return Err(explain(err, "read the first account")),
            }
        }
    }

    fn no_wallets_hint(&self) -> &'static str {
        "No Ledger account could be read. Plug the device in and unlock it."
    }

    fn connect(
        &self,
        _credentials: &Credentials,
        wallet_id: &str,
    ) -> Result<Box<dyn TransactionSigner>> {
        self.validate_wallet_id(wallet_id)?;
        let signer = LedgerSigner::connect_with(config(wallet_id, false))
            .map_err(|err| explain(err, &format!("connect to account `{wallet_id}`")))?;
        Ok(Box::new(signer))
    }

    fn connect_interactive(
        &self,
        _credentials: &Credentials,
        wallet_id: &str,
    ) -> Result<Box<dyn TransactionSigner>> {
        self.validate_wallet_id(wallet_id)?;
        loop {
            if !LedgerSigner::is_attached() {
                std::thread::sleep(DISCOVERY_POLL_INTERVAL);
                continue;
            }

            match LedgerSigner::connect_with(config(wallet_id, false)) {
                Ok(signer) => return Ok(Box::new(signer)),
                Err(err) if discovery_error_is_retryable(&err) => {
                    std::thread::sleep(DISCOVERY_POLL_INTERVAL);
                }
                Err(err) => {
                    return Err(explain(err, &format!("connect to account `{wallet_id}`")));
                }
            }
        }
    }

    fn connect_interactive_for(
        &self,
        _credentials: &Credentials,
        wallet_id: &str,
        timeout: Duration,
    ) -> Result<Option<Box<dyn TransactionSigner>>> {
        self.validate_wallet_id(wallet_id)?;
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            if !LedgerSigner::is_attached() {
                std::thread::sleep(DISCOVERY_POLL_INTERVAL.min(deadline - now));
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match LedgerSigner::connect_with(config_with_timeout(wallet_id, false, remaining)) {
                Ok(signer) => return Ok(Some(Box::new(signer))),
                Err(err) if discovery_error_is_retryable(&err) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(None);
                    }
                    std::thread::sleep(DISCOVERY_POLL_INTERVAL.min(remaining));
                }
                Err(err) => {
                    return Err(explain(err, &format!("connect to account `{wallet_id}`")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_hardware_custody_without_export_or_raw_messages() {
        assert_eq!(Ledger.id(), "ledger");
        assert_eq!(Ledger.custody(), Custody::Hardware);
        assert!(!Ledger.is_exportable());
        assert!(!Ledger.signs_raw_messages());
        assert_eq!(Ledger.approval(), Approval::DeviceConfirmation);
        assert_eq!(Ledger.max_tx_version(), Some(TxVersion::V0));
        assert!(Ledger.credential_fields().is_empty());
        assert!(!Ledger.requires_credentials());
    }

    #[test]
    fn wallet_ids_are_solana_derivation_paths() {
        assert!(Ledger.validate_wallet_id(DEFAULT_DERIVATION_PATH).is_ok());
        assert!(Ledger.validate_wallet_id("m/44'/501'/3'/0'").is_ok());
        assert!(Ledger.validate_wallet_id("m/44'/60'/0'").is_err());
        assert!(Ledger.validate_wallet_id("acc_123").is_err());
        assert!(
            CANDIDATE_PATHS
                .iter()
                .all(|p| Ledger.validate_wallet_id(p).is_ok())
        );
    }

    #[test]
    fn interactive_discovery_retries_only_recoverable_device_states() {
        assert!(discovery_error_is_retryable(&SignerError::NotAvailable(
            "no Ledger device found".to_string()
        )));
        assert!(discovery_error_is_retryable(&SignerError::NotAvailable(
            "the Ledger is locked".to_string()
        )));
        assert!(discovery_error_is_retryable(&SignerError::NotAvailable(
            "Ledger is busy with another operation or awaiting on-device confirmation".to_string()
        )));
        assert!(discovery_error_is_retryable(&SignerError::NotAvailable(
            "Ledger did not answer. It is either locked, or another application is holding it"
                .to_string()
        )));
        assert!(!discovery_error_is_retryable(&SignerError::NotAvailable(
            "2 Ledger devices connected; pass host_device_path to select one".to_string()
        )));
        assert!(!discovery_error_is_retryable(&SignerError::NotAvailable(
            "the Ledger HID subsystem is unavailable".to_string()
        )));
        assert!(!discovery_error_is_retryable(&SignerError::NotAvailable(
            "the Ledger's Solana app speaks a configuration format this build of \
             solana-remote-wallet cannot parse; this needs a newer solana-remote-wallet"
                .to_string()
        )));
        assert!(!discovery_error_is_retryable(&SignerError::ConfigError(
            "invalid derivation path".to_string()
        )));
    }

    #[test]
    fn incompatible_solana_app_error_does_not_suggest_reconnecting() {
        let error = explain(
            SignerError::NotAvailable(
                "the Ledger's Solana app speaks a configuration format this build of \
                 solana-remote-wallet cannot parse; this needs a newer solana-remote-wallet"
                    .to_string(),
            ),
            "read the first account",
        );
        let message = error.to_string();
        assert!(message.contains("device is fine"));
        assert!(message.contains("Solana app 1.16 support"));
        assert!(!message.contains("Plug the device in"));
    }
}
