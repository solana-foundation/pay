//! Wallet balance lookups.
//!
//! - SOL is fetched directly from a Solana JSON-RPC endpoint (`getBalance` /
//!   `getMultipleAccounts`).
//! - Token balances normally come from the **pay-api** stablecoin service
//!   (`GET /v1/balance/stablecoins`). pay-api derives ATAs locally and does a
//!   single `getMultipleAccounts` call against its own configured RPC, so we
//!   pay one HTTP round trip here rather than scanning every token account.
//!   A direct Solana RPC lookup across SPL Token and Token-2022 is the fallback.
//!
//! Environment variables:
//! - `PAY_MAINNET_RPC_URL` — override the default Solana mainnet RPC.
//! - `PAY_API_URL`         — override the pay-api host (default [`DEFAULT_PAY_API_URL`]).

use pay_types::Stablecoin;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Default pay-api host. Override with `PAY_API_URL`.
pub const DEFAULT_PAY_API_URL: &str = "https://api.pay.sh";

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
        || lower.contains("devnet")
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
    pub symbol: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CreditBalance {
    pub program_id: String,
    pub accounts: Vec<String>,
    pub currency: String,
    pub raw_amount: u64,
    pub ui_amount: f64,
}

impl TokenBalance {
    /// Display symbol, falling back to `fallback` when the mint is unknown.
    pub fn symbol_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.symbol.as_deref().unwrap_or(fallback)
    }

    /// Case-insensitive symbol match, e.g. `token.is_symbol("USDC")`.
    pub fn is_symbol(&self, symbol: &str) -> bool {
        self.symbol
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(symbol))
    }

    /// Parse the symbol into a known [`Stablecoin`](pay_types::Stablecoin).
    pub fn currency(&self) -> Option<pay_types::Stablecoin> {
        self.symbol
            .as_deref()
            .and_then(pay_types::Stablecoin::parse_symbol)
    }
}

#[derive(Debug, Clone, Default)]
pub struct AccountBalances {
    pub sol_lamports: u64,
    pub tokens: Vec<TokenBalance>,
    pub credits: Vec<CreditBalance>,
    /// Still-committable escrow held in open payment channels, grouped by mint.
    pub committable_channels: Vec<TokenBalance>,
    pub channel_balances_unavailable: bool,
    /// True when pay-api returned token balances but could not determine
    /// program-backed credit balances.
    pub credits_unavailable: bool,
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
                        symbol: current.symbol.clone(),
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
    pub symbol: Option<String>,
}

impl ReceivedToken {
    /// Display symbol, falling back to `fallback` when the mint is unknown.
    pub fn symbol_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.symbol.as_deref().unwrap_or(fallback)
    }

    /// Case-insensitive symbol match, e.g. `token.is_symbol("USDC")`.
    pub fn is_symbol(&self, symbol: &str) -> bool {
        self.symbol
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(symbol))
    }
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
    #[serde(default)]
    committable_channel_balances: Vec<ApiBalance>,
    #[serde(default)]
    channel_balances_unavailable: bool,
    #[serde(default)]
    credits: std::collections::BTreeMap<String, ApiCredit>,
    #[serde(default)]
    credits_unavailable: bool,
}

#[derive(Deserialize)]
struct ApiBalance {
    mint: String,
    raw_amount: String,
    ui_amount: f64,
    /// Symbol resolved by the stablecoin API — authoritative, and knows mints
    /// the local `Stablecoin` list doesn't (e.g. USDPT). `decimals` is also
    /// returned but unused here.
    #[serde(default)]
    symbol: Option<String>,
}

#[derive(Deserialize)]
struct ApiCredit {
    #[serde(default)]
    accounts: Vec<String>,
    currency: String,
    raw_amount: String,
    ui_amount: f64,
}

struct ApiBalances {
    tokens: Vec<TokenBalance>,
    committable_channels: Vec<TokenBalance>,
    channel_balances_unavailable: bool,
    credits: Vec<CreditBalance>,
    credits_unavailable: bool,
}

async fn fetch_stablecoins_via_api(
    client: &reqwest::Client,
    api_url: &str,
    pubkey: &str,
    network: &str,
) -> crate::Result<ApiBalances> {
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

    Ok(parse_api_balances(parsed))
}

