//! Privy as the connector's identity and wallet.
//!
//! A browser-only MCP user has no CLI and no keychain. With Privy the
//! consent page signs them in (email, passkey, social), Privy holds the
//! wallet, and pay-connect keeps nothing secret per user:
//!
//! 1. The page obtains a Privy access token and posts it with Approve.
//! 2. pay-connect verifies the token offline (ES256, the app's verification
//!    key from the dashboard) and takes the user's DID as identity.
//! 3. It finds the user's Solana wallet, or creates one owned by the user
//!    with pay's key quorum as an additional signer and the configured
//!    policy, and binds a tenant to it.
//! 4. Tool calls sign through pay-core's `privy` provider with the app's
//!    three operator credentials; Privy's policy and pay's spend policy
//!    both apply.
//!
//! Everything pay-connect needs is in the environment: `PRIVY_APP_ID`,
//! `PRIVY_APP_SECRET`, `PRIVY_AUTHORIZATION_PRIVATE_KEY` (`wallet-auth:…`),
//! `PRIVY_SIGNER_ID` (that key's quorum id), optionally `PRIVY_POLICY_ID`,
//! `PRIVY_API_BASE_URL`, and one of `PRIVY_VERIFICATION_KEY` (a single PEM,
//! `\n` escapes accepted) or `PRIVY_JWKS_URL` (default: the app's JWKS at
//! auth.privy.io; it can hold several keys during a rotation). The JWKS is
//! refetched every few minutes and replaced wholesale, so a key Privy
//! retires stops verifying; keys due for a refetch that fails stop verifying
//! too, and a `kid` the current set does not name never verifies.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::tenants::{LoginSession, TenantRecord};

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
/// How old a fetched JWKS may get before a sign-in refetches it.
const JWKS_REFRESH_AFTER: Duration = Duration::from_secs(5 * 60);
/// Least time between refetches, so unknown `kid`s cannot hammer Privy.
const JWKS_RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long a revoked session is remembered: past the lifetime of any
/// access token issued for it.
const REVOKED_SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// Most revoked sessions remembered at once.
const MAX_REVOKED_SESSIONS: usize = 4096;

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
    /// Policy attached to wallets pay-connect creates.
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
    /// The Privy session (`sid`) the token belongs to, and when the token
    /// was issued.
    pub session: LoginSession,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    sid: Option<String>,
    /// When the token was issued; a token without it cannot be placed
    /// against what this process has seen.
    iat: u64,
}

/// A user's Solana wallet at Privy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wallet {
    pub id: String,
    pub address: String,
    /// Whether pay's key quorum is a signer, under the configured policy
    /// when there is one, so tool calls can sign.
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

/// The verification keys in force and when they were last confirmed.
struct KeySet {
    keys: Keys,
    /// When the JWKS was fetched; `None` for a configured PEM.
    fetched_at: Option<Instant>,
}

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
    fn knows(&self, kid: Option<&str>) -> bool {
        kid.is_some_and(|kid| self.0.iter().any(|(k, _)| k.as_deref() == Some(kid)))
    }

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

/// The Privy client pay-connect holds: app credentials and the verifier.
pub struct Privy {
    cfg: Config,
    http: reqwest::Client,
    keys: RwLock<Arc<KeySet>>,
    /// Serializes JWKS refetches and remembers the last attempt.
    refresh: tokio::sync::Mutex<Option<Instant>>,
    refresh_after: Duration,
    retry_after: Duration,
    /// The last refetch failed: a set due for one is no longer confirmed.
    refresh_failed: std::sync::atomic::AtomicBool,
    validation: jsonwebtoken::Validation,
    /// Sessions signed out here, by `sid`, with when.
    revoked: std::sync::Mutex<std::collections::HashMap<String, Instant>>,
    /// Unix second before which no token is accepted. Sign-outs are
    /// remembered in this process only, so a token issued before it started
    /// may belong to a session signed out on a previous run: it is refused,
    /// and its holder signs in again.
    not_before: std::sync::atomic::AtomicU64,
}

impl Privy {
    /// A client verifying with `cfg.verification_key`, which must be set.
    pub fn new(cfg: Config) -> Result<Self, ConfigError> {
        let pem = cfg
            .verification_key
            .clone()
            .ok_or(ConfigError::Missing(VERIFICATION_KEY_ENV))?;
        Self::with_keys(cfg, Keys::from_pem(&pem)?, None)
    }

    /// A client verifying with the keys in a JWKS document fetched now, and
    /// refetching it from `cfg.jwks_url` as it ages.
    pub fn with_jwks(cfg: Config, jwks: &str) -> Result<Self, ConfigError> {
        let keys = Keys::from_jwks(&cfg.jwks_url, jwks)?;
        Self::with_keys(cfg, keys, Some(Instant::now()))
    }

    /// The client for this configuration: the PEM when there is one,
    /// otherwise the app's JWKS fetched now.
    pub async fn connect(cfg: Config) -> Result<Self, ConfigError> {
        if cfg.verification_key.is_some() {
            return Self::new(cfg);
        }
        let body = fetch_jwks(&reqwest::Client::new(), &cfg.jwks_url).await?;
        Self::with_jwks(cfg, &body)
    }

