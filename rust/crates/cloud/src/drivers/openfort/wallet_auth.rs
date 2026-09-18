//! Openfort wallet authentication: the project's ECDSA P-256 "wallet
//! secret" and the ES256 JWTs it signs.
//!
//! Openfort gates every backend-wallet operation behind a JWT signed by a
//! key the project registered. Two request shapes exist:
//!
//! - **Registration** (`register-secret`, `rotate-secrets`): the JWT travels
//!   in the JSON body as `walletAuthToken`, is signed by the key being
//!   registered, and its `uris` entry is `"METHOD path"` with no host and no
//!   expiry. This is what Openfort's own CLI sends.
//! - **Operations** (`POST /v2/accounts/backend`, `…/sign`): the JWT travels
//!   in the `x-wallet-auth` header, is signed by the registered key, and its
//!   `uris` entry is `"METHOD host path"` with an expiry. This is what the
//!   Openfort SDKs and solana-keychain send.
//!
//! Both carry `reqHash`, the SHA-256 of the request body serialised with
//! keys sorted recursively.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Error;

/// Lifetime of an operation JWT.
pub const OPERATION_JWT_LIFETIME_SECS: i64 = 120;

/// A project wallet secret: the P-256 key pair Openfort authenticates
/// backend-wallet requests with.
///
/// The private half is what `pay` stores as the `wallet_secret` credential
/// (base64 PKCS#8 DER, the single-line form solana-keychain accepts). The
/// public half is registered with Openfort.
#[derive(Clone)]
pub struct WalletSecret {
    key_id: String,
    signing: SigningKey,
}

