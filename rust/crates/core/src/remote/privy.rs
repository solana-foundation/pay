//! Privy embedded and server wallets, as a [`RemoteProvider`].
//!
//! The key lives in Privy's enclave. pay holds the Privy *app's*
//! credentials (app id, app secret) plus one authorization key: a P-256
//! private key registered in the dashboard as a key quorum, which Privy
//! requires as a signer on any wallet it should be allowed to use. Signing
//! is one policy-checked HTTPS call per transaction.
//!
//! This is the operator's view: the same three credentials sign for every
//! wallet the key quorum is a signer on. pay-cloud uses it for hosted
//! connector wallets, where the user owns the wallet and grants pay's key
//! as an additional signer; the CLI can use it for an app's server wallets.
//!
//! The signer is `solana-keychain`'s `PrivySigner` (`privy` feature): it
//! pins the address at init, calls `POST /v1/wallets/{id}/rpc` with
//! `signTransaction` so wallet policies with transaction conditions apply,
//! signs the request with the authorization key, and verifies the returned
//! ed25519 signature.

use pay_kit::solana_keychain::TransactionSigner;
use pay_kit::solana_keychain::privy::{
    PrivyAuthorizationContext, PrivyAuthorizationRequestExpiry, PrivySigner, PrivySignerConfig,
};
use serde::Deserialize;

use crate::backend::{Approval, Custody, SigningBackend};
use crate::remote::{CredentialField, Credentials, RemoteProvider, RemoteWallet};
use crate::{Error, Result};

const API_BASE: &str = "https://api.privy.io/v1";
/// Same override solana-keychain's integration tests honour.
const API_BASE_ENV: &str = "PRIVY_API_BASE_URL";

pub const APP_ID_FIELD: &str = "app_id";
pub const APP_SECRET_FIELD: &str = "app_secret";
pub const AUTHORIZATION_KEY_FIELD: &str = "authorization_key";

pub fn api_base() -> String {
    std::env::var(API_BASE_ENV)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| API_BASE.to_string())
}

/// The Privy remote backend.
pub struct Privy;

static FIELDS: &[CredentialField] = &[
    CredentialField {
        key: APP_ID_FIELD,
        label: "Privy app ID",
        secret: false,
    },
    CredentialField {
        key: APP_SECRET_FIELD,
        label: "Privy app secret",
        secret: true,
    },
    CredentialField {
        key: AUTHORIZATION_KEY_FIELD,
        label: "Privy authorization private key (wallet-auth:…)",
        secret: true,
    },
];

impl SigningBackend for Privy {
    fn id(&self) -> &'static str {
        "privy"
    }
    fn display_name(&self) -> &'static str {
        "Privy wallet"
    }
    fn description(&self) -> &'static str {
        "remote signing, the key stays in Privy's enclave"
    }
    fn custody(&self) -> Custody {
        Custody::Remote
    }
    /// Privy wallets export only to their owner through Privy's own UI.
    fn is_exportable(&self) -> bool {
        false
    }
    fn signs_raw_messages(&self) -> bool {
        true
    }
    fn approval(&self) -> Approval {
        Approval::ProviderPolicy
    }
    fn is_available(&self) -> bool {
        true
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
}

fn required<'a>(credentials: &'a Credentials, key: &str, what: &str) -> Result<&'a String> {
    credentials
        .get(key)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| Error::Config(format!("Missing the Privy {what}.")))
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(!api_base().starts_with("http://"))
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| Error::Config(format!("Failed to build HTTP client: {e}")))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Config(format!("Failed to create runtime: {e}")))
}