/// Read supported stablecoins directly from Solana when pay-api is
/// unavailable. Both token programs are queried because USDPT uses Token-2022
/// while the other currently supported mainnet stablecoins use SPL Token.
async fn fetch_stablecoins_via_rpc(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkey: &str,
    spendable_only: bool,
) -> crate::Result<Vec<TokenBalance>> {
    let mut responses = Vec::with_capacity(2);
    for program_id in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
        let mut response = rpc_call(
            client,
            rpc_url,
            "getTokenAccountsByOwner",
            serde_json::json!([
                pubkey,
                { "programId": program_id },
                { "encoding": "jsonParsed", "commitment": "confirmed" }
            ]),
        )
        .await?;
        if spendable_only {
            retain_associated_token_accounts(&mut response, pubkey, program_id)?;
        }
        responses.push(response);
    }
    Ok(parse_rpc_stablecoin_balances(&responses))
}

fn retain_associated_token_accounts(
    response: &mut serde_json::Value,
    owner: &str,
    token_program: &str,
) -> crate::Result<()> {
    use solana_pubkey::Pubkey;
    use std::str::FromStr;

    let owner = Pubkey::from_str(owner).map_err(|e| crate::Error::Config(e.to_string()))?;
    let token_program =
        Pubkey::from_str(token_program).map_err(|e| crate::Error::Config(e.to_string()))?;
    let associated_program = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
        .expect("associated token program is valid");
    if let Some(accounts) = response["result"]["value"].as_array_mut() {
        accounts.retain(|account| {
            let Some(mint) = account["account"]["data"]["parsed"]["info"]["mint"]
                .as_str()
                .and_then(|mint| Pubkey::from_str(mint).ok())
            else {
                return false;
            };
            let (ata, _) = Pubkey::find_program_address(
                &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
                &associated_program,
            );
            account["pubkey"].as_str() == Some(ata.to_string().as_str())
        });
    }
    Ok(())
}

