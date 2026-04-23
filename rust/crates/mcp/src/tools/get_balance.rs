use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Network to check. Defaults to "mainnet".
    #[schemars(
        description = "Network slug (e.g. \"mainnet\", \"localnet\"). Defaults to mainnet."
    )]
    #[serde(default = "default_network")]
    pub network: String,
}

fn default_network() -> String {
    "mainnet".to_string()
}

pub async fn run(params: Params) -> Result<CallToolResult, rmcp::ErrorData> {
    let result = tokio::task::spawn_blocking(move || get_balance_sync(&params.network))
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        result,
    )]))
}

fn get_balance_sync(network: &str) -> Result<String, pay_core::Error> {
    let accounts = pay_core::accounts::AccountsFile::load()?;
    let (name, account) = accounts
        .account_for_network(network)
        .ok_or_else(|| pay_core::Error::Config(format!("No account configured for {network}")))?;

    let pubkey = account
        .pubkey
        .as_deref()
        .ok_or_else(|| pay_core::Error::Config("Account has no pubkey".to_string()))?;

    let rpc_url = if network == "mainnet" {
        pay_core::balance::mainnet_rpc_url()
    } else {
        std::env::var("PAY_RPC_URL").unwrap_or_else(|_| pay_core::balance::mainnet_rpc_url())
    };

    let rt = tokio::runtime::Handle::current();
    let balances = rt
        .block_on(pay_core::client::balance::get_balances(&rpc_url, pubkey))
        .map_err(|e| pay_core::Error::Config(format!("RPC error: {e}")))?;

    let sol = balances.sol_lamports as f64 / 1_000_000_000.0;
    let mut lines = vec![
        format!("Account: {name} ({network})"),
        format!("Address: {pubkey}"),
        format!("SOL: {sol:.4}"),
    ];

    for token in &balances.tokens {
        let label = token.symbol.unwrap_or("unknown");
        lines.push(format!("{label}: {:.2}", token.ui_amount));
    }

    if balances.tokens.is_empty() {
        lines.push("No token balances found.".to_string());
    }

    Ok(lines.join("\n"))
}
