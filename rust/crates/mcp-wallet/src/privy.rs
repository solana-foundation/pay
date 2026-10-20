use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::driver::{Result, WalletDriver, WalletError};
use crate::types::{CreateWalletRequest, DriverCapabilities, Tenant, Wallet, WalletRequest};

pub const DRIVER_ID: &str = "privy";

#[derive(Clone, Debug)]
pub struct PrivyConfig {
    pub app_id: String,
    pub app_secret: String,
    pub api_base: String,
}
impl PrivyConfig {
    pub fn from_env() -> Result<Self> {
        let required = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| WalletError::Configuration(format!("{name} must be set")))
        };
        Ok(Self {
            app_id: required("WALLET_PRIVY_APP_ID")?,
            app_secret: required("WALLET_PRIVY_APP_SECRET")?,
            api_base: std::env::var("WALLET_PRIVY_API_BASE")
                .unwrap_or_else(|_| "https://api.privy.io".into())
                .trim_end_matches('/')
                .into(),
        })
    }
}

#[derive(Clone)]
pub struct PrivyWalletDriver {
    config: PrivyConfig,
    client: reqwest::Client,
}
impl PrivyWalletDriver {
    pub fn new(config: PrivyConfig) -> Result<Self> {
        Ok(Self {
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
    fn external_id(&self, tenant: &Tenant, name: &str) -> Result<String> {
        validate_name(name)?;
        Ok(format!("pay-{}-{name}", tenant.key))
    }
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<(StatusCode, Value)> {
        let response = request
            .basic_auth(&self.config.app_id, Some(&self.config.app_secret))
            .header("privy-app-id", &self.config.app_id)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        Ok((status, value))
    }
    async fn get_external(&self, external_id: &str) -> Result<Option<Value>> {
        let (status, value) = self
            .send(self.client.get(format!(
                "{}/v1/wallets/ext_wal_{external_id}",
                self.config.api_base
            )))
            .await?;
        match status {
            StatusCode::OK => Ok(Some(value)),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(provider(status, &value)),
        }
    }
}

#[async_trait]
impl WalletDriver for PrivyWalletDriver {
    fn id(&self) -> &'static str {
        DRIVER_ID
    }
    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            driver: DRIVER_ID.into(),
            display_name: "Privy Wallet Infrastructure".into(),
            chains: vec!["solana".into()],
            operations: vec!["create".into(), "get".into()],
            custody:
                "service-controlled; provider policy and authorization keys required for spending"
                    .into(),
        }
    }

    async fn create(&self, tenant: &Tenant, request: CreateWalletRequest) -> Result<Wallet> {
        if request.chain != "solana" {
            return Err(WalletError::InvalidRequest(
                "Privy MVP supports the solana chain".into(),
            ));
        }
        let external_id = self.external_id(tenant, &request.name)?;
        if let Some(value) = self.get_external(&external_id).await? {
            return wallet_from_value(value, &external_id, request.name, request.purpose);
        }
        let display_name = match &request.purpose {
            Some(p) => format!("Pay {} ({p})", request.name),
            None => format!("Pay {}", request.name),
        };
        let body =
            json!({"chain_type":"solana","external_id":external_id,"display_name":display_name});
        let (status, value) = self
            .send(
                self.client
                    .post(format!("{}/v1/wallets", self.config.api_base))
                    .json(&body),
            )
            .await?;
        if status.is_success() {
            wallet_from_value(value, &external_id, request.name, request.purpose)
        } else if status == StatusCode::CONFLICT {
            let value = self
                .get_external(&external_id)
                .await?
                .ok_or_else(|| provider(status, &value))?;
            wallet_from_value(value, &external_id, request.name, request.purpose)
        } else {
            Err(provider(status, &value))
        }
    }

    async fn get(&self, tenant: &Tenant, request: WalletRequest) -> Result<Wallet> {
        let external_id = self.external_id(tenant, &request.name)?;
        let value = self
            .get_external(&external_id)
            .await?
            .ok_or_else(|| WalletError::InvalidRequest("wallet was not found".into()))?;
        wallet_from_value(value, &external_id, request.name, None)
    }
}

/// Build the wallet for `external_id` from a Privy response. The lookup URL
/// is owner-scoped, but the response is what names the payout address, so it
/// must itself be the requested owner's Solana wallet: a response for any
/// other external ID or chain is rejected rather than relabelled.
fn wallet_from_value(
    value: Value,
    external_id: &str,
    name: String,
    purpose: Option<String>,
) -> Result<Wallet> {
    if value.get("external_id").and_then(Value::as_str) != Some(external_id) {
        return Err(WalletError::Provider(
            "Privy wallet response is for a different external ID".into(),
        ));
    }
    if value.get("chain_type").and_then(Value::as_str) != Some("solana") {
        return Err(WalletError::Provider(
            "Privy wallet response is not a Solana wallet".into(),
        ));
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| WalletError::Provider("Privy wallet response has no ID".into()))?;
    let address = value
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| WalletError::Provider("Privy wallet response has no address".into()))?;
    Ok(Wallet {
        driver: DRIVER_ID.into(),
        id: id.into(),
        name,
        chain: "solana".into(),
        address: address.into(),
        purpose,
        lifecycle: "retained".into(),
    })
}
pub(crate) fn validate_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 40
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(WalletError::InvalidRequest(
            "wallet name must use lowercase URL-safe characters and be at most 40 bytes".into(),
        ));
    }
    Ok(())
}
fn provider(status: StatusCode, value: &Value) -> WalletError {
    WalletError::Provider(format!("Privy returned {status}: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_external_ids_are_tenant_scoped() {
        let d = PrivyWalletDriver::new(PrivyConfig {
            app_id: "a".into(),
            app_secret: "s".into(),
            api_base: "http://localhost".into(),
        })
        .unwrap();
        let t = Tenant {
            payer: "p".into(),
            key: "0123456789abcdef".into(),
            channel_id: "c".into(),
        };
        assert_eq!(
            d.external_id(&t, "tax").unwrap(),
            "pay-0123456789abcdef-tax"
        );
    }

    #[test]
    fn only_the_requested_solana_wallet_is_accepted() {
        let requested = "pay-0123456789abcdef-tax";
        let wallet = |external_id: Option<&str>, chain: Option<&str>| {
            let mut value =
                json!({"id": "w1", "address": "So11111111111111111111111111111111111111112"});
            if let Some(external_id) = external_id {
                value["external_id"] = json!(external_id);
            }
            if let Some(chain) = chain {
                value["chain_type"] = json!(chain);
            }
            wallet_from_value(value, requested, "tax".into(), None)
        };
        let accepted = wallet(Some(requested), Some("solana")).unwrap();
        assert_eq!(accepted.chain, "solana");
        assert_eq!(
            accepted.address,
            "So11111111111111111111111111111111111111112"
        );
        for (returned, chain) in [
            (Some("pay-fedcba9876543210-tax"), Some("solana")),
            (Some("pay-0123456789abcdef-other"), Some("solana")),
            (None, Some("solana")),
            (Some(requested), Some("ethereum")),
            (Some(requested), None),
        ] {
            assert!(wallet(returned, chain).is_err(), "{returned:?} {chain:?}");
        }
    }
}
