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

/// Fetch SOL and all token balances in a single batched RPC request.
///
/// Uses JSON-RPC batch to avoid rate limiting on public endpoints.
pub fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| crate::Error::Config(e.to_string()))?;

    // Build batch: [getBalance, getTokenAccountsByOwner(SPL), getTokenAccountsByOwner(Token-2022)]
    // Use "confirmed" commitment for faster detection (vs default "finalized").
    let mut batch = vec![serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "getBalance",
        "params": [pubkey, { "commitment": "confirmed" }]
    })];

    for (i, program_id) in TOKEN_PROGRAMS.iter().enumerate() {
        batch.push(serde_json::json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": "getTokenAccountsByOwner",
            "params": [
                pubkey,
                { "programId": program_id },
                { "encoding": "jsonParsed", "commitment": "confirmed" }
            ]
        }));
    }

    let resp = client
        .post(rpc_url)
        .json(&batch)
        .send()
        .map_err(|e| crate::Error::Config(format!("RPC error: {e}")))?;

    // Detect HTTP-level rate limiting
    if resp.status() == 429 {
        return Err(crate::Error::Config("RPC rate limited (429)".to_string()));
    }

    let responses: Vec<serde_json::Value> = resp
        .json()
        .map_err(|e| crate::Error::Config(format!("RPC parse error: {e}")))?;

    let mut sol_lamports = 0u64;
    let mut tokens = Vec::new();
    let mut any_error = false;

    for item in &responses {
        // Detect JSON-RPC level errors (429 inside batch)
        if item.get("error").is_some() {
            any_error = true;
            continue;
        }

        let id = item["id"].as_u64().unwrap_or(u64::MAX);
        let result = &item["result"];

        if id == 0 {
            sol_lamports = result["value"].as_u64().unwrap_or(0);
        } else {
            parse_token_accounts(result, &mut tokens);
        }
    }

    // If all responses were errors, report failure
    if any_error && tokens.is_empty() && sol_lamports == 0 {
        return Err(crate::Error::Config(
            "RPC returned errors for all requests".to_string(),
        ));
    }

    Ok(AccountBalances {
        sol_lamports,
        tokens,
    })
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
