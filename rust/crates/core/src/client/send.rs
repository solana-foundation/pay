//! Send stablecoins to a recipient address.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::Message;
use solana_mpp::protocol::solana::{
    default_rpc_url, default_token_program_for_currency, programs, resolve_stablecoin_mint,
};
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;
use tracing::info;

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
    /// Stablecoin symbol or mint the user selected.
    pub currency: String,
    /// Mint address for the selected stablecoin.
    pub mint: String,
    /// Sender public key (base-58).
    pub from: String,
    /// Recipient public key (base-58).
    pub to: String,
}

/// Parameters for a fee-payer-backed stablecoin send.
pub struct StablecoinSendRequest<'a> {
    pub amount: &'a str,
    pub recipient: &'a str,
    pub currency: &'a str,
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

/// Result of a successful native SOL send.
pub struct SolSendResult {
    /// Transaction signature (base-58).
    pub signature: String,
    /// Amount sent in lamports.
    pub lamports: u64,
    /// Sender public key (base-58).
    pub from: String,
    /// Recipient public key (base-58).
    pub to: String,
}

/// Send a Solana stablecoin to `recipient`.
///
/// - `amount_str`: either a decimal token amount (e.g. `"1.25"`) or `"max"` to
///   send the entire selected stablecoin balance.
/// - `currency`: stablecoin symbol or mint address. Defaults at the CLI layer
///   to `USDC`.
/// - `network`: Solana network slug (`mainnet`, `devnet`, `localnet`).
/// - `active_account_name`: optional legacy signer source string.
/// - `account_override`: optional named account from `accounts.yml`.
/// - `rpc_url`: optional explicit RPC endpoint.
pub async fn send_stablecoin_direct(
    amount_str: &str,
    recipient: &str,
    currency: &str,
    network: &str,
    active_account_name: Option<&str>,
    account_override: Option<&str>,
    rpc_url: Option<&str>,
) -> Result<SendResult> {
    let normalized_currency = currency.trim();
    if normalized_currency.is_empty() {
        return Err(Error::Config("Currency must not be empty".to_string()));
    }

    let mint = resolve_stablecoin_mint(normalized_currency, Some(network)).ok_or_else(|| {
        Error::Config(
            "`pay send` sends stablecoins only; choose USDC, USDT, PYUSD, CASH, or a mint address"
                .to_string(),
        )
    })?;

    let recipient_pubkey: Pubkey = recipient
        .parse()
        .map_err(|e| Error::Config(format!("Invalid recipient address: {e}")))?;
    let mint_pubkey: Pubkey = mint
        .parse()
        .map_err(|e| Error::Config(format!("Invalid stablecoin mint: {e}")))?;

    let amount_label = if sends_entire_balance(amount_str) {
        format!("max {normalized_currency}")
    } else {
        format!("{amount_str} {normalized_currency}")
    };
    let intent = crate::keystore::AuthIntent::authorize_payment(
        &stablecoin_payment_limit_label(amount_str),
        &format!("sending {amount_label} to {recipient}"),
    );

    let signer = if let Some(source) = active_account_name {
        crate::signer::load_signer_with_intent(source, &intent)?
    } else {
        let store = crate::accounts::FileAccountsStore::default_path();
        crate::signer::load_signer_for_network_with_intent(
            network,
            &store,
            account_override,
            &intent,
        )?
        .0
    };

    let rpc_url = rpc_url
        .map(str::to_string)
        .or_else(|| std::env::var("PAY_RPC_URL").ok())
        .unwrap_or_else(|| default_rpc_url(network).to_string());
    let rpc = RpcClient::new(rpc_url);
    let sender_pubkey = signer.pubkey();
    let token_program = token_program_for_currency(normalized_currency, network)?;
    let source_ata = associated_token_address(&sender_pubkey, &mint_pubkey, &token_program);
    let destination_ata = associated_token_address(&recipient_pubkey, &mint_pubkey, &token_program);

    let amount_raw = if sends_entire_balance(amount_str) {
        token_account_raw_balance(&rpc, &source_ata)?
    } else {
        parse_token_amount(amount_str, STABLECOIN_DECIMALS)?
    };

    if amount_raw == 0 {
        return Err(Error::Config("Amount must be greater than 0".to_string()));
    }

    info!(
        amount_raw,
        currency = normalized_currency,
        mint,
        from = %sender_pubkey,
        to = %recipient,
        "Sending stablecoin"
    );

    let instructions = vec![
        create_associated_token_account_idempotent(
            &sender_pubkey,
            &recipient_pubkey,
            &mint_pubkey,
            &token_program,
        ),
        transfer_checked_ix(
            &token_program,
            &source_ata,
            &mint_pubkey,
            &destination_ata,
            &sender_pubkey,
            amount_raw,
            STABLECOIN_DECIMALS,
        ),
    ];

    let signature = sign_and_confirm(&signer, &rpc, instructions).await?;

    Ok(SendResult {
        signature: signature.to_string(),
        amount_raw,
        total_amount_raw: amount_raw,
        fee_refund_raw: 0,
        decimals: STABLECOIN_DECIMALS,
        currency: normalized_currency.to_string(),
        mint: mint.to_string(),
        from: sender_pubkey.to_string(),
        to: recipient.to_string(),
    })
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
        currency,
        network,
        account_override,
        memo,
        fee_within,
        rpc_url,
    } = request;

    let normalized_currency = currency.trim();
    if normalized_currency.is_empty() {
        return Err(Error::Config("Currency must not be empty".to_string()));
    }

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
            stablecoin_raw_balance_for_sender(&rpc_url, &sender, normalized_currency, network)?;
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
            mint: normalized_currency.to_string(),
            from: account_pubkey_for_network(network, account_override)?
                .unwrap_or_else(|| String::from("(unknown)")),
            to: recipient.to_string(),
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

    Ok(SendResult {
        signature: receipt.signature,
        amount_raw,
        total_amount_raw,
        fee_refund_raw,
        decimals: STABLECOIN_DECIMALS,
        currency: normalized_currency.to_string(),
        mint: request_for_result.currency,
        from: sender,
        to: recipient.to_string(),
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
    currency: &str,
    network: &str,
) -> Result<u64> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Config(format!("Failed to create runtime: {e}")))?;
    let balances = rt.block_on(balance::get_balances(rpc_url, sender))?;
    let expected_mint = resolve_stablecoin_mint(currency, Some(network));
    balances
        .tokens
        .iter()
        .find(|token| {
            token
                .symbol
                .is_some_and(|symbol| symbol.eq_ignore_ascii_case(currency))
                || expected_mint.is_some_and(|mint| token.mint == mint)
                || token.mint == currency
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

/// Send native SOL to `recipient`.
///
/// This is kept for lower-level sandbox flows and tests. The `pay send` CLI
/// sends stablecoins via [`send_stablecoin`].
pub async fn send_sol(
    amount_str: &str,
    recipient: &str,
    active_account_name: &str,
    rpc_url: &str,
) -> Result<SolSendResult> {
    let intent = crate::keystore::AuthIntent::send_sol(recipient);
    let signer = crate::signer::load_signer_with_intent(active_account_name, &intent)?;
    let rpc = RpcClient::new(rpc_url.to_string());

    let sender_pubkey = signer.pubkey();
    let recipient_pubkey: Pubkey = recipient
        .parse()
        .map_err(|e| Error::Config(format!("Invalid recipient address: {e}")))?;

    let lamports = if amount_str == "*" {
        let balance = rpc
            .get_balance(&sender_pubkey)
            .map_err(|e| Error::Config(format!("Failed to get balance: {e}")))?;
        let fee = 5000u64;
        if balance <= fee {
            return Err(Error::Config(format!(
                "Insufficient balance: {balance} lamports (need > {fee} for fee)"
            )));
        }
        balance - fee
    } else {
        parse_sol_amount(amount_str)?
    };

    if lamports == 0 {
        return Err(Error::Config("Amount must be greater than 0".to_string()));
    }

    info!(
        lamports,
        from = %sender_pubkey,
        to = %recipient,
        sol = format!("{:.9}", lamports as f64 / 1_000_000_000.0),
        "Sending SOL"
    );

    let ix = system_instruction::transfer(&sender_pubkey, &recipient_pubkey, lamports);
    let signature = sign_and_confirm(&signer, &rpc, vec![ix]).await?;

    Ok(SolSendResult {
        signature: signature.to_string(),
        lamports,
        from: sender_pubkey.to_string(),
        to: recipient.to_string(),
    })
}

fn token_program_for_currency(currency: &str, network: &str) -> Result<Pubkey> {
    let token_program = default_token_program_for_currency(currency, Some(network));
    Pubkey::from_str(token_program)
        .map_err(|e| Error::Config(format!("Invalid token program for {currency}: {e}")))
}

fn token_account_raw_balance(rpc: &RpcClient, token_account: &Pubkey) -> Result<u64> {
    let balance = rpc
        .get_token_account_balance(token_account)
        .map_err(|e| Error::Config(format!("Failed to get stablecoin balance: {e}")))?;
    balance
        .amount
        .parse::<u64>()
        .map_err(|e| Error::Config(format!("Invalid token balance from RPC: {e}")))
}

fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let ata_program = Pubkey::from_str(programs::ASSOCIATED_TOKEN_PROGRAM).unwrap();
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .0
}

fn create_associated_token_account_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = associated_token_address(owner, mint, token_program);
    let ata_program = Pubkey::from_str(programs::ASSOCIATED_TOKEN_PROGRAM).unwrap();
    let system_program = Pubkey::from_str(programs::SYSTEM_PROGRAM).unwrap();

    Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

fn transfer_checked_ix(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);

    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

async fn sign_and_confirm(
    signer: &dyn SolanaSigner,
    rpc: &RpcClient,
    instructions: Vec<Instruction>,
) -> Result<Signature> {
    let sender_pubkey = signer.pubkey();
    let blockhash = rpc
        .get_latest_blockhash()
        .map_err(|e| Error::Config(format!("Failed to get blockhash: {e}")))?;

    let message = Message::new_with_blockhash(&instructions, Some(&sender_pubkey), &blockhash);
    let mut tx = Transaction::new_unsigned(message);
    let sig_bytes = signer
        .sign_message(&tx.message_data())
        .await
        .map_err(|e| Error::Config(format!("Signing failed: {e}")))?;
    let sig = Signature::from(<[u8; 64]>::from(sig_bytes));
    let signer_index = tx
        .message
        .account_keys
        .iter()
        .position(|key| key == &sender_pubkey)
        .ok_or_else(|| Error::Config("Signer not found in transaction".to_string()))?;
    tx.signatures[signer_index] = sig;

    rpc.send_and_confirm_transaction(&tx)
        .map_err(|e| Error::Config(format!("Transaction failed: {e}")))
}

fn stablecoin_payment_limit_label(amount_str: &str) -> String {
    if sends_entire_balance(amount_str) {
        "stablecoin balance".to_string()
    } else {
        format!("${amount_str}")
    }
}

/// Parse a human-friendly SOL amount into lamports.
fn parse_sol_amount(s: &str) -> Result<u64> {
    let sol: f64 = s
        .parse()
        .map_err(|_| Error::Config(format!("Invalid amount: {s}")))?;
    if sol < 0.0 {
        return Err(Error::Config("Amount must be positive".to_string()));
    }
    Ok((sol * 1_000_000_000.0) as u64)
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
    fn parse_sol_amount_one_sol() {
        assert_eq!(parse_sol_amount("1.0").unwrap(), 1_000_000_000);
    }

    #[test]
    fn parse_sol_amount_fractional() {
        assert_eq!(parse_sol_amount("0.5").unwrap(), 500_000_000);
    }

    #[test]
    fn parse_sol_amount_negative() {
        assert!(parse_sol_amount("-1.0").is_err());
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
        };
        assert_eq!(result.signature, "sig123");
        assert_eq!(result.amount_raw, 1_000_000);
        assert_eq!(result.currency, "USDC");
        assert_eq!(result.to, "to_pubkey");
    }
}
