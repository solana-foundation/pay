//! Send stablecoins to a recipient address.

use std::str::FromStr;

use pay_types::Stablecoin;
use serde::{Deserialize, Serialize};
use solana_mpp::protocol::solana::default_rpc_url;
use solana_pubkey::Pubkey;

use crate::accounts::{AccountChoice, AccountsFile, resolve_account_for_network};
use crate::client::{balance, fetch, mpp};
use crate::{Error, Result};

pub const STABLECOIN_DECIMALS: u8 = 6;

/// Result of a successful send.
pub struct SendResult {
    /// Transaction signature (base-58).
    pub signature: String,
    /// Amount sent in the stablecoin's base units.
    pub amount_raw: u64,
    /// Total amount paid in the stablecoin's base units, including any fee
    /// payer refund split.
    pub total_amount_raw: u64,
    /// Fee-payer refund amount in the stablecoin's base units.
    pub fee_refund_raw: u64,
    /// Token decimals used for display and transfer_checked.
    pub decimals: u8,
    /// Stablecoin symbol the user selected.
    pub currency: String,
    /// Mint address for the selected stablecoin.
    pub mint: String,
    /// Sender public key (base-58).
    pub from: String,
    /// Recipient public key (base-58).
    pub to: String,
    /// Solana network used for the transaction.
    pub network: String,
    /// RPC URL used to build or pay for the transaction.
    pub rpc_url: String,
}

/// Parameters for a fee-payer-backed stablecoin send.
pub struct StablecoinSendRequest<'a> {
    pub amount: &'a str,
    pub recipient: &'a str,
    pub stablecoin: Stablecoin,
    pub network: &'a str,
    pub account_override: Option<&'a str>,
    pub memo: Option<&'a str>,
    pub fee_within: bool,
    pub rpc_url: Option<&'a str>,
}

#[derive(Serialize)]
struct ApiSendRequest<'a> {
    recipient: &'a str,
    amount: &'a str,
    currency: &'a str,
    network: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    memo: Option<&'a str>,
    #[serde(rename = "feeWithin", skip_serializing_if = "is_false")]
    fee_within: bool,
}

#[derive(Deserialize)]
struct ApiSendChallengeResponse {
    #[serde(rename = "recipientAmountRaw")]
    recipient_amount_raw: String,
    #[serde(rename = "totalAmountRaw")]
    total_amount_raw: String,
    #[serde(rename = "feeRefundRaw")]
    fee_refund_raw: String,
}

#[derive(Deserialize)]
struct ApiSendReceiptResponse {
    receipt: ApiReceipt,
}

#[derive(Deserialize)]
struct ApiReceipt {
    reference: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn effective_fee_within(amount_str: &str, fee_within: bool) -> bool {
    fee_within || sends_entire_balance(amount_str)
}

fn sends_entire_balance(amount_str: &str) -> bool {
    amount_str == "*" || amount_str.eq_ignore_ascii_case("max")
}

/// Send a stablecoin through the fee-payer-backed send endpoint.
///
/// The server returns an MPP charge challenge. The client signs the stablecoin
/// payment, the server co-signs as fee payer, and the successful retry returns
/// the on-chain transaction signature.
pub fn send_stablecoin(request: StablecoinSendRequest<'_>) -> Result<SendResult> {
    let StablecoinSendRequest {
        amount: amount_str,
        recipient,
        stablecoin,
        network,
        account_override,
        memo,
        fee_within,
        rpc_url,
    } = request;

    let normalized_currency = stablecoin.symbol();

    Pubkey::from_str(recipient)
        .map_err(|e| Error::Config(format!("Invalid recipient address: {e}")))?;

    let network = normalize_send_network(network);
    let api_network = api_network_for_send(network);
    let fee_within = effective_fee_within(amount_str, fee_within);
    let rpc_url = rpc_url
        .map(str::to_string)
        .or_else(|| std::env::var("PAY_RPC_URL").ok())
        .unwrap_or_else(|| default_rpc_url(network).to_string());

    let amount_for_api;
    let amount = if sends_entire_balance(amount_str) {
        let sender = account_pubkey_for_network(network, account_override)?.ok_or_else(|| {
            Error::Config(format!(
                "No {network} account found. Run `pay setup` first."
            ))
        })?;
        let raw_balance =
            stablecoin_raw_balance_for_sender(&rpc_url, &sender, stablecoin, network)?;
        if raw_balance == 0 {
            return Err(Error::Config(format!(
                "No {normalized_currency} balance available to send"
            )));
        }
        amount_for_api = format_token_amount(raw_balance, STABLECOIN_DECIMALS);
        amount_for_api.as_str()
    } else {
        amount_str
    };

    let api_url = format!("{}/v1/send", balance::pay_api_url().trim_end_matches('/'));
    let request = ApiSendRequest {
        recipient,
        amount,
        currency: normalized_currency,
        network: api_network,
        memo: memo.map(str::trim).filter(|value| !value.is_empty()),
        fee_within,
    };
    let body = serde_json::to_string(&request)?;
    let headers = vec![("content-type".to_string(), "application/json".to_string())];

    let first = fetch::fetch_raw("POST", &api_url, &headers, Some(&body))?;
    if first.status != 402 {
        let receipt = parse_send_receipt_or_error(first.status, &first.body)?;
        return Ok(SendResult {
            signature: receipt.signature,
            amount_raw: 0,
            total_amount_raw: 0,
            fee_refund_raw: 0,
            decimals: STABLECOIN_DECIMALS,
            currency: normalized_currency.to_string(),
            mint: stablecoin.mint(Some(network)).to_string(),
            from: account_pubkey_for_network(network, account_override)?
                .unwrap_or_else(|| String::from("(unknown)")),
            to: recipient.to_string(),
            network: network.to_string(),
            rpc_url,
        });
    }

    let challenge_response: Option<ApiSendChallengeResponse> =
        serde_json::from_str(&first.body).ok();
    let challenges = mpp::parse_headers(&first.headers);
    if challenges.is_empty() {
        return Err(Error::InvalidChallenge(
            "pay-api did not return an MPP challenge".to_string(),
        ));
    }

    let store = crate::accounts::FileAccountsStore::default_path();
    let challenge =
        mpp::select_challenge_by_balance(&challenges, &store, Some(network), account_override)?
            .ok_or_else(|| Error::InvalidChallenge("No usable MPP send challenge".to_string()))?;
    let request_for_result: solana_mpp::ChargeRequest = challenge
        .request
        .decode()
        .map_err(|e| Error::InvalidChallenge(format!("Failed to decode send challenge: {e}")))?;

    let (auth_header, _) = mpp::build_credential(
        challenge,
        &store,
        Some(network),
        account_override,
        Some(&api_url),
    )?;

    let retry_headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("authorization".to_string(), auth_header),
    ];
    let retry = fetch::fetch_raw("POST", &api_url, &retry_headers, Some(&body))?;
    let receipt = parse_send_receipt_or_error(retry.status, &retry.body)?;

