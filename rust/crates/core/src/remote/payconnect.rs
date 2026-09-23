//! A tenant-scoped signer hosted by pay-connect.

use std::str::FromStr;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pay_kit::solana_keychain::transaction_util::TransactionUtil;
use pay_kit::solana_keychain::{
    SignTransactionResult, SignerError, SolanaSigner, TransactionSigner,
};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

use crate::backend::{Approval, Custody, SigningBackend};
use crate::remote::{CredentialField, Credentials, RemoteProvider, RemoteWallet};
use crate::{Error, Result};

pub const API_TOKEN_FIELD: &str = "api_token";
const URL_ENV: &str = "PAY_CONNECT_URL";
const DEFAULT_URL: &str = "https://connect.pay.sh";

pub struct PayConnect;

static FIELDS: &[CredentialField] = &[CredentialField {
    key: API_TOKEN_FIELD,
    label: "pay-connect API token",
    secret: true,
}];

impl SigningBackend for PayConnect {
    fn id(&self) -> &'static str {
        "payconnect"
    }
    fn display_name(&self) -> &'static str {
        "pay.sh Cloud wallet"
    }
    fn description(&self) -> &'static str {
        "remote signing through connect.pay.sh"
    }
    fn custody(&self) -> Custody {
        Custody::Remote
    }
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

fn base_url() -> String {
    std::env::var(URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

fn token(credentials: &Credentials) -> Result<&str> {
    credentials
        .get(API_TOKEN_FIELD)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Config("Missing the pay-connect API token.".to_string()))
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(!base_url().starts_with("http://"))
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|error| Error::Config(format!("Failed to build pay-connect client: {error}")))
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::Config(format!("Failed to create runtime: {error}")))
}

impl RemoteProvider for PayConnect {
    fn credential_fields(&self) -> &'static [CredentialField] {
        FIELDS
    }
    fn credentials_hint(&self) -> &'static str {
        "Run `pay setup` and choose Cloud wallet."
    }
    fn discover(&self, credentials: &Credentials) -> Result<Vec<RemoteWallet>> {
        let token = token(credentials)?.to_string();
        let wallets: Vec<WalletResponse> = runtime()?.block_on(async {
            json_request::<(), _>(client()?, reqwest::Method::GET, "/v1/wallets", &token, None)
                .await
        })?;
        Ok(wallets
            .into_iter()
            .map(|wallet| RemoteWallet {
                id: wallet.id,
                address: wallet.address,
            })
            .collect())
    }
    fn no_wallets_hint(&self) -> &'static str {
        "Run `pay setup` again to restore your cloud wallet."
    }
    fn connect(
        &self,
        credentials: &Credentials,
        wallet_id: &str,
    ) -> Result<Box<dyn TransactionSigner>> {
        let wallets = self.discover(credentials)?;
        let wallet = wallets
            .into_iter()
            .find(|wallet| wallet.id == wallet_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "pay-connect token cannot access wallet `{wallet_id}`."
                ))
            })?;
        let pubkey = Pubkey::from_str(&wallet.address).map_err(|error| {
            Error::Config(format!("pay-connect returned an invalid address: {error}"))
        })?;
        Ok(Box::new(PayConnectSigner {
            client: client()?,
            token: token(credentials)?.to_string(),
            wallet_id: wallet_id.to_string(),
            pubkey,
        }))
    }
}

struct PayConnectSigner {
    client: reqwest::Client,
    token: String,
    wallet_id: String,
    pubkey: Pubkey,
}

#[derive(Deserialize)]
struct WalletResponse {
    id: String,
    address: String,
}

#[derive(Serialize)]
struct SignTransactionRequest {
    transaction_b64: String,
}

#[derive(Deserialize)]
struct SignTransactionResponse {
    transaction_b64: String,
    signature: String,
    complete: bool,
}

#[derive(Serialize)]
struct SignMessageRequest {
    message_b64: String,
}

#[derive(Deserialize)]
struct SignMessageResponse {
    signature: String,
}

#[async_trait]
impl SolanaSigner for PayConnectSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    async fn sign_message(&self, message: &[u8]) -> std::result::Result<Signature, SignerError> {
        let response: SignMessageResponse = json_request(
            self.client.clone(),
            reqwest::Method::POST,
            &format!("/v1/wallets/{}/sign-message", self.wallet_id),
            &self.token,
            Some(&SignMessageRequest {
                message_b64: STANDARD.encode(message),
            }),
        )
        .await
        .map_err(to_signer_error)?;
        let signature = Signature::from_str(&response.signature)
            .map_err(|error| SignerError::SerializationError(error.to_string()))?;
        if !signature.verify(&self.pubkey.to_bytes(), message) {
            return Err(SignerError::SigningFailed(
                "pay-connect returned a signature over different bytes".to_string(),
            ));
        }
        Ok(signature)
    }

    async fn is_available(&self) -> bool {
        json_request::<(), Vec<WalletResponse>>(
            self.client.clone(),
            reqwest::Method::GET,
            "/v1/wallets",
            &self.token,
            None,
        )
        .await
        .is_ok()
    }
}

#[async_trait]
impl TransactionSigner for PayConnectSigner {
    async fn sign_transaction(
        &self,
        tx: &mut VersionedTransaction,
    ) -> std::result::Result<SignTransactionResult, SignerError> {
        let original_message = tx.message.serialize();
        let request = SignTransactionRequest {
            transaction_b64: TransactionUtil::serialize_transaction(tx)?,
        };
        let response: SignTransactionResponse = json_request(
            self.client.clone(),
            reqwest::Method::POST,
            &format!("/v1/wallets/{}/sign-transaction", self.wallet_id),
            &self.token,
            Some(&request),
        )
        .await
        .map_err(to_signer_error)?;
        let bytes = STANDARD
            .decode(&response.transaction_b64)
            .map_err(|error| SignerError::SerializationError(error.to_string()))?;
        let signed: VersionedTransaction = bincode::deserialize(&bytes)
            .map_err(|error| SignerError::SerializationError(error.to_string()))?;
        if signed.message.serialize() != original_message {
            return Err(SignerError::SigningFailed(
                "pay-connect changed the transaction message".to_string(),
            ));
        }
        let signature = Signature::from_str(&response.signature)
            .map_err(|error| SignerError::SerializationError(error.to_string()))?;
        if !signature.verify(&self.pubkey.to_bytes(), &original_message) {
            return Err(SignerError::SigningFailed(
                "pay-connect returned an invalid transaction signature".to_string(),
            ));
        }
        *tx = signed;
        let result = (response.transaction_b64, signature);
        Ok(if response.complete {
            SignTransactionResult::Complete(result)
        } else {
            SignTransactionResult::Partial(result)
        })
    }
}

async fn json_request<B: Serialize + ?Sized, R: for<'de> Deserialize<'de>>(
    client: reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&B>,
) -> Result<R> {
    let mut request = client
        .request(method, format!("{}{path}", base_url()))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| Error::Config(format!("Could not reach pay-connect: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return Err(Error::Config(format!(
            "pay-connect rejected the request ({status}): {detail}"
        )));
    }
    response
        .json()
        .await
        .map_err(|error| Error::Config(format!("Invalid pay-connect response: {error}")))
}

fn to_signer_error(error: Error) -> SignerError {
    SignerError::remote_api(error.to_string())
}