fn parse_rpc_stablecoin_balances(responses: &[serde_json::Value]) -> Vec<TokenBalance> {
    let mut amounts = BTreeMap::<String, u64>::new();
    for response in responses {
        let Some(accounts) = response["result"]["value"].as_array() else {
            continue;
        };
        for account in accounts {
            let info = &account["account"]["data"]["parsed"]["info"];
            let Some(mint) = info["mint"].as_str() else {
                continue;
            };
            if Stablecoin::from_mint(mint).is_none() {
                continue;
            }
            let Some(raw) = info["tokenAmount"]["amount"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            *amounts.entry(mint.to_string()).or_default() += raw;
        }
    }
    amounts
        .into_iter()
        .filter(|(_, raw_amount)| *raw_amount > 0)
        .map(|(mint, raw_amount)| {
            let currency = Stablecoin::from_mint(&mint).expect("known mint was filtered above");
            TokenBalance {
                mint,
                raw_amount,
                ui_amount: raw_amount as f64 / 10_f64.powi(i32::from(currency.decimals())),
                symbol: Some(currency.symbol().to_string()),
            }
        })
        .collect()
}

async fn fetch_stablecoins_with_rpc_fallback(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkey: &str,
    spendable_only: bool,
) -> (ApiBalances, bool) {
    fetch_stablecoins_from_endpoints(client, &pay_api_url(), rpc_url, pubkey, spendable_only).await
}

async fn fetch_stablecoins_from_endpoints(
    client: &reqwest::Client,
    api_url: &str,
    rpc_url: &str,
    pubkey: &str,
    spendable_only: bool,
) -> (ApiBalances, bool) {
    match fetch_stablecoins_via_api(client, api_url, pubkey, infer_network(rpc_url)).await {
        Ok(balances) => (balances, false),
        Err(api_error) => {
            match fetch_stablecoins_via_rpc(client, rpc_url, pubkey, spendable_only).await {
                Ok(tokens) => {
                    tracing::debug!(error = %api_error, "pay-api unreachable; used direct RPC stablecoin fallback");
                    (
                        ApiBalances {
                            tokens,
                            committable_channels: Vec::new(),
                            channel_balances_unavailable: true,
                            credits: Vec::new(),
                            credits_unavailable: true,
                        },
                        false,
                    )
                }
                Err(rpc_error) => {
                    tracing::debug!(%api_error, %rpc_error, "stablecoin balance lookup unavailable from pay-api and RPC");
                    (
                        ApiBalances {
                            tokens: Vec::new(),
                            committable_channels: Vec::new(),
                            channel_balances_unavailable: true,
                            credits: Vec::new(),
                            credits_unavailable: true,
                        },
                        true,
                    )
                }
            }
        }
    }
}

fn parse_api_balances(parsed: ApiResponse) -> ApiBalances {
    let tokens = parsed
        .balances
        .into_iter()
        .filter_map(|b| {
            let raw: u64 = b.raw_amount.parse().ok()?;
            // Match the previous behaviour: skip zero balances.
            if raw == 0 {
                return None;
            }
            // Prefer the symbol the stablecoin API returned (it knows mints
            // our local list doesn't, e.g. USDPT); fall back to the local list
            // only when the API omitted one.
            let symbol = b
                .symbol
                .filter(|s| !s.trim().is_empty())
                .or_else(|| mint_symbol(&b.mint).map(str::to_string));
            Some(TokenBalance {
                mint: b.mint,
                raw_amount: raw,
                ui_amount: b.ui_amount,
                symbol,
            })
        })
        .collect();
    let committable_channels = parsed
        .committable_channel_balances
        .into_iter()
        .filter_map(api_token_balance)
        .collect();
    let credits = parsed
        .credits
        .into_iter()
        .filter_map(|(program_id, credit)| {
            let raw_amount = credit.raw_amount.parse().ok()?;
            if raw_amount == 0 {
                return None;
            }
            Some(CreditBalance {
                program_id,
                accounts: credit.accounts,
                currency: credit.currency,
                raw_amount,
                ui_amount: credit.ui_amount,
            })
        })
        .collect();
    ApiBalances {
        tokens,
        committable_channels,
        channel_balances_unavailable: parsed.channel_balances_unavailable,
        credits,
        credits_unavailable: parsed.credits_unavailable,
    }
}

fn api_token_balance(balance: ApiBalance) -> Option<TokenBalance> {
    let raw_amount = balance.raw_amount.parse().ok()?;
    if raw_amount == 0 {
        return None;
    }
    let symbol = balance
        .symbol
        .filter(|symbol| !symbol.trim().is_empty())
        .or_else(|| mint_symbol(&balance.mint).map(str::to_string));
    Some(TokenBalance {
        mint: balance.mint,
        raw_amount,
        ui_amount: balance.ui_amount,
        symbol,
    })
}

// ── public API ──────────────────────────────────────────────────────────────

/// Fetch a wallet's SOL balance in lamports via the standard
/// `getBalance` RPC. Returns the raw u64 — callers that need
/// stablecoin balances too should call [`get_balances`].
pub async fn get_sol_balance(rpc_url: &str, pubkey: &str) -> crate::Result<u64> {
    let client = balance_client()?;
    let resp = rpc_call(
        &client,
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
    Ok(resp["result"]["value"].as_u64().unwrap_or(0))
}

/// Fetch SOL (direct RPC) and stablecoin balances (via pay-api) for a single pubkey.
pub async fn get_balances(rpc_url: &str, pubkey: &str) -> crate::Result<AccountBalances> {
    let client = balance_client()?;

    let sol_resp = rpc_call(
        &client,
        rpc_url,
        "getBalance",
        serde_json::json!([pubkey, { "commitment": "confirmed" }]),
    )
    .await?;
    let sol_lamports = sol_resp["result"]["value"].as_u64().unwrap_or(0);

    let (api_balances, tokens_unavailable) =
        fetch_stablecoins_with_rpc_fallback(&client, rpc_url, pubkey, false).await;

    Ok(AccountBalances {
        sol_lamports,
        tokens: api_balances.tokens,
        credits: api_balances.credits,
        committable_channels: api_balances.committable_channels,
        channel_balances_unavailable: api_balances.channel_balances_unavailable,
        credits_unavailable: api_balances.credits_unavailable,
        tokens_unavailable,
    })
}

/// Fetch stablecoin holdings via pay-api, with an owner-account RPC fallback.
///
/// This skips the SOL lookup. The fallback includes custom token accounts for
/// display; payment selection must use [`get_spendable_stablecoin_balances`].
pub async fn get_stablecoin_balances(
    rpc_url: &str,
    pubkey: &str,
) -> crate::Result<AccountBalances> {
    stablecoin_balances(rpc_url, pubkey, false).await
}

/// Fetch balances available to payment builders, which debit associated token accounts.
///
/// pay-api already returns ATA balances. Unlike display balances, the RPC
/// fallback excludes custom token accounts for both SPL Token and Token-2022.
pub async fn get_spendable_stablecoin_balances(
    rpc_url: &str,
    pubkey: &str,
) -> crate::Result<AccountBalances> {
    stablecoin_balances(rpc_url, pubkey, true).await
}

async fn stablecoin_balances(
    rpc_url: &str,
    pubkey: &str,
    spendable_only: bool,
) -> crate::Result<AccountBalances> {
    let client = balance_client()?;
    let (api_balances, tokens_unavailable) =
        fetch_stablecoins_with_rpc_fallback(&client, rpc_url, pubkey, spendable_only).await;

    Ok(AccountBalances {
        sol_lamports: 0,
        tokens: api_balances.tokens,
        credits: api_balances.credits,
        committable_channels: api_balances.committable_channels,
        channel_balances_unavailable: api_balances.channel_balances_unavailable,
        credits_unavailable: api_balances.credits_unavailable,
        tokens_unavailable,
    })
}

fn balance_client() -> crate::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| crate::Error::Config(e.to_string()))
}

