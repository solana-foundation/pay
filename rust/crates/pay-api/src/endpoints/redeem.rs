//! `POST /v1/redeem`
//!
//! Activation-campaign redemption. Takes a code + destination, looks up
//! the gateway hot wallet's recent transactions via Helius, atomically claims
//! the code in Redis, then builds + signs + broadcasts a transaction that pays
//! the code's campaign amount of the configured token to the destination and
//! stamps `Redeem code <code>` as a memo. Success is only reported once the
//! payout confirms; an unconfirmed payout keeps its signature in the claim and
//! is reconciled on the next attempt.
//!
//! The fee payer / hot wallet is `state.send.fee_payer` — the same
//! GCP-KMS-backed signer the `/v1/send` endpoint uses. The redemption
//! endpoint additionally needs the mint, amount, network, provider credential,
//! and Redis claim store.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pay_api_core::Error;
use pay_api_core::ata::{ATA_PROGRAM_ID, associated_token_address};
use pay_api_core::receipt::MEMO_PROGRAM;
use pay_api_types::Network;
use pay_kit::mpp::solana_keychain::TransactionSigner;
use pay_types::landing::{
    ChainState, ChainTip, Landing, RecordedPayout, ReplaceReason, Replaceable, SignatureStatuses,
    confirm_not_landed, signature_status_from_rpc,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use spl_token_2022_interface::instruction as token_ix;
use tracing::{info, warn};

use crate::config::validate_redemption_code;
use crate::state::AppState;

// ── Constants ─────────────────────────────────────────────────────────────

/// SPL Memo v2 program ID, typed. String form lives in
/// `pay_api_core::receipt::MEMO_PROGRAM` (also used by the receipt
/// parser); we parse it once here for the instruction-builder path.
static MEMO_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str(MEMO_PROGRAM).expect("MEMO_PROGRAM is a valid pubkey"));

/// Memo prefix for every redemption Transfer. The burn check matches
/// the full string `"Redeem code <CODE>"` so a hand-written memo would
/// have to use this exact phrasing + a real code to collide.
const REDEEM_MEMO_PREFIX: &str = "Redeem code ";

/// Cap on Helius pages walked per request. Eight pages covers the
/// current campaigns with slack.
const DEFAULT_MAX_SCAN_PAGES: usize = 8;

/// Helius enhanced-transactions page limit.
const HELIUS_PAGE_LIMIT: usize = 100;

/// Stays below the CLI's 30s redeem request timeout.
const REDEEM_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

// ── HTTP shapes ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RedeemRequest {
    pub code: String,
    pub destination: String,
}

#[derive(Debug, Serialize)]
struct RedeemResponse {
    signature: String,
    destination: String,
    campaign: String,
    /// USD-formatted activation amount (e.g. `"$0.10"`). Derived from
    /// the campaign amount + decimals so the CLI can
    /// surface it in the success notice without needing to know the
    /// atomic unit / stablecoin layout. Always `$`-prefixed since
    /// every supported `redemption.currency` is a USD stablecoin.
    amount: String,
}

// ── Handler ───────────────────────────────────────────────────────────────

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RedeemRequest>,
) -> Response {
    let cfg = match &state.redemption {
        Some(cfg) if cfg.enabled => cfg,
        _ => return err_resp(StatusCode::SERVICE_UNAVAILABLE, "redemption disabled"),
    };
    if let Err(msg) = validate_redemption_code(&req.code) {
        return err_resp(StatusCode::BAD_REQUEST, &msg);
    }
    let signer = match fee_payer_signer(&state).await {
        Ok(s) => s,
        Err(_) => {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "fee-payer signer unavailable",
            );
        }
    };
    redeem(&state, cfg, signer, &req).await
}