    let sender = account_pubkey_for_network(network, account_override)?
        .unwrap_or_else(|| String::from("(unknown)"));
    let amount_raw = challenge_response
        .as_ref()
        .and_then(|response| response.recipient_amount_raw.parse::<u64>().ok())
        .unwrap_or_else(|| recipient_amount_from_challenge(&request_for_result, recipient));
    let total_amount_raw = challenge_response
        .as_ref()
        .and_then(|response| response.total_amount_raw.parse::<u64>().ok())
        .or_else(|| request_for_result.amount.parse::<u64>().ok())
        .unwrap_or(amount_raw);
    let fee_refund_raw = challenge_response
        .as_ref()
        .and_then(|response| response.fee_refund_raw.parse::<u64>().ok())
        .unwrap_or_else(|| total_amount_raw.saturating_sub(amount_raw));
    let result_mint = Stablecoin::parse_symbol(&request_for_result.currency)
        .map(|stablecoin| stablecoin.mint(Some(network)).to_string())
        .unwrap_or_else(|| request_for_result.currency.clone());

    Ok(SendResult {
        signature: receipt.signature,
        amount_raw,
        total_amount_raw,
        fee_refund_raw,
        decimals: STABLECOIN_DECIMALS,
        currency: normalized_currency.to_string(),
        mint: result_mint,
        from: sender,
        to: recipient.to_string(),
        network: network.to_string(),
        rpc_url,
    })
}

struct ApiSendSuccess {
    signature: String,
}

fn parse_send_receipt_or_error(status: u16, body: &str) -> Result<ApiSendSuccess> {
    if (200..300).contains(&status) {
        let parsed: ApiSendReceiptResponse = serde_json::from_str(body)
            .map_err(|e| Error::Config(format!("pay-api send decode error: {e}")))?;
        return Ok(ApiSendSuccess {
            signature: parsed.receipt.reference,
        });
    }

    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(|message| message.as_str())
                        .map(str::to_string)
                })
        })
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    Err(Error::Config(format!(
        "pay-api send returned HTTP {status}: {detail}"
    )))
}

fn normalize_send_network(network: &str) -> &str {
    match network {
        "sandbox" => "localnet",
        other => other,
    }
}

fn api_network_for_send(network: &str) -> &'static str {
    match network {
        "localnet" | "sandbox" | "devnet" => "sandbox",
        _ => "mainnet",
    }
}

fn account_pubkey_for_network(
    network: &str,
    account_override: Option<&str>,
) -> Result<Option<String>> {
    let file = AccountsFile::load()?;
    if let Some(name) = account_override {
        return Ok(file
            .named_account_for_network(network, name)
            .and_then(|account| account.pubkey.clone()));
    }

    match resolve_account_for_network(network, &file) {
        AccountChoice::Resolved { account, .. } => Ok(account.pubkey),
        AccountChoice::Missing => Ok(None),
    }
}

