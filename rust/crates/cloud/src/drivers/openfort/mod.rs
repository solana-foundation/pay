//! Openfort driver: from "Continue with Openfort" to a Solana backend wallet
//! the CLI can sign with, without anyone touching the Openfort dashboard or
//! CLI.
//!
//! What Openfort's own tooling makes the user do by hand, this driver does
//! with the user's project credentials:
//!
//! ```text
//! openfort login                      → dashboard consent page (browser hop)
//! openfort backend-wallet setup       → POST /v2/accounts/backend/register-secret
//!                                       PUT  /v1/project/apikey  { type: pk_wallet }
//! openfort accounts solana create     → POST /v2/accounts/backend { chainType: SVM }
//! pay account new … --backend openfort → the CLI, from the exchange result
//! ```
//!
//! The consent page (`/oauth/consent`) is the same one Openfort's CLI uses:
//! it signs the user in or up, lets them pick or create a project, and
//! redirects back with the project's keys in the URL fragment. pay-cloud
//! never sees an Openfort password and never keeps the keys past the
//! onboarding session.

pub mod wallet_auth;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{ConsentGrant, DriverError, ProvisionedWallet, WalletDriver};
pub use wallet_auth::WalletSecret;

/// Error alias for this module.
pub type Error = DriverError;

pub const PROVIDER_ID: &str = "openfort";
pub const DEFAULT_API_BASE: &str = "https://api.openfort.io";
pub const DEFAULT_DASHBOARD_URL: &str = "https://dashboard.openfort.io";

const REGISTER_SECRET_PATH: &str = "/v2/accounts/backend/register-secret";
const PROJECT_APIKEY_PATH: &str = "/v1/project/apikey";
const BACKEND_ACCOUNTS_PATH: &str = "/v2/accounts/backend";

/// Credential field names, as `pay_core::remote::openfort` declares them.
pub const SECRET_KEY_FIELD: &str = "secret_key";
pub const WALLET_SECRET_FIELD: &str = "wallet_secret";

pub struct Openfort {
    api_base: String,
    dashboard_url: String,
    client: reqwest::Client,
}

/// Environment overrides, named as Openfort's own CLI names them, so a
/// staging or mock Openfort can be pointed at without code changes.
pub const API_BASE_ENV: &str = "OPENFORT_BASE_URL";
pub const DASHBOARD_URL_ENV: &str = "OPENFORT_AUTH_PAGE_URL";

impl Default for Openfort {
    fn default() -> Self {
        let env = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self::with_urls(
            &env(API_BASE_ENV, DEFAULT_API_BASE),
            &env(DASHBOARD_URL_ENV, DEFAULT_DASHBOARD_URL),
        )
    }
}

impl Openfort {
    /// Driver against custom hosts (tests, staging).
    pub fn with_urls(api_base: &str, dashboard_url: &str) -> Self {
        // rustls explicitly: api.openfort.io requires TLS 1.3, which the
        // platform TLS stack does not offer on macOS.
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest client with static configuration");
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            dashboard_url: dashboard_url.trim_end_matches('/').to_string(),
            client,
        }
    }

    fn api_host(&self) -> String {
        url::Url::parse(&self.api_base)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| match u.port() {
                    Some(p) => format!("{h}:{p}"),
                    None => h.to_string(),
                })
            })
            .unwrap_or_default()
    }

    /// Register a fresh wallet secret with the project. The JWT is signed by
    /// the key being registered and travels in the body.
    async fn register_secret(&self, api_key: &str, secret: &WalletSecret) -> Result<(), Error> {
        let body = json!({
            "publicKey": secret.public_key_pem()?,
            "keyId": secret.key_id(),
        });
        let token = secret.registration_jwt("POST", REGISTER_SECRET_PATH, &body)?;
        let mut body = body;
        body["walletAuthToken"] = Value::String(token);

        let response = self
            .client
            .post(format!("{}{REGISTER_SECRET_PATH}", self.api_base))
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(unreachable)?;
        expect_success(response, "register the wallet secret").await?;
        Ok(())
    }

    /// Record the public half on the project, as the dashboard and CLI do.
    async fn store_public_key(&self, api_key: &str, secret: &WalletSecret) -> Result<(), Error> {
        let response = self
            .client
            .put(format!("{}{PROJECT_APIKEY_PATH}", self.api_base))
            .bearer_auth(api_key)
            .json(&json!({ "type": "pk_wallet", "uuid": secret.public_key_b64()? }))
            .send()
            .await
            .map_err(unreachable)?;
        expect_success(response, "store the wallet public key").await?;
        Ok(())
    }

    /// Create a developer-custody Solana backend wallet.
    async fn create_solana_account(
        &self,
        api_key: &str,
        secret: &WalletSecret,
    ) -> Result<BackendAccount, Error> {
        let body = json!({ "chainType": "SVM" });
        let token =
            secret.operation_jwt("POST", &self.api_host(), BACKEND_ACCOUNTS_PATH, Some(&body))?;
        let response = self
            .client
            .post(format!("{}{BACKEND_ACCOUNTS_PATH}", self.api_base))
            .bearer_auth(api_key)
            .header("x-wallet-auth", token)
            .json(&body)
            .send()
            .await
            .map_err(unreachable)?;
        let response = expect_success(response, "create the Solana wallet").await?;
        let account: BackendAccount = response.json().await.map_err(|e| Error::Protocol {
            provider: PROVIDER_ID,
            message: format!("account response: {e}"),
        })?;
        if account.id.is_empty() || account.address.is_empty() {
            return Err(Error::Protocol {
                provider: PROVIDER_ID,
                message: "account response is missing `id` or `address`".to_string(),
            });
        }
        Ok(account)
    }
}