async fn redeem(
    state: &AppState,
    cfg: &RedemptionState,
    signer: Arc<dyn TransactionSigner>,
    req: &RedeemRequest,
) -> Response {
    let grant = match cfg.grants.get(&req.code) {
        Some(grant) => grant,
        None => return err_resp(StatusCode::BAD_REQUEST, "unknown code"),
    };
    let destination_pk = match Pubkey::from_str(req.destination.trim()) {
        Ok(pk) => pk,
        Err(_) => return err_resp(StatusCode::BAD_REQUEST, "invalid destination pubkey"),
    };
    let hot_wallet = signer.pubkey();
    let rpc_url = match state.rpc_url_for(cfg.network) {
        Ok(url) => url.to_string(),
        Err(e) => return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    // 1. Dedup scan.
    match find_prior_burn(state, &rpc_url, &hot_wallet, &req.code, cfg).await {
        Ok(Some(sig)) => {
            return err_resp_with_extra(
                StatusCode::CONFLICT,
                "code already redeemed",
                json!({ "signature": sig }),
            );
        }
        Ok(None) => {}
        Err(DedupScanError::Request(e)) => {
            // Strip the request URL before logging — it carries the provider
            // `api-key=` query param and `reqwest::Error`'s
            // Display impl prints it verbatim.
            warn!(error = %e.without_url(), "Helius dedup scan failed");
            return err_resp(StatusCode::BAD_GATEWAY, "redemption dedup check failed");
        }
        Err(error) => {
            warn!(%error, "redemption dedup scan failed closed");
            return err_resp(StatusCode::BAD_GATEWAY, "redemption dedup check failed");
        }
    }

    // 2. Atomically claim the code in durable storage. A claim is only
    // released when its payout provably cannot land; an existing claim is
    // reconciled against the chain before a retry may proceed.
    let mut reconciled = false;
    loop {
        match acquire_redemption_claim(cfg, &req.code, &destination_pk).await {
            Ok(true) => break,
            Ok(false) if !reconciled => {
                reconciled = true;
                match reconcile_existing_claim(state, cfg, &rpc_url, &req.code).await {
                    Ok(()) => continue,
                    Err(response) => return response,
                }
            }
            Ok(false) => {
                return err_resp(StatusCode::CONFLICT, "code already redeemed or claimed");
            }
            Err(error) => {
                warn!(%error, "redemption claim store unavailable");
                return err_resp(StatusCode::BAD_GATEWAY, "redemption claim failed");
            }
        }
    }

    // 3. Build the unsigned tx.
    let (blockhash_str, last_valid_block_height) = match state
        .rpc
        .get_latest_blockhash_with_last_valid_height(&rpc_url)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            release_redemption_claim_best_effort(cfg, &req.code, &destination_pk).await;
            return err_resp(StatusCode::BAD_GATEWAY, &e.to_string());
        }
    };
    let blockhash = match solana_hash::Hash::from_str(&blockhash_str) {
        Ok(b) => b,
        Err(_) => {
            release_redemption_claim_best_effort(cfg, &req.code, &destination_pk).await;
            return err_resp(StatusCode::BAD_GATEWAY, "blockhash decode failed");
        }
    };

    let unsigned = match build_unsigned(
        &req.code,
        grant.amount,
        &destination_pk,
        &hot_wallet,
        cfg,
        blockhash,
    ) {
        Ok(tx) => tx,
        Err(msg) => {
            release_redemption_claim_best_effort(cfg, &req.code, &destination_pk).await;
            return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };

    // 4. Sign before recording the signature. A signing failure has not
    // sent funds and can safely be retried.
    let (signature, signed_tx) = match sign_transaction(unsigned, signer).await {
        Ok(signed) => signed,
        Err(msg) => {
            release_redemption_claim_best_effort(cfg, &req.code, &destination_pk).await;
            return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };
    let pending = ClaimState::Pending {
        signature,
        last_valid_block_height: Some(last_valid_block_height),
    }
    .encode();
    match transition_claim(
        cfg,
        &req.code,
        &ClaimState::Claimed(destination_pk).encode(),
        Some(&pending),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return err_resp(StatusCode::CONFLICT, "code claim was lost"),
        Err(error) => {
            warn!(%error, "redemption claim could not record the payout signature");
            release_redemption_claim_best_effort(cfg, &req.code, &destination_pk).await;
            return err_resp(StatusCode::BAD_GATEWAY, "redemption claim failed");
        }
    }

    // 5. Broadcast, then wait for the payout to confirm before reporting it.
    if let Err(error) = broadcast_transaction(state, &rpc_url, &signed_tx).await {
        if let Some(proof) = NeverBroadcast::from_preflight_rejection(&error) {
            // A rejected preflight never submits the transaction, so the
            // claim can be retried.
            if let Err(error) = release_signed_claim(
                cfg,
                &req.code,
                &pending,
                ReleaseProof::NeverBroadcast(proof),
            )
            .await
            {
                warn!(%error, "failed to release redemption claim");
            }
            return err_resp(StatusCode::BAD_GATEWAY, &error.to_string());
        }
        // Transport and timeout errors keep the pending claim: the RPC may
        // have accepted the transaction. A retry reconciles it.
        warn!(%error, %signature, "redemption broadcast outcome unknown");
        return pending_resp(&signature);
    }

    let finalized = match await_signature(state, &rpc_url, &signature, cfg.confirm_timeout).await {
        PayoutStatus::Finalized(finalized) => finalized,
        PayoutStatus::Failed => {
            // Released only once the failure is final, as for any retry.
            warn!(%signature, "redemption payout failed on-chain");
            return match reconcile_existing_claim(state, cfg, &rpc_url, &req.code).await {
                Ok(()) => err_resp(StatusCode::BAD_GATEWAY, "redemption payout failed; retry"),
                Err(response) => response,
            };
        }
        // Confirmed but not yet final stays pending: a confirmed block can
        // still be dropped, and a terminal claim would consume the code.
        PayoutStatus::Pending => return pending_resp(&signature),
    };

    if let Err(error) = mark_claim_redeemed(cfg, &req.code, &pending, signature, finalized).await {
        // The pending claim already prevents another payout and reconciles
        // to confirmed on the next attempt.
        warn!(%error, %signature, "failed to mark redemption claim confirmed");
    }

    info!(
        campaign = %grant.campaign_id,
        destination = %destination_pk,
        signature = %signature,
        "redeem succeeded"
    );

    let amount = format!(
        "${}",
        super::send::format_base_units(grant.amount, cfg.decimals)
    );

    (
        StatusCode::OK,
        Json(RedeemResponse {
            signature: signature.to_string(),
            destination: destination_pk.to_string(),
            campaign: grant.campaign_id.clone(),
            amount,
        }),
    )
        .into_response()
}

/// Resolve a claim left by an earlier attempt. `Ok(())` means the claim was
/// released because its payout can never land, so the caller may claim again.
#[allow(clippy::result_large_err)]
async fn reconcile_existing_claim(
    state: &AppState,
    cfg: &RedemptionState,
    rpc_url: &str,
    code: &str,
) -> Result<(), Response> {
    let raw = match read_claim(cfg, code).await {
        Ok(Some(raw)) => raw,
        // Released concurrently; claim again.
        Ok(None) => return Ok(()),
        Err(error) => {
            warn!(%error, "redemption claim store unavailable");
            return Err(err_resp(StatusCode::BAD_GATEWAY, "redemption claim failed"));
        }
    };
    let (signature, last_valid_block_height) = match ClaimState::decode(&raw) {
        Some(ClaimState::Confirmed(signature)) => {
            return Err(already_redeemed_resp(&signature));
        }
        Some(ClaimState::Pending {
            signature,
            last_valid_block_height,
        }) => (signature, last_valid_block_height),
        // A claim without a recorded signature may still be mid-flight.
        Some(ClaimState::Claimed(_)) | None => {
            return Err(err_resp(
                StatusCode::CONFLICT,
                "code already redeemed or claimed",
            ));
        }
    };

    let cluster = Cluster {
        rpc: &state.rpc,
        rpc_url,
    };
    let payout = RecordedPayout {
        signature: signature.to_string(),
        last_valid_block_height,
    };
    let landing = match confirm_not_landed(&cluster, std::slice::from_ref(&payout)).await {
        Ok(mut landings) => landings.pop().unwrap_or(Landing::Unknown),
        Err(error) => {
            warn!(%error, %signature, "redemption reconciliation lookup failed");
            return Err(err_resp(StatusCode::BAD_GATEWAY, "redemption claim failed"));
        }
    };
    match landing {
        Landing::Confirmed => {
            let status = match state
                .rpc
                .get_signature_status(rpc_url, &signature.to_string())
                .await
            {
                Ok(status) => classify_signature_status(status.as_ref()),
                Err(error) => {
                    warn!(%error, %signature, "redemption finality lookup failed");
                    return Err(pending_resp(&signature));
                }
            };
            let PayoutStatus::Finalized(finalized) = status else {
                // Landed but not final: the pending claim keeps the code.
                return Err(pending_resp(&signature));
            };
            if let Err(error) = mark_claim_redeemed(cfg, code, &raw, signature, finalized).await {
                warn!(%error, %signature, "failed to mark redemption claim confirmed");
            }
            Err(already_redeemed_resp(&signature))
        }
        Landing::Replaceable(proof) => {
            match release_signed_claim(cfg, code, &raw, ReleaseProof::CannotLand(proof)).await {
                Ok(_) => Ok(()),
                Err(error) => {
                    warn!(%error, "redemption claim store unavailable");
                    Err(err_resp(StatusCode::BAD_GATEWAY, "redemption claim failed"))
                }
            }
        }
        Landing::MayLand | Landing::Unknown => Err(pending_resp(&signature)),
    }
}

/// The cluster `rpc_url` serves, as [`confirm_not_landed`] reads it.
struct Cluster<'a> {
    rpc: &'a pay_api_core::RpcClient,
    rpc_url: &'a str,
}

impl ChainState for Cluster<'_> {
    type Error = Error;

    async fn finalized_tip(&self) -> Result<ChainTip, Error> {
        let (slot, block_height) = self.rpc.get_finalized_tip(self.rpc_url).await?;
        Ok(ChainTip { slot, block_height })
    }

    async fn signature_statuses(&self, signatures: &[String]) -> Result<SignatureStatuses, Error> {
        let (context_slot, entries) = self
            .rpc
            .get_signature_statuses(self.rpc_url, signatures)
            .await?;
        Ok(SignatureStatuses {
            context_slot,
            statuses: entries.iter().map(signature_status_from_rpc).collect(),
        })
    }
}

/// Why a claim holding a signed payout may be released: nothing signed
/// under it can ever land.
enum ReleaseProof {
    /// The chain proves the payout failed for good or expired unseen.
    CannotLand(Replaceable),
    NeverBroadcast(NeverBroadcast),
}

/// The RPC rejected the payout in preflight, so it was never forwarded.
struct NeverBroadcast(());

impl NeverBroadcast {
    fn from_preflight_rejection(error: &Error) -> Option<Self> {
        is_deterministic_preflight_error(error).then_some(Self(()))
    }
}

