//! Privy as the connector's identity and wallet.
//!
//! A browser-only MCP user has no CLI and no keychain. With Privy the
//! consent page signs them in (email, passkey, social), Privy holds the
//! wallet, and pay-cloud keeps nothing secret per user:
//!
//! 1. The page obtains a Privy access token and posts it with Approve.
//! 2. pay-cloud verifies the token offline (ES256, the app's verification
//!    key from the dashboard) and takes the user's DID as identity.
//! 3. It finds the user's Solana wallet, or creates one owned by the user
//!    with pay's key quorum as an additional signer and the configured
//!    policy, and binds a tenant to it.
//! 4. Tool calls sign through pay-core's `privy` provider with the app's
//!    three operator credentials; Privy's policy and pay's spend policy
//!    both apply.
//!
//! Everything pay-cloud needs is in the environment: `PRIVY_APP_ID`,
//! `PRIVY_APP_SECRET`, `PRIVY_AUTHORIZATION_PRIVATE_KEY` (`wallet-auth:…`),
//! `PRIVY_SIGNER_ID` (that key's quorum id), optionally `PRIVY_POLICY_ID`,
//! `PRIVY_API_BASE_URL`, and one of `PRIVY_VERIFICATION_KEY` (a single PEM,
//! `\n` escapes accepted) or `PRIVY_JWKS_URL` (default: the app's JWKS at
//! auth.privy.io, fetched once at startup; it can hold several keys during
//! a rotation).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::drivers::ProvisionedWallet;
use crate::tenants::TenantRecord;

pub const PROVIDER_ID: &str = "privy";
pub const APP_ID_ENV: &str = "PRIVY_APP_ID";
pub const APP_SECRET_ENV: &str = "PRIVY_APP_SECRET";
pub const VERIFICATION_KEY_ENV: &str = "PRIVY_VERIFICATION_KEY";
pub const JWKS_URL_ENV: &str = "PRIVY_JWKS_URL";
pub const AUTHORIZATION_KEY_ENV: &str = "PRIVY_AUTHORIZATION_PRIVATE_KEY";
pub const SIGNER_ID_ENV: &str = "PRIVY_SIGNER_ID";
pub const POLICY_ID_ENV: &str = "PRIVY_POLICY_ID";
pub const API_BASE_ENV: &str = "PRIVY_API_BASE_URL";
const DEFAULT_API_BASE: &str = "https://api.privy.io/v1";
/// `iss` of every Privy access token.
const ISSUER: &str = "privy.io";

#[derive(Clone)]
pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    /// SPKI PEM of the app's ES256 verification key; `None` to fetch the
    /// app's JWKS from `jwks_url` instead.
    pub verification_key: Option<String>,
    pub jwks_url: String,
    /// `wallet-auth:` P-256 private key; signs every wallet RPC.
    pub authorization_key: String,
    /// Key quorum id of that key, as Privy lists it on wallets.
    pub signer_id: String,
    /// Policy attached to wallets pay-cloud creates.
    pub policy_id: Option<String>,
    pub api_base: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required when {APP_ID_ENV} is set")]
    Missing(&'static str),
    #[error("{VERIFICATION_KEY_ENV} is not a PEM public key: {0}")]
    BadVerificationKey(String),
    #[error("could not load the app's JWKS from {url}: {why}")]
    Jwks { url: String, why: String },
    #[error("could not build an HTTP client: {0}")]
    Http(String),
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    /// `None` when `PRIVY_APP_ID` is unset: the consent page offers no Privy
    /// login. Every other field is required once it is set.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        let Some(app_id) = non_empty(std::env::var(APP_ID_ENV).ok()) else {
            return Ok(None);
        };
        let need = |name: &'static str| {
            non_empty(std::env::var(name).ok()).ok_or(ConfigError::Missing(name))
        };
        let jwks_url = non_empty(std::env::var(JWKS_URL_ENV).ok())
            .unwrap_or_else(|| default_jwks_url(&app_id));
        Ok(Some(Self {
            app_id,
            app_secret: need(APP_SECRET_ENV)?,
            verification_key: non_empty(std::env::var(VERIFICATION_KEY_ENV).ok())
                .map(|k| k.replace("\\n", "\n")),
            jwks_url,
            authorization_key: need(AUTHORIZATION_KEY_ENV)?,
            signer_id: need(SIGNER_ID_ENV)?,
            policy_id: non_empty(std::env::var(POLICY_ID_ENV).ok()),
            api_base: non_empty(std::env::var(API_BASE_ENV).ok())
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        }))
    }
}