impl RemoteProvider for Privy {
    fn credential_fields(&self) -> &'static [CredentialField] {
        FIELDS
    }

    fn credentials_hint(&self) -> &'static str {
        "Connect a Privy app (dashboard.privy.io → App settings for the id and secret; \
         Wallet infrastructure → Authorization keys for the signing key)."
    }

    fn validate_credential(&self, key: &str, value: &str) -> Result<()> {
        if key == AUTHORIZATION_KEY_FIELD
            && !(value.starts_with("wallet-auth:")
                || value.starts_with("wallet-api:")
                || value.contains("-----BEGIN"))
        {
            return Err(Error::Config(
                "The Privy authorization key is the private key from the dashboard, \
                 starting with `wallet-auth:`."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// `GET /v1/wallets?chain_type=solana`: the app's wallets. Needs only
    /// the app id and secret.
    fn discover(&self, credentials: &Credentials) -> Result<Vec<RemoteWallet>> {
        let app_id = required(credentials, APP_ID_FIELD, "app ID")?;
        let app_secret = required(credentials, APP_SECRET_FIELD, "app secret")?;
        let client = http_client()?;
        let page: WalletsPage = runtime()?.block_on(async {
            let response = client
                .get(format!(
                    "{}/wallets?chain_type=solana&limit=100",
                    api_base()
                ))
                .basic_auth(app_id, Some(app_secret))
                .header("privy-app-id", app_id)
                .send()
                .await
                .map_err(|e| Error::Config(format!("Could not reach Privy: {e}")))?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Config(match status.as_u16() {
                    401 | 403 => "Privy rejected those app credentials (HTTP 401/403). Check \
                                  the app ID and app secret in dashboard.privy.io → App settings."
                        .to_string(),
                    code => format!("Privy API error {code} listing wallets."),
                }));
            }
            response
                .json::<WalletsPage>()
                .await
                .map_err(|e| Error::Config(format!("Failed to parse the Privy wallet list: {e}")))
        })?;
        Ok(solana_wallets(page.data))
    }

    fn no_wallets_hint(&self) -> &'static str {
        "This Privy app has no Solana wallet yet.\n\
         Create one at https://dashboard.privy.io → Wallets → New wallet (Solana), \
         with your authorization key as a signer, then run this command again."
    }

    fn connect(
        &self,
        credentials: &Credentials,
        wallet_id: &str,
    ) -> Result<Box<dyn TransactionSigner>> {
        let app_id = required(credentials, APP_ID_FIELD, "app ID")?;
        let app_secret = required(credentials, APP_SECRET_FIELD, "app secret")?;
        let authorization_key =
            required(credentials, AUTHORIZATION_KEY_FIELD, "authorization key")?;
        let mut signer = PrivySigner::from_config(PrivySignerConfig {
            app_id: app_id.clone(),
            app_secret: app_secret.clone(),
            wallet_id: wallet_id.to_string(),
            api_base_url: Some(api_base()),
            http_client_config: None,
            authorization_context: Some(
                PrivyAuthorizationContext {
                    authorization_private_keys: vec![authorization_key.clone()],
                    ..Default::default()
                }
                .into(),
            ),
            authorization_request_expiry: PrivyAuthorizationRequestExpiry::Default,
        })
        .map_err(|e| Error::Config(format!("Invalid Privy credentials for `{wallet_id}`: {e}")))?;

        // Same pattern as Openfort: the payment client paths are synchronous,
        // so a throwaway current-thread runtime covers the init round-trip.
        runtime()?.block_on(signer.init()).map_err(|e| {
            Error::Config(format!(
                "Could not reach the Privy wallet `{wallet_id}`: {e}.\n\
                 Check the app ID, app secret and wallet ID, and that the wallet is on Solana."
            ))
        })?;
        Ok(Box::new(signer))
    }
}

#[derive(Deserialize)]
struct WalletsPage {
    #[serde(default)]
    data: Vec<WalletRecord>,
}

#[derive(Deserialize)]
struct WalletRecord {
    id: String,
    address: String,
    chain_type: Option<String>,
}

fn solana_wallets(records: Vec<WalletRecord>) -> Vec<RemoteWallet> {
    records
        .into_iter()
        .filter(|w| w.chain_type.as_deref() == Some("solana"))
        .map(|w| RemoteWallet {
            id: w.id,
            address: w.address,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_operator_credentials() {
        let keys: Vec<&str> = Privy.credential_fields().iter().map(|f| f.key).collect();
        assert_eq!(keys, ["app_id", "app_secret", "authorization_key"]);
        assert!(!FIELDS[0].secret && FIELDS[1].secret && FIELDS[2].secret);
        assert!(
            Privy
                .validate_credential("authorization_key", "wallet-auth:MIGH…")
                .is_ok()
        );
        assert!(
            Privy
                .validate_credential("authorization_key", "abc")
                .is_err()
        );
        assert!(Privy.validate_credential("app_id", "anything").is_ok());
    }

    #[test]
    fn only_solana_wallets_are_offered() {
        let page: WalletsPage = serde_json::from_str(
            r#"{"data":[
                {"id":"w1","address":"CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z","chain_type":"solana"},
                {"id":"w2","address":"0xabc","chain_type":"ethereum"}
            ],"next_cursor":null}"#,
        )
        .unwrap();
        let wallets = solana_wallets(page.data);
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].id, "w1");
    }

    #[test]
    fn connect_needs_every_credential() {
        let mut creds = Credentials::new();
        creds.insert("app_id".into(), "app".into());
        let err = match Privy.connect(&creds, "w1") {
            Ok(_) => panic!("connect without a secret must fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("app secret"), "{err}");
    }
}