/// Release a claim that recorded a signed payout. The only way to delete
/// one: a release needs a proof the payout cannot land.
async fn release_signed_claim(
    cfg: &RedemptionState,
    code: &str,
    expected: &str,
    proof: ReleaseProof,
) -> Result<bool, redis::RedisError> {
    let reason = match proof {
        ReleaseProof::CannotLand(proof) => match proof.reason() {
            ReplaceReason::Failed => "payout failed for good",
            ReplaceReason::Expired => "payout expired unseen",
        },
        ReleaseProof::NeverBroadcast(_) => "payout rejected in preflight",
    };
    info!(reason, "releasing redemption claim");
    transition_claim(cfg, code, expected, None).await
}

/// Proof a payout is finalized without error. Only
/// [`classify_signature_status`] makes one, and only it unlocks the terminal
/// claim: anything short of finality can still be rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FinalizedPayout(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayoutStatus {
    Finalized(FinalizedPayout),
    Failed,
    /// Unseen, processed, or confirmed but not yet final.
    Pending,
}

/// Record the code as redeemed for good. Terminal, so it takes the proof
/// that the payout behind `expected` is finalized.
async fn mark_claim_redeemed(
    cfg: &RedemptionState,
    code: &str,
    expected: &str,
    signature: Signature,
    _finalized: FinalizedPayout,
) -> Result<bool, redis::RedisError> {
    transition_claim(
        cfg,
        code,
        expected,
        Some(&ClaimState::Confirmed(signature).encode()),
    )
    .await
}

fn classify_signature_status(status: Option<&serde_json::Value>) -> PayoutStatus {
    let Some(status) = status else {
        return PayoutStatus::Pending;
    };
    if status.get("err").is_some_and(|err| !err.is_null()) {
        return PayoutStatus::Failed;
    }
    match status
        .get("confirmationStatus")
        .and_then(serde_json::Value::as_str)
    {
        Some("finalized") => PayoutStatus::Finalized(FinalizedPayout(())),
        _ => PayoutStatus::Pending,
    }
}

/// Poll the payout signature until it finalizes, fails, or `timeout` elapses.
async fn await_signature(
    state: &AppState,
    rpc_url: &str,
    signature: &Signature,
    timeout: std::time::Duration,
) -> PayoutStatus {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut backoff = std::time::Duration::from_millis(200);
    loop {
        match state
            .rpc
            .get_signature_status(rpc_url, &signature.to_string())
            .await
        {
            Ok(status) => match classify_signature_status(status.as_ref()) {
                PayoutStatus::Pending => {}
                outcome => return outcome,
            },
            Err(error) => warn!(%error, %signature, "redemption status poll failed"),
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return PayoutStatus::Pending;
        }
        tokio::time::sleep(backoff.min(deadline - now)).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
    }
}

fn redeem_memo(code: &str) -> String {
    format!("{REDEEM_MEMO_PREFIX}{code}")
}

// ── Helius dedup scan ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HeliusTx {
    signature: String,
    #[serde(default)]
    instructions: Vec<HeliusIx>,
}

#[derive(Debug, Deserialize)]
struct HeliusIx {
    #[serde(rename = "programId", default)]
    program_id: String,
    #[serde(default)]
    data: String,
}

#[derive(Debug, thiserror::Error)]
enum DedupScanError {
    #[error("Helius request failed")]
    Request(#[from] reqwest::Error),
    #[error("Helius history scan exhausted its page cap")]
    PageCap,
    #[error("redemption dedup is not configured")]
    NotConfigured,
    #[error("prior redemption candidate could not be verified: {0}")]
    Unverifiable(String),
}

/// Walk the hot wallet's transaction-history provider "transactions for address" history
/// looking for any prior tx whose memo program ix decodes to
/// `Redeem code <code>`. History matches are only candidates: each is fetched
/// from the trusted RPC and must be a successful hot-wallet payout carrying
/// the memo. Returns the verified signature when found.
async fn find_prior_burn(
    state: &AppState,
    rpc_url: &str,
    hot_wallet: &Pubkey,
    code: &str,
    cfg: &RedemptionState,
) -> Result<Option<String>, DedupScanError> {
    if cfg.solana_rpc_api_key.is_empty() {
        return Err(DedupScanError::NotConfigured);
    }

    let http = &cfg.http_client;
    let needle = redeem_memo(code);
    let hot_wallet_str = hot_wallet.to_string();
    let mut cursor: Option<String> = None;

    for page in 0..cfg.max_scan_pages {
        let mut url = reqwest::Url::parse(&format!(
            "{}/v0/addresses/{}/transactions",
            cfg.helius_base.trim_end_matches('/'),
            hot_wallet_str
        ))
        .expect("static URL parses");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("api-key", &cfg.solana_rpc_api_key);
            // No `type=` filter: Helius's heuristic classifier can mark a
            // multi-instruction tx (idempotent-ATA-create + TransferChecked
            // + memo) as something other than `TRANSFER`, which would
            // silently exclude prior burns from the scan. Match purely on
            // the memo content so reclassification can't cause a miss.
            q.append_pair("limit", &HELIUS_PAGE_LIMIT.to_string());
            if let Some(c) = cursor.as_deref() {
                q.append_pair("before-signature", c);
            }
        }
        let txs: Vec<HeliusTx> = http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if txs.is_empty() {
            return Ok(None);
        }
        for tx in &txs {
            for ix in &tx.instructions {
                if ix.program_id == MEMO_PROGRAM
                    && let Some(decoded_bytes) = bs58::decode(&ix.data).into_vec().ok()
                    && let Ok(decoded) = std::str::from_utf8(&decoded_bytes)
                    && decoded == needle
                    && verify_prior_payout(state, rpc_url, hot_wallet, &tx.signature, &needle, cfg)
                        .await?
                {
                    return Ok(Some(tx.signature.clone()));
                }
            }
        }
        // Underfull page → end of history. Only the end of history proves a
        // code unused: redemptions predating the Redis claim store live
        // nowhere else, so a scan that cannot reach it fails closed below.
        if txs.len() < HELIUS_PAGE_LIMIT {
            return Ok(None);
        }
        cursor = txs.last().map(|t| t.signature.clone());
        if page + 1 == cfg.max_scan_pages {
            return Err(DedupScanError::PageCap);
        }
    }
    Err(DedupScanError::PageCap)
}

/// Whether `signature` is a successful payout signed by the hot wallet that
/// moves the configured mint out of its token account and carries `memo`.
async fn verify_prior_payout(
    state: &AppState,
    rpc_url: &str,
    hot_wallet: &Pubkey,
    signature: &str,
    memo: &str,
    cfg: &RedemptionState,
) -> Result<bool, DedupScanError> {
    let tx = state
        .rpc
        .get_transaction_json_parsed(rpc_url, signature)
        .await
        .map_err(|error| DedupScanError::Unverifiable(error.to_string()))?
        .ok_or_else(|| DedupScanError::Unverifiable(format!("{signature} not found")))?;
    Ok(is_hot_wallet_payout(&tx, hot_wallet, memo, cfg))
}

