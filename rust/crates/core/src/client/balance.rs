//! Wallet balance lookups.
//!
//! - SOL is fetched directly from a Solana JSON-RPC endpoint (`getBalance` /
//!   `getMultipleAccounts`).
//! - Token balances come from the **pay-api** stablecoin service
//!   (`GET /v1/balance/stablecoins`). pay-api derives ATAs locally and does a
//!   single `getMultipleAccounts` call against its own configured RPC, so we
//!   pay one HTTP round trip here rather than scanning every token account.
//!
//! Environment variables:
//! - `PAY_MAINNET_RPC_URL` — override the default Solana mainnet RPC.
//! - `PAY_API_URL`         — override the pay-api host (default [`DEFAULT_PAY_API_URL`]).

use pay_types::Stablecoin;
use serde::Deserialize;

/// Default pay-api host. Override with `PAY_API_URL`.
pub const DEFAULT_PAY_API_URL: &str = "https://api.gateway-402.com";

/// Default mainnet RPC URL. Override with `PAY_MAINNET_RPC_URL`.
pub fn mainnet_rpc_url() -> String {
    std::env::var("PAY_MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string())
}

/// Default pay-api host. Override with `PAY_API_URL`.
pub fn pay_api_url() -> String {
    std::env::var("PAY_API_URL").unwrap_or_else(|_| DEFAULT_PAY_API_URL.to_string())
}

fn mint_symbol(mint: &str) -> Option<&'static str> {
    Stablecoin::symbol_for_mint(mint)
}

/// Map an RPC URL to the network name pay-api expects.
fn infer_network(rpc_url: &str) -> &'static str {
    let lower = rpc_url.to_lowercase();
    if lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("surfnet")
        || lower.contains("surfpool")
    {
        "sandbox"
    } else {
        "mainnet"
    }
}

#[derive(Debug, Clone)]
pub struct TokenBalance {
    pub mint: String,
    pub raw_amount: u64,
    pub ui_amount: f64,
    pub symbol: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct AccountBalances {
    pub sol_lamports: u64,
    pub tokens: Vec<TokenBalance>,
    /// True when the pay-api stablecoin lookup failed for this account (e.g.
    /// pay-api unreachable). `tokens` will be empty in that case; callers
    /// should render an "unavailable" indicator instead of treating the
    /// account as zero-balance.
    pub tokens_unavailable: bool,
}

impl AccountBalances {
    pub fn diff_received(&self, baseline: &AccountBalances) -> ReceivedFunds {
        let sol_gained = self.sol_lamports.saturating_sub(baseline.sol_lamports);
        // If either side could not reach pay-api, the token list on that side
        // is missing rather than zero — diffing it would falsely report the
        // entire other side as "received". Skip token diff in that case; SOL
        // is still safe because it comes from RPC.
        let mut tokens = Vec::new();
        if !self.tokens_unavailable && !baseline.tokens_unavailable {
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

// ── pay-api wire types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse {
    balances: Vec<ApiBalance>,
}

#[derive(Deserialize)]
struct ApiBalance {
    mint: String,
    raw_amount: String,
    ui_amount: f64,
    // symbol & decimals are also returned but we recompute the symbol locally
    // so the rest of pay keeps a stable `Option<&'static str>`.
}

async fn fetch_stablecoins_via_api(
    client: &reqwest::Client,
    api_url: &str,
    pubkey: &str,
    network: &str,
) -> crate::Result<Vec<TokenBalance>> {
    let url = format!(
        "{}/v1/balance/stablecoins?address={}&network={}",
        api_url.trim_end_matches('/'),
        pubkey,
        network,
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| crate::Error::Config(format!("pay-api request error: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::Error::Config(format!(
            "pay-api returned HTTP {status}: {body}"
        )));
    }

    let parsed: ApiResponse = resp
        .json()
        .await
        .map_err(|e| crate::Error::Config(format!("pay-api decode error: {e}")))?;

    Ok(parsed
        .balances
        .into_iter()
        .filter_map(|b| {
            let raw: u64 = b.raw_amount.parse().ok()?;
            // Match the previous behaviour: skip zero balances.
            if raw == 0 {
                return None;
            }
            let symbol = mint_symbol(&b.mint);
            Some(TokenBalance {
                mint: b.mint,
                raw_amount: raw,
                ui_amount: b.ui_amount,
                symbol,
            })
        })
        .collect())
}

// ── public API ──────────────────────────────────────────────────────────────

/// Fetch SOL (direct RPC) and stablecoin balances (via pay-api) for a single pubkey.
pub async fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| crate::Error::Config(e.to_string()))?;