#[async_trait::async_trait]
impl WalletDriver for Openfort {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Openfort"
    }

    fn account_identity(&self, grant: &ConsentGrant) -> Option<String> {
        let api_key = grant.api_key.trim();
        if api_key.is_empty() {
            return None;
        }
        // `project_id` comes from the browser fragment and is not an
        // authenticated principal. The registry binds returning tenants to
        // possession of this high-entropy credential with a keyed HMAC; this
        // raw value is used only transiently and is never stored as identity.
        Some(api_key.to_string())
    }

    /// `{dashboard}/oauth/consent?redirect_uri=…&state=…`, the page
    /// Openfort's CLI opens for `openfort login`.
    fn consent_url(&self, redirect_uri: &str, state: &str) -> String {
        let mut url = url::Url::parse(&format!("{}/oauth/consent", self.dashboard_url))
            .expect("dashboard url is valid");
        url.query_pairs_mut()
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state);
        url.to_string()
    }

    /// Refresh the credential after the same authenticated grant is reused.
    fn refresh_credentials(
        &self,
        credentials: &mut std::collections::BTreeMap<String, String>,
        grant: &ConsentGrant,
    ) {
        credentials.insert(SECRET_KEY_FIELD.to_string(), grant.api_key.clone());
    }

    fn parse_grant(&self, fragment: &str) -> Result<(ConsentGrant, String), Error> {
        parse_consent_fragment(fragment)
    }

    async fn provision(&self, grant: &ConsentGrant) -> Result<ProvisionedWallet, Error> {
        let api_key = grant.api_key.trim();
        if !api_key.starts_with("sk_") {
            return Err(Error::InvalidGrant(
                "the Openfort consent did not return a secret key (`sk_live_…` / `sk_test_…`)"
                    .to_string(),
            ));
        }

        let secret = WalletSecret::generate();
        tracing::info!(
            key_id = secret.key_id(),
            "registering Openfort wallet secret"
        );
        self.register_secret(api_key, &secret).await?;
        self.store_public_key(api_key, &secret).await?;
        let account = self.create_solana_account(api_key, &secret).await?;
        tracing::info!(account = %account.id, address = %account.address, "Openfort Solana wallet created");

        let mut credentials = std::collections::BTreeMap::new();
        credentials.insert(SECRET_KEY_FIELD.to_string(), api_key.to_string());
        credentials.insert(WALLET_SECRET_FIELD.to_string(), secret.private_key_b64()?);
        Ok(ProvisionedWallet {
            provider: PROVIDER_ID,
            credentials,
            wallet_id: account.id,
            address: account.address,
            project_id: grant.project_id.clone(),
        })
    }
}

/// Parse the consent redirect fragment
/// (`api_key=…&publishable_key=…&project_id=…&project=…&state=…`).
/// Returns the grant and the echoed state.
pub fn parse_consent_fragment(fragment: &str) -> Result<(ConsentGrant, String), Error> {
    let fragment = fragment.trim_start_matches('#');
    let mut grant = ConsentGrant::default();
    let mut state = None;
    let mut error = None;
    for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "api_key" => grant.api_key = value,
            "publishable_key" => grant.publishable_key = Some(value),
            "project_id" => grant.project_id = Some(value),
            "project" => grant.project = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(Error::InvalidGrant(format!(
            "Openfort returned an error: {error}"
        )));
    }
    if grant.api_key.is_empty() {
        return Err(Error::InvalidGrant(
            "no api_key in the consent response".to_string(),
        ));
    }
    let state =
        state.ok_or_else(|| Error::InvalidGrant("no state in the consent response".to_string()))?;
    Ok((grant, state))
}

#[derive(Debug, Deserialize)]
struct BackendAccount {
    #[serde(default)]
    id: String,
    #[serde(default)]
    address: String,
}

fn unreachable(e: reqwest::Error) -> Error {
    Error::Unreachable {
        provider: PROVIDER_ID,
        source: Box::new(e),
    }
}