/// Where Privy publishes an app's token-signing keys.
pub fn default_jwks_url(app_id: &str) -> String {
    format!("https://auth.privy.io/api/v1/apps/{app_id}/jwks.json")
}

/// Who a verified access token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `did:privy:…`, stable for the user in this app.
    pub user_id: String,
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    sid: Option<String>,
}

/// A user's Solana wallet at Privy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wallet {
    pub id: String,
    pub address: String,
    /// Whether pay's key quorum is a signer, so tool calls can sign.
    pub pay_can_sign: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PrivyError {
    #[error("Privy is unreachable: {0}")]
    Unreachable(#[from] reqwest::Error),
    #[error("Privy rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("Privy returned an unexpected response: {0}")]
    Protocol(String),
}

/// The app's ES256 public keys, by `kid` when the JWKS names one.
struct Keys(Vec<(Option<String>, jsonwebtoken::DecodingKey)>);

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

impl Keys {
    fn from_pem(pem: &str) -> Result<Self, ConfigError> {
        let key = jsonwebtoken::DecodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|e| ConfigError::BadVerificationKey(e.to_string()))?;
        Ok(Self(vec![(None, key)]))
    }

    fn from_jwks(url: &str, body: &str) -> Result<Self, ConfigError> {
        let bad = |why: String| ConfigError::Jwks {
            url: url.to_string(),
            why,
        };
        let jwks: Jwks = serde_json::from_str(body).map_err(|e| bad(e.to_string()))?;
        let mut keys = Vec::new();
        for jwk in jwks.keys {
            if jwk.kty != "EC" || jwk.crv.as_deref() != Some("P-256") {
                continue;
            }
            let (Some(x), Some(y)) = (jwk.x.as_deref(), jwk.y.as_deref()) else {
                continue;
            };
            let key = jsonwebtoken::DecodingKey::from_ec_components(x, y)
                .map_err(|e| bad(format!("key {:?}: {e}", jwk.kid)))?;
            keys.push((jwk.kid, key));
        }
        if keys.is_empty() {
            return Err(bad("no P-256 keys in the document".to_string()));
        }
        Ok(Self(keys))
    }

    /// The keys to try for a token: the one its `kid` names, else all.
    fn candidates(&self, kid: Option<&str>) -> Vec<&jsonwebtoken::DecodingKey> {
        let named: Vec<_> = self
            .0
            .iter()
            .filter(|(k, _)| kid.is_some() && k.as_deref() == kid)
            .map(|(_, key)| key)
            .collect();
        if named.is_empty() {
            self.0.iter().map(|(_, key)| key).collect()
        } else {
            named
        }
    }
}

/// The Privy client pay-cloud holds: app credentials and the verifier.
pub struct Privy {
    cfg: Config,
    http: reqwest::Client,
    keys: Keys,
    validation: jsonwebtoken::Validation,
}

impl Privy {
    /// A client verifying with `cfg.verification_key`, which must be set.
    pub fn new(cfg: Config) -> Result<Self, ConfigError> {
        let pem = cfg
            .verification_key
            .clone()
            .ok_or(ConfigError::Missing(VERIFICATION_KEY_ENV))?;
        Self::with_keys(cfg, Keys::from_pem(&pem)?)
    }