impl std::fmt::Debug for WalletSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletSecret")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl WalletSecret {
    /// Generate a fresh key pair. `key_id` follows Openfort's CLI: `ws_<unix ms>`.
    pub fn generate() -> Self {
        let signing = SigningKey::random(&mut rand::rngs::OsRng);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            key_id: format!("ws_{millis}"),
            signing,
        }
    }

    /// Rebuild from a stored secret (base64 PKCS#8 DER or PEM) and its id.
    pub fn from_secret(key_id: impl Into<String>, secret: &str) -> Result<Self, Error> {
        let trimmed = secret.trim();
        let signing = if trimmed.starts_with("-----BEGIN") {
            SigningKey::from_pkcs8_pem(trimmed)
        } else {
            let der = STANDARD
                .decode(trimmed.split_whitespace().collect::<String>())
                .map_err(|e| Error::Crypto(format!("wallet secret is not base64: {e}")))?;
            SigningKey::from_pkcs8_der(&der)
        }
        .map_err(|e| Error::Crypto(format!("wallet secret is not a PKCS#8 P-256 key: {e}")))?;
        Ok(Self {
            key_id: key_id.into(),
            signing,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Private key as base64 PKCS#8 DER, one line. This is the value stored
    /// as the `wallet_secret` credential.
    pub fn private_key_b64(&self) -> Result<String, Error> {
        let der = self
            .signing
            .to_pkcs8_der()
            .map_err(|e| Error::Crypto(format!("encode PKCS#8: {e}")))?;
        Ok(STANDARD.encode(der.as_bytes()))
    }

    /// Public key as base64 SPKI DER, one line. Openfort stores this as the
    /// project's `pk_wallet` reference.
    pub fn public_key_b64(&self) -> Result<String, Error> {
        let der = self
            .signing
            .verifying_key()
            .to_public_key_der()
            .map_err(|e| Error::Crypto(format!("encode SPKI: {e}")))?;
        Ok(STANDARD.encode(der.as_bytes()))
    }

    /// Public key as a PEM block, the shape `register-secret` expects.
    pub fn public_key_pem(&self) -> Result<String, Error> {
        self.signing
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .map(|pem| pem.trim_end().to_string())
            .map_err(|e| Error::Crypto(format!("encode public PEM: {e}")))
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        self.signing.verifying_key()
    }

    /// JWT for a registration call: signed by this (new) key, `uris` without
    /// host, no expiry, `reqHash` over `body`.
    pub fn registration_jwt(
        &self,
        method: &str,
        path: &str,
        body: &Value,
    ) -> Result<String, Error> {
        let claims = Claims {
            uris: vec![format!("{} {path}", method.to_ascii_uppercase())],
            req_hash: req_hash(body),
            iat: now(),
            nbf: now(),
            exp: None,
            jti: random_hex(16),
        };
        self.sign_jwt(&claims)
    }

    /// JWT for an operation call: `uris` with host, short expiry, `reqHash`
    /// over `body` when present.
    pub fn operation_jwt(
        &self,
        method: &str,
        host: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<String, Error> {
        let now = now();
        let claims = Claims {
            uris: vec![format!("{} {host}{path}", method.to_ascii_uppercase())],
            req_hash: body.and_then(req_hash),
            iat: now,
            nbf: now,
            exp: Some(now + OPERATION_JWT_LIFETIME_SECS),
            jti: random_hex(16),
        };
        self.sign_jwt(&claims)
    }

    fn sign_jwt(&self, claims: &Claims) -> Result<String, Error> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","typ":"JWT"}"#);
        let payload = serde_json::to_vec(claims)
            .map_err(|e| Error::Crypto(format!("encode JWT claims: {e}")))?;
        let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(payload));
        let signature: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }
}

#[derive(Serialize)]
struct Claims {
    uris: Vec<String>,
    #[serde(rename = "reqHash", skip_serializing_if = "Option::is_none")]
    req_hash: Option<String>,
    iat: i64,
    nbf: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exp: Option<i64>,
    jti: String,
}

/// `reqHash`: hex SHA-256 of the body with object keys sorted recursively.
/// `None` for a null or empty-object body, matching the SDKs.
pub fn req_hash(body: &Value) -> Option<String> {
    if body.is_null() || matches!(body, Value::Object(m) if m.is_empty()) {
        return None;
    }
    let sorted = sort_keys(body);
    let json = serde_json::to_string(&sorted).ok()?;
    Some(hex(&Sha256::digest(json.as_bytes())))
}

fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sort_keys(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use serde_json::json;

    fn decode_jwt(jwt: &str) -> (Value, Vec<u8>, String) {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, json!({"alg": "ES256", "typ": "JWT"}));
        let payload: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        let sig = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        (payload, sig, format!("{}.{}", parts[0], parts[1]))
    }

    #[test]
    fn registration_jwt_matches_the_cli_shape_and_verifies() {
        let secret = WalletSecret::generate();
        let body = json!({ "publicKey": "-----BEGIN PUBLIC KEY-----\nx\n-----END PUBLIC KEY-----", "keyId": secret.key_id() });
        let jwt = secret
            .registration_jwt("post", "/v2/accounts/backend/register-secret", &body)
            .unwrap();
        let (claims, sig, signing_input) = decode_jwt(&jwt);

        assert_eq!(
            claims["uris"],
            json!(["POST /v2/accounts/backend/register-secret"])
        );
        assert_eq!(claims["reqHash"], json!(req_hash(&body).unwrap()));
        assert!(
            claims.get("exp").is_none(),
            "registration JWTs carry no exp"
        );
        assert_eq!(claims["iat"], claims["nbf"]);
        assert_eq!(claims["jti"].as_str().unwrap().len(), 32);

        let signature = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        secret
            .verifying_key()
            .verify(signing_input.as_bytes(), &signature)
            .expect("ES256 signature verifies with the public key");
    }

    #[test]
    fn operation_jwt_carries_host_and_expiry() {
        let secret = WalletSecret::generate();
        let body = json!({ "chainType": "SVM" });
        let jwt = secret
            .operation_jwt(
                "POST",
                "api.openfort.io",
                "/v2/accounts/backend",
                Some(&body),
            )
            .unwrap();
        let (claims, _, _) = decode_jwt(&jwt);
        assert_eq!(
            claims["uris"],
            json!(["POST api.openfort.io/v2/accounts/backend"])
        );
        let exp = claims["exp"].as_i64().unwrap();
        let iat = claims["iat"].as_i64().unwrap();
        assert_eq!(exp - iat, OPERATION_JWT_LIFETIME_SECS);
        assert_eq!(claims["reqHash"], json!(req_hash(&body).unwrap()));

        let jwt = secret
            .operation_jwt("GET", "api.openfort.io", "/v2/accounts/acc_1", None)
            .unwrap();
        let (claims, _, _) = decode_jwt(&jwt);
        assert!(claims.get("reqHash").is_none());
    }

    #[test]
    fn req_hash_sorts_keys_recursively_and_skips_empty_bodies() {
        let a = json!({ "b": 1, "a": { "d": [3, { "z": 1, "y": 2 }], "c": 2 } });
        let b = json!({ "a": { "c": 2, "d": [3, { "y": 2, "z": 1 }] }, "b": 1 });
        assert_eq!(req_hash(&a), req_hash(&b));
        // sha256 of the canonical string, computed independently.
        let canonical = r#"{"a":{"c":2,"d":[3,{"y":2,"z":1}]},"b":1}"#;
        assert_eq!(
            req_hash(&a).unwrap(),
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        assert_eq!(req_hash(&json!({})), None);
        assert_eq!(req_hash(&Value::Null), None);
    }

    #[test]
    fn secret_round_trips_through_its_stored_forms() {
        let secret = WalletSecret::generate();
        assert!(secret.key_id().starts_with("ws_"));

        let b64 = secret.private_key_b64().unwrap();
        let again = WalletSecret::from_secret(secret.key_id(), &b64).unwrap();
        assert_eq!(
            again.public_key_b64().unwrap(),
            secret.public_key_b64().unwrap()
        );

        let pem = secret.public_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        assert!(pem.ends_with("-----END PUBLIC KEY-----"));
        // The PEM body is the SPKI DER we also expose as one line.
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        assert_eq!(body, secret.public_key_b64().unwrap());

        assert!(WalletSecret::from_secret("ws_x", "not base64!!").is_err());
    }
}