    fn with_keys(
        cfg: Config,
        keys: Keys,
        fetched_at: Option<Instant>,
    ) -> Result<Self, ConfigError> {
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
            keys: RwLock::new(Arc::new(KeySet { keys, fetched_at })),
            refresh: tokio::sync::Mutex::new(None),
            refresh_after: JWKS_REFRESH_AFTER,
            retry_after: JWKS_RETRY_AFTER,
            refresh_failed: std::sync::atomic::AtomicBool::new(false),
            validation,
            revoked: Default::default(),
            not_before: std::sync::atomic::AtomicU64::new(unix_now()),
        })
    }

    pub fn key_count(&self) -> usize {
        self.key_set().keys.0.len()
    }

    fn key_set(&self) -> Arc<KeySet> {
        self.keys.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Verify a token against keys refreshed from the JWKS when they have
    /// aged or do not name the token's `kid`.
    pub async fn authenticate(&self, access_token: &str) -> Result<Identity, String> {
        let kid = jsonwebtoken::decode_header(access_token.trim())
            .map_err(|e| e.to_string())?
            .kid;
        let set = self.key_set();
        if let Some(fetched_at) = set.fetched_at
            && (fetched_at.elapsed() >= self.refresh_after || !set.keys.knows(kid.as_deref()))
        {
            self.refresh_jwks().await;
        }
        self.verify(access_token)
    }

    async fn refresh_jwks(&self) {
        let mut last_attempt = self.refresh.lock().await;
        if last_attempt.is_some_and(|at| at.elapsed() < self.retry_after) {
            return;
        }
        let set = self.key_set();
        *last_attempt = Some(Instant::now());
        let keys = match fetch_jwks(&self.http, &self.cfg.jwks_url).await {
            Ok(body) => Keys::from_jwks(&self.cfg.jwks_url, &body),
            Err(error) => Err(error),
        };
        match keys {
            Ok(keys) => {
                *self.keys.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(KeySet {
                    keys,
                    fetched_at: Some(Instant::now()),
                });
                self.refresh_failed
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            Err(error) => {
                self.refresh_failed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::error!(
                    %error,
                    age_secs = set.fetched_at.map(|at| at.elapsed().as_secs()),
                    "Privy JWKS refresh failed; sign-ins fail until it succeeds"
                )
            }
        }
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
    /// issuer, audience (this app), expiry. The subject is the user. Only
    /// [`Privy::authenticate`] calls this, after refreshing what is due.
    fn verify(&self, access_token: &str) -> Result<Identity, String> {
        let token = access_token.trim();
        let kid = jsonwebtoken::decode_header(token)
            .map_err(|e| e.to_string())?
            .kid;
        let set = self.key_set();
        if let Some(fetched_at) = set.fetched_at {
            // A set due for a refetch is trusted only while refetches work;
            // an outage must not keep a key Privy has retired alive.
            if fetched_at.elapsed() >= self.refresh_after
                && self
                    .refresh_failed
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err("the Privy verification keys could not be refreshed".to_string());
            }
            if kid.is_some() && !set.keys.knows(kid.as_deref()) {
                return Err("the token's signing key is not in the app's JWKS".to_string());
            }
        }
        let mut last = String::from("no verification key");
        let mut data = None;
        for key in set.keys.candidates(kid.as_deref()) {
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
        let session = data
            .claims
            .sid
            .filter(|sid| !sid.trim().is_empty())
            .ok_or_else(|| "the token carries no Privy session".to_string())?;
        let session = LoginSession {
            id: session,
            issued_at: data.claims.iat,
        };
        if !self.session_is_live(&session) {
            return Err("the Privy session was signed out".to_string());
        }
        Ok(Identity {
            user_id: data.claims.sub,
            session,
        })
    }

    /// Whether credentials may still be derived from `session`: it was not
    /// signed out here, and was proven by a token issued since this process
    /// began remembering sign-outs.
    pub fn session_is_live(&self, session: &LoginSession) -> bool {
        session.issued_at >= self.not_before.load(std::sync::atomic::Ordering::SeqCst)
            && !self.is_revoked(&session.id)
    }

    /// Refuse further sign-ins with `session` for as long as its access
    /// tokens can live.
    pub fn revoke_session(&self, session: &str) {
        let now = Instant::now();
        let mut revoked = self.revoked.lock().unwrap_or_else(|p| p.into_inner());
        revoked.retain(|_, at| now.saturating_duration_since(*at) <= REVOKED_SESSION_TTL);
        if revoked.len() >= MAX_REVOKED_SESSIONS
            && let Some(oldest) = revoked
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(sid, _)| sid.clone())
        {
            // Forgetting a sign-out must not revive its tokens: first refuse
            // every token issued so far, which signs everyone out, then
            // make room.
            self.not_before
                .fetch_max(unix_now() + 1, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!("too many Privy sign-outs to remember; every earlier token is refused");
            revoked.remove(&oldest);
        }
        revoked.insert(session.to_string(), now);
    }

    fn is_revoked(&self, session: &str) -> bool {
        self.revoked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session)
            .is_some_and(|at| at.elapsed() <= REVOKED_SESSION_TTL)
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
        if user.id.as_deref().is_some_and(|id| id != user_id) {
            return Err(PrivyError::Protocol(
                "user: the response is for another user".to_string(),
            ));
        }
        let Some(linked) = embedded_solana_wallet(&user.linked_accounts) else {
            return Ok(None);
        };
        let id = linked.id.clone().expect("filtered on id");
        let wallet: WalletResponse = Self::read(
            self.get(&format!("/wallets/{}", urlencode(&id)))
                .send()
                .await?,
            "wallet",
        )
        .await?;
        if wallet.id != id
            || wallet
                .chain_type
                .as_deref()
                .is_some_and(|chain| chain != "solana")
            || linked
                .address
                .as_deref()
                .is_some_and(|address| address != wallet.address)
        {
            return Err(PrivyError::Protocol(
                "wallet: the response does not match the user's linked wallet".to_string(),
            ));
        }
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
        let signer = wallet
            .additional_signers
            .iter()
            .find(|s| s.id() == self.cfg.signer_id);
        let is_owner = wallet.owner_id.as_deref() == Some(self.cfg.signer_id.as_str());
        // With a policy configured, it must govern the wallet, and pay's
        // signer must not carry overrides that replace it.
        let policy_bound = match &self.cfg.policy_id {
            None => true,
            Some(policy) => {
                wallet.policy_ids.iter().any(|p| p == policy)
                    && signer.is_none_or(|s| {
                        let overrides = s.override_policy_ids();
                        overrides.is_empty() || overrides == [policy.as_str()]
                    })
            }
        };
        let pay_can_sign = (signer.is_some() || is_owner) && policy_bound;
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
        TenantRecord {
            subject: subject.to_string(),
            account_name: crate::tenants::CONNECTOR_ACCOUNT.to_string(),
            provider: PROVIDER_ID.to_string(),
            wallet_id: wallet.id.clone(),
            pubkey: wallet.address.clone(),
            credentials,
            policy: crate::tenants::DEFAULT_POLICY,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn fetch_jwks(http: &reqwest::Client, url: &str) -> Result<String, ConfigError> {
    let bad = |e: reqwest::Error| ConfigError::Jwks {
        url: url.to_string(),
        why: e.to_string(),
    };
    http.get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(bad)?
        .text()
        .await
        .map_err(bad)
}

/// A wallet's stablecoin outflow today, read from the chain: the record of
/// what its hosted signatures actually spent.
pub struct OnChainSpendHistory {
    rpc_url: String,
    http: reqwest::Client,
}

/// Signature pages read per address; an address past this many transactions
/// today is reported as unknown rather than undercounted.
const HISTORY_PAGES: usize = 4;
const HISTORY_PAGE: usize = 1_000;

impl OnChainSpendHistory {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
            http: reqwest::Client::new(),
        }
    }

    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let response: serde_json::Value = self
            .http
            .post(&self.rpc_url)
            .timeout(Duration::from_secs(10))
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("{method}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("{method}: {e}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("{method}: {error}"));
        }
        Ok(response["result"].clone())
    }
}

