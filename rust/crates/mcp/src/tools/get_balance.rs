use rmcp::model::CallToolResult;
use rmcp::schemars;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Params {
    /// Network whose wallet and credit balances to check. Defaults to "mainnet".
    #[schemars(
        description = "Network slug (e.g. \"mainnet\", \"localnet\"). Defaults to mainnet."
    )]
    #[serde(default = "default_network")]
    pub network: String,
}

fn default_network() -> String {
    "mainnet".to_string()
}

pub async fn run(
    params: Params,
    scope: &crate::context::CallScope,
) -> Result<CallToolResult, rmcp::ErrorData> {
    // A forced network wins over the parameter, as it does for payments.
    let network = scope.network_override.clone().unwrap_or(params.network);
    let accounts = match scope.accounts.load() {
        Ok(accounts) => accounts,
        Err(err) => {
            return Ok(super::tool_error(format!(
                "Failed to load Pay accounts: {err}"
            )));
        }
    };
    let selected = match scope.account_override.as_deref() {
        Some(name) => accounts
            .named_account_for_network(&network, name)
            .map(|account| (name, account)),
        None => accounts.account_for_network(&network),
    };
    let Some((_name, account)) = selected else {
        return Ok(super::tool_error(format!(
            "No account configured for {network}. Run `pay setup` first."
        )));
    };

    let Some(pubkey) = account.pubkey.as_deref() else {
        return Ok(super::tool_error(
            "Account has no pubkey. Run `pay setup` again.",
        ));
    };
    let pubkey = pubkey.to_string();
    let rpc_url = scope.rpc_url(&network);

    let balances = match pay_core::client::balance::get_stablecoin_balances(&rpc_url, &pubkey).await
    {
        Ok(balances) => balances,
        Err(err) => return Ok(super::tool_error(format!("Balance lookup error: {err}"))),
    };

    let mut lines = vec![];

    for token in &balances.tokens {
        let label = token.symbol_or("unknown");
        lines.push(format!("{label}: {:.2}", token.ui_amount));
    }

    for credit in &balances.credits {
        lines.push(format!(
            "{} credits: {:.2} (program {})",
            credit.currency, credit.ui_amount, credit.program_id
        ));
    }

    if balances.tokens_unavailable {
        lines.push(
            "Stablecoin balances unavailable: both pay-api and Solana RPC lookups failed."
                .to_string(),
        );
    } else if balances.tokens.is_empty() {
        lines.push("No stablecoin balances found.".to_string());
    }
    if balances.credits_unavailable {
        lines.push("Program credit balances unavailable from pay-api.".to_string());
    } else if balances.credits.is_empty() {
        lines.push("No program credit balances found.".to_string());
    }
    let nothing_to_spend = !balances.tokens_unavailable
        && !balances.credits_unavailable
        && balances.tokens.iter().all(|t| t.raw_amount == 0)
        && balances.credits.iter().all(|c| c.raw_amount == 0);
    if nothing_to_spend {
        lines.push(
            "The balance is empty. Two ways to fund it: `topup` deposits stablecoins, \
             `sell_inference` earns them by serving this agent's inference for a while. \
             Offer both and let the user choose."
                .to_string(),
        );
    }

    Ok(CallToolResult::success(vec![
        rmcp::model::ContentBlock::text(lines.join("\n")),
    ]))
}