/// Fetch SOL and stablecoin balances for multiple pubkeys efficiently.
///
/// SOL: one `getMultipleAccounts` call.
/// Tokens: one concurrent pay-api call per pubkey.
pub async fn get_balances_batch(
    rpc_url: &str,
    pubkeys: &[String],
) -> HashMap<String, AccountBalances> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }

    let client = match balance_client() {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    // Initialise every pubkey with zero balances so missing entries still surface.
    let mut balances: HashMap<String, AccountBalances> = pubkeys
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

    fetch_stablecoin_balances_batch_into(&client, rpc_url, pubkeys, &mut balances).await;

    balances
}

/// Fetch only stablecoin balances for multiple pubkeys.
///
/// This skips the direct Solana RPC `getMultipleAccounts` request and uses one
/// concurrent pay-api call per pubkey.
pub async fn get_stablecoin_balances_batch(
    rpc_url: &str,
    pubkeys: &[String],
) -> HashMap<String, AccountBalances> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }

    let client = match balance_client() {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut balances: HashMap<String, AccountBalances> = pubkeys
        .iter()
        .map(|pk| (pk.clone(), AccountBalances::default()))
        .collect();

    fetch_stablecoin_balances_batch_into(&client, rpc_url, pubkeys, &mut balances).await;

    balances
}

async fn fetch_stablecoin_balances_batch_into(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkeys: &[String],
    balances: &mut HashMap<String, AccountBalances>,
) {
    fetch_stablecoin_balances_batch_from_api(client, &pay_api_url(), rpc_url, pubkeys, balances)
        .await;
}

