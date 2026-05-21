use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Base64-encoded legacy or v0 Solana transaction to sign and submit.
    #[schemars(
        description = "Base64-encoded serialized legacy or v0 Solana transaction to sign and submit."
    )]
    pub transaction: String,
    /// Solana network for Pay account selection and RPC defaults. Defaults to mainnet unless the MCP server enforces a network.
    #[serde(default)]
    pub network: Option<String>,
    /// Pay account name. Defaults to the active/default Pay account unless the MCP server enforces an account.
    #[serde(default)]
    pub account: Option<String>,
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let transaction = params.transaction;
    let network = resolve_network(
        params.network.as_deref(),
        std::env::var("PAY_NETWORK_ENFORCED").ok().as_deref(),
    );
    let account = resolve_account(
        params.account.as_deref(),
        std::env::var("PAY_ACTIVE_ACCOUNT").ok().as_deref(),
    );

    let result = tokio::task::spawn_blocking(move || {
        pay_core::sign::sign_and_submit_base64_transaction(
            &transaction,
            &network,
            account.as_deref(),
        )
    })
    .await
    .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    match result {
        Ok(signature) => Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            signature,
        )])),
        Err(err) => Ok(super::tool_error(format!("Pay sign failed: {err}"))),
    }
}

fn resolve_network(requested: Option<&str>, enforced: Option<&str>) -> String {
    normalize(enforced)
        .or_else(|| normalize(requested))
        .unwrap_or_else(|| pay_core::accounts::MAINNET_NETWORK.to_string())
}

fn resolve_account(requested: Option<&str>, enforced: Option<&str>) -> Option<String> {
    normalize(enforced).or_else(|| normalize(requested))
}

fn normalize(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_deserialize_minimal() {
        let params: Params = serde_json::from_str(r#"{"transaction":"dHg="}"#).unwrap();

        assert_eq!(params.transaction, "dHg=");
        assert!(params.network.is_none());
        assert!(params.account.is_none());
    }

    #[test]
    fn enforced_network_wins_over_requested_network() {
        assert_eq!(
            resolve_network(Some("mainnet"), Some("localnet")),
            "localnet"
        );
    }

    #[test]
    fn requested_network_falls_back_to_mainnet() {
        assert_eq!(resolve_network(Some(" devnet "), None), "devnet");
        assert_eq!(resolve_network(Some(" "), None), "mainnet");
    }

    #[test]
    fn enforced_account_wins_over_requested_account() {
        assert_eq!(
            resolve_account(Some("requested"), Some(" active ")).as_deref(),
            Some("active")
        );
    }
}