fn is_hot_wallet_payout(
    tx: &serde_json::Value,
    hot_wallet: &Pubkey,
    memo: &str,
    cfg: &RedemptionState,
) -> bool {
    use serde_json::Value;

    if tx.pointer("/meta/err").is_none_or(|err| !err.is_null()) {
        return false;
    }
    let hot_ata = associated_token_address(hot_wallet, &cfg.mint, &cfg.token_program).to_string();
    let hot_wallet = hot_wallet.to_string();
    let message = tx.pointer("/transaction/message");
    let signed = message
        .and_then(|message| message.get("accountKeys"))
        .and_then(Value::as_array)
        .is_some_and(|keys| {
            keys.iter().any(|key| {
                key.get("pubkey").and_then(Value::as_str) == Some(hot_wallet.as_str())
                    && key.get("signer").and_then(Value::as_bool) == Some(true)
            })
        });
    if !signed {
        return false;
    }
    let Some(instructions) = message
        .and_then(|message| message.get("instructions"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let mint = cfg.mint.to_string();
    let token_program = cfg.token_program.to_string();
    let has_memo = instructions.iter().any(|ix| {
        ix.get("programId").and_then(Value::as_str) == Some(MEMO_PROGRAM)
            && (ix.get("parsed").and_then(Value::as_str) == Some(memo)
                || ix
                    .get("data")
                    .and_then(Value::as_str)
                    .and_then(|data| bs58::decode(data).into_vec().ok())
                    .is_some_and(|bytes| bytes == memo.as_bytes()))
    });
    let has_transfer = instructions.iter().any(|ix| {
        let info = ix.pointer("/parsed/info");
        let field = |name: &str| info.and_then(|info| info.get(name)).and_then(Value::as_str);
        ix.get("programId").and_then(Value::as_str) == Some(token_program.as_str())
            && matches!(
                ix.pointer("/parsed/type").and_then(Value::as_str),
                Some("transfer" | "transferChecked")
            )
            && field("source") == Some(hot_ata.as_str())
            && field("authority") == Some(hot_wallet.as_str())
            && field("mint").is_none_or(|m| m == mint)
    });
    has_memo && has_transfer
}

fn redemption_claim_key(code: &str) -> String {
    format!(
        "pay-api:redemption:{}",
        blake3::hash(code.as_bytes()).to_hex()
    )
}

async fn acquire_redemption_claim(
    cfg: &RedemptionState,
    code: &str,
    destination: &Pubkey,
) -> Result<bool, redis::RedisError> {
    let mut connection = cfg.claim_store.get_connection_manager().await?;
    let acquired: Option<String> = redis::cmd("SET")
        .arg(redemption_claim_key(code))
        .arg(ClaimState::Claimed(*destination).encode())
        .arg("NX")
        .query_async(&mut connection)
        .await?;
    Ok(acquired.is_some())
}

/// Durable redemption claim value stored under [`redemption_claim_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimState {
    /// Claimed; no transaction signed yet.
    Claimed(Pubkey),
    /// A payout was signed and may have been submitted. Legacy `broadcast:`
    /// claims decode here without an expiry height.
    Pending {
        signature: Signature,
        last_valid_block_height: Option<u64>,
    },
    /// The payout confirmed on-chain.
    Confirmed(Signature),
}

impl ClaimState {
    fn encode(&self) -> String {
        match self {
            Self::Claimed(destination) => format!("claimed:{destination}"),
            Self::Pending {
                signature,
                last_valid_block_height: Some(height),
            } => format!("pending:{signature}:{height}"),
            Self::Pending {
                signature,
                last_valid_block_height: None,
            } => format!("broadcast:{signature}"),
            Self::Confirmed(signature) => format!("confirmed:{signature}"),
        }
    }

    fn decode(raw: &str) -> Option<Self> {
        let (kind, rest) = raw.split_once(':')?;
        match kind {
            "claimed" => Pubkey::from_str(rest).ok().map(Self::Claimed),
            "pending" => {
                let (signature, height) = rest.split_once(':')?;
                Some(Self::Pending {
                    signature: Signature::from_str(signature).ok()?,
                    last_valid_block_height: Some(height.parse().ok()?),
                })
            }
            "broadcast" => Some(Self::Pending {
                signature: Signature::from_str(rest).ok()?,
                last_valid_block_height: None,
            }),
            "confirmed" => Signature::from_str(rest).ok().map(Self::Confirmed),
            _ => None,
        }
    }
}

async fn read_claim(
    cfg: &RedemptionState,
    code: &str,
) -> Result<Option<String>, redis::RedisError> {
    let mut connection = cfg.claim_store.get_connection_manager().await?;
    redis::cmd("GET")
        .arg(redemption_claim_key(code))
        .query_async(&mut connection)
        .await
}

/// Atomically replace the claim when it still equals `expected`; `None`
/// deletes it. Returns whether the claim changed.
async fn transition_claim(
    cfg: &RedemptionState,
    code: &str,
    expected: &str,
    next: Option<&str>,
) -> Result<bool, redis::RedisError> {
    const TRANSITION: &str = r#"
if redis.call('GET', KEYS[1]) ~= ARGV[1] then
  return 0
end
if ARGV[2] == '' then
  redis.call('DEL', KEYS[1])
else
  redis.call('SET', KEYS[1], ARGV[2])
end
return 1
"#;
    let mut connection = cfg.claim_store.get_connection_manager().await?;
    let changed: i32 = redis::Script::new(TRANSITION)
        .key(redemption_claim_key(code))
        .arg(expected)
        .arg(next.unwrap_or(""))
        .invoke_async(&mut connection)
        .await?;
    Ok(changed == 1)
}

/// Release a claim that never recorded a payout signature. It only matches
/// `Claimed`, so a pending or confirmed claim is never released here.
async fn release_redemption_claim_best_effort(
    cfg: &RedemptionState,
    code: &str,
    destination: &Pubkey,
) {
    let unsigned = ClaimState::Claimed(*destination).encode();
    if let Err(error) = transition_claim(cfg, code, &unsigned, None).await {
        warn!(%error, "failed to release redemption claim");
    }
}

fn pending_resp(signature: &Signature) -> Response {
    err_resp_with_extra(
        StatusCode::GATEWAY_TIMEOUT,
        "redemption pending confirmation; retry later",
        json!({ "pending_signature": signature.to_string() }),
    )
}

fn already_redeemed_resp(signature: &Signature) -> Response {
    err_resp_with_extra(
        StatusCode::CONFLICT,
        "code already redeemed",
        json!({ "signature": signature.to_string() }),
    )
}

// ── VersionedTransaction construction ──────────────────────────────────────────────

fn build_unsigned(
    code: &str,
    amount: u64,
    destination: &Pubkey,
    hot_wallet: &Pubkey,
    cfg: &RedemptionState,
    blockhash: solana_hash::Hash,
) -> Result<VersionedTransaction, String> {
    let hot_ata = associated_token_address(hot_wallet, &cfg.mint, &cfg.token_program);
    let dest_ata = associated_token_address(destination, &cfg.mint, &cfg.token_program);

    let mut ixs: Vec<Instruction> = Vec::with_capacity(3);

    // 1. Idempotent ATA creation for destination — no-op if it exists.
    ixs.push(create_associated_token_account_idempotent_ix(
        hot_wallet,
        destination,
        &cfg.mint,
        &cfg.token_program,
    ));

    // 2. TransferChecked (mint + decimals validated on-chain).
    ixs.push(
        token_ix::transfer_checked(
            &cfg.token_program,
            &hot_ata,
            &cfg.mint,
            &dest_ata,
            hot_wallet,
            &[],
            amount,
            cfg.decimals,
        )
        .map_err(|e| format!("transfer_checked: {e}"))?,
    );

    // 3. Memo.
    ixs.push(Instruction {
        program_id: *MEMO_PROGRAM_ID,
        accounts: Vec::<AccountMeta>::new(),
        data: redeem_memo(code).into_bytes(),
    });

    pay_kit::core::tx::build_unsigned(
        pay_kit::core::tx::TxVersion::V0,
        hot_wallet,
        &ixs,
        blockhash,
        None,
    )
    .map_err(|e| format!("build redeem transaction: {e}"))
}

/// Sign and serialize the transaction without contacting the RPC.
async fn sign_transaction(
    mut tx: VersionedTransaction,
    signer: Arc<dyn TransactionSigner>,
) -> Result<(Signature, String), String> {
    let fee_payer = signer.pubkey();
    pay_kit::core::signing::cosign_versioned_fee_payer(signer.as_ref(), &fee_payer, &mut tx)
        .await
        .map_err(|e| format!("fee-payer sign failed: {e}"))?;
    let signature = *tx
        .signatures
        .first()
        .ok_or_else(|| "signed transaction has no signature".to_string())?;
    let encoded = pay_kit::core::tx::encode(&tx).map_err(|e| format!("encode: {e}"))?;
    Ok((signature, encoded))
}

async fn broadcast_transaction(
    state: &AppState,
    rpc_url: &str,
    tx_b64: &str,
) -> Result<Signature, Error> {
    let sig_str = state.rpc.send_raw_transaction(rpc_url, tx_b64).await?;
    Signature::from_str(&sig_str).map_err(|_| Error::RpcMalformed)
}

fn is_deterministic_preflight_error(error: &Error) -> bool {
    let Error::RpcResponse(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    [
        "simulation failed",
        "transaction simulation failed",
        "instructionerror",
        "custom program error",
        "insufficient funds",
        "blockhash not found",
        "blockhashnotfound",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

async fn fee_payer_signer(state: &AppState) -> Result<Arc<dyn TransactionSigner>, Error> {
    crate::signer::build_fee_payer_signer(
        &state.send.fee_payer,
        "send.fee_payer.key_name is missing",
        "send.fee_payer.pubkey is missing",
    )
    .await
}

// ── State carried at boot ─────────────────────────────────────────────────

/// Cached redemption settings the api boots with. Held on AppState so
/// the handler is a pure dispatch.
#[derive(Clone, Debug)]
pub struct RedemptionState {
    pub enabled: bool,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub decimals: u8,
    pub network: Network,
    pub solana_rpc_api_key: String,
    pub helius_base: String,
    pub max_scan_pages: usize,
    /// How long a request waits for its payout to confirm before answering
    /// with a pending status.
    pub confirm_timeout: std::time::Duration,
    /// Active code → campaign grant, loaded from `REDEMPTION_CODES`
    /// (Doppler) at boot. Lookups in the handler are O(1).
    pub grants: HashMap<String, RedemptionGrant>,
    /// Pooled HTTP client for the Helius dedup scan. Shared across all
    /// `/v1/redeem` requests so connections / TLS handshakes are reused.
    pub http_client: reqwest::Client,
    /// Durable atomic claim store shared by every API replica.
    pub claim_store: redis::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedemptionGrant {
    pub campaign_id: String,
    pub amount: u64,
}

impl RedemptionState {
    /// Resolve mint / token program from `redemption.currency` via the
    /// same pay-kit helpers `/v1/send` uses, then fold in the rest of
    /// the static config.
    pub fn from_config(cfg: &crate::config::RedemptionConfig) -> Result<Self, Error> {
        use pay_kit::mpp::protocol::solana::{
            default_token_program_for_currency, resolve_stablecoin_mint,
        };

        let network_str = cfg.network.as_str();
        let mint_str =
            resolve_stablecoin_mint(&cfg.currency, Some(network_str)).ok_or_else(|| {
                Error::SendNotConfigured(format!(
                    "redemption.currency `{}` resolves to native SOL — pick a stablecoin",
                    cfg.currency
                ))
            })?;
        let token_program_str =
            default_token_program_for_currency(&cfg.currency, Some(network_str));

        let mint = Pubkey::from_str(mint_str).map_err(|_| {
            Error::SendNotConfigured(format!(
                "redemption.currency `{}` resolved to an unparseable mint",
                cfg.currency
            ))
        })?;
        let token_program =
            Pubkey::from_str(token_program_str).expect("pay-kit program ids are valid pubkeys");

        // Every stablecoin pay-kit knows about on Solana is 6-decimals.
        // Hardcoded so we don't have to pull `getMint` at boot.
        const STABLECOIN_DECIMALS: u8 = 6;

        let mut grants = HashMap::new();
        for code in &cfg.codes {
            grants.insert(
                code.clone(),
                RedemptionGrant {
                    campaign_id: "legacy".to_string(),
                    amount: cfg.amount,
                },
            );
        }
        for campaign in cfg.campaigns.iter().filter(|campaign| campaign.enabled) {
            for code in &campaign.codes {
                grants.insert(
                    code.clone(),
                    RedemptionGrant {
                        campaign_id: campaign.id.clone(),
                        amount: campaign.amount,
                    },
                );
            }
        }
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest::Client builds with default tls");
        let claim_store = redis::Client::open(cfg.claim_store_url.as_str()).map_err(|error| {
            Error::SendNotConfigured(format!("invalid redemption.claim_store_url: {error}"))
        })?;

        Ok(Self {
            enabled: cfg.enabled,
            mint,
            token_program,
            decimals: STABLECOIN_DECIMALS,
            network: cfg.network,
            solana_rpc_api_key: cfg.solana_rpc_api_key.clone(),
            helius_base: cfg.helius_base.clone(),
            max_scan_pages: cfg.max_scan_pages.unwrap_or(DEFAULT_MAX_SCAN_PAGES),
            confirm_timeout: REDEEM_CONFIRM_TIMEOUT,
            grants,
            http_client,
            claim_store,
        })
    }
}

// ── ATA idempotent-create helper ──────────────────────────────────────────
//
// `pay_api_core::ata` exports the program id + address derivation; we
// only need the small instruction builder, which the rest of pay-api
// doesn't have a use for yet.

/// `CreateIdempotent` instruction (discriminator = 1, no payload).
fn create_associated_token_account_idempotent_ix(
    funding: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = associated_token_address(owner, mint, token_program);
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*funding, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_system_interface::program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

// ── Response helpers ──────────────────────────────────────────────────────

fn err_resp(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn err_resp_with_extra(status: StatusCode, message: &str, extra: serde_json::Value) -> Response {
    let mut body = json!({ "error": message });
    if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{ClaimState, RedemptionGrant, RedemptionState, redemption_claim_key};
    use crate::config::{RedemptionCampaignConfig, RedemptionConfig};
    use solana_pubkey::Pubkey;
    use solana_signature::Signature;

    #[test]
    fn claim_states_round_trip() {
        let destination = Pubkey::new_unique();
        let signature = Signature::from([7u8; 64]);
        for state in [
            ClaimState::Claimed(destination),
            ClaimState::Pending {
                signature,
                last_valid_block_height: Some(1_234),
            },
            ClaimState::Pending {
                signature,
                last_valid_block_height: None,
            },
            ClaimState::Confirmed(signature),
        ] {
            assert_eq!(ClaimState::decode(&state.encode()), Some(state));
        }
        assert_eq!(
            ClaimState::decode(&format!("broadcast:{signature}")),
            Some(ClaimState::Pending {
                signature,
                last_valid_block_height: None,
            })
        );
        assert_eq!(
            ClaimState::decode(&format!("broadcasting:{destination}")),
            None
        );
        assert_eq!(ClaimState::decode(&format!("pending:{signature}:x")), None);
        assert_eq!(ClaimState::decode("garbage"), None);
    }

    #[test]
    fn resolves_codes_to_campaign_specific_grants() {
        let cfg = RedemptionConfig {
            enabled: true,
            amount: 5_000_000,
            claim_store_url: "redis://127.0.0.1/".to_string(),
            codes: vec!["LEGACY123".to_string()],
            campaigns: vec![
                RedemptionCampaignConfig {
                    id: "anthropic-tokyo-Q2-2026".to_string(),
                    enabled: true,
                    amount: 5_000_000,
                    codes: vec!["TOKYO123".to_string()],
                },
                RedemptionCampaignConfig {
                    id: "superteam-uk-Q3-2026".to_string(),
                    enabled: true,
                    amount: 50_000_000,
                    codes: vec!["SUPER123".to_string()],
                },
                RedemptionCampaignConfig {
                    id: "disabled-campaign".to_string(),
                    enabled: false,
                    amount: 100_000_000,
                    codes: vec!["OFFLINE1".to_string()],
                },
            ],
            ..RedemptionConfig::default()
        };

        let state = RedemptionState::from_config(&cfg).expect("redemption state should resolve");

        assert_eq!(
            state.grants.get("LEGACY123"),
            Some(&RedemptionGrant {
                campaign_id: "legacy".to_string(),
                amount: 5_000_000,
            })
        );
        assert_eq!(
            state.grants.get("TOKYO123"),
            Some(&RedemptionGrant {
                campaign_id: "anthropic-tokyo-Q2-2026".to_string(),
                amount: 5_000_000,
            })
        );
        assert_eq!(
            state.grants.get("SUPER123"),
            Some(&RedemptionGrant {
                campaign_id: "superteam-uk-Q3-2026".to_string(),
                amount: 50_000_000,
            })
        );
        assert!(!state.grants.contains_key("OFFLINE1"));
    }

    #[test]
    fn redemption_claim_keys_do_not_expose_codes() {
        let key = redemption_claim_key("SECRET-CODE-123");
        assert!(key.starts_with("pay-api:redemption:"));
        assert!(!key.contains("SECRET-CODE-123"));
        assert_eq!(key, redemption_claim_key("SECRET-CODE-123"));
    }

    pub(crate) mod flow {
        use std::collections::HashMap;
        use std::process::{Child, Command, Stdio};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use axum::Json;
        use axum::Router;
        use axum::extract::{Query, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use pay_api_types::Network;
        use pay_kit::mpp::solana_keychain::{MemorySigner, TransactionSigner};
        use serde_json::{Value, json};
        use solana_pubkey::Pubkey;
        use solana_signature::Signature;

        use super::super::{
            ClaimState, RedeemRequest, RedemptionGrant, RedemptionState, redeem,
            redemption_claim_key,
        };
        use crate::state::AppState;

        pub(crate) struct RedisProcess(Child, pub(crate) u16);

        impl Drop for RedisProcess {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        /// Spawn a throwaway redis-server, or `None` when it isn't installed.
        pub(crate) async fn spawn_redis() -> Option<RedisProcess> {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .ok()?
                .local_addr()
                .ok()?
                .port();
            let child = Command::new("redis-server")
                .args([
                    "--bind",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                    "--save",
                    "",
                    "--appendonly",
                    "no",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(child) = child else {
                eprintln!("skipping: redis-server not installed");
                return None;
            };
            let process = RedisProcess(child, port);
            for _ in 0..250 {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    return Some(process);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("redis-server did not start");
        }

        #[derive(Clone, Copy)]
        pub(super) enum SendMode {
            Accept,
            Preflight,
            Transport,
        }

        pub(super) struct MockRpc {
            pub(super) send_mode: SendMode,
            /// Slot of the state `getSignatureStatuses` answers from.
            pub(super) status_slot: u64,
            /// Status every sent signature takes once the finalized tip is read.
            pub(super) lands_after_tip_read: Option<Value>,
            pub(super) sent: Vec<Signature>,
            pub(super) statuses: HashMap<String, Value>,
            /// Status applied to every signature sent from now on.
            pub(super) next_status: Option<Value>,
            pub(super) block_height: u64,
            pub(super) last_valid_block_height: u64,
            pub(super) transactions: HashMap<String, Value>,
        }

        impl Default for MockRpc {
            fn default() -> Self {
                Self {
                    send_mode: SendMode::Accept,
                    status_slot: 10,
                    lands_after_tip_read: None,
                    sent: Vec::new(),
                    statuses: HashMap::new(),
                    next_status: Some(finalized()),
                    block_height: 50,
                    last_valid_block_height: 100,
                    transactions: HashMap::new(),
                }
            }
        }

        pub(super) fn confirmed() -> Value {
            json!({ "err": null, "confirmationStatus": "confirmed" })
        }

        pub(super) fn finalized() -> Value {
            json!({ "err": null, "confirmationStatus": "finalized" })
        }

        pub(super) fn failed() -> Value {
            json!({ "err": { "InstructionError": [1, { "Custom": 1 }] }, "confirmationStatus": "finalized" })
        }

        pub(super) fn failed_unfinalized() -> Value {
            json!({ "err": { "InstructionError": [1, { "Custom": 1 }] }, "confirmationStatus": "confirmed" })
        }

        type Shared = Arc<Mutex<MockRpc>>;

        async fn rpc_handler(
            State(mock): State<Shared>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let mut mock = mock.lock().unwrap();
            let method = body["method"].as_str().unwrap_or_default();
            let result = match method {
                "getEpochInfo" => {
                    if let Some(status) = mock.lands_after_tip_read.take() {
                        for signature in mock.sent.clone() {
                            mock.statuses.insert(signature.to_string(), status.clone());
                        }
                    }
                    json!({ "absoluteSlot": 10, "blockHeight": mock.block_height })
                }
                "getLatestBlockhash" => json!({
                    "context": { "slot": 1 },
                    "value": {
                        "blockhash": solana_hash::Hash::new_unique().to_string(),
                        "lastValidBlockHeight": mock.last_valid_block_height,
                    }
                }),
                "sendTransaction" => match mock.send_mode {
                    SendMode::Accept => {
                        let tx =
                            pay_kit::core::tx::decode(body["params"][0].as_str().unwrap()).unwrap();
                        let signature = tx.signatures[0];
                        mock.sent.push(signature);
                        if let Some(status) = mock.next_status.clone() {
                            mock.statuses.insert(signature.to_string(), status);
                        }
                        json!(signature.to_string())
                    }
                    SendMode::Preflight => {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "error": { "code": -32002, "message": "Transaction simulation failed: insufficient funds" }
                            })),
                        );
                    }
                    SendMode::Transport => {
                        let tx =
                            pay_kit::core::tx::decode(body["params"][0].as_str().unwrap()).unwrap();
                        mock.sent.push(tx.signatures[0]);
                        return (StatusCode::BAD_GATEWAY, Json(json!({})));
                    }
                },
                "getSignatureStatuses" => {
                    let signature = body["params"][0][0].as_str().unwrap_or_default();
                    json!({
                        "context": { "slot": mock.status_slot },
                        "value": [mock.statuses.get(signature).cloned().unwrap_or(Value::Null)]
                    })
                }
                "getBlockHeight" => json!(mock.block_height),
                "getTransaction" => {
                    let signature = body["params"][0].as_str().unwrap_or_default();
                    mock.transactions
                        .get(signature)
                        .cloned()
                        .unwrap_or(Value::Null)
                }
                other => panic!("unexpected RPC method {other}"),
            };
            (
                StatusCode::OK,
                Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result })),
            )
        }

        /// Newest-first history paged like the provider: `limit` entries
        /// strictly older than `before-signature`.
        async fn helius_handler(
            State(history): State<Arc<Mutex<Vec<Value>>>>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            let history = history.lock().unwrap();
            let start = query
                .get("before-signature")
                .and_then(|before| history.iter().position(|tx| tx["signature"] == **before))
                .map_or(0, |position| position + 1);
            let limit = query
                .get("limit")
                .and_then(|limit| limit.parse().ok())
                .unwrap_or(usize::MAX);
            Json(Value::Array(
                history.iter().skip(start).take(limit).cloned().collect(),
            ))
        }

        pub(super) struct Harness {
            pub(super) state: AppState,
            pub(super) cfg: RedemptionState,
            pub(super) signer: Arc<dyn TransactionSigner>,
            pub(super) rpc: Shared,
            pub(super) history: Arc<Mutex<Vec<Value>>>,
            _redis: RedisProcess,
        }

        pub(super) const CODE: &str = "TESTCODE1";

        async fn serve(router: Router) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            format!("http://{addr}")
        }

        impl Harness {
            pub(super) async fn new() -> Option<Self> {
                let redis = spawn_redis().await?;
                let rpc: Shared = Arc::new(Mutex::new(MockRpc::default()));
                let rpc_url = serve(
                    Router::new()
                        .route("/", post(rpc_handler))
                        .with_state(rpc.clone()),
                )
                .await;
                let history = Arc::new(Mutex::new(Vec::new()));
                let helius_url = serve(
                    Router::new()
                        .route("/v0/addresses/{address}/transactions", get(helius_handler))
                        .with_state(history.clone()),
                )
                .await;
                let config: crate::config::Config = serde_json::from_value(json!({
                    "networks": { "sandbox": { "rpc_url": rpc_url } },
                    "stablecoins": [],
                    "send": { "enabled": false },
                }))
                .unwrap();
                let state = AppState::new(&config).await.unwrap();
                let keypair = solana_keypair::Keypair::new();
                let signer: Arc<dyn TransactionSigner> = Arc::new(
                    MemorySigner::from_private_key_string(
                        &serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap(),
                    )
                    .unwrap(),
                );
                let mut grants = HashMap::new();
                grants.insert(
                    CODE.to_string(),
                    RedemptionGrant {
                        campaign_id: "test".to_string(),
                        amount: 5_000_000,
                    },
                );
                let cfg = RedemptionState {
                    enabled: true,
                    mint: Pubkey::new_unique(),
                    token_program: pay_api_core::ata::SPL_TOKEN_PROGRAM_ID,
                    decimals: 6,
                    network: Network::Sandbox,
                    solana_rpc_api_key: "key".to_string(),
                    helius_base: helius_url,
                    max_scan_pages: 2,
                    confirm_timeout: Duration::from_millis(300),
                    grants,
                    http_client: reqwest::Client::new(),
                    claim_store: redis::Client::open(format!("redis://127.0.0.1:{}/", redis.1))
                        .unwrap(),
                };
                Some(Self {
                    state,
                    cfg,
                    signer,
                    rpc,
                    history,
                    _redis: redis,
                })
            }

            pub(super) async fn redeem(&self, destination: &Pubkey) -> (StatusCode, Value) {
                let response = redeem(
                    &self.state,
                    &self.cfg,
                    self.signer.clone(),
                    &RedeemRequest {
                        code: CODE.to_string(),
                        destination: destination.to_string(),
                    },
                )
                .await;
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                )
            }

            pub(super) async fn claim(&self) -> Option<ClaimState> {
                let mut connection = self
                    .cfg
                    .claim_store
                    .get_multiplexed_async_connection()
                    .await
                    .unwrap();
                let raw: Option<String> = redis::cmd("GET")
                    .arg(redemption_claim_key(CODE))
                    .query_async(&mut connection)
                    .await
                    .unwrap();
                raw.map(|raw| ClaimState::decode(&raw).expect("claim decodes"))
            }

            pub(super) fn hot_wallet(&self) -> Pubkey {
                self.signer.pubkey()
            }

            /// Index `signature` in the address history with the redemption memo
            /// and serve `transaction` for it from the RPC.
            pub(super) fn index_candidate(&self, signature: &str, transaction: Value) {
                self.history.lock().unwrap().push(json!({
                    "signature": signature,
                    "timestamp": 1_700_000_000,
                    "instructions": [{
                        "programId": pay_api_core::receipt::MEMO_PROGRAM,
                        "data": bs58::encode(format!("Redeem code {CODE}")).into_string(),
                    }],
                }));
                if !transaction.is_null() {
                    self.rpc
                        .lock()
                        .unwrap()
                        .transactions
                        .insert(signature.to_string(), transaction);
                }
            }

            /// A jsonParsed transaction with `fee_payer` signing a transfer of
            /// the campaign mint from `source_owner`'s token account.
            pub(super) fn payout_tx(
                &self,
                fee_payer: &Pubkey,
                source_owner: &Pubkey,
                err: Value,
            ) -> Value {
                let source = pay_api_core::ata::associated_token_address(
                    source_owner,
                    &self.cfg.mint,
                    &self.cfg.token_program,
                );
                json!({
                    "slot": 1,
                    "meta": { "err": err },
                    "transaction": { "message": {
                        "accountKeys": [
                            { "pubkey": fee_payer.to_string(), "signer": true, "writable": true },
                            { "pubkey": self.hot_wallet().to_string(), "signer": fee_payer == &self.hot_wallet(), "writable": true },
                        ],
                        "instructions": [
                            {
                                "programId": self.cfg.token_program.to_string(),
                                "program": "spl-token",
                                "parsed": { "type": "transferChecked", "info": {
                                    "source": source.to_string(),
                                    "authority": source_owner.to_string(),
                                    "destination": Pubkey::new_unique().to_string(),
                                    "mint": self.cfg.mint.to_string(),
                                    "tokenAmount": { "amount": "5000000", "decimals": 6 },
                                }},
                            },
                            {
                                "programId": pay_api_core::receipt::MEMO_PROGRAM,
                                "program": "spl-memo",
                                "parsed": format!("Redeem code {CODE}"),
                            },
                        ],
                    }},
                })
            }

            pub(super) fn sent(&self) -> Vec<Signature> {
                self.rpc.lock().unwrap().sent.clone()
            }
        }

        #[tokio::test]
        async fn confirmed_payout_returns_success_and_records_confirmed_claim() {
            let Some(h) = Harness::new().await else {
                return;
            };
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let sent = h.sent();
            assert_eq!(sent.len(), 1);
            assert_eq!(body["signature"], sent[0].to_string());
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(sent[0])));

            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["signature"], sent[0].to_string());
            assert_eq!(h.sent().len(), 1);
        }

        #[tokio::test]
        async fn payout_failing_on_chain_is_not_success_and_releases_the_code() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = Some(failed());
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            assert!(body.get("signature").is_none());
            assert_eq!(h.claim().await, None);

            h.rpc.lock().unwrap().next_status = Some(finalized());
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(h.sent().len(), 2);
        }

        #[tokio::test]
        async fn a_failure_short_of_finalized_keeps_the_claim_until_final() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = Some(failed_unfinalized());
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
            let first = h.sent()[0];
            assert!(matches!(
                h.claim().await,
                Some(ClaimState::Pending { signature, .. }) if signature == first
            ));

            // Positive control: once the failure is final, a retry pays.
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.statuses.insert(first.to_string(), failed());
                rpc.next_status = Some(finalized());
            }
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(h.sent().len(), 2);
        }

        #[tokio::test]
        async fn unconfirmed_payout_stays_pending_until_its_blockhash_expires() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = None;
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
            let first = h.sent()[0];
            assert_eq!(body["pending_signature"], first.to_string());
            assert!(body.get("signature").is_none());
            assert_eq!(
                h.claim().await,
                Some(ClaimState::Pending {
                    signature: first,
                    last_valid_block_height: Some(100),
                })
            );

            // Still within the blockhash lifetime: no second payout.
            h.rpc.lock().unwrap().block_height = 100;
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            assert_eq!(h.sent().len(), 1);

            // Expired at finalized commitment: the first payout can never land.
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.block_height = 101;
                rpc.last_valid_block_height = 200;
                rpc.next_status = Some(finalized());
            }
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let sent = h.sent();
            assert_eq!(sent.len(), 2);
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(sent[1])));
        }

        #[tokio::test]
        async fn a_payout_confirmed_short_of_finality_never_consumes_the_code() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = Some(confirmed());
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
            let first = h.sent()[0];
            let pending = Some(ClaimState::Pending {
                signature: first,
                last_valid_block_height: Some(100),
            });
            assert_eq!(h.claim().await, pending);

            // A retry while it is still only confirmed keeps it pending.
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            assert_eq!(h.claim().await, pending);

            // The confirmed block is dropped and the blockhash expires: the
            // code was never redeemed, so it pays out.
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.statuses.remove(&first.to_string());
                rpc.block_height = 101;
                rpc.last_valid_block_height = 200;
                rpc.next_status = Some(finalized());
            }
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let sent = h.sent();
            assert_eq!(sent.len(), 2);
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(sent[1])));
        }

        #[tokio::test]
        async fn a_confirmed_payout_is_terminal_once_finalized() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = Some(confirmed());
            let destination = Pubkey::new_unique();
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            let first = h.sent()[0];
            h.rpc
                .lock()
                .unwrap()
                .statuses
                .insert(first.to_string(), finalized());
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["signature"], first.to_string());
            assert_eq!(h.sent().len(), 1);
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(first)));
        }

        #[tokio::test]
        async fn a_payout_landing_just_before_expiry_is_never_repaid() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = None;
            let destination = Pubkey::new_unique();
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            let first = h.sent()[0];

            // It lands as the tip moves past its expiry: absent to a status
            // read taken before the tip, confirmed to one taken after it.
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.block_height = 101;
                rpc.lands_after_tip_read = Some(finalized());
            }
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["signature"], first.to_string());
            assert_eq!(h.sent().len(), 1);
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(first)));
        }

        #[tokio::test]
        async fn statuses_older_than_the_finalized_tip_keep_the_claim() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = None;
            let destination = Pubkey::new_unique();
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.block_height = 101;
                rpc.status_slot = 9;
            }
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            assert_eq!(h.sent().len(), 1);

            // Positive control: current statuses let the expired claim go.
            {
                let mut rpc = h.rpc.lock().unwrap();
                rpc.status_slot = 10;
                rpc.last_valid_block_height = 200;
                rpc.next_status = Some(finalized());
            }
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(h.sent().len(), 2);
        }

        #[tokio::test]
        async fn pending_payout_that_later_lands_is_reported_redeemed_without_repaying() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().next_status = None;
            let destination = Pubkey::new_unique();
            let (status, _) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
            let first = h.sent()[0];

            h.rpc
                .lock()
                .unwrap()
                .statuses
                .insert(first.to_string(), finalized());
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["signature"], first.to_string());
            assert_eq!(h.sent().len(), 1);
            assert_eq!(h.claim().await, Some(ClaimState::Confirmed(first)));
        }

        #[tokio::test]
        async fn ambiguous_broadcast_keeps_the_signed_payout_pending() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().send_mode = SendMode::Transport;
            let destination = Pubkey::new_unique();
            let (status, body) = h.redeem(&destination).await;
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
            let first = h.sent()[0];
            assert!(matches!(
                h.claim().await,
                Some(ClaimState::Pending { signature, .. }) if signature == first
            ));
        }

        #[tokio::test]
        async fn rejected_preflight_releases_the_claim() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.rpc.lock().unwrap().send_mode = SendMode::Preflight;
            let (status, _) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(h.claim().await, None);
        }

        #[tokio::test]
        async fn memo_from_an_unrelated_transaction_does_not_block_the_code() {
            let Some(h) = Harness::new().await else {
                return;
            };
            let attacker = Pubkey::new_unique();
            // Attacker-signed, mentions the hot wallet, carries the exact memo.
            let mut tx = h.payout_tx(&attacker, &attacker, Value::Null);
            tx["transaction"]["message"]["instructions"]
                .as_array_mut()
                .unwrap()
                .remove(0);
            h.index_candidate("AttackerSig", tx);
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(h.sent().len(), 1);
        }

        #[tokio::test]
        async fn attacker_signed_token_transfer_with_the_memo_is_not_a_payout() {
            let Some(h) = Harness::new().await else {
                return;
            };
            let attacker = Pubkey::new_unique();
            h.index_candidate(
                "AttackerSig",
                h.payout_tx(&attacker, &attacker, Value::Null),
            );
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }

        #[tokio::test]
        async fn failed_hot_wallet_payout_does_not_count_as_redeemed() {
            let Some(h) = Harness::new().await else {
                return;
            };
            let hot = h.hot_wallet();
            h.index_candidate(
                "FailedSig",
                h.payout_tx(&hot, &hot, json!({ "InstructionError": [0, "Custom"] })),
            );
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }

        #[tokio::test]
        async fn verified_hot_wallet_payout_blocks_a_second_redemption() {
            let Some(h) = Harness::new().await else {
                return;
            };
            let hot = h.hot_wallet();
            h.index_candidate("PriorSig", h.payout_tx(&hot, &hot, Value::Null));
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["signature"], "PriorSig");
            assert!(h.sent().is_empty());
            assert_eq!(h.claim().await, None);
        }

        /// `count` memo-less history entries named `{prefix}-{index}`.
        fn filler(h: &Harness, prefix: &str, count: usize, timestamp: i64) {
            let mut history = h.history.lock().unwrap();
            for index in 0..count {
                history.push(json!({
                    "signature": format!("{prefix}-{index}"),
                    "timestamp": timestamp,
                    "instructions": [],
                }));
            }
        }

        #[tokio::test]
        async fn a_payout_older_than_the_claim_store_still_blocks_the_code() {
            let Some(mut h) = Harness::new().await else {
                return;
            };
            h.cfg.max_scan_pages = 4;
            // Two full pages of newer activity end exactly at the Redis
            // rollout; the legacy payout sits on the page after them.
            let rollout = 1_000;
            filler(&h, "newer", 100, rollout + 1);
            filler(&h, "rollout", 100, rollout);
            let hot = h.hot_wallet();
            h.index_candidate("LegacySig", h.payout_tx(&hot, &hot, Value::Null));
            filler(&h, "older", 10, rollout - 1);
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["signature"], "LegacySig");
            assert!(h.sent().is_empty());
        }

        #[tokio::test]
        async fn history_beyond_the_page_cap_fails_closed() {
            let Some(h) = Harness::new().await else {
                return;
            };
            // Two full pages with no end of history in reach (cap is two).
            filler(&h, "busy", 250, 1_000);
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            assert!(h.sent().is_empty());

            // Positive control: history that ends within the cap proves the
            // code unused and pays it.
            h.history.lock().unwrap().truncate(150);
            let (status, body) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(h.sent().len(), 1);
        }

        #[tokio::test]
        async fn unverifiable_candidate_fails_closed() {
            let Some(h) = Harness::new().await else {
                return;
            };
            h.index_candidate("UnknownSig", Value::Null);
            let (status, _) = h.redeem(&Pubkey::new_unique()).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert!(h.sent().is_empty());
        }
    }
}
