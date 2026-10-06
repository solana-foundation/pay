//! Sponsored payment-channel lifecycle operations.
//!
//! `POST /v1/channels/close` follows the same two-step MPP charge flow as
//! subscription cancellation: discovery returns a USD stablecoin charge, then
//! the authenticated request carries a payer-signed `request_close`
//! transaction which the operator co-signs as fee payer and broadcasts.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pay_api_core::{Error, Stablecoin};
use pay_api_types::Network;
use pay_kit::core::payment_channels::{
    PAYMENT_CHANNELS_PROGRAM_ID, default_program_id, find_channel_pda,
};
use pay_kit::generated::payment_channels::accounts::Channel;
use pay_kit::generated::payment_channels::instructions::REQUEST_CLOSE_DISCRIMINATOR;
use pay_kit::mpp::protocol::solana::MethodDetails;
use pay_kit::mpp::server::{Config as MppConfig, Mpp};
use pay_kit::mpp::solana_keychain::TransactionSigner;
use pay_kit::mpp::{ChargeRequest, PaymentCredential};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use tracing::{info, warn};

use crate::state::AppState;

const PAYMENT_RECEIPT_HEADER: HeaderName = HeaderName::from_static("payment-receipt");
const COMPUTE_BUDGET_PROGRAM_ID: &str = "ComputeBudget111111111111111111111111111111";
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const STATUS_OPEN: u8 = 0;

#[derive(Debug, Deserialize)]
pub struct CloseRequest {
    #[serde(default)]
    tx: Option<String>,
    /// Channel PDA. Required for discovery; the signed transaction is the
    /// source of truth on the authenticated request.
    #[serde(default, rename = "channel")]
    channel_id: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    currency: Option<String>,
}

#[derive(Debug, Serialize)]
struct CloseChallengeResponse {
    challenge: pay_kit::mpp::PaymentChallenge,
    #[serde(rename = "wwwAuthenticate")]
    www_authenticate: String,
    network: Network,
    currency: String,
    #[serde(rename = "feeRaw")]
    fee_raw: String,
    #[serde(rename = "estimatedFeeLamports")]
    estimated_fee_lamports: u64,
    #[serde(rename = "solUsdPrice")]
    sol_usd_price: f64,
    #[serde(rename = "feePayer")]
    fee_payer: String,
    channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payer: Option<String>,
}

#[derive(Debug, Serialize)]
struct CloseReceiptResponse {
    signature: String,
    receipt: pay_kit::mpp::Receipt,
    channel: String,
}

struct ParsedCloseTx {
    tx: VersionedTransaction,
    payer: Pubkey,
    channel: Pubkey,
}

struct ResolvedClose {
    network: Network,
    cluster: &'static str,
    rpc_url: String,
    coin: Stablecoin,
    parsed: Option<ParsedCloseTx>,
    channel: Pubkey,
    fee_payer: String,
}

pub async fn close_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CloseRequest>,
) -> Result<Response, ApiError> {
    let resolved = resolve_request(&state, &request).map_err(ApiError)?;
    validate_open_channel(&state, &resolved)
        .await
        .map_err(ApiError)?;

    if let Some(header) = headers.get(AUTHORIZATION) {
        return verify_and_broadcast(state, resolved, header).await;
    }

    let price_rpc_url = state.rpc_url_for(Network::Mainnet).map_err(ApiError)?;
    let sol_usd_price = state
        .rpc
        .get_asset_price_per_token(price_rpc_url, &state.channels.sol_price_asset)
        .await
        .map_err(ApiError)?;
    let fee_raw = fee_base_units(
        state.channels.estimated_fee_lamports,
        sol_usd_price,
        resolved.coin.decimals,
    )
    .map_err(ApiError)?;
    let recent_blockhash = state.rpc.get_latest_blockhash(&resolved.rpc_url).await.ok();
    let charge_request =
        build_charge_request(&resolved, fee_raw, recent_blockhash).map_err(ApiError)?;
    let mpp = new_mpp(&state, &resolved, None).map_err(ApiError)?;
    let challenge = mpp
        .charge_challenge(&charge_request)
        .map_err(|_| ApiError(Error::PaymentChallenge))?;
    let www_authenticate = pay_kit::mpp::format_www_authenticate(&challenge)
        .map_err(|_| ApiError(Error::PaymentChallenge))?;
    let mut response = (
        StatusCode::PAYMENT_REQUIRED,
        Json(CloseChallengeResponse {
            challenge,
            www_authenticate: www_authenticate.clone(),
            network: resolved.network,
            currency: resolved.coin.symbol.clone(),
            fee_raw: fee_raw.to_string(),
            estimated_fee_lamports: state.channels.estimated_fee_lamports,
            sol_usd_price,
            fee_payer: resolved.fee_payer.clone(),
            channel: resolved.channel.to_string(),
            payer: resolved
                .parsed
                .as_ref()
                .map(|parsed| parsed.payer.to_string()),
        }),
    )
        .into_response();
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_str(&www_authenticate).map_err(|_| ApiError(Error::PaymentChallenge))?,
    );
    Ok(response)
}