fn stablecoin_raw_balance_for_sender(
    rpc_url: &str,
    sender: &str,
    stablecoin: Stablecoin,
    network: &str,
) -> Result<u64> {
    let currency = stablecoin.symbol();
    let expected_mint = stablecoin.mint(Some(network));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Config(format!("Failed to create runtime: {e}")))?;
    let balances = rt.block_on(balance::get_balances(rpc_url, sender))?;
    balances
        .tokens
        .iter()
        .find(|token| {
            token
                .symbol
                .is_some_and(|symbol| symbol.eq_ignore_ascii_case(currency))
                || token.mint == expected_mint
        })
        .map(|token| token.raw_amount)
        .ok_or_else(|| Error::Config(format!("No {currency} balance available to send")))
}

fn recipient_amount_from_challenge(request: &solana_mpp::ChargeRequest, recipient: &str) -> u64 {
    let total = request.amount.parse::<u64>().unwrap_or(0);
    let details: solana_mpp::protocol::solana::MethodDetails = request
        .method_details
        .as_ref()
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    let splits = details.splits.unwrap_or_default();

    if request.recipient.as_deref() == Some(recipient) {
        let split_total: u64 = splits
            .iter()
            .filter_map(|split| split.amount.parse::<u64>().ok())
            .sum();
        return total.saturating_sub(split_total);
    }

    splits
        .iter()
        .find(|split| split.recipient == recipient)
        .and_then(|split| split.amount.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Parse a human-friendly token amount into raw base units.
pub fn parse_token_amount(s: &str, decimals: u8) -> Result<u64> {
    if decimals > 18 {
        return Err(Error::Config("Token decimals too large".to_string()));
    }

    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Config("Amount must not be empty".to_string()));
    }
    if s.starts_with('-') {
        return Err(Error::Config("Amount must be positive".to_string()));
    }

    let mut parts = s.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > decimals as usize
    {
        return Err(Error::Config(format!(
            "Invalid amount: {s} (max {decimals} decimal places)"
        )));
    }

    let scale = 10_u64.pow(decimals as u32);
    let whole_units = whole
        .parse::<u64>()
        .map_err(|_| Error::Config(format!("Invalid amount: {s}")))?
        .checked_mul(scale)
        .ok_or_else(|| Error::Config("Amount is too large".to_string()))?;

    let mut fraction_units = 0u64;
    for (index, byte) in fraction.bytes().enumerate() {
        let digit = (byte - b'0') as u64;
        let place = 10_u64.pow(decimals as u32 - index as u32 - 1);
        fraction_units = fraction_units
            .checked_add(digit * place)
            .ok_or_else(|| Error::Config("Amount is too large".to_string()))?;
    }

    whole_units
        .checked_add(fraction_units)
        .ok_or_else(|| Error::Config("Amount is too large".to_string()))
}

pub fn format_token_amount(raw: u64, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }

    let scale = 10_u64.pow(decimals as u32);
    let whole = raw / scale;
    let fraction = raw % scale;
    if fraction == 0 {
        return whole.to_string();
    }

    let mut fraction = format!("{fraction:0width$}", width = decimals as usize);
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_amount_integer() {
        assert_eq!(parse_token_amount("10", 6).unwrap(), 10_000_000);
    }

    #[test]
    fn parse_token_amount_fractional() {
        assert_eq!(parse_token_amount("0.5", 6).unwrap(), 500_000);
        assert_eq!(parse_token_amount("1.234567", 6).unwrap(), 1_234_567);
    }

    #[test]
    fn parse_token_amount_zero() {
        assert_eq!(parse_token_amount("0", 6).unwrap(), 0);
    }

    #[test]
    fn parse_token_amount_rejects_too_many_decimals() {
        assert!(parse_token_amount("1.0000001", 6).is_err());
    }

    #[test]
    fn parse_token_amount_negative() {
        assert!(parse_token_amount("-1.0", 6).is_err());
    }

    #[test]
    fn parse_token_amount_invalid() {
        assert!(parse_token_amount("abc", 6).is_err());
    }

    #[test]
    fn format_token_amount_trims_fraction() {
        assert_eq!(format_token_amount(1_000_000, 6), "1");
        assert_eq!(format_token_amount(1_230_000, 6), "1.23");
        assert_eq!(format_token_amount(1, 6), "0.000001");
    }

    #[test]
    fn effective_fee_within_defaults_max_to_true() {
        assert!(effective_fee_within("max", false));
        assert!(effective_fee_within("MAX", false));
        assert!(effective_fee_within("*", false));
        assert!(effective_fee_within("1", true));
        assert!(!effective_fee_within("1", false));
    }

    #[test]
    fn send_result_fields() {
        let result = SendResult {
            signature: "sig123".to_string(),
            amount_raw: 1_000_000,
            total_amount_raw: 1_001_500,
            fee_refund_raw: 1_500,
            decimals: 6,
            currency: "USDC".to_string(),
            mint: "mint".to_string(),
            from: "from_pubkey".to_string(),
            to: "to_pubkey".to_string(),
            network: "mainnet".to_string(),
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
        };
        assert_eq!(result.signature, "sig123");
        assert_eq!(result.amount_raw, 1_000_000);
        assert_eq!(result.currency, "USDC");
        assert_eq!(result.to, "to_pubkey");
    }
}