impl OnChainSpendHistory {
    /// The wallet's stablecoin token accounts, under either token program.
    /// A delegate's transfer names only the token account, never the owner,
    /// so the owner's own signature index misses it.
    async fn stablecoin_accounts(&self, wallet: &str) -> Result<Vec<String>, String> {
        use pay_kit::mpp::protocol::solana::{is_known_stablecoin_mint, programs};
        let mut accounts = Vec::new();
        for program in [programs::TOKEN_PROGRAM, programs::TOKEN_2022_PROGRAM] {
            let found = self
                .rpc(
                    "getTokenAccountsByOwner",
                    json!([wallet, { "programId": program },
                           { "encoding": "jsonParsed", "commitment": "confirmed" }]),
                )
                .await?;
            let found = found["value"]
                .as_array()
                .ok_or("getTokenAccountsByOwner: not a list")?;
            for entry in found {
                let pubkey = entry["pubkey"]
                    .as_str()
                    .ok_or("getTokenAccountsByOwner: account without pubkey")?;
                let mint = entry["account"]["data"]["parsed"]["info"]["mint"]
                    .as_str()
                    .ok_or_else(|| format!("getTokenAccountsByOwner: {pubkey} has no mint"))?;
                if is_known_stablecoin_mint(mint) {
                    accounts.push(pubkey.to_string());
                }
            }
        }
        Ok(accounts)
    }

    /// Signatures of `address`'s successful transactions since `day_start`.
    async fn signatures_today(&self, address: &str, day_start: i64) -> Result<Vec<String>, String> {
        let mut before: Option<String> = None;
        let mut signatures = Vec::new();
        for _ in 0..HISTORY_PAGES {
            let mut config = json!({ "limit": HISTORY_PAGE, "commitment": "confirmed" });
            if let Some(before) = &before {
                config["before"] = json!(before);
            }
            let page = self
                .rpc("getSignaturesForAddress", json!([address, config]))
                .await?;
            let page = page
                .as_array()
                .ok_or("getSignaturesForAddress: not a list")?;
            for entry in page {
                // An unknown block time counts as today.
                if entry["blockTime"].as_i64().is_some_and(|t| t < day_start) {
                    return Ok(signatures);
                }
                let signature = entry["signature"].as_str().ok_or("signature missing")?;
                before = Some(signature.to_string());
                if entry["err"].is_null() {
                    signatures.push(signature.to_string());
                }
            }
            if page.len() < HISTORY_PAGE {
                return Ok(signatures);
            }
        }
        Err(format!(
            "{address} has too many transactions today to count"
        ))
    }
}

#[async_trait::async_trait]
impl crate::tenants::SpendHistory for OnChainSpendHistory {
    async fn spent_today(&self, wallet: &str) -> Result<u64, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let day_start = (now / 86_400 * 86_400) as i64;
        // The owner's own transactions, and every transaction touching one of
        // its stablecoin accounts, each counted once.
        let mut signatures = std::collections::BTreeSet::new();
        signatures.extend(self.signatures_today(wallet, day_start).await?);
        for account in self.stablecoin_accounts(wallet).await? {
            signatures.extend(self.signatures_today(&account, day_start).await?);
        }
        let mut raw = 0_u64;
        for signature in signatures {
            let tx = self
                .rpc(
                    "getTransaction",
                    json!([signature, { "encoding": "json", "commitment": "confirmed",
                                        "maxSupportedTransactionVersion": 0 }]),
                )
                .await?;
            let tx: ConfirmedTransaction = serde_json::from_value(tx)
                .map_err(|e| format!("getTransaction {signature}: history unavailable: {e}"))?;
            let outflow = tx
                .stablecoin_outflow(wallet)
                .map_err(|e| format!("getTransaction {signature}: {e}"))?;
            raw = raw.saturating_add(outflow);
        }
        Ok(raw.div_ceil(100))
    }
}