async fn verify_and_broadcast(
    state: Arc<AppState>,
    resolved: ResolvedClose,
    header: &HeaderValue,
) -> Result<Response, ApiError> {
    if resolved.parsed.is_none() {
        return Err(ApiError(Error::InvalidPaymentCredential));
    }
    let credential = PaymentCredential::from_header(
        header
            .to_str()
            .map_err(|_| ApiError(Error::InvalidPaymentCredential))?,
    )
    .map_err(|_| ApiError(Error::InvalidPaymentCredential))?;
    let charge_request: ChargeRequest = credential
        .challenge
        .request
        .decode()
        .map_err(|_| ApiError(Error::InvalidPaymentCredential))?;
    validate_paid_request(&charge_request, &resolved).map_err(ApiError)?;

    let signer = crate::signer::build_fee_payer_signer(
        &state.channels_fee_payer,
        "channels.fee_payer.key_name (or send.fee_payer.key_name) is missing",
        "channels.fee_payer.pubkey (or send.fee_payer.pubkey) is missing",
    )
    .await
    .map_err(|_| ApiError(Error::FeePayerSigner))?;
    let mpp = new_mpp(&state, &resolved, Some(Arc::clone(&signer))).map_err(ApiError)?;
    let receipt = mpp
        .verify(&credential, &charge_request)
        .await
        .map_err(|error| {
            warn!(%error, channel = %resolved.channel, "channel close charge verification failed");
            ApiError(Error::InvalidPaymentCredential)
        })?;

    let signature = co_sign_and_broadcast(&state, &resolved, signer)
        .await
        .map_err(ApiError)?;
    state
        .rpc
        .confirm_signature(
            &resolved.rpc_url,
            &signature.to_string(),
            Duration::from_secs(state.channels.confirm_timeout_seconds),
        )
        .await
        .map_err(ApiError)?;
    info!(%signature, channel = %resolved.channel, "payment channel close requested");

    let receipt_header = receipt
        .to_header()
        .map_err(|_| ApiError(Error::PaymentChallenge))?;
    let mut response = (
        StatusCode::OK,
        Json(CloseReceiptResponse {
            signature: signature.to_string(),
            receipt,
            channel: resolved.channel.to_string(),
        }),
    )
        .into_response();
    response.headers_mut().insert(
        PAYMENT_RECEIPT_HEADER,
        HeaderValue::from_str(&receipt_header).map_err(|_| ApiError(Error::PaymentChallenge))?,
    );
    Ok(response)
}

fn resolve_request(state: &AppState, request: &CloseRequest) -> Result<ResolvedClose, Error> {
    if !state.channels.enabled {
        return Err(Error::SendNotConfigured(
            "set PAY_API_CHANNELS__ENABLED=true and configure a fee payer".into(),
        ));
    }
    let network = match request.network.as_deref().map(str::trim) {
        Some("") | None => Network::Mainnet,
        Some(value) => value.parse::<Network>().map_err(Error::from)?,
    };
    let cluster = match network {
        Network::Mainnet => "mainnet",
        Network::Sandbox => "localnet",
    };
    let rpc_url = state.rpc_url_for(network)?.to_string();
    let currency = request
        .currency
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("USDC");
    let coin = state
        .stablecoins
        .iter()
        .find(|coin| {
            coin.symbol.eq_ignore_ascii_case(currency) || coin.mint.to_string() == currency
        })
        .cloned()
        .ok_or_else(|| Error::UnsupportedCurrency(currency.to_string()))?;
    let fee_payer = state
        .channels_fee_payer
        .pubkey
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::SendNotConfigured("channels fee payer is missing".into()))?
        .to_string();

    let (parsed, channel) = match request.tx.as_deref().filter(|tx| !tx.trim().is_empty()) {
        Some(tx) => {
            let parsed = parse_close_tx(tx, &fee_payer)?;
            let channel = parsed.channel;
            (Some(parsed), channel)
        }
        None => {
            let channel = request
                .channel_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(Error::InvalidPaymentCredential)?;
            (
                None,
                Pubkey::from_str(channel).map_err(|_| Error::InvalidAddress)?,
            )
        }
    };
    Ok(ResolvedClose {
        network,
        cluster,
        rpc_url,
        coin,
        parsed,
        channel,
        fee_payer,
    })
}