    /// A client verifying with the keys in a JWKS document.
    pub fn with_jwks(cfg: Config, jwks: &str) -> Result<Self, ConfigError> {
        let keys = Keys::from_jwks(&cfg.jwks_url, jwks)?;
        Self::with_keys(cfg, keys)
    }

    /// The client for this configuration: the PEM when there is one,
    /// otherwise the app's JWKS fetched now.
    pub async fn connect(cfg: Config) -> Result<Self, ConfigError> {
        if cfg.verification_key.is_some() {
            return Self::new(cfg);
        }
        let url = cfg.jwks_url.clone();
        let body = reqwest::get(&url)
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| ConfigError::Jwks {
                url: url.clone(),
                why: e.to_string(),
            })?
            .text()
            .await
            .map_err(|e| ConfigError::Jwks {
                url: url.clone(),
                why: e.to_string(),
            })?;
        Self::with_jwks(cfg, &body)
    }

    fn with_keys(cfg: Config, keys: Keys) -> Result<Self, ConfigError> {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[cfg.app_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "sub", "aud", "iss"]);
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| ConfigError::Http(e.to_string()))?;
        Ok(Self {
            cfg,
            http,
            keys,
            validation,
        })
    }

    pub fn key_count(&self) -> usize {
        self.keys.0.len()
    }

    pub fn app_id(&self) -> &str {
        &self.cfg.app_id
    }

    pub fn signer_id(&self) -> &str {
        &self.cfg.signer_id
    }

    pub fn policy_id(&self) -> Option<&str> {
        self.cfg.policy_id.as_deref()
    }

    /// Verify an access token the browser obtained from Privy: signature,
    /// issuer, audience (this app), expiry. The subject is the user.
    pub fn verify(&self, access_token: &str) -> Result<Identity, String> {
        let token = access_token.trim();
        let kid = jsonwebtoken::decode_header(token)
            .map_err(|e| e.to_string())?
            .kid;
        let mut last = String::from("no verification key");
        let mut data = None;
        for key in self.keys.candidates(kid.as_deref()) {
            match jsonwebtoken::decode::<Claims>(token, key, &self.validation) {
                Ok(d) => {
                    data = Some(d);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        let data = data.ok_or(last)?;
        if !data.claims.sub.starts_with("did:privy:") {
            return Err("the token subject is not a Privy user".to_string());
        }
        Ok(Identity {
            user_id: data.claims.sub,
            session_id: data.claims.sid,
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.cfg.api_base))
            .basic_auth(&self.cfg.app_id, Some(&self.cfg.app_secret))
            .header("privy-app-id", &self.cfg.app_id)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.cfg.api_base))
            .basic_auth(&self.cfg.app_id, Some(&self.cfg.app_secret))
            .header("privy-app-id", &self.cfg.app_id)
    }

    async fn read<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
        what: &str,
    ) -> Result<T, PrivyError> {
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .and_then(|e| e.as_str().or_else(|| e.get("message")?.as_str()))
                        .map(str::to_string)
                })
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(PrivyError::Rejected {
                status: status.as_u16(),
                message,
            });
        }
        serde_json::from_str(&text).map_err(|e| PrivyError::Protocol(format!("{what}: {e}")))
    }

    /// The user's embedded Solana wallet, if they have one, with whether
    /// pay may sign for it.
    pub async fn solana_wallet(&self, user_id: &str) -> Result<Option<Wallet>, PrivyError> {
        let path = format!("/users/{}", urlencode(user_id));
        let user: UserResponse = Self::read(self.get(&path).send().await?, "user").await?;
        let Some(id) = embedded_solana_wallet_id(&user.linked_accounts) else {
            return Ok(None);
        };
        let wallet: WalletResponse = Self::read(
            self.get(&format!("/wallets/{}", urlencode(&id)))
                .send()
                .await?,
            "wallet",
        )
        .await?;
        Ok(Some(self.wallet_from(wallet)))
    }

    /// Create a Solana wallet owned by the user, with pay's key quorum as a
    /// signer and the configured policy. Needs only the app credentials.
    pub async fn create_wallet(&self, user_id: &str) -> Result<Wallet, PrivyError> {
        let mut body = json!({
            "chain_type": "solana",
            "owner": { "user_id": user_id },
            "additional_signers": [{ "signer_id": self.cfg.signer_id }],
        });
        if let Some(policy) = &self.cfg.policy_id {
            body["policy_ids"] = json!([policy]);
        }
        let wallet: WalletResponse =
            Self::read(self.post("/wallets").json(&body).send().await?, "wallet").await?;
        Ok(self.wallet_from(wallet))
    }

    /// The wallet a tenant for `user_id` signs with: the existing one, or a
    /// new one.
    pub async fn wallet_for(&self, user_id: &str) -> Result<Wallet, PrivyError> {
        match self.solana_wallet(user_id).await? {
            Some(wallet) => Ok(wallet),
            None => self.create_wallet(user_id).await,
        }
    }

    fn wallet_from(&self, wallet: WalletResponse) -> Wallet {
        let pay_can_sign = wallet
            .additional_signers
            .iter()
            .any(|s| s.id() == self.cfg.signer_id)
            || wallet.owner_id.as_deref() == Some(self.cfg.signer_id.as_str());
        Wallet {
            id: wallet.id,
            address: wallet.address,
            pay_can_sign,
        }
    }

    /// The tenant record for a user's wallet: pay-core's `privy` provider
    /// with the app's operator credentials, the same for every tenant.
    pub fn tenant_record(&self, subject: &str, wallet: &Wallet) -> TenantRecord {
        let mut credentials = BTreeMap::new();
        credentials.insert(
            pay_core::remote::privy::APP_ID_FIELD.to_string(),
            self.cfg.app_id.clone(),
        );
        credentials.insert(
            pay_core::remote::privy::APP_SECRET_FIELD.to_string(),
            self.cfg.app_secret.clone(),
        );
        credentials.insert(
            pay_core::remote::privy::AUTHORIZATION_KEY_FIELD.to_string(),
            self.cfg.authorization_key.clone(),
        );
        TenantRecord::from_wallet(
            subject,
            &ProvisionedWallet {
                provider: PROVIDER_ID,
                credentials,
                wallet_id: wallet.id.clone(),
                address: wallet.address.clone(),
                project_id: None,
            },
        )
    }
}