/// A `getTransaction` result with its status metadata. A null result or null
/// meta does not deserialize, so missing history can't count as zero spend.
#[derive(Deserialize)]
struct ConfirmedTransaction {
    meta: TransactionMeta,
    transaction: TransactionBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionMeta {
    err: Option<serde_json::Value>,
    pre_token_balances: Vec<serde_json::Value>,
    post_token_balances: Vec<serde_json::Value>,
    #[serde(default)]
    inner_instructions: Option<Vec<InnerInstructions>>,
    /// Keys a version-0 transaction loaded from lookup tables.
    #[serde(default)]
    loaded_addresses: Option<LoadedAddresses>,
}

#[derive(Deserialize)]
struct TransactionBody {
    message: Message,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    account_keys: Vec<String>,
    instructions: Vec<Instruction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instruction {
    program_id_index: usize,
    /// Base58, as `json` encoding returns it.
    data: String,
}

#[derive(Deserialize)]
struct InnerInstructions {
    instructions: Vec<Instruction>,
}

#[derive(Deserialize)]
struct LoadedAddresses {
    #[serde(default)]
    writable: Vec<String>,
    #[serde(default)]
    readonly: Vec<String>,
}

/// Token-2022 instruction tags whose amounts live in encrypted balances:
/// `ConfidentialTransferExtension`, `ConfidentialTransferFeeExtension`,
/// `ConfidentialMintBurnExtension`. Public balance deltas do not show them.
const CONFIDENTIAL_TOKEN_INSTRUCTIONS: [u8; 3] = [27, 37, 42];

impl ConfirmedTransaction {
    /// Raw six-decimal stablecoin units `owner`'s token accounts lost. An
    /// error when the transaction moved confidential balances, whose amounts
    /// cannot be read from it: unknown spend is not zero spend.
    fn stablecoin_outflow(&self, owner: &str) -> Result<u64, String> {
        if self.meta.err.is_some() {
            return Ok(0);
        }
        if self.moves_confidential_balances()? {
            return Err("confidential token history is not auditable".to_string());
        }
        let balances = |entries: &[serde_json::Value]| -> BTreeMap<u64, u64> {
            entries
                .iter()
                .filter(|b| b["owner"].as_str() == Some(owner))
                .filter(|b| {
                    b["mint"]
                        .as_str()
                        .is_some_and(pay_kit::mpp::protocol::solana::is_known_stablecoin_mint)
                })
                .filter_map(|b| {
                    Some((
                        b["accountIndex"].as_u64()?,
                        b["uiTokenAmount"]["amount"].as_str()?.parse().ok()?,
                    ))
                })
                .collect()
        };
        let pre = balances(&self.meta.pre_token_balances);
        let post = balances(&self.meta.post_token_balances);
        Ok(pre
            .iter()
            .map(|(index, before)| before.saturating_sub(post.get(index).copied().unwrap_or(0)))
            .fold(0, u64::saturating_add))
    }

    /// Whether any instruction, top-level or inner, is a Token-2022
    /// confidential-balance instruction. Unreadable instructions count as one.
    fn moves_confidential_balances(&self) -> Result<bool, String> {
        let token_2022 = pay_kit::mpp::protocol::solana::programs::TOKEN_2022_PROGRAM;
        let loaded = self.meta.loaded_addresses.as_ref();
        let keys: Vec<&str> = self
            .transaction
            .message
            .account_keys
            .iter()
            .chain(loaded.into_iter().flat_map(|l| l.writable.iter()))
            .chain(loaded.into_iter().flat_map(|l| l.readonly.iter()))
            .map(String::as_str)
            .collect();
        let inner = self
            .meta
            .inner_instructions
            .iter()
            .flatten()
            .flat_map(|set| set.instructions.iter());
        for instruction in self.transaction.message.instructions.iter().chain(inner) {
            let program = keys
                .get(instruction.program_id_index)
                .ok_or("an instruction names a program outside the transaction")?;
            if *program != token_2022 {
                continue;
            }
            let data = bs58::decode(&instruction.data)
                .into_vec()
                .map_err(|e| format!("unreadable Token-2022 instruction: {e}"))?;
            if data
                .first()
                .is_none_or(|tag| CONFIDENTIAL_TOKEN_INSTRUCTIONS.contains(tag))
            {
                return Ok(true);
            }
        }
        Ok(false)
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
    id: Option<String>,
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
    address: Option<String>,
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
#[cfg(test)]
fn embedded_solana_wallet_id(accounts: &[LinkedAccount]) -> Option<String> {
    embedded_solana_wallet(accounts).and_then(|a| a.id.clone())
}

fn embedded_solana_wallet(accounts: &[LinkedAccount]) -> Option<&LinkedAccount> {
    accounts
        .iter()
        .filter(|a| a.kind == "wallet" && a.chain_type.as_deref() == Some("solana"))
        .filter(|a| {
            a.connector_type.as_deref() == Some("embedded")
                || a.wallet_client_type.as_deref() == Some("privy")
        })
        .filter(|a| a.id.is_some())
        .min_by_key(|a| a.wallet_index.unwrap_or(i64::MAX))
}

#[derive(Deserialize)]
struct WalletResponse {
    id: String,
    address: String,
    #[serde(default)]
    chain_type: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
    #[serde(default)]
    additional_signers: Vec<Signer>,
    #[serde(default)]
    policy_ids: Vec<String>,
}

/// Privy has listed signers both as bare ids and as `{ "signer_id": … }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Signer {
    Id(String),
    Object {
        signer_id: String,
        #[serde(default)]
        override_policy_ids: Vec<String>,
    },
}

impl Signer {
    fn id(&self) -> &str {
        match self {
            Signer::Id(id) => id,
            Signer::Object { signer_id, .. } => signer_id,
        }
    }