    let sol_resp = rpc_call(
        &client,
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
    let sol_lamports = sol_resp["result"]["value"].as_u64().unwrap_or(0);

    let (tokens, tokens_unavailable) =
        match fetch_stablecoins_via_api(&client, &pay_api_url(), pubkey, infer_network(rpc_url))
            .await
        {
            Ok(t) => (t, false),
            Err(e) => {
                tracing::debug!(error = %e, "pay-api unreachable; returning empty token balances");
                (Vec::new(), true)
            }
        };

    Ok(AccountBalances {
        sol_lamports,
        tokens,
        tokens_unavailable,
    })
}

/// Fetch SOL and stablecoin balances for multiple pubkeys efficiently.
///
/// SOL: one `getMultipleAccounts` call.
/// Tokens: one concurrent pay-api call per pubkey.
pub async fn get_balances_batch(
    rpc_url: &str,
    pubkeys: &[String],
) -> std::collections::HashMap<String, AccountBalances> {
    if pubkeys.is_empty() {
        return std::collections::HashMap::new();
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return std::collections::HashMap::new(),
    };

    // Initialise every pubkey with zero balances so missing entries still surface.
    let mut balances: std::collections::HashMap<String, AccountBalances> = pubkeys
        .iter()
        .map(|pk| (pk.clone(), AccountBalances::default()))
        .collect();

    // ── SOL: one getMultipleAccounts call ────────────────────────────────
    if let Ok(resp) = rpc_call(
        &client,
        rpc_url,
        "getMultipleAccounts",
        serde_json::json!([pubkeys, { "commitment": "confirmed" }]),
    )
    .await
        && let Some(accounts) = resp["result"]["value"].as_array()
    {
        for (pk, account) in pubkeys.iter().zip(accounts.iter()) {
            let lamports = account["lamports"].as_u64().unwrap_or(0);
            if let Some(entry) = balances.get_mut(pk) {
                entry.sol_lamports = lamports;
            }
        }
    }

    // ── Tokens: concurrent pay-api calls ─────────────────────────────────
    let api = pay_api_url();
    let network = infer_network(rpc_url);
    let mut set = tokio::task::JoinSet::new();
    for pk in pubkeys {
        let client = client.clone();
        let api = api.clone();
        let pk = pk.clone();
        set.spawn(async move {
            (
                pk.clone(),
                fetch_stablecoins_via_api(&client, &api, &pk, network).await,
            )
        });
    }

    while let Some(Ok((pk, result))) = set.join_next().await {
        match result {
            Ok(tokens) => {
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens = tokens;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, %pk, "pay-api token fetch failed");
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens_unavailable = true;
                }
            }
        }
    }

    balances
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
    fn mint_symbol_usdg() {
        assert_eq!(
            mint_symbol("2u1tszSeqZ3qBWF3uNGPFc8TzMk2tdiwknnRMWGWjGWH"),
            Some("USDG")
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
        // SAFETY: called in single-threaded test context
        unsafe { std::env::remove_var("PAY_MAINNET_RPC_URL") };
        assert_eq!(mainnet_rpc_url(), "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn pay_api_url_default() {
        // SAFETY: called in single-threaded test context
        unsafe { std::env::remove_var("PAY_API_URL") };
        assert_eq!(pay_api_url(), DEFAULT_PAY_API_URL);
    }

    #[test]
    fn infer_network_classifies_local_and_mainnet() {
        assert_eq!(infer_network("http://127.0.0.1:8899"), "sandbox");
        assert_eq!(infer_network("http://localhost:8899"), "sandbox");
        assert_eq!(infer_network("https://402.surfnet.dev:8899"), "sandbox");
        assert_eq!(
            infer_network("https://api.mainnet-beta.solana.com"),
            "mainnet"
        );
        assert_eq!(infer_network("https://my-helius.example.com"), "mainnet");
    }

    #[test]
    fn account_balances_default() {
        let b = AccountBalances::default();
        assert_eq!(b.sol_lamports, 0);
        assert!(b.tokens.is_empty());
        assert!(!b.tokens_unavailable);
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
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
            tokens_unavailable: false,
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
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
            tokens_unavailable: false,
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
                raw_amount: 10_000_000,
                ui_amount: 10.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                raw_amount: 25_500_000,
                ui_amount: 25.5,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
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
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "NEW_MINT".to_string(),
                raw_amount: 100_000_000,
                ui_amount: 100.0,
                symbol: None,
            }],
            tokens_unavailable: false,
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
                raw_amount: 50_000_000,
                ui_amount: 50.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let diff = balances.diff_received(&balances);
        assert_eq!(diff.sol_lamports, 0);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_skips_token_diff_when_baseline_unavailable() {
        // Baseline was captured while pay-api was offline → its empty
        // tokens list is missing data, not a true zero. Diffing against a
        // healthy `current` that shows funds must NOT report a "received".
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 5_000_000,
                ui_amount: 5.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_skips_token_diff_when_current_unavailable() {
        // Mid-poll pay-api blip: current.tokens is empty but
        // tokens_unavailable=true. Don't report negative deltas as anything.
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 5_000_000,
                ui_amount: 5.0,
                symbol: Some("USDC"),
            }],
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let diff = current.diff_received(&baseline);
        assert!(diff.tokens.is_empty());
    }

    #[test]
    fn diff_received_still_diffs_sol_when_tokens_unavailable() {
        // SOL comes from RPC, not pay-api, so it remains trustworthy even
        // when tokens_unavailable is set.
        let baseline = AccountBalances {
            sol_lamports: 100,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 1_000,
            tokens: vec![],
            tokens_unavailable: true,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 900);
        assert!(diff.tokens.is_empty());
    }
}
