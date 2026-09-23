//! Tenant-scoped wallet access for a linked CLI.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use solana_keychain::TransactionSigner;
use solana_transaction::versioned::VersionedTransaction;

use crate::AppState;
use crate::protocol::ApiError;
use crate::tenants::TenantRecord;

const MAX_TRANSACTION_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;

#[derive(Serialize)]
pub struct WalletView {
    id: String,
    address: String,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WalletView>>, ApiError> {
    let tenant = authenticate(&state, &headers)?;
    Ok(Json(vec![WalletView {
        id: tenant.wallet_id.clone(),
        address: tenant.pubkey.clone(),
    }]))
}

#[derive(Deserialize)]
pub struct SignTransactionRequest {
    transaction_b64: String,
}

#[derive(Serialize)]
pub struct SignTransactionResponse {
    transaction_b64: String,
    signature: String,
    complete: bool,
}

pub async fn sign_transaction(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SignTransactionRequest>,
) -> Result<Json<SignTransactionResponse>, ApiError> {
    let tenant = authenticate_wallet(&state, &headers, &wallet)?;
    let bytes = decode_bounded(&request.transaction_b64, MAX_TRANSACTION_BYTES)?;
    let mut transaction: VersionedTransaction = bincode::deserialize(&bytes).map_err(|_| {
        ApiError::bad_request(
            "invalid_transaction",
            "The transaction is not valid Solana wire data.",
        )
    })?;
    let signer = connect_signer(tenant).await?;
    let result = signer
        .sign_transaction(&mut transaction)
        .await
        .map_err(provider_error)?;
    let complete = matches!(result, solana_keychain::SignTransactionResult::Complete(_));
    let (transaction_b64, signature) = result.into_signed_transaction();
    Ok(Json(SignTransactionResponse {
        transaction_b64,
        signature: signature.to_string(),
        complete,
    }))
}

#[derive(Deserialize)]
pub struct SignMessageRequest {
    message_b64: String,
}

#[derive(Serialize)]
pub struct SignMessageResponse {
    signature: String,
}

pub async fn sign_message(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SignMessageRequest>,
) -> Result<Json<SignMessageResponse>, ApiError> {
    let tenant = authenticate_wallet(&state, &headers, &wallet)?;
    let message = decode_bounded(&request.message_b64, MAX_MESSAGE_BYTES)?;
    let signer = connect_signer(tenant).await?;
    let signature = signer
        .sign_message(&message)
        .await
        .map_err(provider_error)?;
    Ok(Json(SignMessageResponse {
        signature: signature.to_string(),
    }))
}

fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<std::sync::Arc<TenantRecord>, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(unauthorized)?;
    let subject = state.cli().authenticate(token).ok_or_else(unauthorized)?;
    state.tenants().get(&subject).ok_or_else(unauthorized)
}

fn authenticate_wallet(
    state: &AppState,
    headers: &HeaderMap,
    wallet: &str,
) -> Result<std::sync::Arc<TenantRecord>, ApiError> {
    let tenant = authenticate(state, headers)?;
    if tenant.wallet_id != wallet {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "wallet_not_found",
            "That wallet does not belong to this CLI token.",
        ));
    }
    Ok(tenant)
}

async fn connect_signer(
    tenant: std::sync::Arc<TenantRecord>,
) -> Result<Box<dyn TransactionSigner>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let provider = pay_core::remote::provider(&tenant.provider).ok_or_else(|| {
            ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", "Wallet provider is unavailable.")
        })?;
        provider.connect(&tenant.credentials, &tenant.wallet_id).map_err(|error| {
            tracing::warn!(provider = %tenant.provider, error = %error, "hosted wallet connection failed");
            ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", "Wallet provider is unavailable.")
        })
    })
    .await
    .map_err(|_| provider_error("wallet task failed"))?
}

fn decode_bounded(value: &str, limit: usize) -> Result<Vec<u8>, ApiError> {
    if value.len() > limit.saturating_mul(2) {
        return Err(ApiError::bad_request(
            "payload_too_large",
            "Signing payload is too large.",
        ));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| ApiError::bad_request("invalid_payload", "Signing payload must be base64."))?;
    if bytes.len() > limit {
        return Err(ApiError::bad_request(
            "payload_too_large",
            "Signing payload is too large.",
        ));
    }
    Ok(bytes)
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "A valid CLI token is required.",
    )
}

fn provider_error(error: impl std::fmt::Display) -> ApiError {
    tracing::warn!(error = %error, "hosted wallet signing failed");
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "provider_error",
        "Wallet signing failed.",
    )
}
