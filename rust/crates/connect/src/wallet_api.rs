//! Tenant-scoped wallet access for a linked CLI.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use solana_keychain::TransactionSigner;
use solana_message::VersionedMessage;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

use crate::AppState;
use crate::protocol::ApiError;
use crate::tenants::TenantRecord;

const MAX_TRANSACTION_BYTES: usize = 16 * 1024;
const MAX_INSTRUCTIONS: usize = 16;
const MAX_TRANSFERS: usize = 8;
const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
const MAX_COMPUTE_UNIT_PRICE_MICROLAMPORTS: u64 = 50_000;

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
    let amount_minor = transaction_spend_minor(&transaction, &tenant.pubkey)?;
    let reservation = state
        .tenants()
        .reserve_cli_transaction(&tenant, amount_minor)
        .map_err(policy_denied)?;
    let signer = connect_signer(tenant).await?;
    let result = signer
        .sign_transaction(&mut transaction)
        .await
        .map_err(provider_error)?;
    let complete = matches!(result, solana_keychain::SignTransactionResult::Complete(_));
    let (transaction_b64, signature) = result.into_signed_transaction();
    reservation.commit();
    Ok(Json(SignTransactionResponse {
        transaction_b64,
        signature: signature.to_string(),
        complete,
    }))
}

pub async fn sign_message(
    State(state): State<AppState>,
    Path(wallet): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    authenticate_wallet(&state, &headers, &wallet)?;
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "message_signing_not_allowed",
        "Hosted CLI wallets only sign policy-checked payment transactions.",
    ))
}

/// Derive the USD policy amount from the transaction itself while rejecting
/// every instruction shape that could move unaccounted funds. Hosted CLI
/// wallets deliberately support only direct, known-stablecoin payments here;
/// richer flows run inside the hosted MCP where their typed intent is gated.
fn transaction_spend_minor(
    transaction: &VersionedTransaction,
    signer: &str,
) -> Result<u64, ApiError> {
    use pay_kit::mpp::protocol::solana::{is_known_stablecoin_mint, programs};

    transaction
        .message
        .sanitize()
        .map_err(|_| signing_denied("The transaction message is malformed."))?;
    match &transaction.message {
        VersionedMessage::V1(_) => {
            return Err(signing_denied(
                "Version 1 transactions are not enabled for hosted CLI wallets.",
            ));
        }
        VersionedMessage::V0(message) if !message.address_table_lookups.is_empty() => {
            return Err(signing_denied(
                "Address lookup tables are not enabled for hosted CLI wallets.",
            ));
        }
        VersionedMessage::Legacy(_) | VersionedMessage::V0(_) => {}
    }

    let signer: Pubkey = signer
        .parse()
        .map_err(|_| signing_denied("The hosted wallet address is invalid."))?;
    let keys = transaction.message.static_account_keys();
    let signer_index = keys
        .iter()
        .position(|key| key == &signer)
        .ok_or_else(|| signing_denied("The hosted wallet is not an account in the transaction."))?;
    if !transaction.message.is_signer(signer_index) {
        return Err(signing_denied(
            "The hosted wallet is not a required transaction signer.",
        ));
    }
    let instructions = transaction.message.instructions();
    if instructions.is_empty() || instructions.len() > MAX_INSTRUCTIONS {
        return Err(signing_denied(
            "The transaction has an unsupported number of instructions.",
        ));
    }

    let mut total_raw = 0_u64;
    let mut transfers = 0_usize;
    let mut compute_limit_seen = false;
    let mut compute_price_seen = false;
    for instruction in instructions {
        let program = key_at(keys, instruction.program_id_index)?;
        match program.to_string().as_str() {
            programs::TOKEN_PROGRAM | programs::TOKEN_2022_PROGRAM => {
                if instruction.accounts.len() != 4
                    || instruction.data.len() != 10
                    || instruction.data[0] != 12
                    || instruction.data[9] != 6
                {
                    return Err(signing_denied(
                        "Only six-decimal transfer_checked stablecoin instructions are allowed.",
                    ));
                }
                let source = key_at(keys, instruction.accounts[0])?;
                let mint = key_at(keys, instruction.accounts[1])?;
                let authority = key_at(keys, instruction.accounts[3])?;
                if authority != &signer {
                    return Err(signing_denied(
                        "Every transfer must be authorized by the hosted wallet.",
                    ));
                }
                if !is_known_stablecoin_mint(&mint.to_string()) {
                    return Err(signing_denied(
                        "Only known stablecoin mints are allowed for hosted CLI wallets.",
                    ));
                }
                let expected_source = associated_token_address(&signer, mint, program);
                if source != &expected_source {
                    return Err(signing_denied(
                        "A transfer source does not belong to the hosted wallet.",
                    ));
                }
                let amount = u64::from_le_bytes(
                    instruction.data[1..9]
                        .try_into()
                        .map_err(|_| signing_denied("A transfer amount is malformed."))?,
                );
                if amount == 0 {
                    return Err(signing_denied("Zero-value transfers are not allowed."));
                }
                total_raw = total_raw
                    .checked_add(amount)
                    .ok_or_else(|| signing_denied("The transfer total overflows."))?;
                transfers += 1;
                if transfers > MAX_TRANSFERS {
                    return Err(signing_denied("The transaction has too many transfers."));
                }
            }
            programs::ASSOCIATED_TOKEN_PROGRAM => {
                return Err(signing_denied(
                    "Hosted CLI wallets do not fund associated-token-account creation.",
                ));
            }
            programs::COMPUTE_BUDGET_PROGRAM => match instruction.data.as_slice() {
                [2, bytes @ ..] if bytes.len() == 4 && !compute_limit_seen => {
                    compute_limit_seen = true;
                    let limit = u32::from_le_bytes(bytes.try_into().unwrap());
                    if limit > MAX_COMPUTE_UNIT_LIMIT {
                        return Err(signing_denied("The compute-unit limit is too high."));
                    }
                }
                [3, bytes @ ..] if bytes.len() == 8 && !compute_price_seen => {
                    compute_price_seen = true;
                    let price = u64::from_le_bytes(bytes.try_into().unwrap());
                    if price > MAX_COMPUTE_UNIT_PRICE_MICROLAMPORTS {
                        return Err(signing_denied("The compute-unit price is too high."));
                    }
                }
                _ => return Err(signing_denied("The compute-budget instruction is invalid.")),
            },
            programs::MEMO_PROGRAM if instruction.data.len() <= 566 => {}
            _ => {
                return Err(signing_denied(
                    "The transaction contains an instruction that hosted CLI wallets do not allow.",
                ));
            }
        }
    }
    if transfers == 0 {
        return Err(signing_denied(
            "The transaction does not contain a stablecoin payment.",
        ));
    }

    // Stablecoins accepted above have six decimals. Policy accounting uses
    // four decimals; round upward and charge at least one cent so dust cannot
    // be used to exhaust the wallet through transaction fees.
    Ok(total_raw.div_ceil(100).max(100))
}

fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    use pay_kit::mpp::protocol::solana::programs;
    let ata_program: Pubkey = programs::ASSOCIATED_TOKEN_PROGRAM
        .parse()
        .expect("pay-kit ATA program id is valid");
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    )
    .0
}

fn key_at(keys: &[Pubkey], index: u8) -> Result<&Pubkey, ApiError> {
    keys.get(index as usize)
        .ok_or_else(|| signing_denied("An instruction account index is out of range."))
}

fn signing_denied(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "signing_not_allowed", message.into())
}

fn policy_denied(error: impl std::fmt::Display) -> ApiError {
    tracing::warn!(error = %error, "hosted wallet policy denied transaction");
    ApiError::new(
        StatusCode::FORBIDDEN,
        "policy_denied",
        "The transaction exceeds this wallet's spending policy.",
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use solana_message::{Message, MessageHeader, compiled_instruction::CompiledInstruction};

    fn payment_transaction(amount: u64) -> (VersionedTransaction, Pubkey) {
        use pay_kit::mpp::protocol::solana::{mints, programs};

        let signer = Pubkey::new_unique();
        let mint: Pubkey = mints::USDC_MAINNET.parse().unwrap();
        let token_program: Pubkey = programs::TOKEN_PROGRAM.parse().unwrap();
        let source = associated_token_address(&signer, &mint, &token_program);
        let destination = Pubkey::new_unique();
        let mut data = vec![12];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(6);
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 2,
            },
            account_keys: vec![signer, source, mint, destination, token_program],
            recent_blockhash: Default::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 4,
                accounts: vec![1, 2, 3, 0],
                data,
            }],
        };
        (
            VersionedTransaction {
                signatures: vec![Default::default()],
                message: VersionedMessage::Legacy(message),
            },
            signer,
        )
    }

    #[test]
    fn derives_policy_amount_from_allowed_stablecoin_transfer() {
        let (transaction, signer) = payment_transaction(123_456);
        assert_eq!(
            transaction_spend_minor(&transaction, &signer.to_string()).unwrap(),
            1_235
        );
    }

    #[test]
    fn charges_dust_transfer_at_least_one_cent() {
        let (transaction, signer) = payment_transaction(1);
        assert_eq!(
            transaction_spend_minor(&transaction, &signer.to_string()).unwrap(),
            100
        );
    }

    #[test]
    fn rejects_unapproved_instruction_program() {
        let (mut transaction, signer) = payment_transaction(100_000);
        let system_program: Pubkey = pay_kit::mpp::protocol::solana::programs::SYSTEM_PROGRAM
            .parse()
            .unwrap();
        let VersionedMessage::Legacy(message) = &mut transaction.message else {
            unreachable!()
        };
        message.account_keys.push(system_program);
        message.instructions.push(CompiledInstruction {
            program_id_index: 5,
            accounts: vec![0, 3],
            data: vec![2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
        });

        let error = transaction_spend_minor(&transaction, &signer.to_string()).unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
    }
}