/// The tenant subject for a Privy user.
pub fn subject_for_user(user_id: &str) -> String {
    crate::tenants::subject_for(PROVIDER_ID, user_id)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Deserialize)]
struct UserResponse {
    #[serde(default)]
    linked_accounts: Vec<LinkedAccount>,
}

#[derive(Deserialize)]
struct LinkedAccount {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    chain_type: Option<String>,
    #[serde(default)]
    wallet_client_type: Option<String>,
    #[serde(default)]
    connector_type: Option<String>,
    #[serde(default)]
    wallet_index: Option<i64>,
}

/// The first embedded Solana wallet on a user, by index.
fn embedded_solana_wallet_id(accounts: &[LinkedAccount]) -> Option<String> {
    accounts
        .iter()
        .filter(|a| a.kind == "wallet" && a.chain_type.as_deref() == Some("solana"))
        .filter(|a| {
            a.connector_type.as_deref() == Some("embedded")
                || a.wallet_client_type.as_deref() == Some("privy")
        })
        .filter(|a| a.id.is_some())
        .min_by_key(|a| a.wallet_index.unwrap_or(i64::MAX))
        .and_then(|a| a.id.clone())
}

#[derive(Deserialize)]
struct WalletResponse {
    id: String,
    address: String,
    #[serde(default)]
    owner_id: Option<String>,
    #[serde(default)]
    additional_signers: Vec<Signer>,
}

