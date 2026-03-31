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
    let sol_resp = rpc_call(
        &client,
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
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

    Ok(AccountBalances {
        sol_lamports,
        tokens,
    })
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

    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
        }
        let resp = client
            .post(rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| crate::Error::Config(format!("RPC error: {e}")))?;

        if resp.status() == 429 {
            last_err = Some(crate::Error::Config("RPC rate limited (429)".to_string()));
            continue;
        }

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| crate::Error::Config(format!("RPC parse error: {e}")))?;

        if let Some(err) = result.get("error") {
            return Err(crate::Error::Config(format!("RPC error: {err}")));
        }

        return Ok(result);
    }

    Err(last_err.unwrap_or_else(|| crate::Error::Config("RPC failed".to_string())))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_symbol_usdc() {
        assert_eq!(
            mint_symbol("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            Some("USDC")
        );
    }

    #[test]
    fn mint_symbol_usdt() {
        assert_eq!(
            mint_symbol("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
            Some("USDT")
        );
    }

    #[test]
    fn mint_symbol_unknown() {
        assert_eq!(
            mint_symbol("SomeRandomMint1111111111111111111111111111"),
            None
        );
    }

    #[test]
    fn mainnet_rpc_url_default() {
        // Unset the env var to test default
        // SAFETY: called in single-threaded test context
        unsafe { std::env::remove_var("PAY_MAINNET_RPC_URL") };
        assert_eq!(mainnet_rpc_url(), "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn account_balances_default() {
        let b = AccountBalances::default();
        assert_eq!(b.sol_lamports, 0);
        assert!(b.tokens.is_empty());
    }

    #[test]
    fn received_funds_has_any_sol() {
        let r = ReceivedFunds {
            sol_lamports: 100,
            tokens: vec![],
        };
        assert!(r.has_any());
    }

    #[test]
    fn received_funds_has_any_tokens() {
        let r = ReceivedFunds {
            sol_lamports: 0,
            tokens: vec![ReceivedToken {
                mint: "abc".to_string(),
                ui_amount: 1.0,
                symbol: None,
            }],
        };
        assert!(r.has_any());
    }

    #[test]
    fn received_funds_has_any_empty() {
        let r = ReceivedFunds {
            sol_lamports: 0,
            tokens: vec![],
        };
        assert!(!r.has_any());
    }

    #[test]
    fn diff_received_sol_increase() {
        let baseline = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
        };
        let current = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 1_000_000);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_sol_decrease_is_zero() {
        let baseline = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
        };
        let current = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 0);
    }

    #[test]
    fn diff_received_token_increase() {
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                ui_amount: 10.0,
                symbol: Some("USDC"),
            }],
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                ui_amount: 25.5,
                symbol: Some("USDC"),
            }],
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.tokens.len(), 1);
        assert!((diff.tokens[0].ui_amount - 15.5).abs() < f64::EPSILON);
        assert_eq!(diff.tokens[0].symbol, Some("USDC"));
    }

    #[test]
    fn diff_received_new_token() {
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "NEW_MINT".to_string(),
                ui_amount: 100.0,
                symbol: None,
            }],
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.tokens.len(), 1);
        assert!((diff.tokens[0].ui_amount - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn diff_received_no_change() {
        let balances = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                ui_amount: 50.0,
                symbol: Some("USDC"),
            }],
        };
        let diff = balances.diff_received(&balances);
        assert_eq!(diff.sol_lamports, 0);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn parse_token_accounts_empty_value() {
        let result = serde_json::json!({"value": []});
        let mut tokens = vec![];
        parse_token_accounts(&result, &mut tokens);
        assert!(tokens.is_empty());
    }

    #[test]
    fn parse_token_accounts_null_value() {
        let result = serde_json::json!({"value": null});
        let mut tokens = vec![];
        parse_token_accounts(&result, &mut tokens);
        assert!(tokens.is_empty());
    }

    #[test]
    fn parse_token_accounts_with_data() {
        let result = serde_json::json!({
            "value": [{
                "account": {
                    "data": {
                        "parsed": {
                            "info": {
                                "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                                "tokenAmount": {
                                    "uiAmount": 100.5,
                                    "amount": "100500000"
                                }
                            }
                        }
                    }
                }
            }]
        });
        let mut tokens = vec![];
        parse_token_accounts(&result, &mut tokens);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].symbol, Some("USDC"));
        assert!((tokens[0].ui_amount - 100.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_token_accounts_skips_zero_balance() {
        let result = serde_json::json!({
            "value": [{
                "account": {
                    "data": {
                        "parsed": {
                            "info": {
                                "mint": "SomeMint",
                                "tokenAmount": {
                                    "uiAmount": 0.0,
                                    "amount": "0"
                                }
                            }
                        }
                    }
                }
            }]
        });
        let mut tokens = vec![];
        parse_token_accounts(&result, &mut tokens);
        assert!(tokens.is_empty());
    }

    #[test]
    fn parse_token_accounts_missing_mint() {
        let result = serde_json::json!({
            "value": [{
                "account": {
                    "data": {
                        "parsed": {
                            "info": {
                                "tokenAmount": {
                                    "uiAmount": 10.0,
                                    "amount": "10000000"
                                }
                            }
                        }
                    }
                }
            }]
        });
        let mut tokens = vec![];
        parse_token_accounts(&result, &mut tokens);
        assert!(tokens.is_empty());
    }
}
