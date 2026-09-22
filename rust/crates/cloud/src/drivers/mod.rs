//! Wallet drivers: how pay-cloud turns a signed-in user into a wallet the
//! `pay` CLI can use.
//!
//! pay-cloud holds no custody account of its own. A driver walks the user
//! through the provider's own sign-in (a consent page that hands back the
//! user's project credentials), then uses those credentials to prepare a
//! wallet, and returns everything the CLI needs to register the account
//! locally. Credentials live only in the pending onboarding session, for
//! minutes, and are handed to the CLI once.
//!
//! One driver per provider, each behind a cargo feature:
//!
//! - [`openfort`] (`openfort` feature): Openfort backend wallets.
//!
//! Adding a provider is a module implementing [`WalletDriver`] and a line in
//! [`all`].

#[cfg(feature = "openfort")]
pub mod openfort;

use std::collections::BTreeMap;

/// What a provider's consent page hands back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsentGrant {
    /// Provider secret API key for the user's project.
    pub api_key: String,
    /// Provider publishable key, when the provider has one.
    pub publishable_key: Option<String>,
    /// Provider-side project id.
    pub project_id: Option<String>,
    /// Human-readable project name.
    pub project: Option<String>,
}

/// A wallet ready for the CLI to register as a remote account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedWallet {
    /// `pay_core::remote` provider id (`openfort`).
    pub provider: &'static str,
    /// Credential fields exactly as the provider's `RemoteProvider`
    /// declares them (`secret_key`, `wallet_secret`, …).
    pub credentials: BTreeMap<String, String>,
    /// Provider-side wallet id, stored as the account's `account` field.
    pub wallet_id: String,
    /// Base58 Solana address.
    pub address: String,
    /// Provider project id, informational.
    pub project_id: Option<String>,
}

impl std::fmt::Display for ProvisionedWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} wallet {} ({})",
            self.provider, self.wallet_id, self.address
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The provider rejected the grant or a request (status and message).
    #[error("{provider} rejected the request ({status}): {message}")]
    Rejected {
        provider: &'static str,
        status: u16,
        message: String,
    },
    /// The provider could not be reached.
    #[error("could not reach {provider}: {source}")]
    Unreachable {
        provider: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The provider answered with something unexpected.
    #[error("{provider} returned an unexpected response: {message}")]
    Protocol {
        provider: &'static str,
        message: String,
    },
    /// Local key generation or encoding failed.
    #[error("cryptography error: {0}")]
    Crypto(String),
    /// The grant is missing something the driver needs.
    #[error("invalid grant: {0}")]
    InvalidGrant(String),
}

/// A custody provider pay-cloud can onboard a user with.
#[async_trait::async_trait]
pub trait WalletDriver: Send + Sync {
    /// `pay_core::remote` provider id.
    fn id(&self) -> &'static str;

    /// Human-readable name for the page.
    fn display_name(&self) -> &'static str;

    /// Where to send the browser so the user signs in (or up) with the
    /// provider and returns a [`ConsentGrant`] to `redirect_uri`, echoing
    /// `state`.
    fn consent_url(&self, redirect_uri: &str, state: &str) -> String;

    /// Parse what the consent page sent back (the URL fragment) into a grant
    /// and the echoed `state`.
    fn parse_grant(&self, fragment: &str) -> Result<(ConsentGrant, String), DriverError>;

    /// Turn a grant into a wallet the CLI can use.
    async fn provision(&self, grant: &ConsentGrant) -> Result<ProvisionedWallet, DriverError>;

    /// A provider-authenticated account identity, when the driver can derive
    /// one from the grant. Display metadata supplied by the browser must not
    /// be used here.
    fn account_identity(&self, _grant: &ConsentGrant) -> Option<String> {
        None
    }

    /// A returning user signed in again: carry any credential the new
    /// grant supersedes (a rotated API key) into the stored set.
    fn refresh_credentials(
        &self,
        _credentials: &mut BTreeMap<String, String>,
        _grant: &ConsentGrant,
    ) {
    }
}

/// Every compiled-in driver.
pub fn all() -> Vec<Box<dyn WalletDriver>> {
    vec![
        #[cfg(feature = "openfort")]
        Box::new(openfort::Openfort::default()),
    ]
}

/// Look a driver up by id.
pub fn by_id(id: &str) -> Option<Box<dyn WalletDriver>> {
    all().into_iter().find(|d| d.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_ids_are_unique_and_resolve() {
        let ids: Vec<&str> = all().iter().map(|d| d.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "{ids:?}");
        for id in ids {
            assert!(by_id(id).is_some(), "{id}");
        }
        assert!(by_id("nope").is_none());
    }

    #[cfg(feature = "openfort")]
    #[test]
    fn openfort_is_compiled_in_by_default() {
        assert!(by_id("openfort").is_some());
    }
}