/// Privy has listed signers both as bare ids and as `{ "signer_id": … }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Signer {
    Id(String),
    Object { signer_id: String },
}

impl Signer {
    fn id(&self) -> &str {
        match self {
            Signer::Id(id) => id,
            Signer::Object { signer_id } => signer_id,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey};
    use std::sync::{Arc, Mutex};

    pub(crate) const ADDRESS: &str = "Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS";
    pub(crate) const USER: &str = "did:privy:cm3np4u9j001rc8b73seqmqqk";
    pub(crate) const SIGNER: &str = "kq_pay_quorum";

    /// A Privy app for tests: its own ES256 key pair, so the test can mint
    /// access tokens the verifier accepts.
    pub(crate) struct FakeApp {
        signing: SigningKey,
        pub cfg: Config,
    }

    impl FakeApp {
        pub(crate) fn new(api_base: &str) -> Self {
            let signing = SigningKey::random(&mut rand::rngs::OsRng);
            let public = signing
                .verifying_key()
                .to_public_key_pem(p256::pkcs8::LineEnding::LF)
                .unwrap();
            Self {
                signing,
                cfg: Config {
                    app_id: "app_test".to_string(),
                    app_secret: "secret_test".to_string(),
                    verification_key: Some(public),
                    jwks_url: "https://auth.test/jwks.json".to_string(),
                    authorization_key: "wallet-auth:AAAA".to_string(),
                    signer_id: SIGNER.to_string(),
                    policy_id: Some("pol_1".to_string()),
                    api_base: api_base.to_string(),
                },
            }
        }

        pub(crate) fn token_for(&self, user: &str) -> String {
            self.token(json!({
                "sub": user, "aud": "app_test", "iss": ISSUER, "sid": "sess_1",
                "exp": unix() + 3600, "iat": unix()
            }))
        }

        pub(crate) fn token(&self, claims: serde_json::Value) -> String {
            let pem = self
                .signing
                .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
                .unwrap();
            let key = jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes()).unwrap();
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
            header.kid = Some("kid_current".to_string());
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        }

        /// The app's JWKS as Privy serves it: a retired key first, then
        /// the one that signs.
        pub(crate) fn jwks(&self) -> String {
            use base64::Engine;
            let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            let point = self.signing.verifying_key().to_encoded_point(false);
            let retired = SigningKey::random(&mut rand::rngs::OsRng);
            let old = retired.verifying_key().to_encoded_point(false);
            json!({ "keys": [
                { "kty": "EC", "crv": "P-256", "kid": "kid_old", "use": "sig", "alg": "ES256",
                  "x": b64(old.x().unwrap()), "y": b64(old.y().unwrap()) },
                { "kty": "EC", "crv": "P-256", "kid": "kid_current", "use": "sig", "alg": "ES256",
                  "x": b64(point.x().unwrap()), "y": b64(point.y().unwrap()) },
                { "kty": "RSA", "kid": "ignored", "n": "AQAB", "e": "AQAB" }
            ]})
            .to_string()
        }
    }

    fn unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// What the mock Privy holds: wallets by user, and the create calls it saw.
    #[derive(Default)]
    pub(crate) struct MockPrivy {
        /// user id → (wallet id, signers)
        pub wallets: Mutex<BTreeMap<String, (String, Vec<String>)>>,
        pub created: Mutex<Vec<serde_json::Value>>,
    }

