//! Check SOL and SPL token balances via Solana JSON-RPC.

/// Default mainnet RPC URL. Override with `PAY_MAINNET_RPC_URL` env var.
pub fn mainnet_rpc_url() -> String {
    std::env::var("PAY_MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string())
}

const TOKEN_PROGRAMS: &[&str] = &[
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", // SPL Token
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", // Token-2022
];

fn mint_symbol(mint: &str) -> Option<&'static str> {
    match mint {
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => Some("USDC"),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => Some("USDT"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub mint: String,
    pub ui_amount: f64,
    pub symbol: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct AccountBalances {
    pub sol_lamports: u64,
    pub tokens: Vec<TokenBalance>,
}

impl AccountBalances {
    pub fn diff_received(&self, baseline: &AccountBalances) -> ReceivedFunds {
        let sol_gained = self.sol_lamports.saturating_sub(baseline.sol_lamports);
        let mut tokens = Vec::new();
        for current in &self.tokens {
            let prev = baseline
                .tokens
                .iter()
                .find(|t| t.mint == current.mint)
                .map(|t| t.ui_amount)
                .unwrap_or(0.0);
            let gained = current.ui_amount - prev;
            if gained > f64::EPSILON {
                tokens.push(ReceivedToken {
                    mint: current.mint.clone(),
                    ui_amount: gained,
                    symbol: current.symbol,
                });
            }
        }
        ReceivedFunds {
            sol_lamports: sol_gained,
            tokens,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedFunds {
    pub sol_lamports: u64,
    pub tokens: Vec<ReceivedToken>,
}

#[derive(Debug, Clone)]
pub struct ReceivedToken {
    pub mint: String,
    pub ui_amount: f64,
    pub symbol: Option<&'static str>,
}

impl ReceivedFunds {
    pub fn has_any(&self) -> bool {
        self.sol_lamports > 0 || !self.tokens.is_empty()
    }
}

/// Fetch SOL and all token balances via individual RPC requests.
pub async fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| crate::Error::Config(e.to_string()))?;

    // Get SOL balance
    let sol_resp = rpc_call(&client, rpc_url, "getBalance", serde_json::json!([pubkey, { "commitment": "confirmed" }])).await?;
    let sol_lamports = sol_resp["result"]["value"].as_u64().unwrap_or(0);

    // Get token accounts from both token programs
    let mut tokens = Vec::new();
    for program_id in TOKEN_PROGRAMS {
        let resp = rpc_call(
            &client,
            rpc_url,
            "getTokenAccountsByOwner",
            serde_json::json!([pubkey, { "programId": program_id }, { "encoding": "jsonParsed", "commitment": "confirmed" }]),
        )
        .await;

        if let Ok(resp) = resp {
            parse_token_accounts(&resp["result"], &mut tokens);
        }
    }

    Ok(AccountBalances { sol_lamports, tokens })
}

async fn rpc_call(
    client: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| crate::Error::Config(format!("RPC error: {e}")))?;

    if resp.status() == 429 {
        return Err(crate::Error::Config("RPC rate limited (429)".to_string()));
    }

    let result: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| crate::Error::Config(format!("RPC parse error: {e}")))?;

    if let Some(err) = result.get("error") {
        return Err(crate::Error::Config(format!("RPC error: {err}")));
    }

    Ok(result)
}

fn parse_token_accounts(result: &serde_json::Value, tokens: &mut Vec<TokenBalance>) {
    let accounts = match result["value"].as_array() {
        Some(arr) => arr,
        None => return,
    };

    for entry in accounts {
        let info = &entry["account"]["data"]["parsed"]["info"];
        let mint = match info["mint"].as_str() {
            Some(m) => m,
            None => continue,
        };
        let token_amount = &info["tokenAmount"];
        let ui = token_amount["uiAmount"].as_f64().unwrap_or(0.0);
        let raw = token_amount["amount"].as_str().unwrap_or("0");

        if ui > 0.0 || raw != "0" {
            tokens.push(TokenBalance {
                mint: mint.to_string(),
                ui_amount: ui,
                symbol: mint_symbol(mint),
            });
        }
    }
}