/// Turn a non-2xx answer into a [`DriverError::Rejected`] with the most
/// useful message Openfort gave us.
async fn expect_success(
    response: reqwest::Response,
    action: &str,
) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or(text);
    let message = match status.as_u16() {
        401 | 403 => format!(
            "could not {action}: the project secret key was rejected (rotated, revoked, or from another project). {detail}"
        ),
        _ if detail.to_ascii_lowercase().contains("already exists") => format!(
            "could not {action}: this project already has a wallet secret. Rotate it from the Openfort dashboard or CLI, or use a new project. {detail}"
        ),
        _ => format!("could not {action}: {detail}"),
    };
    Err(Error::Rejected {
        provider: PROVIDER_ID,
        status: status.as_u16(),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{post, put};
    use axum::{Json, Router};
    use base64::Engine;
    use p256::ecdsa::signature::Verifier;
    use p256::pkcs8::DecodePublicKey;
    use std::sync::{Arc, Mutex};

    #[test]
    fn consent_url_targets_the_dashboard_consent_page() {
        let d = Openfort::default();
        let url = d.consent_url("https://cloud.pay.sh/onboard/openfort/callback", "st-1");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.origin().ascii_serialization(), DEFAULT_DASHBOARD_URL);
        assert_eq!(parsed.path(), "/oauth/consent");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            q["redirect_uri"],
            "https://cloud.pay.sh/onboard/openfort/callback"
        );
        assert_eq!(q["state"], "st-1");
    }

    #[test]
    fn fragment_parses_the_cli_login_shape() {
        let (grant, state) = parse_consent_fragment(
            "#api_key=sk_test_abc&publishable_key=pk_test_x&project_id=pro_1&project=My%20App&state=st-1",
        )
        .unwrap();
        assert_eq!(grant.api_key, "sk_test_abc");
        assert_eq!(grant.publishable_key.as_deref(), Some("pk_test_x"));
        assert_eq!(grant.project_id.as_deref(), Some("pro_1"));
        assert_eq!(grant.project.as_deref(), Some("My App"));
        assert_eq!(state, "st-1");

        assert!(matches!(
            parse_consent_fragment("state=st-1"),
            Err(Error::InvalidGrant(_))
        ));
        assert!(matches!(
            parse_consent_fragment("api_key=sk_1"),
            Err(Error::InvalidGrant(_))
        ));
        let err = parse_consent_fragment("error=access_denied&state=st-1").unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn account_identity_is_bound_to_the_credential_not_claimed_project_metadata() {
        let driver = Openfort::default();
        let grant = |api_key: &str, project_id: &str| ConsentGrant {
            api_key: api_key.to_string(),
            project_id: Some(project_id.to_string()),
            ..ConsentGrant::default()
        };

        assert_eq!(
            driver.account_identity(&grant("sk_one", "pro_1")),
            driver.account_identity(&grant("sk_one", "pro_attacker"))
        );
        assert_ne!(
            driver.account_identity(&grant("sk_one", "pro_1")),
            driver.account_identity(&grant("sk_attacker", "pro_1"))
        );
    }

    #[tokio::test]
    async fn provision_rejects_a_non_secret_key_before_any_request() {
        let d = Openfort::with_urls("http://127.0.0.1:9", DEFAULT_DASHBOARD_URL);
        let err = d
            .provision(&ConsentGrant {
                api_key: "pk_test_nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidGrant(_)), "{err}");
    }

    #[derive(Clone, Default)]
    struct Seen {
        calls: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
    }

    async fn record(State(seen): State<Seen>, name: &'static str, headers: HeaderMap, body: Value) {
        seen.calls
            .lock()
            .unwrap()
            .push((name.to_string(), headers, body));
    }

    async fn mock_openfort(seen: Seen, reject_register: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                REGISTER_SECRET_PATH,
                post(
                    move |state: State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        record(state, "register", headers, body).await;
                        if reject_register {
                            (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(json!({ "error": { "message": "Wallet already exists" } })),
                            )
                        } else {
                            (axum::http::StatusCode::OK, Json(json!({ "ok": true })))
                        }
                    },
                ),
            )
            .route(
                PROJECT_APIKEY_PATH,
                put(
                    |state: State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        record(state, "apikey", headers, body).await;
                        Json(json!({ "ok": true }))
                    },
                ),
            )
            .route(
                BACKEND_ACCOUNTS_PATH,
                post(
                    |state: State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        record(state, "create", headers, body).await;
                        Json(json!({
                            "id": "acc_123",
                            "address": "C78fUoBw1YDJDmzNx7viRZFnuhku3t3eiy9eiV2hafff",
                            "chainType": "SVM",
                            "custody": "Developer"
                        }))
                    },
                ),
            )
            .with_state(seen);
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        format!("http://{addr}")
    }

    fn decode_claims(jwt: &str) -> (Value, String, Vec<u8>) {
        let parts: Vec<&str> = jwt.split('.').collect();
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        (claims, format!("{}.{}", parts[0], parts[1]), sig)
    }

    #[tokio::test]
    async fn provision_walks_register_store_create_with_valid_jwts() {
        let seen = Seen::default();
        let base = mock_openfort(seen.clone(), false).await;
        let d = Openfort::with_urls(&base, DEFAULT_DASHBOARD_URL);

        let wallet = d
            .provision(&ConsentGrant {
                api_key: "sk_test_abc".into(),
                project_id: Some("pro_1".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(wallet.provider, "openfort");
        assert_eq!(wallet.wallet_id, "acc_123");
        assert_eq!(
            wallet.address,
            "C78fUoBw1YDJDmzNx7viRZFnuhku3t3eiy9eiV2hafff"
        );
        assert_eq!(wallet.project_id.as_deref(), Some("pro_1"));
        assert_eq!(wallet.credentials["secret_key"], "sk_test_abc");

        let calls = seen.calls.lock().unwrap();
        let names: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, ["register", "apikey", "create"]);
        for (_, headers, _) in calls.iter() {
            assert_eq!(headers["authorization"], "Bearer sk_test_abc");
        }

        // The stored wallet secret is the key that was registered.
        let secret =
            WalletSecret::from_secret("ws_test", &wallet.credentials["wallet_secret"]).unwrap();
        let (_, _, register_body) = &calls[0];
        assert_eq!(
            register_body["publicKey"],
            json!(secret.public_key_pem().unwrap())
        );
        assert!(register_body["keyId"].as_str().unwrap().starts_with("ws_"));
        let pem = register_body["publicKey"].as_str().unwrap();
        let registered = p256::ecdsa::VerifyingKey::from_public_key_pem(pem).unwrap();

        // Registration JWT: in the body, CLI shape, signed by the new key.
        let (claims, input, sig) =
            decode_claims(register_body["walletAuthToken"].as_str().unwrap());
        assert_eq!(
            claims["uris"],
            json!([format!("POST {REGISTER_SECRET_PATH}")])
        );
        assert!(claims.get("exp").is_none());
        let mut hashed = register_body.clone();
        hashed.as_object_mut().unwrap().remove("walletAuthToken");
        assert_eq!(
            claims["reqHash"],
            json!(wallet_auth::req_hash(&hashed).unwrap())
        );
        registered
            .verify(
                input.as_bytes(),
                &p256::ecdsa::Signature::from_slice(&sig).unwrap(),
            )
            .unwrap();

        // Public key reference on the project.
        let (_, _, apikey_body) = &calls[1];
        assert_eq!(apikey_body["type"], "pk_wallet");
        assert_eq!(apikey_body["uuid"], json!(secret.public_key_b64().unwrap()));

        // Create: header JWT, operation shape with host and expiry.
        let (_, headers, create_body) = &calls[2];
        assert_eq!(*create_body, json!({ "chainType": "SVM" }));
        let (claims, input, sig) = decode_claims(headers["x-wallet-auth"].to_str().unwrap());
        let host = url::Url::parse(&base).unwrap();
        let host = format!("{}:{}", host.host_str().unwrap(), host.port().unwrap());
        assert_eq!(
            claims["uris"],
            json!([format!("POST {host}{BACKEND_ACCOUNTS_PATH}")])
        );
        assert!(claims["exp"].as_i64().unwrap() > claims["iat"].as_i64().unwrap());
        assert_eq!(
            claims["reqHash"],
            json!(wallet_auth::req_hash(create_body).unwrap())
        );
        registered
            .verify(
                input.as_bytes(),
                &p256::ecdsa::Signature::from_slice(&sig).unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn existing_wallet_secret_is_explained_and_stops_the_flow() {
        let seen = Seen::default();
        let base = mock_openfort(seen.clone(), true).await;
        let d = Openfort::with_urls(&base, DEFAULT_DASHBOARD_URL);
        let err = d
            .provision(&ConsentGrant {
                api_key: "sk_test_abc".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        match err {
            Error::Rejected {
                status, message, ..
            } => {
                assert_eq!(status, 400);
                assert!(message.contains("already has a wallet secret"), "{message}");
            }
            other => panic!("{other}"),
        }
        assert_eq!(
            seen.calls.lock().unwrap().len(),
            1,
            "stops after the first failure"
        );
    }

    #[tokio::test]
    async fn unreachable_api_is_reported_as_such() {
        let d = Openfort::with_urls("http://127.0.0.1:9", DEFAULT_DASHBOARD_URL);
        let err = d
            .provision(&ConsentGrant {
                api_key: "sk_test_abc".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unreachable { .. }), "{err}");
    }
}
