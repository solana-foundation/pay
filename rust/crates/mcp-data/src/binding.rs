use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::driver::{DataError, Result};

pub const CAPABILITY_HEADER: &str = "x-pay-data-capability";
pub const INTERNAL_PROOF_HEADER: &str = "x-pay-data-binding-proof";
const MAX_BINDING_SECONDS: u32 = 30 * 24 * 60 * 60;
const DEFAULT_BINDING_SECONDS: u32 = 24 * 60 * 60;
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct BindingIssuer {
    key: Vec<u8>,
    internal_proof: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateBindingRequest {
    pub payer: String,
    pub binding_name: String,
    pub driver: String,
    pub store_id: String,
    pub workload_id: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    pub lease_seconds: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreateBindingResponse {
    pub url_path: String,
    pub secret_id: String,
    pub expires_at: u64,
}

pub struct IssuedBinding {
    pub url_path: String,
    pub capability: String,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    version: u8,
    pub tenant: String,
    pub driver: String,
    pub store_id: String,
    workload_id: String,
    pub read: bool,
    pub write: bool,
    pub expires_at: u64,
}

impl BindingIssuer {
    pub fn new(key: Vec<u8>, internal_proof: Vec<u8>) -> Result<Self> {
        if key.len() < 32 {
            return Err(DataError::Configuration(
                "DATA_BINDING_SIGNING_KEY must contain at least 32 bytes".into(),
            ));
        }
        if internal_proof.len() < 32 {
            return Err(DataError::Configuration(
                "DATA_BINDING_INTERNAL_PROOF must contain at least 32 bytes".into(),
            ));
        }
        Ok(Self {
            key,
            internal_proof,
        })
    }

    pub fn verify_internal_proof(&self, proof: &str) -> Result<()> {
        if proof.as_bytes().ct_eq(&self.internal_proof).into() {
            Ok(())
        } else {
            Err(DataError::InvalidRequest("invalid binding proof".into()))
        }
    }

    pub fn issue(&self, tenant: String, request: &CreateBindingRequest) -> Result<IssuedBinding> {
        if !request.read && !request.write {
            return Err(DataError::InvalidRequest(
                "binding must grant read, write, or both".into(),
            ));
        }
        validate_workload_id(&request.workload_id)?;
        let lease = request.lease_seconds.unwrap_or(DEFAULT_BINDING_SECONDS);
        if !(300..=MAX_BINDING_SECONDS).contains(&lease) {
            return Err(DataError::InvalidRequest(format!(
                "binding lease_seconds must be between 300 and {MAX_BINDING_SECONDS}"
            )));
        }
        let expires_at = now_seconds()?.saturating_add(u64::from(lease));
        let capability = Capability {
            version: 1,
            tenant,
            driver: request.driver.clone(),
            store_id: request.store_id.clone(),
            workload_id: request.workload_id.clone(),
            read: request.read,
            write: request.write,
            expires_at,
        };
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&capability)?);
        let mut mac = self.mac()?;
        mac.update(encoded.as_bytes());
        let signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(IssuedBinding {
            url_path: format!("/__402/bindings/{}", request.store_id),
            capability: format!("{encoded}.{signature}"),
            expires_at,
        })
    }

    pub fn verify(&self, token: &str, store_id: &str) -> Result<Capability> {
        let (encoded, signature) = token
            .split_once('.')
            .ok_or_else(|| DataError::InvalidRequest("invalid data capability".into()))?;
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| DataError::InvalidRequest("invalid data capability".into()))?;
        let mut mac = self.mac()?;
        mac.update(encoded.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| DataError::InvalidRequest("invalid data capability".into()))?;
        let capability: Capability = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| DataError::InvalidRequest("invalid data capability".into()))?,
        )?;
        if capability.version != 1
            || capability.store_id != store_id
            || capability.expires_at < now_seconds()?
        {
            return Err(DataError::InvalidRequest(
                "expired or mismatched data capability".into(),
            ));
        }
        Ok(capability)
    }

    fn mac(&self) -> Result<HmacSha256> {
        HmacSha256::new_from_slice(&self.key)
            .map_err(|_| DataError::Configuration("invalid data binding key".into()))
    }
}

fn now_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| DataError::Configuration("system clock is before Unix epoch".into()))
}

fn validate_workload_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DataError::InvalidRequest("invalid workload_id".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_scoped_and_tamper_evident() {
        let issuer = BindingIssuer::new(vec![7; 32], vec![9; 32]).unwrap();
        let request = CreateBindingRequest {
            payer: "payer".into(),
            binding_name: "weather".into(),
            driver: "gcp-firestore".into(),
            store_id: "fds-0123456789abcdef-weather".into(),
            workload_id: "gcf-0123456789abcdef-weather".into(),
            read: true,
            write: true,
            lease_seconds: Some(300),
        };
        let binding = issuer.issue("0123456789abcdef".into(), &request).unwrap();
        assert!(
            issuer
                .verify(&binding.capability, "fds-0123456789abcdef-weather")
                .is_ok()
        );
        assert!(
            issuer
                .verify(&binding.capability, "fds-0123456789abcdef-other")
                .is_err()
        );
        let mut tampered = binding.capability.into_bytes();
        tampered[0] ^= 1;
        assert!(
            issuer
                .verify(
                    &String::from_utf8(tampered).unwrap(),
                    "fds-0123456789abcdef-weather"
                )
                .is_err()
        );
    }
}