    fn override_policy_ids(&self) -> Vec<&str> {
        match self {
            Signer::Id(_) => Vec::new(),
            Signer::Object {
                override_policy_ids,
                ..
            } => override_policy_ids.iter().map(String::as_str).collect(),
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
                "additional_signers": signers.iter().map(|s| json!({ "signer_id": s })).collect::<Vec<_>>(),
                "policy_ids": ["pol_1"]
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
        assert_eq!(identity.session.id, "sess_1");

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
    fn tokens_need_a_live_privy_session() {
        let app = FakeApp::new("http://unused");
        let privy = Privy::new(app.cfg.clone()).unwrap();
        let sessionless = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": ISSUER, "exp": unix() + 60
        }));
        assert!(privy.verify(&sessionless).is_err());
        let blank = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": " ", "exp": unix() + 60
        }));
        assert!(privy.verify(&blank).is_err());

        let token = app.token_for(USER);
        assert!(privy.verify(&token).is_ok());
        privy.revoke_session("sess_1");
        assert!(
            privy.verify(&token).is_err(),
            "a signed-out session is refused"
        );
        let other = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": "sess_2", "exp": unix() + 60,
            "iat": unix()
        }));
        assert!(
            privy.verify(&other).is_ok(),
            "other sessions are unaffected"
        );
    }

    /// Sign-outs live in this process only; a token issued before it began
    /// may belong to a session signed out on a previous run, so it is
    /// refused rather than trusted.
    #[test]
    fn tokens_from_before_this_process_are_refused() {
        let app = FakeApp::new("http://unused");
        let privy = Privy::new(app.cfg.clone()).unwrap();
        let token = |iat: u64| {
            app.token(json!({
                "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": "sess_1",
                "exp": unix() + 600, "iat": iat
            }))
        };
        assert!(privy.verify(&token(unix())).is_ok());
        assert!(privy.verify(&token(unix() - 120)).is_err());
        let undated = app.token(json!({
            "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": "sess_1", "exp": unix() + 600
        }));
        assert!(privy.verify(&undated).is_err(), "no iat, no placement");
        let cookie_session = |issued_at| LoginSession {
            id: "sess_1".to_string(),
            issued_at,
        };
        assert!(privy.session_is_live(&cookie_session(unix())));
        assert!(!privy.session_is_live(&cookie_session(unix() - 120)));
    }

    /// A full sign-out list never forgets a sign-out in a way that lets its
    /// token back in: it refuses every token issued so far instead.
    #[test]
    fn a_full_sign_out_list_refuses_rather_than_forgets() {
        let app = FakeApp::new("http://unused");
        let privy = Privy::new(app.cfg.clone()).unwrap();
        let token = |sid: &str, iat: u64| {
            app.token(json!({
                "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": sid,
                "exp": unix() + 600, "iat": iat
            }))
        };
        let victim = token("sess_victim", unix());
        let bystander = token("sess_bystander", unix());
        privy.revoke_session("sess_victim");
        for i in 1..MAX_REVOKED_SESSIONS {
            privy.revoke_session(&format!("sess_{i}"));
        }
        assert!(privy.verify(&victim).is_err());
        assert!(privy.verify(&bystander).is_ok(), "not full yet");

        privy.revoke_session("sess_one_too_many");
        assert!(privy.verify(&victim).is_err(), "the evicted sign-out holds");
        assert!(privy.verify(&bystander).is_err(), "everyone signs in again");
        assert!(privy.verify(&token("sess_new", unix() + 2)).is_ok());
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
                &json!({ "sub": USER, "aud": "app_test", "iss": ISSUER, "sid": "sess_1",
                         "exp": unix() + 60, "iat": unix() }),
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

    /// A JWKS endpoint whose document the test can replace or break.
    pub(crate) async fn mock_jwks(doc: Arc<Mutex<Option<String>>>) -> String {
        async fn serve(
            State(doc): State<Arc<Mutex<Option<String>>>>,
        ) -> Result<String, axum::http::StatusCode> {
            doc.lock()
                .unwrap()
                .clone()
                .ok_or(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        }
        let app = Router::new()
            .route("/jwks.json", get(serve))
            .with_state(doc);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/jwks.json")
    }

    pub(crate) fn jwk(kid: &str, key: &SigningKey) -> serde_json::Value {
        use base64::Engine;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let point = key.verifying_key().to_encoded_point(false);
        json!({ "kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
                "x": b64(point.x().unwrap()), "y": b64(point.y().unwrap()) })
    }

    pub(crate) fn signed_by(key: &SigningKey, kid: &str, user: &str) -> String {
        let pem = key.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
        let encoding = jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes()).unwrap();
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(kid.to_string());
        let claims = json!({ "sub": user, "aud": "app_test", "iss": ISSUER, "sid": "sess_1",
                             "exp": unix() + 600, "iat": unix() });
        jsonwebtoken::encode(&header, &claims, &encoding).unwrap()
    }

    #[tokio::test]
    async fn retired_jwks_keys_stop_verifying_after_refresh() {
        let retired = SigningKey::random(&mut rand::rngs::OsRng);
        let current = SigningKey::random(&mut rand::rngs::OsRng);
        let doc = Arc::new(Mutex::new(Some(
            json!({ "keys": [jwk("kid_retired", &retired), jwk("kid_current", &current)] })
                .to_string(),
        )));
        let mut cfg = FakeApp::new("http://unused").cfg;
        cfg.verification_key = None;
        cfg.jwks_url = mock_jwks(doc.clone()).await;
        let mut privy = Privy::connect(cfg).await.unwrap();
        let victim = "did:privy:victim";
        assert!(
            privy
                .authenticate(&signed_by(&retired, "kid_retired", victim))
                .await
                .is_ok()
        );

        // Privy retires the key; the next sign-in after the refresh window
        // fetches the new set and the retired key is gone.
        *doc.lock().unwrap() = Some(json!({ "keys": [jwk("kid_current", &current)] }).to_string());
        privy.refresh_after = Duration::ZERO;
        privy.retry_after = Duration::ZERO;
        let forged = signed_by(&retired, "kid_retired", victim);
        assert!(privy.authenticate(&forged).await.is_err());
        assert_eq!(privy.key_count(), 1);
        assert_eq!(
            privy
                .authenticate(&signed_by(&current, "kid_current", victim))
                .await
                .unwrap()
                .user_id,
            victim
        );
    }

    #[tokio::test]
    async fn a_new_kid_is_fetched_without_waiting_for_the_refresh_window() {
        let first = SigningKey::random(&mut rand::rngs::OsRng);
        let rotated = SigningKey::random(&mut rand::rngs::OsRng);
        let doc = Arc::new(Mutex::new(Some(
            json!({ "keys": [jwk("kid_1", &first)] }).to_string(),
        )));
        let mut cfg = FakeApp::new("http://unused").cfg;
        cfg.verification_key = None;
        cfg.jwks_url = mock_jwks(doc.clone()).await;
        let privy = Privy::connect(cfg).await.unwrap();
        *doc.lock().unwrap() = Some(json!({ "keys": [jwk("kid_2", &rotated)] }).to_string());
        assert!(
            privy
                .authenticate(&signed_by(&rotated, "kid_2", USER))
                .await
                .is_ok()
        );
        assert!(
            privy
                .authenticate(&signed_by(&first, "kid_1", USER))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn keys_that_cannot_be_refreshed_expire() {
        let key = SigningKey::random(&mut rand::rngs::OsRng);
        let doc = Arc::new(Mutex::new(Some(
            json!({ "keys": [jwk("kid_1", &key)] }).to_string(),
        )));
        let mut cfg = FakeApp::new("http://unused").cfg;
        cfg.verification_key = None;
        cfg.jwks_url = mock_jwks(doc.clone()).await;
        let mut privy = Privy::connect(cfg).await.unwrap();
        let token = signed_by(&key, "kid_1", USER);
        *doc.lock().unwrap() = None;
        // Keys not yet due for a refetch need no JWKS.
        assert!(privy.authenticate(&token).await.is_ok());
        // Once due, a failed refetch fails closed: the key may be retired.
        privy.refresh_after = Duration::ZERO;
        privy.retry_after = Duration::ZERO;
        assert!(privy.authenticate(&token).await.is_err(), "fails closed");
        // Also while refetches are throttled after the failure.
        privy.retry_after = Duration::from_secs(60);
        assert!(privy.authenticate(&token).await.is_err(), "still closed");
        // A working refetch restores sign-ins.
        privy.retry_after = Duration::ZERO;
        *doc.lock().unwrap() = Some(json!({ "keys": [jwk("kid_1", &key)] }).to_string());
        assert!(privy.authenticate(&token).await.is_ok());
        // A configured PEM never ages.
        let app = FakeApp::new("http://unused");
        let mut pem = Privy::new(app.cfg.clone()).unwrap();
        pem.refresh_after = Duration::ZERO;
        assert!(pem.authenticate(&app.token_for(USER)).await.is_ok());
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

    async fn serve_json(user: serde_json::Value, wallet: serde_json::Value) -> String {
        let app = Router::new()
            .route("/users/{id}", get(move || async move { Json(user) }))
            .route("/wallets/{id}", get(move || async move { Json(wallet) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn provider_responses_must_match_the_requested_user_and_wallet() {
        let linked = |address: &str| {
            json!({ "id": USER, "linked_accounts": [
                { "type": "wallet", "id": "w_1", "address": address, "chain_type": "solana",
                  "connector_type": "embedded", "wallet_index": 0 }
            ]})
        };
        let wallet = |id: &str, chain: &str| {
            json!({ "id": id, "address": ADDRESS, "chain_type": chain,
                    "additional_signers": [{ "signer_id": SIGNER }] })
        };
        let ok = serve_json(linked(ADDRESS), wallet("w_1", "solana")).await;
        let privy = Privy::new(FakeApp::new(&ok).cfg).unwrap();
        assert_eq!(privy.solana_wallet(USER).await.unwrap().unwrap().id, "w_1");

        for (user, wallet) in [
            (linked(ADDRESS), wallet("w_other", "solana")),
            (linked(ADDRESS), wallet("w_1", "ethereum")),
            (
                linked("SomeOtherAddress1111111111111111111111111111"),
                wallet("w_1", "solana"),
            ),
            (
                json!({ "id": "did:privy:other", "linked_accounts": linked(ADDRESS)["linked_accounts"] }),
                wallet("w_1", "solana"),
            ),
        ] {
            let base = serve_json(user, wallet).await;
            let privy = Privy::new(FakeApp::new(&base).cfg).unwrap();
            assert!(matches!(
                privy.solana_wallet(USER).await,
                Err(PrivyError::Protocol(_))
            ));
        }
    }

    /// A transaction body with nothing in it but balances.
    static NO_INSTRUCTIONS: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(
        || json!({ "message": { "accountKeys": [], "instructions": [] } }),
    );

    /// A confidential debit changes no public balance, so a transaction
    /// carrying one is unknown spend, never zero; other Token-2022
    /// instructions are read as usual.
    #[test]
    fn confidential_token_instructions_make_history_unauditable() {
        use pay_kit::mpp::protocol::solana::programs;
        let tx = |instructions: serde_json::Value, inner: serde_json::Value| {
            serde_json::from_value::<ConfirmedTransaction>(json!({
                "transaction": { "message": {
                    "accountKeys": [ADDRESS, programs::TOKEN_2022_PROGRAM],
                    "instructions": instructions } },
                "meta": { "err": null, "preTokenBalances": [], "postTokenBalances": [],
                          "innerInstructions": inner,
                          "loadedAddresses": { "writable": [], "readonly": [programs::TOKEN_PROGRAM] } }
            }))
            .unwrap()
        };
        let ix = |program: usize, data: &[u8]| json!({ "programIdIndex": program, "accounts": [], "data": bs58::encode(data).into_string() });
        // TransferChecked under Token-2022, Transfer under the token program
        // (tag 27 is not confidential there).
        let public = tx(json!([ix(1, &[12, 0]), ix(2, &[27])]), json!([]));
        assert_eq!(public.stablecoin_outflow(ADDRESS), Ok(0));
        for tag in CONFIDENTIAL_TOKEN_INSTRUCTIONS {
            let top = tx(json!([ix(1, &[tag, 7])]), json!([]));
            assert!(top.stablecoin_outflow(ADDRESS).is_err(), "tag {tag}");
            let inner = tx(
                json!([]),
                json!([{ "index": 0, "instructions": [ix(1, &[tag, 7])] }]),
            );
            assert!(
                inner.stablecoin_outflow(ADDRESS).is_err(),
                "inner tag {tag}"
            );
        }
        let unreadable = tx(
            json!([{ "programIdIndex": 1, "accounts": [], "data": "0OIl" }]),
            json!([]),
        );
        assert!(unreadable.stablecoin_outflow(ADDRESS).is_err());
    }

    #[test]
    fn outflow_counts_only_the_owners_stablecoin_debits() {
        use pay_kit::mpp::protocol::solana::mints;
        let usdc = mints::USDC_MAINNET;
        let balance = |index: u64, owner: &str, mint: &str, amount: u64| {
            json!({ "accountIndex": index, "owner": owner, "mint": mint,
                    "uiTokenAmount": { "amount": amount.to_string(), "decimals": 6 } })
        };
        let tx = json!({ "transaction": NO_INSTRUCTIONS.clone(), "meta": { "err": null,
            "preTokenBalances": [
                balance(1, ADDRESS, usdc, 5_000_000),
                balance(2, "someone", usdc, 0),
                balance(3, ADDRESS, "So11111111111111111111111111111111111111112", 9_000),
            ],
            "postTokenBalances": [
                balance(1, ADDRESS, usdc, 3_750_000),
                balance(2, "someone", usdc, 1_250_000),
                balance(3, ADDRESS, "So11111111111111111111111111111111111111112", 0),
            ],
        }});
        let outflow = |tx: &serde_json::Value, owner: &str| {
            serde_json::from_value::<ConfirmedTransaction>(tx.clone())
                .unwrap()
                .stablecoin_outflow(owner)
                .unwrap()
        };
        assert_eq!(outflow(&tx, ADDRESS), 1_250_000);
        assert_eq!(outflow(&tx, "someone"), 0, "an inflow is not spend");
        let mut failed = tx.clone();
        failed["meta"]["err"] = json!({ "InstructionError": [0, "Custom"] });
        assert_eq!(outflow(&failed, ADDRESS), 0);
        // A closed account (absent afterwards) spent everything it held.
        let mut closed = tx;
        closed["meta"]["postTokenBalances"] = json!([]);
        assert_eq!(outflow(&closed, ADDRESS), 5_000_000);
        // Unavailable history is not zero spend.
        for missing in [json!(null), json!({ "meta": null })] {
            assert!(serde_json::from_value::<ConfirmedTransaction>(missing).is_err());
        }
    }

    #[tokio::test]
    async fn on_chain_history_sums_todays_outflows() {
        use crate::tenants::SpendHistory;
        use pay_kit::mpp::protocol::solana::mints;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day_start = now / 86_400 * 86_400;
        let debit = |amount: u64| {
            json!({ "transaction": NO_INSTRUCTIONS.clone(), "meta": { "err": null,
                "preTokenBalances": [{ "accountIndex": 1, "owner": ADDRESS, "mint": mints::USDC_MAINNET,
                                       "uiTokenAmount": { "amount": "10000000" } }],
                "postTokenBalances": [{ "accountIndex": 1, "owner": ADDRESS, "mint": mints::USDC_MAINNET,
                                        "uiTokenAmount": { "amount": (10_000_000 - amount).to_string() } }] } })
        };
        let rpc = move |Json(request): Json<serde_json::Value>| async move {
            use pay_kit::mpp::protocol::solana::programs;
            let account = |pubkey: &str, mint: &str| json!({ "pubkey": pubkey, "account": { "data": { "parsed": { "info": { "mint": mint } } } } });
            let result = match request["method"].as_str().unwrap() {
                "getTokenAccountsByOwner" => {
                    assert_eq!(request["params"][0], ADDRESS);
                    match request["params"][1]["programId"].as_str().unwrap() {
                        programs::TOKEN_PROGRAM => json!({ "value": [
                            account("usdc_account", mints::USDC_MAINNET),
                            account("meme_account", "MemeMint1111111111111111111111111111111111"),
                        ] }),
                        programs::TOKEN_2022_PROGRAM => json!({ "value": [] }),
                        other => panic!("{other}"),
                    }
                }
                "getSignaturesForAddress" => match request["params"][0].as_str().unwrap() {
                    ADDRESS => json!([
                        { "signature": "today_1", "blockTime": now, "err": null },
                        { "signature": "today_failed", "blockTime": now, "err": { "x": 1 } },
                        { "signature": "today_2", "blockTime": day_start, "err": null },
                        { "signature": "yesterday", "blockTime": day_start - 1, "err": null },
                    ]),
                    // A delegate's transfer names the token account, not the
                    // owner; the owner's own transfers name both.
                    "usdc_account" => json!([
                        { "signature": "delegated", "blockTime": now, "err": null },
                        { "signature": "today_1", "blockTime": now, "err": null },
                        { "signature": "yesterday", "blockTime": day_start - 1, "err": null },
                    ]),
                    other => panic!("{other} must not be read"),
                },
                "getTransaction" => match request["params"][0].as_str().unwrap() {
                    "today_1" => debit(1_000_001),
                    "today_2" => debit(500_000),
                    "delegated" => debit(2_000_000),
                    other => panic!("{other} must not be read"),
                },
                other => panic!("{other}"),
            };
            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
        };
        let app = Router::new().route("/", post(rpc));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let history = OnChainSpendHistory::new(format!("http://{addr}/"));
        // 3.500001 USDC, the delegated transfer and each transaction counted
        // once, rounds up to 35,001 minor units.
        assert_eq!(history.spent_today(ADDRESS).await.unwrap(), 35_001);
        assert!(
            OnChainSpendHistory::new("http://127.0.0.1:1/")
                .spent_today(ADDRESS)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unavailable_transaction_history_leaves_the_wallet_unseeded() {
        use crate::tenants::{SpendHistory, TenantRegistry};
        use pay_kit::mpp::protocol::solana::mints;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let spend_rpc = |transaction: serde_json::Value| async move {
            let rpc = move |Json(request): Json<serde_json::Value>| {
                let transaction = transaction.clone();
                async move {
                    let result = match request["method"].as_str().unwrap() {
                        "getTokenAccountsByOwner" => json!({ "value": [] }),
                        "getSignaturesForAddress" => {
                            json!([{ "signature": "today", "blockTime": now, "err": null }])
                        }
                        "getTransaction" => transaction,
                        other => panic!("{other}"),
                    };
                    Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
                }
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, Router::new().route("/", post(rpc)))
                    .await
                    .unwrap();
            });
            OnChainSpendHistory::new(format!("http://{addr}/"))
        };
        let mut credentials = pay_core::remote::Credentials::new();
        credentials.insert("secret_key".to_string(), "sk_test_1".to_string());
        let tenant = TenantRecord {
            subject: "sub_1".to_string(),
            account_name: "grok".to_string(),
            provider: PROVIDER_ID.to_string(),
            wallet_id: "w_1".to_string(),
            pubkey: ADDRESS.to_string(),
            credentials,
            policy: pay_mcp::policy::SpendPolicy::dollars(1.0, 5.0),
        };
        let registry = TenantRegistry::new();
        for missing in [json!(null), json!({ "meta": null })] {
            let history = spend_rpc(missing.clone()).await;
            let error = history.spent_today(ADDRESS).await.unwrap_err();
            assert!(error.contains("history unavailable"), "{missing}: {error}");
            assert!(registry.seed_spend(&tenant, &history).await.is_err());
        }
        // A failed seed left the wallet unseeded: complete history still lands.
        let full = spend_rpc(
            json!({ "transaction": NO_INSTRUCTIONS.clone(), "meta": { "err": null,
            "preTokenBalances": [{ "accountIndex": 1, "owner": ADDRESS, "mint": mints::USDC_MAINNET,
                                   "uiTokenAmount": { "amount": "5000000" } }],
            "postTokenBalances": [] } }),
        )
        .await;
        assert_eq!(full.spent_today(ADDRESS).await.unwrap(), 50_000);
        registry.seed_spend(&tenant, &full).await.unwrap();
        assert!(registry.reserve_cli_transaction(&tenant, 1).is_err());
    }

    #[tokio::test]
    async fn pay_signs_only_under_the_configured_policy() {
        let user = json!({ "id": USER, "linked_accounts": [
            { "type": "wallet", "id": "w_1", "address": ADDRESS, "chain_type": "solana",
              "connector_type": "embedded", "wallet_index": 0 }
        ]});
        let wallet = |policies: serde_json::Value, signer: serde_json::Value| {
            json!({ "id": "w_1", "address": ADDRESS, "chain_type": "solana",
                    "additional_signers": [signer], "policy_ids": policies })
        };
        let plain = json!({ "signer_id": SIGNER });
        for (policies, signer, can_sign) in [
            (json!(["pol_1"]), plain.clone(), true),
            (json!(["pol_other", "pol_1"]), plain.clone(), true),
            (
                json!(["pol_1"]),
                json!({ "signer_id": SIGNER, "override_policy_ids": ["pol_1"] }),
                true,
            ),
            (json!(["pol_1"]), json!(SIGNER), true),
            (json!([]), plain.clone(), false),
            (json!(["pol_other"]), plain.clone(), false),
            (
                json!(["pol_1"]),
                json!({ "signer_id": SIGNER, "override_policy_ids": ["pol_broad"] }),
                false,
            ),
            (
                json!(["pol_1"]),
                json!({ "signer_id": SIGNER, "override_policy_ids": ["pol_1", "pol_broad"] }),
                false,
            ),
        ] {
            let base = serve_json(user.clone(), wallet(policies.clone(), signer.clone())).await;
            let privy = Privy::new(FakeApp::new(&base).cfg).unwrap();
            assert_eq!(
                privy
                    .solana_wallet(USER)
                    .await
                    .unwrap()
                    .unwrap()
                    .pay_can_sign,
                can_sign,
                "{policies} {signer}"
            );
        }
        // Without a configured policy only signer membership matters.
        let base = serve_json(user, wallet(json!([]), plain)).await;
        let mut cfg = FakeApp::new(&base).cfg;
        cfg.policy_id = None;
        assert!(
            Privy::new(cfg)
                .unwrap()
                .solana_wallet(USER)
                .await
                .unwrap()
                .unwrap()
                .pay_can_sign
        );
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