async fn fetch_stablecoin_balances_batch_from_api(
    client: &reqwest::Client,
    api: &str,
    rpc_url: &str,
    pubkeys: &[String],
    balances: &mut HashMap<String, AccountBalances>,
) {
    let network = infer_network(rpc_url);
    let mut set = tokio::task::JoinSet::new();
    for pk in pubkeys {
        let client = client.clone();
        let api = api.to_string();
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
            Ok(api_balances) => {
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens = api_balances.tokens;
                    entry.committable_channels = api_balances.committable_channels;
                    entry.channel_balances_unavailable = api_balances.channel_balances_unavailable;
                    entry.credits = api_balances.credits;
                    entry.credits_unavailable = api_balances.credits_unavailable;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, %pk, "pay-api token fetch failed");
                if let Some(entry) = balances.get_mut(&pk) {
                    entry.tokens_unavailable = true;
                    entry.credits_unavailable = true;
                    entry.channel_balances_unavailable = true;
                }
            }
        }
    }
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

    async fn http_fixture(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (status, response) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let body = response.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (url, task)
    }

    #[tokio::test]
    async fn batch_api_preserves_channel_balances_and_availability() {
        let response = serde_json::json!({
            "balances": [],
            "committable_channel_balances": [{
                "mint": Stablecoin::Usdc.mint(None).to_string(),
                "raw_amount": "700", "ui_amount": 0.0007, "symbol": "USDC"
            }],
            "channel_balances_unavailable": true
        });
        let (api, task) = http_fixture(vec![
            (200, response.clone()),
            (200, response),
            (503, serde_json::json!({})),
        ])
        .await;
        let client = balance_client().unwrap();
        let single = fetch_stablecoins_via_api(&client, &api, "payer", "mainnet")
            .await
            .unwrap();
        let keys = vec!["payer".to_string()];
        let mut batch = HashMap::from([("payer".to_string(), AccountBalances::default())]);
        fetch_stablecoin_balances_batch_from_api(&client, &api, "mainnet", &keys, &mut batch).await;
        assert_eq!(single.committable_channels.len(), 1);
        assert_eq!(
            batch["payer"].committable_channels[0].raw_amount,
            single.committable_channels[0].raw_amount
        );
        assert_eq!(
            batch["payer"].channel_balances_unavailable,
            single.channel_balances_unavailable
        );
        let mut failed = HashMap::from([("payer".to_string(), AccountBalances::default())]);
        fetch_stablecoin_balances_batch_from_api(&client, &api, "mainnet", &keys, &mut failed)
            .await;
        assert!(failed["payer"].channel_balances_unavailable);
        assert!(failed["payer"].tokens_unavailable);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rpc_fallback_keeps_custom_holdings_for_display_but_not_funding() {
        use solana_pubkey::Pubkey;
        use std::str::FromStr;
        let owner = Pubkey::new_unique();
        let token_program = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
        let associated_program =
            Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
        let account = |currency: Stablecoin, raw: &str, custom: bool| {
            let mint = currency.mint(None).to_string();
            let mint_key = Pubkey::from_str(&mint).unwrap();
            let ata = Pubkey::find_program_address(
                &[owner.as_ref(), token_program.as_ref(), mint_key.as_ref()],
                &associated_program,
            )
            .0;
            serde_json::json!({
                "pubkey": if custom { Pubkey::new_unique().to_string() } else { ata.to_string() },
                "account": {"data": {"parsed": {"info": {"mint": mint, "tokenAmount": {"amount": raw}}}}}
            })
        };
        let tokens = serde_json::json!({"result": {"value": [
            account(Stablecoin::Usdc, "10000000", true),
            account(Stablecoin::Usdc, "0", false),
            account(Stablecoin::Usdt, "2000000", false)
        ]}});
        let empty = serde_json::json!({"result": {"value": []}});
        let (api, api_task) = http_fixture(vec![(503, serde_json::json!({})); 2]).await;
        let (rpc, rpc_task) = http_fixture(vec![
            (200, tokens.clone()),
            (200, empty.clone()),
            (200, tokens),
            (200, empty),
        ])
        .await;
        let client = balance_client().unwrap();
        let (display, unavailable) =
            fetch_stablecoins_from_endpoints(&client, &api, &rpc, &owner.to_string(), false).await;
        assert!(!unavailable);
        assert_eq!(display.tokens.len(), 2);
        assert_eq!(
            display
                .tokens
                .iter()
                .find(|t| t.is_symbol("USDC"))
                .unwrap()
                .raw_amount,
            10_000_000
        );
        let (funding, unavailable) =
            fetch_stablecoins_from_endpoints(&client, &api, &rpc, &owner.to_string(), true).await;
        assert!(!unavailable);
        assert_eq!(funding.tokens.len(), 1);
        assert!(funding.tokens[0].is_symbol("USDT"));
        assert_eq!(funding.tokens[0].raw_amount, 2_000_000);
        api_task.await.unwrap();
        rpc_task.await.unwrap();
    }

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
        assert_eq!(infer_network("https://api.devnet.solana.com"), "sandbox");
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
        assert!(b.credits.is_empty());
        assert!(!b.credits_unavailable);
        assert!(!b.tokens_unavailable);
    }

    #[test]
    fn api_credits_are_kept_separate_from_wallet_tokens() {
        let parsed: ApiResponse = serde_json::from_value(serde_json::json!({
            "balances": [],
            "credits": {
                "FD1amxhTsDpwzoVX41dxp2ygAESURV2zdUACzxM1Dfw9": {
                    "accounts": ["34LSBjeswZorbZyYdV3XUmmTDhZyvLXpnDjjBdTjeKPa"],
                    "currency": "USD",
                    "raw_amount": "5000000",
                    "ui_amount": 5.0
                }
            }
        }))
        .unwrap();
        let balances = parse_api_balances(parsed);
        assert!(balances.tokens.is_empty());
        assert_eq!(balances.credits.len(), 1);
        assert_eq!(balances.credits[0].raw_amount, 5_000_000);
        assert_eq!(balances.credits[0].ui_amount, 5.0);
        assert!(!balances.credits_unavailable);
    }

    #[test]
    fn api_reports_unavailable_credits_separately_from_tokens() {
        let parsed: ApiResponse = serde_json::from_value(serde_json::json!({
            "balances": [{
                "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "raw_amount": "1000000",
                "ui_amount": 1.0,
                "symbol": "USDC"
            }],
            "credits": {},
            "credits_unavailable": true
        }))
        .unwrap();

        let balances = parse_api_balances(parsed);
        assert_eq!(balances.tokens.len(), 1);
        assert!(balances.credits.is_empty());
        assert!(balances.credits_unavailable);
    }

    #[test]
    fn rpc_fallback_aggregates_known_stablecoins_and_ignores_unknown_tokens() {
        let response = serde_json::json!({
            "result": { "value": [
                {
                    "account": { "data": { "parsed": { "info": {
                        "mint": pay_types::stablecoin_mints::USDC_MAINNET,
                        "tokenAmount": { "amount": "46072908" }
                    }}}}
                },
                {
                    "account": { "data": { "parsed": { "info": {
                        "mint": pay_types::stablecoin_mints::USDC_MAINNET,
                        "tokenAmount": { "amount": "1000000" }
                    }}}}
                },
                {
                    "account": { "data": { "parsed": { "info": {
                        "mint": "UnknownMint1111111111111111111111111111111",
                        "tokenAmount": { "amount": "999999999" }
                    }}}}
                }
            ]}
        });

        let balances = parse_rpc_stablecoin_balances(&[response]);
        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].symbol.as_deref(), Some("USDC"));
        assert_eq!(balances[0].raw_amount, 47_072_908);
        assert!((balances[0].ui_amount - 47.072_908).abs() < f64::EPSILON);
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
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 2_000_000,
            tokens: vec![],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 1_000_000,
            tokens: vec![],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
                symbol: Some("USDC".to_string()),
            }],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC_MINT".to_string(),
                raw_amount: 25_500_000,
                ui_amount: 25.5,
                symbol: Some("USDC".to_string()),
            }],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: false,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.tokens.len(), 1);
        assert!((diff.tokens[0].ui_amount - 15.5).abs() < f64::EPSILON);
        assert_eq!(diff.tokens[0].symbol.as_deref(), Some("USDC"));
    }

    #[test]
    fn diff_received_new_token() {
        let baseline = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
                symbol: Some("USDC".to_string()),
            }],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![TokenBalance {
                mint: "USDC".to_string(),
                raw_amount: 5_000_000,
                ui_amount: 5.0,
                symbol: Some("USDC".to_string()),
            }],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
                symbol: Some("USDC".to_string()),
            }],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: false,
        };
        let current = AccountBalances {
            sol_lamports: 0,
            tokens: vec![],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
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
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: true,
        };
        let current = AccountBalances {
            sol_lamports: 1_000,
            tokens: vec![],
            credits: vec![],
            committable_channels: vec![],
            channel_balances_unavailable: false,
            credits_unavailable: false,
            tokens_unavailable: true,
        };
        let diff = current.diff_received(&baseline);
        assert_eq!(diff.sol_lamports, 900);
        assert!(diff.tokens.is_empty());
    }
}