fn parse_close_tx(tx_b64: &str, expected_fee_payer: &str) -> Result<ParsedCloseTx, Error> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(tx_b64.trim())
        .map_err(|_| Error::InvalidPaymentCredential)?;
    let tx = pay_kit::core::tx::decode_bytes(&raw).map_err(|_| Error::InvalidPaymentCredential)?;
    pay_kit::core::tx::check_envelope(&tx, &[pay_kit::core::tx::TxVersion::V0])
        .map_err(|_| Error::InvalidPaymentCredential)?;
    tx.sanitize().map_err(|_| Error::InvalidPaymentCredential)?;
    let keys = tx.message.static_account_keys();
    let required = usize::from(tx.message.header().num_required_signatures);
    if keys.first().map(ToString::to_string).as_deref() != Some(expected_fee_payer)
        || tx.signatures.len() != required
    {
        return Err(Error::InvalidPaymentCredential);
    }
    let program = Pubkey::from_str(PAYMENT_CHANNELS_PROGRAM_ID).expect("valid program id");
    let compute = Pubkey::from_str(COMPUTE_BUDGET_PROGRAM_ID).expect("valid program id");
    let memo = Pubkey::from_str(MEMO_PROGRAM_ID).expect("valid program id");
    let mut close_ix = None;
    for instruction in tx.message.instructions() {
        let instruction_program = *keys
            .get(usize::from(instruction.program_id_index))
            .ok_or(Error::InvalidPaymentCredential)?;
        if instruction_program == program {
            if instruction.data.as_slice() != [REQUEST_CLOSE_DISCRIMINATOR] || close_ix.is_some() {
                return Err(Error::InvalidPaymentCredential);
            }
            close_ix = Some(instruction);
        } else if instruction_program != compute && instruction_program != memo {
            return Err(Error::InvalidPaymentCredential);
        }
    }
    let close_ix = close_ix.ok_or(Error::InvalidPaymentCredential)?;
    if close_ix.accounts.len() != 2 {
        return Err(Error::InvalidPaymentCredential);
    }
    let payer_index = usize::from(close_ix.accounts[0]);
    let channel_index = usize::from(close_ix.accounts[1]);
    let payer = *keys
        .get(payer_index)
        .ok_or(Error::InvalidPaymentCredential)?;
    let channel = *keys
        .get(channel_index)
        .ok_or(Error::InvalidPaymentCredential)?;
    // The fee payer intentionally has no signature yet; every other required
    // signer, including the channel payer, must already be valid before the
    // operator accepts the USD fee.
    if payer_index == 0
        || payer_index >= required
        || !tx.message.is_signer(payer_index)
        || !tx.message.is_maybe_writable_with_reserved_addresses(
            channel_index,
            None::<&std::collections::HashSet<Pubkey>>,
        )
    {
        return Err(Error::InvalidPaymentCredential);
    }
    let verified = tx.verify_with_results();
    if !verified.get(payer_index).copied().unwrap_or(false) {
        return Err(Error::InvalidPaymentCredential);
    }
    Ok(ParsedCloseTx { tx, payer, channel })
}

async fn validate_open_channel(state: &AppState, resolved: &ResolvedClose) -> Result<(), Error> {
    let accounts = state
        .rpc
        .get_multiple_accounts(&resolved.rpc_url, &[resolved.channel.to_string()])
        .await?;
    let data = accounts
        .into_iter()
        .next()
        .flatten()
        .ok_or(Error::InvalidAddress)?;
    let channel = Channel::from_bytes(&data).map_err(|_| Error::RpcMalformed)?;
    if channel.status != STATUS_OPEN {
        return Err(Error::InvalidPaymentCredential);
    }
    let payer = Pubkey::from(channel.payer.to_bytes());
    let (expected_channel, expected_bump) = find_channel_pda(
        &payer,
        &Pubkey::from(channel.payee.to_bytes()),
        &Pubkey::from(channel.mint.to_bytes()),
        &Pubkey::from(channel.authorized_signer.to_bytes()),
        channel.salt,
        channel.open_slot,
        &default_program_id(),
    );
    if expected_channel != resolved.channel || expected_bump != channel.bump {
        return Err(Error::InvalidPaymentCredential);
    }
    if let Some(parsed) = &resolved.parsed
        && payer != parsed.payer
    {
        return Err(Error::InvalidPaymentCredential);
    }
    Ok(())
}