    /// Mock Privy users and wallets API on an ephemeral port.
    pub(crate) async fn mock_privy(state: Arc<MockPrivy>) -> String {
        async fn user(
            State(s): State<Arc<MockPrivy>>,
            Path(id): Path<String>,
        ) -> Json<serde_json::Value> {
            let wallets = s.wallets.lock().unwrap();
            let accounts = match wallets.get(&id) {
                Some((wallet_id, _)) => json!([
                    { "type": "email", "address": "a@b.c" },
                    { "type": "wallet", "id": wallet_id, "address": ADDRESS, "chain_type": "solana",
                      "wallet_client_type": "privy", "connector_type": "embedded", "wallet_index": 0 }
                ]),
                None => json!([{ "type": "email", "address": "a@b.c" }]),
            };
            Json(json!({ "id": id, "linked_accounts": accounts }))
        }
        async fn wallet(
            State(s): State<Arc<MockPrivy>>,
            Path(id): Path<String>,
        ) -> Json<serde_json::Value> {
            let wallets = s.wallets.lock().unwrap();
            let (user, (_, signers)) = wallets.iter().find(|(_, (w, _))| *w == id).unwrap();
            Json(json!({
                "id": id, "address": ADDRESS, "chain_type": "solana", "owner_id": format!("kq_{user}"),
                "additional_signers": signers.iter().map(|s| json!({ "signer_id": s })).collect::<Vec<_>>()
            }))
        }
        async fn create(
            State(s): State<Arc<MockPrivy>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            let user = body["owner"]["user_id"].as_str().unwrap().to_string();
            let signers: Vec<String> = body["additional_signers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["signer_id"].as_str().unwrap().to_string())
                .collect();
            s.created.lock().unwrap().push(body.clone());
            let id = format!("w_{}", s.created.lock().unwrap().len());
            s.wallets
                .lock()
                .unwrap()
                .insert(user.clone(), (id.clone(), signers.clone()));
            Json(json!({
                "id": id, "address": ADDRESS, "chain_type": "solana", "owner_id": format!("kq_{user}"),
                "additional_signers": signers.iter().map(|s| json!({ "signer_id": s })).collect::<Vec<_>>(),
                "policy_ids": body["policy_ids"]
            }))
        }
        let app = Router::new()
            .route("/users/{id}", get(user))
            .route("/wallets/{id}", get(wallet))
            .route("/wallets", post(create))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[test]
    fn tokens_are_checked_for_signature_audience_issuer_and_expiry() {
        let app = FakeApp::new("http://unused");
        let privy = Privy::new(app.cfg.clone()).unwrap();
        let identity = privy.verify(&app.token_for(USER)).unwrap();
        assert_eq!(identity.user_id, USER);
        assert_eq!(identity.session_id.as_deref(), Some("sess_1"));

        let other_app = FakeApp::new("http://unused");
        assert!(
            privy.verify(&other_app.token_for(USER)).is_err(),
            "other key"
        );
        let wrong_aud = app.token(json!({
            "sub": USER, "aud": "app_other", "iss": ISSUER, "exp": unix() + 60
        }));
        assert!(privy.verify(&wrong_aud).is_err());
        let wrong_iss = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": "evil.io", "exp": unix() + 60
        }));
        assert!(privy.verify(&wrong_iss).is_err());
        let expired = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": ISSUER, "exp": unix() - 120
        }));
        assert!(privy.verify(&expired).is_err());
        let not_a_user = app.token(json!({
            "sub": "acct_1", "aud": "app_test", "iss": ISSUER, "exp": unix() + 60
        }));
        assert!(privy.verify(&not_a_user).is_err());
        assert!(privy.verify("garbage").is_err());
    }

    #[test]
    fn jwks_keys_verify_by_kid_and_by_trial() {
        let app = FakeApp::new("http://unused");
        let mut cfg = app.cfg.clone();
        cfg.verification_key = None;
        let privy = Privy::with_jwks(cfg.clone(), &app.jwks()).unwrap();
        assert_eq!(privy.key_count(), 2, "the RSA entry is skipped");
        assert_eq!(privy.verify(&app.token_for(USER)).unwrap().user_id, USER);
        // A token with an unknown kid is tried against every key.
        let unlabeled = {
            let pem = app
                .signing
                .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
                .unwrap();
            let key = jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes()).unwrap();
            jsonwebtoken::encode(
                &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256),
                &json!({ "sub": USER, "aud": "app_test", "iss": ISSUER, "exp": unix() + 60 }),
                &key,
            )
            .unwrap()
        };
        assert!(privy.verify(&unlabeled).is_ok());
        assert!(
            privy
                .verify(&FakeApp::new("http://x").token_for(USER))
                .is_err()
        );
        assert!(matches!(
            Privy::with_jwks(cfg.clone(), r#"{"keys":[]}"#),
            Err(ConfigError::Jwks { .. })
        ));
        assert!(Privy::with_jwks(cfg, "nope").is_err());
        assert_eq!(
            default_jwks_url("app_1"),
            "https://auth.privy.io/api/v1/apps/app_1/jwks.json"
        );
    }

    #[test]
    fn config_needs_every_field_once_enabled() {
        // Only shape checks: from_env reads the process environment.
        let app = FakeApp::new("http://x");
        assert!(Privy::new(app.cfg.clone()).is_ok());
        let mut bad = app.cfg.clone();
        bad.verification_key = Some("not a key".to_string());
        assert!(matches!(
            Privy::new(bad),
            Err(ConfigError::BadVerificationKey(_))
        ));
    }

    #[test]
    fn picks_the_first_embedded_solana_wallet() {
        let user: UserResponse = serde_json::from_value(json!({ "linked_accounts": [
            { "type": "wallet", "id": "w_eth", "chain_type": "ethereum", "connector_type": "embedded", "wallet_index": 0 },
            { "type": "wallet", "address": "Ext…", "chain_type": "solana", "connector_type": "injected", "wallet_client_type": "phantom" },
            { "type": "wallet", "id": "w_sol2", "chain_type": "solana", "connector_type": "embedded", "wallet_index": 1 },
            { "type": "wallet", "id": "w_sol1", "chain_type": "solana", "connector_type": "embedded", "wallet_index": 0 }
        ]}))
        .unwrap();
        assert_eq!(
            embedded_solana_wallet_id(&user.linked_accounts).as_deref(),
            Some("w_sol1")
        );
        assert!(embedded_solana_wallet_id(&[]).is_none());
    }

    #[tokio::test]
    async fn finds_creates_and_flags_wallets() {
        let mock = Arc::new(MockPrivy::default());
        let base = mock_privy(mock.clone()).await;
        let app = FakeApp::new(&base);
        let privy = Privy::new(app.cfg.clone()).unwrap();

        assert!(privy.solana_wallet(USER).await.unwrap().is_none());
        let created = privy.wallet_for(USER).await.unwrap();
        assert_eq!(created.address, ADDRESS);
        assert!(created.pay_can_sign);
        // Clone out: `&guard[0]` would keep the lock for the whole test.
        let body = mock.created.lock().unwrap()[0].clone();
        assert_eq!(body["chain_type"], "solana");
        assert_eq!(body["owner"]["user_id"], USER);
        assert_eq!(body["additional_signers"][0]["signer_id"], SIGNER);
        assert_eq!(body["policy_ids"][0], "pol_1");

        // Second time: found, not created again.
        let found = privy.wallet_for(USER).await.unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(mock.created.lock().unwrap().len(), 1);

        // A wallet the user made without pay as a signer is found but
        // flagged: the page must add the signer before approving.
        mock.wallets.lock().unwrap().insert(
            "did:privy:other".to_string(),
            ("w_theirs".to_string(), vec!["kq_someone_else".to_string()]),
        );
        let theirs = privy.wallet_for("did:privy:other").await.unwrap();
        assert!(!theirs.pay_can_sign);

        let record = privy.tenant_record("sub_1", &created);
        assert_eq!(record.provider, "privy");
        assert_eq!(record.wallet_id, created.id);
        assert_eq!(record.pubkey, ADDRESS);
        assert_eq!(record.credentials["app_id"], "app_test");
        assert_eq!(record.credentials["authorization_key"], "wallet-auth:AAAA");
    }
}