fn build_charge_request(
    resolved: &ResolvedClose,
    fee_raw: u64,
    recent_blockhash: Option<String>,
) -> Result<ChargeRequest, Error> {
    Ok(ChargeRequest {
        amount: fee_raw.to_string(),
        currency: resolved.coin.mint.to_string(),
        recipient: Some(resolved.fee_payer.clone()),
        description: Some(format!(
            "Sponsor close request for channel {}",
            resolved.channel
        )),
        external_id: Some(resolved.channel.to_string()),
        method_details: Some(
            serde_json::to_value(MethodDetails {
                network: Some(resolved.cluster.to_string()),
                decimals: Some(resolved.coin.decimals),
                token_program: Some(resolved.coin.token_program.to_string()),
                fee_payer: Some(true),
                fee_payer_key: Some(resolved.fee_payer.clone()),
                recent_blockhash,
                transaction_versions: None,
                splits: None,
                confidential: None,
                auditor_elgamal_pubkey: None,
                recipient_elgamal_pubkey: None,
            })
            .map_err(|_| Error::PaymentChallenge)?,
        ),
        ..Default::default()
    })
}

fn validate_paid_request(request: &ChargeRequest, resolved: &ResolvedClose) -> Result<(), Error> {
    if request.currency != resolved.coin.mint.to_string()
        || request.recipient.as_deref() != Some(resolved.fee_payer.as_str())
        || request.external_id.as_deref() != Some(resolved.channel.to_string().as_str())
    {
        return Err(Error::InvalidPaymentCredential);
    }
    Ok(())
}

fn new_mpp(
    state: &AppState,
    resolved: &ResolvedClose,
    signer: Option<Arc<dyn TransactionSigner>>,
) -> Result<Mpp, Error> {
    Mpp::new(MppConfig {
        recipient: resolved.fee_payer.clone(),
        currency: resolved.coin.mint.to_string(),
        decimals: resolved.coin.decimals,
        network: resolved.cluster.to_string(),
        rpc_url: Some(resolved.rpc_url.clone()),
        challenge_binding_secret: state.channels_challenge_binding_secret.clone(),
        realm: Some(state.channels.realm.clone()),
        fee_payer: signer.is_some(),
        fee_payer_signer: signer,
        html: false,
        ..Default::default()
    })
    .map_err(|_| Error::PaymentChallenge)
}

async fn co_sign_and_broadcast(
    state: &AppState,
    resolved: &ResolvedClose,
    signer: Arc<dyn TransactionSigner>,
) -> Result<Signature, Error> {
    let mut tx = resolved
        .parsed
        .as_ref()
        .ok_or(Error::InvalidPaymentCredential)?
        .tx
        .clone();
    let fee_payer = signer.pubkey();
    pay_kit::core::signing::cosign_versioned_fee_payer(signer.as_ref(), &fee_payer, &mut tx)
        .await
        .map_err(|_| Error::FeePayerSigner)?;
    let encoded = pay_kit::core::tx::encode(&tx).map_err(|_| Error::PaymentChallenge)?;
    let signature = state
        .rpc
        .send_raw_transaction(&resolved.rpc_url, &encoded)
        .await?;
    Signature::from_str(&signature).map_err(|_| Error::RpcMalformed)
}

fn fee_base_units(lamports: u64, sol_usd_price: f64, decimals: u8) -> Result<u64, Error> {
    if !sol_usd_price.is_finite() || sol_usd_price <= 0.0 {
        return Err(Error::PriceUnavailable);
    }
    let raw =
        ((lamports as f64 / 1_000_000_000.0) * sol_usd_price * 10f64.powi(i32::from(decimals)))
            .ceil();
    if !raw.is_finite() || raw <= 0.0 || raw > u64::MAX as f64 {
        return Err(Error::PriceUnavailable);
    }
    Ok(raw as u64)
}

pub struct ApiError(Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
