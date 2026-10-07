//! Client-side channel state for the x402 `batch-settlement` scheme.
//!
//! Unlike `exact` and `upto`, `batch-settlement` is stateful across requests:
//! one escrow channel backs many cheap calls, and each request signs a voucher
//! for `previous cumulative + price`. The client therefore has to remember,
//! per channel, how much the server has confirmed charging.
//!
//! [`BatchChannelCache`] is that memory. It lives for the life of the process,
//! which is the same shape as the MPP session cache the MCP server keeps. A
//! long-lived host reuses the channel and can choose a deposit large enough to
//! amortize funding across many requests, while a one-shot `pay curl` opens a
//! channel, spends it, and can force-close to recover the remainder.
//!
//! Exact receipts advance the watermark to the server-confirmed commitment — see
//! [`pay_kit::x402::client::batch_settlement::BatchChannel::apply_payment_response`].
//! Server-signed authorizations are registered before dispatch. Their possible
//! charges remain separate from this confirmed watermark until signed evidence
//! arrives. Missing receipts never manufacture spend or trigger extra funding.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pay_kit::x402::batch_settlement::{
    BatchAuthorization, BatchPayload, BatchRequirements, BatchSettlementResponse, BatchVoucher,
};
use pay_kit::x402::client::batch_settlement::BatchChannel;

// Re-exported so hosts (the MCP server, a proxy) can hold and confirm
// batch-settlement state without taking a direct pay-kit dependency.
pub use pay_kit::x402::batch_settlement::{
    BatchAuthorization as Authorization, BatchRequirements as Requirements,
    BatchSettlementResponse as SettlementResponse, BatchVoucher as Voucher,
};

use crate::{Error, Result};

/// Identifies the channel that serves a given offer.
///
/// Every component is a channel-PDA seed or an immutable channel property, so
/// two offers that differ in any of them cannot share a channel — reusing one
/// across, say, two `payTo` addresses would sign vouchers redeemable by the
/// wrong receiver.
fn cache_key(requirements: &BatchRequirements) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        requirements.network,
        requirements.asset,
        requirements.pay_to,
        requirements.extra.fee_payer,
        requirements.extra.withdraw_delay,
        requirements
            .extra
            .voucher_signer
            .as_deref()
            .unwrap_or("client"),
        requirements.extra.operator.as_deref().unwrap_or(""),
    )
}

/// Credential submitted for one batch-settlement request.
#[derive(Debug, Clone)]
pub enum Submission {
    Voucher(BatchVoucher),
    Authorization {
        authorization: BatchAuthorization,
        /// Approved total escrow in a payer-signed open or top-up. This is a
        /// planned upper bound until a verified receipt or chain discovery
        /// confirms funding, not authority taken from server-reported balance.
        confirmed_deposit: Option<u64>,
        /// Shared across retries and clones; register before dispatch.
        attempt: Attempt,
    },
}

/// An authorization's process-local lifecycle. Cloning preserves its identity.
///
/// Dropping a handle does not release its budget: cancellation or a lost
/// transport response cannot prove that the operator did not charge it.
#[derive(Debug, Clone, Default)]
pub struct Attempt(Arc<Mutex<Option<RegisteredAttempt>>>);

#[derive(Debug)]
struct RegisteredAttempt {
    generation: Arc<()>,
    request_id: String,
    ceiling: u64,
    floor: u64,
    settled: Option<(u64, u64)>,
    rejected: bool,
}

impl Submission {
    pub(crate) fn from_payload(payload: &BatchPayload) -> Result<Self> {
        match payload {
            BatchPayload::Deposit {
                voucher: Some(voucher),
                ..
            }
            | BatchPayload::Voucher { voucher, .. } => Ok(Self::Voucher(voucher.clone())),
            BatchPayload::Deposit {
                authorization: Some(authorization),
                ..
            }
            | BatchPayload::Authorization { authorization, .. } => Ok(Self::Authorization {
                authorization: authorization.clone(),
                confirmed_deposit: None,
                attempt: Attempt::default(),
            }),
            _ => Err(Error::Mpp(
                "batch-settlement paid request carries no credential".to_string(),
            )),
        }
    }

    pub(crate) fn with_confirmed_deposit(self, confirmed_deposit: u64) -> Self {
        match self {
            Self::Authorization {
                authorization,
                attempt,
                ..
            } => Self::Authorization {
                authorization,
                confirmed_deposit: Some(confirmed_deposit),
                attempt,
            },
            submission => submission,
        }
    }
}

/// Process-lifetime cache of open `batch-settlement` channels.
#[derive(Clone, Default)]
pub struct BatchChannelCache {
    channels: Arc<Mutex<HashMap<String, CachedChannel>>>,
    /// Signers for funded channels remain resident for the host process. The
    /// escrow open/top-up is still approval-gated; requests spending capacity
    /// that was already approved must not reopen the keystore every time.
    signers: Arc<Mutex<HashMap<String, Arc<crate::signer::ResolvedSigner>>>>,
}

struct CachedChannel {
    confirmed: BatchChannel,
    /// Sum of submitted ceilings minus confirmed advances and released excess.
    /// This is potential spend, not a balance, and must not be clipped to escrow.
    remaining: u128,
    generation: Arc<()>,
    /// Chain discovery knows settled spend but not earlier offchain receipts.
    /// Exactly one verified baseline may account for that pre-process history.
    recovered: bool,
    /// Spend adopted as pre-process history rather than debited from live
    /// budgets. Late exact receipts inside this range must debit their charge.
    recovery_exempt_through: u64,
    /// A signed funding transaction is not confirmed by live response headers.
    /// Retain at most one unresolved funding lifecycle per channel.
    pending_funding: Option<(Attempt, u64)>,
}

impl BatchChannelCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The channel already open for this offer, if any.
    pub fn get(&self, requirements: &BatchRequirements) -> Result<Option<BatchChannel>> {
        let channels = self.lock()?;
        Ok(channels
            .get(&cache_key(requirements))
            .map(|entry| entry.confirmed.clone()))
    }

    /// Return the cached channel only when it belongs to the payer selected
    /// for this request. Switching accounts invalidates both channel state and
    /// its resident signer, even when the server offer itself is unchanged.
    pub fn get_for_payer(
        &self,
        requirements: &BatchRequirements,
        payer: &solana_pubkey::Pubkey,
    ) -> Result<Option<BatchChannel>> {
        let channel = self.get(requirements)?;
        if channel
            .as_ref()
            .is_some_and(|channel| channel.config().payer != payer.to_string())
        {
            self.remove(requirements)?;
            return Ok(None);
        }
        Ok(channel)
    }

    /// Remember a channel opened for this offer.
    pub fn insert(&self, requirements: &BatchRequirements, channel: BatchChannel) -> Result<()> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        // Signing happens outside this lock. Never overwrite a receipt that
        // arrived while the builder was holding an older channel snapshot.
        if channels
            .get(&key)
            .is_some_and(|entry| entry.confirmed.channel_id() == channel.channel_id())
        {
            return Ok(());
        }
        channels.insert(
            key,
            CachedChannel {
                confirmed: channel,
                remaining: 0,
                generation: Arc::new(()),
                recovered: false,
                recovery_exempt_through: 0,
                pending_funding: None,
            },
        );
        Ok(())
    }

    /// Insert only kit-discovered onchain state. A same-channel discovery may
    /// confirm pending funding without replacing newer offchain history.
    pub(crate) fn insert_recovered(
        &self,
        requirements: &BatchRequirements,
        channel: BatchChannel,
    ) -> Result<()> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        if let Some(entry) = channels.get_mut(&key) {
            if entry.confirmed.channel_id() == channel.channel_id() {
                if let Some((_, planned)) = &entry.pending_funding {
                    if channel.deposit() < *planned {
                        return Err(pending_funding_error());
                    }
                    entry.confirmed = BatchChannel::new(
                        *channel.channel_id(),
                        entry.confirmed.config().clone(),
                        entry.confirmed.charged_cumulative_amount(),
                        *planned,
                    );
                    entry.pending_funding = None;
                }
                return Ok(());
            }
            if entry.pending_funding.is_some() {
                return Err(pending_funding_error());
            }
        }
        channels.insert(
            key,
            CachedChannel {
                confirmed: channel,
                remaining: 0,
                generation: Arc::new(()),
                recovered: true,
                recovery_exempt_through: 0,
                pending_funding: None,
            },
        );
        Ok(())
    }

    pub(crate) fn has_pending_funding(&self, requirements: &BatchRequirements) -> Result<bool> {
        Ok(self
            .lock()?
            .get(&cache_key(requirements))
            .is_some_and(|entry| entry.pending_funding.is_some()))
    }

    pub(crate) fn signer(
        &self,
        requirements: &BatchRequirements,
    ) -> Result<Option<Arc<crate::signer::ResolvedSigner>>> {
        let signers = self
            .signers
            .lock()
            .map_err(|_| Error::Mpp("batch-settlement signer cache lock poisoned".to_string()))?;
        Ok(signers.get(&cache_key(requirements)).cloned())
    }

    pub(crate) fn insert_signer(
        &self,
        requirements: &BatchRequirements,
        signer: Arc<crate::signer::ResolvedSigner>,
    ) -> Result<()> {
        let mut signers = self
            .signers
            .lock()
            .map_err(|_| Error::Mpp("batch-settlement signer cache lock poisoned".to_string()))?;
        signers.insert(cache_key(requirements), signer);
        Ok(())
    }

    /// Forget a channel — after a close, or when the server no longer
    /// recognizes it and the client must open a fresh one.
    pub fn remove(&self, requirements: &BatchRequirements) -> Result<()> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        channels.remove(&key);
        drop(channels);
        let mut signers = self
            .signers
            .lock()
            .map_err(|_| Error::Mpp("batch-settlement signer cache lock poisoned".to_string()))?;
        signers.remove(&key);
        Ok(())
    }

    /// Adopt the server's confirmation of a payment.
    ///
    /// Returns the nondecreasing confirmed watermark. Kit verifies signatures,
    /// channel binding, charge limits and escrow. The lifecycle ledger bounds
    /// intervening spend and releases only this attempt's unused ceiling.
    /// Rejection leaves the channel, budget and attempt unchanged.
    pub fn apply_settlement(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
        response: &BatchSettlementResponse,
    ) -> Result<u64> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let entry = channels.get_mut(&key).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        let mut channel = entry.confirmed.clone();
        match submitted {
            Submission::Voucher(voucher) => channel
                .apply_payment_response(response, requirements, voucher)
                .map_err(|e| Error::Mpp(format!("batch-settlement settlement rejected: {e}")))?,
            Submission::Authorization {
                authorization,
                confirmed_deposit,
                attempt,
            } => {
                let mut state = attempt
                    .0
                    .lock()
                    .map_err(|_| Error::Mpp("batch attempt lock poisoned".to_string()))?;
                let registered = state.as_mut().ok_or_else(|| {
                    Error::Mpp("batch authorization was not registered before dispatch".to_string())
                })?;
                check_attempt(entry, registered, authorization)?;
                if registered.rejected {
                    return Err(Error::Mpp(
                        "batch attempt was definitively rejected".to_string(),
                    ));
                }
                let (cumulative, charged) = response
                    .extra
                    .as_ref()
                    .and_then(|extra| {
                        Some((
                            extra.voucher.as_ref()?.max_claimable().ok()?,
                            extra.charged_amount.as_deref()?.parse::<u64>().ok()?,
                        ))
                    })
                    .ok_or_else(|| Error::Mpp("invalid batch receipt amounts".to_string()))?;
                let prior = cumulative.checked_sub(charged).ok_or_else(|| {
                    Error::Mpp("batch receipt charge exceeds cumulative".to_string())
                })?;
                if prior < registered.floor {
                    return Err(Error::Mpp(
                        "batch receipt predates its authorization".to_string(),
                    ));
                }
                if registered
                    .settled
                    .is_some_and(|value| value != (cumulative, charged))
                {
                    return Err(Error::Mpp(
                        "batch attempt has conflicting receipts".to_string(),
                    ));
                }
                // Try kit's native reconciliation first: recovered channels
                // already know their pre-process offchain history is incomplete.
                let deposit = confirmed_deposit.map(|deposit| deposit.max(channel.deposit()));
                let mut result = channel.apply_authorization_response_with_deposit(
                    response,
                    authorization,
                    deposit,
                );
                let native = result.is_ok();
                if !native {
                    // This temporary baseline is never published. Kit still
                    // verifies every receipt field and its exact own charge;
                    // the aggregate ledger below bounds the intervening spend.
                    channel = BatchChannel::new(
                        *entry.confirmed.channel_id(),
                        entry.confirmed.config().clone(),
                        prior,
                        entry.confirmed.deposit(),
                    );
                    result = channel.apply_authorization_response_with_deposit(
                        response,
                        authorization,
                        deposit,
                    );
                }
                result.map_err(|e| {
                    Error::Mpp(format!("batch-settlement settlement rejected: {e}"))
                })?;
                let release = if registered.settled.is_none() {
                    registered.ceiling.checked_sub(charged).ok_or_else(|| {
                        Error::Mpp("batch receipt exceeds registered ceiling".to_string())
                    })?
                } else {
                    0
                };
                let available = entry
                    .remaining
                    .checked_sub(u128::from(release))
                    .ok_or_else(|| {
                        Error::Mpp(
                            "batch receipt contradicts previously confirmed spend".to_string(),
                        )
                    })?;
                let floor = entry.confirmed.charged_cumulative_amount();
                let delta = u128::from(cumulative.saturating_sub(floor));
                let exempt_charge = if registered.settled.is_none() {
                    entry
                        .recovery_exempt_through
                        .saturating_sub(prior)
                        .min(charged)
                } else {
                    0
                };
                let remaining = if entry.recovered {
                    // The first authenticated recovered baseline may include
                    // pre-process history. It cannot identify which other live
                    // attempts completed, so preserve their potential budgets.
                    available.checked_sub(u128::from(charged)).ok_or_else(|| {
                        Error::Mpp("recovered batch receipt exceeds its budget".to_string())
                    })?
                } else {
                    available
                        .checked_sub(delta + u128::from(exempt_charge))
                        .ok_or_else(|| {
                            Error::Mpp(
                                "batch receipt exceeds submitted authorization budget".to_string(),
                            )
                        })?
                };
                if cumulative < floor {
                    // Spend and funding can be confirmed in different response
                    // orders. Keep both verified high-water marks independently.
                    channel = BatchChannel::new(
                        *entry.confirmed.channel_id(),
                        entry.confirmed.config().clone(),
                        floor,
                        channel.deposit().max(entry.confirmed.deposit()),
                    );
                }
                entry.remaining = remaining;
                if entry.recovered {
                    entry.recovery_exempt_through = prior;
                }
                entry.recovered = false;
                if entry
                    .pending_funding
                    .as_ref()
                    .is_some_and(|(pending, _)| Arc::ptr_eq(&pending.0, &attempt.0))
                {
                    entry.pending_funding = None;
                }
                registered.settled = Some((cumulative, charged));
            }
        }
        let cumulative = channel.charged_cumulative_amount();
        entry.confirmed = channel;
        Ok(cumulative)
    }

    /// Resynchronize from a corrective 402.
    ///
    /// The server proves how much it has charged with a voucher this client
    /// signed; the channel refuses anything it cannot verify against its own
    /// authorizer key. Invalid proofs leave both the channel and its budget
    /// untouched. A snapshot consumes budget, not individual attempt identities.
    pub fn adopt_corrective(&self, requirements: &BatchRequirements) -> Result<Option<u64>> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let Some(entry) = channels.get_mut(&key) else {
            return Ok(None);
        };
        let mut candidate = entry.confirmed.clone();
        let cumulative = candidate
            .adopt_corrective_state(requirements)
            .map_err(|e| Error::Mpp(format!("batch-settlement corrective state rejected: {e}")))?;
        let floor = entry.confirmed.charged_cumulative_amount();
        if cumulative > floor {
            if !entry.recovered && entry.confirmed.config().voucher_signer.is_some() {
                entry.remaining = entry
                    .remaining
                    .checked_sub(u128::from(cumulative - floor))
                    .ok_or_else(|| {
                        Error::Mpp(
                            "batch corrective exceeds submitted authorization budget".to_string(),
                        )
                    })?;
            }
            entry.confirmed = candidate;
        }
        // Clear even on a zero/nonadvancing baseline. Do not retain kit's
        // private incomplete-history flag after the one-time recovery grant.
        if entry.recovered {
            entry.recovery_exempt_through = floor.max(cumulative);
            entry.confirmed = BatchChannel::new(
                *entry.confirmed.channel_id(),
                entry.confirmed.config().clone(),
                floor.max(cumulative),
                entry.confirmed.deposit(),
            );
            entry.recovered = false;
        }
        Ok(Some(floor.max(cumulative)))
    }

    /// Adopt a settlement from a completed response's `PAYMENT-RESPONSE`
    /// header.
    ///
    /// Returns `Ok(None)` when the response carried no settlement header at
    /// all, which leaves the watermark untouched: the next request re-signs the
    /// same cumulative amount, and the server recognizes it as the idempotent
    /// retry it is.
    pub fn apply_settlement_from_headers(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
        response_headers: &[(String, String)],
    ) -> Result<Option<u64>> {
        let Some(response) = decode_settlement(response_headers) else {
            return Ok(None);
        };
        self.apply_settlement(requirements, submitted, &response)
            .map(Some)
    }

    /// Register every authorization before dispatch, including calls whose
    /// receipt may arrive immediately. Retries must reuse the shared handle.
    ///
    /// No per-request map is retained by the cache. Dropped handles leave only
    /// aggregate budget behind because transport cancellation is ambiguous.
    pub fn register_submission(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
    ) -> Result<()> {
        let Submission::Authorization {
            authorization,
            confirmed_deposit,
            attempt,
        } = submitted
        else {
            return Ok(());
        };
        let authorized = authorization
            .authorized_amount
            .parse::<u64>()
            .map_err(|_| {
                Error::Mpp("batch-settlement authorization amount is invalid".to_string())
            })?;
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let entry = channels.get_mut(&key).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        let mut state = attempt
            .0
            .lock()
            .map_err(|_| Error::Mpp("batch attempt lock poisoned".to_string()))?;
        if let Some(registered) = state.as_ref() {
            check_attempt(entry, registered, authorization)?;
            if registered.rejected {
                return Err(Error::Mpp(
                    "cannot resubmit a definitively rejected batch attempt".to_string(),
                ));
            }
            return Ok(());
        }
        if entry.pending_funding.is_some() {
            return Err(pending_funding_error());
        }
        let channel = &entry.confirmed;
        if authorization.channel_id != channel.channel_id().to_string()
            || authorization.payer != channel.config().payer
        {
            return Err(Error::Mpp(
                "batch-settlement authorization does not match cached channel".to_string(),
            ));
        }
        let deposit = confirmed_deposit.unwrap_or(channel.deposit());
        if deposit < channel.deposit()
            || authorized > deposit.saturating_sub(channel.charged_cumulative_amount())
        {
            return Err(Error::Mpp(
                "batch-settlement authorization exceeds confirmed escrow".to_string(),
            ));
        }
        entry.remaining = entry
            .remaining
            .checked_add(u128::from(authorized))
            .ok_or_else(|| Error::Mpp("batch authorization budget overflow".to_string()))?;
        *state = Some(RegisteredAttempt {
            generation: Arc::clone(&entry.generation),
            request_id: authorization.request_id.clone(),
            ceiling: authorized,
            floor: channel.charged_cumulative_amount(),
            settled: None,
            rejected: false,
        });
        if let Some(deposit) = confirmed_deposit {
            entry.pending_funding = Some((attempt.clone(), *deposit));
        }
        Ok(())
    }

    /// A receiptless response changes neither confirmed spend nor the budget
    /// already registered before dispatch. In particular, late headers cannot
    /// resurrect an attempt already reconciled by a signed receipt.
    pub fn reserve_authorization_without_receipt(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
    ) -> Result<Option<u64>> {
        let Submission::Authorization {
            authorization,
            attempt,
            ..
        } = submitted
        else {
            return Ok(None);
        };
        let channels = self.lock()?;
        let entry = channels.get(&cache_key(requirements)).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        let state = attempt
            .0
            .lock()
            .map_err(|_| Error::Mpp("batch attempt lock poisoned".to_string()))?;
        let registered = state.as_ref().ok_or_else(|| {
            Error::Mpp("batch authorization was not registered before dispatch".to_string())
        })?;
        check_attempt(entry, registered, authorization)?;
        Ok(Some(entry.confirmed.charged_cumulative_amount()))
    }

    /// Release an authorization only when the caller can prove it was never
    /// submitted or was rejected without a charge. HTTP 5xx, disconnects and
    /// cancellation alone are not such proof.
    pub fn reject_submission(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
    ) -> Result<()> {
        let Submission::Authorization {
            authorization,
            attempt,
            ..
        } = submitted
        else {
            return Ok(());
        };
        let mut channels = self.lock()?;
        let entry = channels.get_mut(&cache_key(requirements)).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        let mut state = attempt
            .0
            .lock()
            .map_err(|_| Error::Mpp("batch attempt lock poisoned".to_string()))?;
        let registered = state.as_mut().ok_or_else(|| {
            Error::Mpp("batch authorization was not registered before dispatch".to_string())
        })?;
        check_attempt(entry, registered, authorization)?;
        if registered.rejected {
            return Ok(());
        }
        if registered.settled.is_some() {
            return Err(Error::Mpp(
                "cannot reject a settled batch attempt".to_string(),
            ));
        }
        entry.remaining = entry
            .remaining
            .checked_sub(u128::from(registered.ceiling))
            .ok_or_else(|| Error::Mpp("batch rejection contradicts confirmed spend".to_string()))?;
        registered.rejected = true;
        if entry
            .pending_funding
            .as_ref()
            .is_some_and(|(pending, _)| Arc::ptr_eq(&pending.0, &attempt.0))
        {
            entry.pending_funding = None;
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, CachedChannel>>> {
        self.channels
            .lock()
            .map_err(|_| Error::Mpp("batch-settlement channel cache lock poisoned".to_string()))
    }
}

pub(crate) fn pending_funding_error() -> Error {
    Error::Mpp(
        "batch funding is pending confirmation; retry after the original payment completes"
            .to_string(),
    )
}

fn check_attempt(
    entry: &CachedChannel,
    registered: &RegisteredAttempt,
    authorization: &BatchAuthorization,
) -> Result<()> {
    if !Arc::ptr_eq(&entry.generation, &registered.generation)
        || registered.request_id != authorization.request_id
        || authorization.authorized_amount.parse::<u64>().ok() != Some(registered.ceiling)
        || authorization.channel_id != entry.confirmed.channel_id().to_string()
        || authorization.payer != entry.confirmed.config().payer
    {
        return Err(Error::Mpp(
            "batch attempt does not match registered authorization".to_string(),
        ));
    }
    Ok(())
}

/// Decode the `PAYMENT-RESPONSE` settlement receipt from response headers.
fn decode_settlement(headers: &[(String, String)]) -> Option<BatchSettlementResponse> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("payment-response"))
        .map(|(_, value)| value)?;
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, value).ok()?;
    serde_json::from_slice(&decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_kit::x402::batch_settlement::{BatchChannelConfig, BatchExtra};
    use pay_kit::x402::solana_keychain::SolanaSigner;

    fn requirements(pay_to: &str, fee_payer: &str) -> BatchRequirements {
        BatchRequirements {
            scheme: "batch-settlement".to_string(),
            network: "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp".to_string(),
            amount: "1000".to_string(),
            asset: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            pay_to: pay_to.to_string(),
            max_timeout_seconds: 300,
            extra: BatchExtra {
                payment_flow: None,
                fee_payer: fee_payer.to_string(),
                receiver_authorizer: None,
                withdraw_delay: 3600,
                token_program: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
                memo: None,
                recent_blockhash: None,
                recent_slot: None,
                min_deposit: None,
                voucher_signer: None,
                operator: None,
                max_idle_secs: None,
                channel_state: None,
                voucher_state: None,
                transaction_versions: None,
            },
        }
    }

    #[test]
    fn offers_that_differ_in_any_channel_seed_do_not_share_a_channel() {
        let base = requirements("payTo1", "feePayer1");
        // A channel is bound to its receiver and its sponsor; reusing one across
        // either would sign vouchers redeemable by the wrong party.
        assert_ne!(
            cache_key(&base),
            cache_key(&requirements("payTo2", "feePayer1"))
        );
        assert_ne!(
            cache_key(&base),
            cache_key(&requirements("payTo1", "feePayer2"))
        );

        let mut different_delay = requirements("payTo1", "feePayer1");
        different_delay.extra.withdraw_delay = 900;
        assert_ne!(cache_key(&base), cache_key(&different_delay));

        // The per-request price is not a channel property: the same channel
        // funds cheap and expensive routes alike.
        let mut different_price = requirements("payTo1", "feePayer1");
        different_price.amount = "5000".to_string();
        assert_eq!(cache_key(&base), cache_key(&different_price));
    }

    #[test]
    fn an_unknown_channel_reports_nothing_rather_than_failing() {
        let cache = BatchChannelCache::new();
        let requirements = requirements("payTo1", "feePayer1");
        assert!(cache.get(&requirements).unwrap().is_none());
        assert!(cache.adopt_corrective(&requirements).unwrap().is_none());
        // Removing something absent is a no-op, so a close is idempotent.
        cache.remove(&requirements).unwrap();
    }

    #[test]
    fn receiptless_server_authorizations_reserve_the_approved_ceiling() {
        let cache = BatchChannelCache::new();
        let requirements = requirements("payTo1", "feePayer1");
        let channel_id = solana_pubkey::Pubkey::new_unique();
        let config = BatchChannelConfig {
            payer: "payer".to_string(),
            payer_authorizer: "operator".to_string(),
            receiver: requirements.pay_to.clone(),
            receiver_authorizer: None,
            token: requirements.asset.clone(),
            withdraw_delay: requirements.extra.withdraw_delay,
            salt: "1".to_string(),
            open_slot: 1,
            voucher_signer: Some("server".to_string()),
        };
        cache
            .insert(
                &requirements,
                BatchChannel::new(channel_id, config, 3_000, 5_000),
            )
            .unwrap();
        let submitted = Submission::Authorization {
            authorization: BatchAuthorization {
                kind: "proof".to_string(),
                channel_id: channel_id.to_string(),
                payer: "payer".to_string(),
                request_id: "request".to_string(),
                authorized_amount: "1000".to_string(),
                expires_at: i64::MAX,
                signature: "signature".to_string(),
            },
            confirmed_deposit: None,
            attempt: Attempt::default(),
        };

        cache
            .register_submission(&requirements, &submitted)
            .unwrap();
        assert_eq!(
            cache
                .reserve_authorization_without_receipt(&requirements, &submitted)
                .unwrap(),
            Some(3_000)
        );
        assert_eq!(
            cache
                .get(&requirements)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            3_000
        );
    }

    fn uncertain_fixture(
        floor: u64,
        deposit: u64,
    ) -> (
        BatchChannelCache,
        BatchRequirements,
        Submission,
        crate::signer::ResolvedSigner,
    ) {
        let memory = pay_kit::solana_keychain::MemorySigner::from_bytes(
            &solana_keypair::Keypair::new().to_bytes(),
        )
        .unwrap();
        let operator = crate::signer::ResolvedSigner::local(&crate::backend::Ephemeral, memory);
        let requirements = requirements("payTo1", "feePayer1");
        let channel_id = solana_pubkey::Pubkey::new_unique();
        let cache = BatchChannelCache::new();
        cache
            .insert(
                &requirements,
                BatchChannel::new(
                    channel_id,
                    BatchChannelConfig {
                        payer: "payer".to_string(),
                        payer_authorizer: operator.pubkey().to_string(),
                        receiver: requirements.pay_to.clone(),
                        receiver_authorizer: None,
                        token: requirements.asset.clone(),
                        withdraw_delay: requirements.extra.withdraw_delay,
                        salt: "1".to_string(),
                        open_slot: 1,
                        voucher_signer: Some("server".to_string()),
                    },
                    floor,
                    deposit,
                ),
            )
            .unwrap();
        let submission = Submission::Authorization {
            authorization: BatchAuthorization {
                kind: "proof".to_string(),
                channel_id: channel_id.to_string(),
                payer: "payer".to_string(),
                request_id: "request".to_string(),
                authorized_amount: "1000".to_string(),
                expires_at: i64::MAX,
                signature: "payer-signature".to_string(),
            },
            confirmed_deposit: None,
            attempt: Attempt::default(),
        };
        cache
            .register_submission(&requirements, &submission)
            .unwrap();
        (cache, requirements, submission, operator)
    }

    async fn signed_receipt(
        operator: &crate::signer::ResolvedSigner,
        channel: &BatchChannel,
        cumulative: u64,
        charged: u64,
    ) -> BatchSettlementResponse {
        let voucher = pay_kit::x402::client::batch_settlement::sign_voucher(
            operator,
            channel.channel_id(),
            cumulative,
        )
        .await
        .unwrap();
        BatchSettlementResponse {
            success: true,
            error_reason: None,
            payer: Some(channel.config().payer.clone()),
            transaction: String::new(),
            network: "solana:mainnet".to_string(),
            amount: String::new(),
            extra: Some(pay_kit::x402::batch_settlement::BatchSettlementExtra {
                commitment_id: Some("receipt".to_string()),
                charged_amount: Some(charged.to_string()),
                channel_state: None,
                voucher: Some(voucher),
            }),
        }
    }

    #[tokio::test]
    async fn ordinary_receipt_reconciles_uncertain_ceiling_without_poisoning_history() {
        let (cache, requirements, submitted, operator) = uncertain_fixture(0, 10_000);
        cache
            .reserve_authorization_without_receipt(&requirements, &submitted)
            .unwrap();
        let channel = cache.get(&requirements).unwrap().unwrap();
        // Real payment building reinserts the channel before the next response.
        cache.insert(&requirements, channel.clone()).unwrap();
        let next = new_attempt(&cache, &requirements, &submitted, "next", 100);
        let receipt = signed_receipt(&operator, &channel, 200, 100).await;
        assert_eq!(
            cache
                .apply_settlement(&requirements, &next, &receipt)
                .unwrap(),
            200
        );
        assert_eq!(
            cache
                .get(&requirements)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            200
        );
        assert_eq!(
            cache.lock().unwrap()[&cache_key(&requirements)].remaining,
            900
        );
        // Delayed exact evidence releases only the first attempt's excess.
        let receipt = signed_receipt(&operator, &channel, 100, 100).await;
        assert_eq!(
            cache
                .apply_settlement(&requirements, &submitted, &receipt)
                .unwrap(),
            200
        );
        assert_eq!(
            cache.lock().unwrap()[&cache_key(&requirements)].remaining,
            0
        );
    }

    #[tokio::test]
    async fn uncertain_receipts_keep_floor_cap_signature_and_escrow_checks() {
        let (cache, requirements, submitted, operator) = uncertain_fixture(500, 10_000);
        cache
            .reserve_authorization_without_receipt(&requirements, &submitted)
            .unwrap();
        let channel = cache.get(&requirements).unwrap().unwrap();
        for (cumulative, charged) in [(400, 100), (1_601, 100), (1_701, 1_001)] {
            let receipt = signed_receipt(&operator, &channel, cumulative, charged).await;
            assert!(
                cache
                    .apply_settlement(&requirements, &submitted, &receipt)
                    .is_err()
            );
            assert_eq!(
                cache
                    .get(&requirements)
                    .unwrap()
                    .unwrap()
                    .charged_cumulative_amount(),
                500
            );
            assert_eq!(
                cache.lock().unwrap()[&cache_key(&requirements)].remaining,
                1_000
            );
        }
        let (_, _, _, wrong_operator) = uncertain_fixture(0, 10_000);
        let receipt = signed_receipt(&wrong_operator, &channel, 700, 100).await;
        assert!(
            cache
                .apply_settlement(&requirements, &submitted, &receipt)
                .is_err()
        );
        assert_eq!(
            cache
                .get(&requirements)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            500
        );
        assert_eq!(
            cache.lock().unwrap()[&cache_key(&requirements)].remaining,
            1_000
        );
        // Even a valid signed voucher cannot exceed the approved deposit.
        let (small_cache, req, sub, signer) = uncertain_fixture(0, 1_000);
        small_cache
            .reserve_authorization_without_receipt(&req, &sub)
            .unwrap();
        let channel = small_cache.get(&req).unwrap().unwrap();
        let receipt = signed_receipt(&signer, &channel, 1_100, 100).await;
        assert!(small_cache.apply_settlement(&req, &sub, &receipt).is_err());
        assert_eq!(small_cache.get(&req).unwrap().unwrap().deposit(), 1_000);
    }

    #[tokio::test]
    async fn repeated_receiptless_ceilings_do_not_spuriously_exhaust_escrow() {
        let (cache, requirements, submitted, operator) = uncertain_fixture(0, 10_000);
        let original = cache.get(&requirements).unwrap().unwrap();
        for index in 0..1_000 {
            let channel = cache.get(&requirements).unwrap().unwrap();
            assert!(channel.can_cover(1_000));
            assert_eq!(channel.channel_id(), original.channel_id());
            assert_eq!(channel.deposit(), 10_000);
            cache.insert(&requirements, channel).unwrap();
            let attempt = new_attempt(&cache, &requirements, &submitted, &index.to_string(), 1_000);
            let maximum = cache
                .reserve_authorization_without_receipt(&requirements, &attempt)
                .unwrap()
                .unwrap();
            assert!(maximum <= 10_000);
        }
        let receipt = signed_receipt(&operator, &original, 1_200, 100).await;
        assert_eq!(
            cache
                .apply_settlement(&requirements, &submitted, &receipt)
                .unwrap(),
            1_200
        );
        let channel = cache.get(&requirements).unwrap().unwrap();
        assert!(channel.can_cover(1_000));
        assert_eq!(channel.deposit(), 10_000);
        assert_eq!(cache.lock().unwrap().len(), 1);
        assert_eq!(
            cache.lock().unwrap()[&cache_key(&requirements)].remaining,
            998_900
        );
        // Only the cache and the original still-live Submission retain this
        // generation. The 1,000 completed receiptless handles were not indexed.
        assert_eq!(
            Arc::strong_count(&cache.lock().unwrap()[&cache_key(&requirements)].generation),
            2
        );
    }

    async fn corrective(
        requirements: &BatchRequirements,
        operator: &crate::signer::ResolvedSigner,
        channel: &BatchChannel,
        cumulative: u64,
    ) -> BatchRequirements {
        use pay_kit::x402::batch_settlement::{ChannelStateSnapshot, VoucherState};
        let voucher = pay_kit::x402::client::batch_settlement::sign_voucher(
            operator,
            channel.channel_id(),
            cumulative,
        )
        .await
        .unwrap();
        let mut requirements = requirements.clone();
        requirements.extra.channel_state = Some(ChannelStateSnapshot {
            channel_id: channel.channel_id().to_string(),
            balance: channel.deposit().to_string(),
            total_claimed: "0".to_string(),
            withdraw_requested_at: 0,
            charged_cumulative_amount: Some(cumulative.to_string()),
        });
        requirements.extra.voucher_state = Some(VoucherState {
            signed_max_claimable: cumulative.to_string(),
            expires_at: voucher.expires_at,
            signature: voucher.signature,
        });
        requirements
    }

    #[tokio::test]
    async fn delayed_receipt_and_corrective_preserve_other_pending_attempts() {
        for use_corrective in [false, true] {
            let (cache, req, prototype, operator) = uncertain_fixture(0, 10_000);
            cache.reject_submission(&req, &prototype).unwrap();
            let a = new_attempt(&cache, &req, &prototype, "A", 100);
            let b = new_attempt(&cache, &req, &prototype, "B", 1_000);
            let channel = cache.get(&req).unwrap().unwrap();
            let delayed = signed_receipt(&operator, &channel, 100, 100).await;
            assert_eq!(
                cache
                    .reserve_authorization_without_receipt(&req, &b)
                    .unwrap(),
                Some(0)
            );
            if use_corrective {
                let proof = corrective(&req, &operator, &channel, 100).await;
                assert_eq!(cache.adopt_corrective(&proof).unwrap(), Some(100));
            } else {
                assert_eq!(cache.apply_settlement(&req, &a, &delayed).unwrap(), 100);
            }
            assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
            let c = new_attempt(&cache, &req, &prototype, "C", 100);
            let receipt = signed_receipt(&operator, &channel, 300, 100).await;
            assert_eq!(cache.apply_settlement(&req, &c, &receipt).unwrap(), 300);
            assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 900);
            // Older proofs and stale builder reinsertion cannot lower 300.
            assert_eq!(cache.apply_settlement(&req, &a, &delayed).unwrap(), 300);
            let proof = corrective(&req, &operator, &channel, 100).await;
            assert_eq!(cache.adopt_corrective(&proof).unwrap(), Some(300));
            cache.insert(&req, channel.clone()).unwrap();
            assert_eq!(
                cache
                    .get(&req)
                    .unwrap()
                    .unwrap()
                    .charged_cumulative_amount(),
                300
            );
            assert_eq!(
                cache
                    .reserve_authorization_without_receipt(&req, &a)
                    .unwrap(),
                Some(300)
            );
            assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 900);
        }
    }

    #[tokio::test]
    async fn one_receiptless_ceiling_cannot_explain_two_gaps() {
        let (cache, req, prototype, operator) = uncertain_fixture(0, 10_000);
        cache.reject_submission(&req, &prototype).unwrap();
        let b = new_attempt(&cache, &req, &prototype, "B", 100);
        cache
            .reserve_authorization_without_receipt(&req, &b)
            .unwrap();
        let c = new_attempt(&cache, &req, &prototype, "C", 100);
        let channel = cache.get(&req).unwrap().unwrap();
        let receipt = signed_receipt(&operator, &channel, 200, 100).await;
        assert_eq!(cache.apply_settlement(&req, &c, &receipt).unwrap(), 200);
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
        let d = new_attempt(&cache, &req, &prototype, "D", 100);
        let receipt = signed_receipt(&operator, &channel, 400, 100).await;
        assert!(cache.apply_settlement(&req, &d, &receipt).is_err());
        assert_eq!(
            cache
                .get(&req)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            200
        );
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 100);
    }

    #[tokio::test]
    async fn corrective_consumes_budget_but_does_not_complete_arbitrary_attempts() {
        for has_delayed_a in [false, true] {
            let (cache, req, prototype, operator) = uncertain_fixture(0, 10_000);
            cache.reject_submission(&req, &prototype).unwrap();
            let _a = has_delayed_a.then(|| new_attempt(&cache, &req, &prototype, "A", 100));
            let b = new_attempt(&cache, &req, &prototype, "B", 100);
            cache
                .reserve_authorization_without_receipt(&req, &b)
                .unwrap();
            let channel = cache.get(&req).unwrap().unwrap();
            let proof = corrective(&req, &operator, &channel, 100).await;
            assert_eq!(cache.adopt_corrective(&proof).unwrap(), Some(100));
            let c = new_attempt(&cache, &req, &prototype, "C", 100);
            let receipt = signed_receipt(&operator, &channel, 300, 100).await;
            let result = cache.apply_settlement(&req, &c, &receipt);
            assert_eq!(result.is_ok(), has_delayed_a);
            assert_eq!(
                cache
                    .get(&req)
                    .unwrap()
                    .unwrap()
                    .charged_cumulative_amount(),
                if has_delayed_a { 300 } else { 100 }
            );
        }
    }

    #[tokio::test]
    async fn overlapping_ceilings_and_duplicate_lifecycle_are_accounted_once() {
        let (cache, req, a, operator) = uncertain_fixture(0, 1_000);
        let b = new_attempt(&cache, &req, &a, "B", 1_000);
        cache.register_submission(&req, &a.clone()).unwrap();
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 2_000);
        let channel = cache.get(&req).unwrap().unwrap();
        let first = signed_receipt(&operator, &channel, 100, 100).await;
        assert_eq!(cache.apply_settlement(&req, &a, &first).unwrap(), 100);
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
        assert_eq!(
            cache.apply_settlement(&req, &a.clone(), &first).unwrap(),
            100
        );
        cache
            .reserve_authorization_without_receipt(&req, &a)
            .unwrap();
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
        let second = signed_receipt(&operator, &channel, 200, 100).await;
        assert_eq!(cache.apply_settlement(&req, &b, &second).unwrap(), 200);
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
        assert_eq!(cache.get(&req).unwrap().unwrap().deposit(), 1_000);
    }

    #[tokio::test]
    async fn invalid_corrective_and_receipt_leave_all_trusted_state_unchanged() {
        let (cache, req, a, operator) = uncertain_fixture(0, 1_000);
        let channel = cache.get(&req).unwrap().unwrap();
        let (_, _, _, stranger) = uncertain_fixture(0, 1_000);
        for signer in [&operator, &stranger] {
            let mut proof = corrective(&req, signer, &channel, 100).await;
            if signer.pubkey() == operator.pubkey() {
                proof.extra.channel_state.as_mut().unwrap().channel_id =
                    solana_pubkey::Pubkey::new_unique().to_string();
            }
            assert!(cache.adopt_corrective(&proof).is_err());
            assert_eq!(
                cache
                    .get(&req)
                    .unwrap()
                    .unwrap()
                    .charged_cumulative_amount(),
                0
            );
            assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
        }
        for (cumulative, charged) in [(1_001, 1_001), (1_100, 100)] {
            let receipt = signed_receipt(&operator, &channel, cumulative, charged).await;
            assert!(cache.apply_settlement(&req, &a, &receipt).is_err());
        }
        let mut wrong_channel = signed_receipt(&operator, &channel, 100, 100).await;
        wrong_channel
            .extra
            .as_mut()
            .unwrap()
            .voucher
            .as_mut()
            .unwrap()
            .channel_id = solana_pubkey::Pubkey::new_unique().to_string();
        assert!(cache.apply_settlement(&req, &a, &wrong_channel).is_err());
        let valid = signed_receipt(&operator, &channel, 100, 100).await;
        assert_eq!(cache.apply_settlement(&req, &a, &valid).unwrap(), 100);
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovered_kit_history_is_reconciled_once_without_weakening_live_bounds() {
        use pay_kit::core::payment_channels as pc;
        use pay_kit::x402::client::batch_settlement as kit;
        use pay_kit::x402::solana_rpc_client::{api::request::RpcRequest, rpc_client::RpcClient};

        let (_, _, _, operator) = uncertain_fixture(0, 10_000);
        let payer = solana_pubkey::Pubkey::new_unique();
        let receiver = solana_pubkey::Pubkey::new_unique();
        let sponsor = solana_pubkey::Pubkey::new_unique();
        let mut req = requirements(&receiver.to_string(), &sponsor.to_string());
        req.extra.voucher_signer = Some("server".to_string());
        req.extra.operator = Some(operator.pubkey().to_string());
        let policy = kit::ServerSignedChannelsPolicy::new()
            .allow_operator(operator.pubkey(), 10_000)
            .unwrap();
        let terms = kit::resolve_terms_with_token_program_and_policy(
            &req,
            req.extra.token_program.parse().unwrap(),
            None,
            Some(&policy),
        )
        .unwrap();
        let (id, bump) = pc::find_channel_pda(
            &payer,
            &sponsor,
            &terms.mint,
            &operator.pubkey(),
            1,
            1,
            &pc::default_program_id(),
        );
        let account = pc::generated::accounts::Channel {
            discriminator: 0,
            version: 1,
            bump,
            status: 0,
            salt: 1,
            deposit: 10_000,
            settlement: pc::generated::types::SettlementWatermarks {
                settled: 500,
                payout_watermark: 0,
            },
            closure_started_at: 0,
            payer_withdrawn_at: 0,
            grace_period: terms.withdraw_delay,
            distribution_hash: pc::distribution_hash(&pc::sole_recipient(&receiver)),
            payer: pc::to_address(&payer),
            payee: pc::to_address(&sponsor),
            authorized_signer: pc::to_address(&operator.pubkey()),
            mint: pc::to_address(&terms.mint),
            rent_payer: pc::to_address(&sponsor),
            open_slot: 1,
        };
        let data = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            borsh::to_vec(&account).unwrap(),
        );
        let mocks = HashMap::from([(
            RpcRequest::GetProgramAccounts,
            serde_json::json!([{
                "pubkey": id.to_string(),
                "account": {
                    "lamports": 1, "owner": pc::default_program_id().to_string(),
                    "data": [data, "base64"], "executable": false, "rentEpoch": 0
                }
            }]),
        )]);
        let discovery_req = req.clone();
        let discovery_terms = terms.clone();
        let recovered = tokio::task::spawn_blocking(move || {
            let rpc = RpcClient::new_mock_with_mocks("succeeds", mocks);
            kit::discover_channel(&rpc, &payer, &discovery_req, &discovery_terms)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
        let cache = BatchChannelCache::new();
        cache.insert_recovered(&req, recovered.clone()).unwrap();
        let a = Submission::Authorization {
            authorization: BatchAuthorization {
                kind: "proof".to_string(),
                channel_id: id.to_string(),
                payer: payer.to_string(),
                request_id: "recovered".to_string(),
                authorized_amount: "1000".to_string(),
                expires_at: i64::MAX,
                signature: "payer-signature".to_string(),
            },
            confirmed_deposit: None,
            attempt: Attempt::default(),
        };
        cache.register_submission(&req, &a).unwrap();
        // The extra 100 predates this process. Only kit's actual recovered
        // channel (not BatchChannel::new) can establish this incomplete history.
        let receipt = signed_receipt(&operator, &recovered, 700, 100).await;
        assert_eq!(cache.apply_settlement(&req, &a, &receipt).unwrap(), 700);
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
        let b = new_attempt(&cache, &req, &a, "after-recovery", 100);
        let unexplained = signed_receipt(&operator, &recovered, 900, 100).await;
        assert!(cache.apply_settlement(&req, &b, &unexplained).is_err());
        assert_eq!(
            cache
                .get(&req)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            700
        );
        let valid = signed_receipt(&operator, &recovered, 800, 100).await;
        assert_eq!(cache.apply_settlement(&req, &b, &valid).unwrap(), 800);

        // A recovered receipt must not erase another live authorization.
        let concurrent = BatchChannelCache::new();
        concurrent
            .insert_recovered(&req, recovered.clone())
            .unwrap();
        let first = new_attempt(&concurrent, &req, &a, "first", 100);
        let second = new_attempt(&concurrent, &req, &a, "second", 100);
        let first_receipt = signed_receipt(&operator, &recovered, 700, 100).await;
        assert_eq!(
            concurrent
                .apply_settlement(&req, &first, &first_receipt)
                .unwrap(),
            700
        );
        assert_eq!(concurrent.lock().unwrap()[&cache_key(&req)].remaining, 100);
        let second_receipt = signed_receipt(&operator, &recovered, 800, 100).await;
        assert_eq!(
            concurrent
                .apply_settlement(&req, &second, &second_receipt)
                .unwrap(),
            800
        );
        assert_eq!(concurrent.lock().unwrap()[&cache_key(&req)].remaining, 0);

        // A corrective baseline cannot tell whether a live attempt was included.
        // Its late exact receipt must consume the charge exempted as history.
        let corrected = BatchChannelCache::new();
        corrected.insert_recovered(&req, recovered.clone()).unwrap();
        let late = new_attempt(&corrected, &req, &a, "late", 100);
        let proof = corrective(&req, &operator, &recovered, 600).await;
        assert_eq!(corrected.adopt_corrective(&proof).unwrap(), Some(600));
        assert_eq!(corrected.lock().unwrap()[&cache_key(&req)].remaining, 100);
        let late_receipt = signed_receipt(&operator, &recovered, 600, 100).await;
        assert_eq!(
            corrected
                .apply_settlement(&req, &late, &late_receipt)
                .unwrap(),
            600
        );
        assert_eq!(corrected.lock().unwrap()[&cache_key(&req)].remaining, 0);
        assert_eq!(
            corrected
                .apply_settlement(&req, &late.clone(), &late_receipt)
                .unwrap(),
            600
        );
        let next = new_attempt(&corrected, &req, &a, "next", 100);
        assert!(
            corrected
                .apply_settlement(&req, &next, &second_receipt)
                .is_err()
        );
        assert_eq!(
            corrected
                .get(&req)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            600
        );

        // Even a zero-advance recovery proof consumes the one-time allowance.
        let unchanged = BatchChannelCache::new();
        unchanged.insert_recovered(&req, recovered.clone()).unwrap();
        let proof = corrective(&req, &operator, &recovered, 500).await;
        assert_eq!(unchanged.adopt_corrective(&proof).unwrap(), Some(500));
        let next = new_attempt(&unchanged, &req, &a, "next", 100);
        assert!(
            unchanged
                .apply_settlement(&req, &next, &first_receipt)
                .is_err()
        );

        // A lost top-up receipt is repaired by actual kit chain discovery, not
        // by trusting live HTTP headers or a server-reported deposit balance.
        let funding = BatchChannelCache::new();
        funding
            .insert(
                &req,
                BatchChannel::new(id, recovered.config().clone(), 500, 10_000),
            )
            .unwrap();
        let mut top_up = a.clone();
        if let Submission::Authorization {
            attempt,
            confirmed_deposit,
            ..
        } = &mut top_up
        {
            *attempt = Attempt::default();
            *confirmed_deposit = Some(11_000);
        }
        funding.register_submission(&req, &top_up).unwrap();
        funding
            .reserve_authorization_without_receipt(&req, &top_up)
            .unwrap();
        assert!(funding.has_pending_funding(&req).unwrap());
        assert_eq!(funding.get(&req).unwrap().unwrap().deposit(), 10_000);
        let mut duplicate_funding = top_up.clone();
        if let Submission::Authorization {
            attempt,
            authorization,
            ..
        } = &mut duplicate_funding
        {
            *attempt = Attempt::default();
            authorization.request_id = "must-not-dispatch".to_string();
        }
        assert!(
            funding
                .register_submission(&req, &duplicate_funding)
                .is_err()
        );
        assert!(funding.insert_recovered(&req, recovered.clone()).is_err());
        assert!(funding.has_pending_funding(&req).unwrap());
        assert_eq!(funding.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
        let mut funded_account = account;
        // Even if chain contains an additional donation, the planned approved
        // funding total remains the ceiling available to this process.
        funded_account.deposit = 12_000;
        let data = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            borsh::to_vec(&funded_account).unwrap(),
        );
        let mocks = HashMap::from([(
            RpcRequest::GetProgramAccounts,
            serde_json::json!([{
                "pubkey": id.to_string(),
                "account": {
                    "lamports": 1, "owner": pc::default_program_id().to_string(),
                    "data": [data, "base64"], "executable": false, "rentEpoch": 0
                }
            }]),
        )]);
        let discovery_req = req.clone();
        let observed = tokio::task::spawn_blocking(move || {
            let rpc = RpcClient::new_mock_with_mocks("succeeds", mocks);
            kit::discover_channel(&rpc, &payer, &discovery_req, &terms)
                .unwrap()
                .unwrap()
        })
        .await
        .unwrap();
        funding.insert_recovered(&req, observed).unwrap();
        assert!(!funding.has_pending_funding(&req).unwrap());
        assert_eq!(funding.get(&req).unwrap().unwrap().deposit(), 11_000);
        assert_eq!(
            funding
                .get(&req)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            500
        );
        let following = new_attempt(&funding, &req, &a, "following", 100);
        let late_receipt = signed_receipt(&operator, &recovered, 600, 100).await;
        assert_eq!(
            funding
                .apply_settlement(&req, &top_up, &late_receipt)
                .unwrap(),
            600
        );
        assert_eq!(funding.lock().unwrap()[&cache_key(&req)].remaining, 100);
        let next_receipt = signed_receipt(&operator, &recovered, 700, 100).await;
        assert_eq!(
            funding
                .apply_settlement(&req, &following, &next_receipt)
                .unwrap(),
            700
        );
        assert_eq!(funding.lock().unwrap()[&cache_key(&req)].remaining, 0);
    }

    #[tokio::test]
    async fn late_topup_receipt_preserves_verified_deposit_and_newer_spend() {
        let (cache, req, ordinary, operator) = uncertain_fixture(0, 1_000);
        let mut top_up = ordinary.clone();
        if let Submission::Authorization {
            attempt,
            confirmed_deposit,
            authorization,
        } = &mut top_up
        {
            *attempt = Attempt::default();
            *confirmed_deposit = Some(3_000);
            authorization.request_id = "late-top-up".into();
            authorization.authorized_amount = "2000".into();
        }
        cache.register_submission(&req, &top_up).unwrap();
        let channel = cache.get(&req).unwrap().unwrap();
        let topup_receipt = signed_receipt(&operator, &channel, 100, 100).await;
        let ordinary_receipt = signed_receipt(&operator, &channel, 200, 100).await;

        cache
            .apply_settlement(&req, &ordinary, &ordinary_receipt)
            .unwrap();
        assert!(cache.has_pending_funding(&req).unwrap());
        assert_eq!(
            cache
                .apply_settlement(&req, &top_up, &topup_receipt)
                .unwrap(),
            200
        );
        let confirmed = cache.get(&req).unwrap().unwrap();
        assert_eq!(confirmed.charged_cumulative_amount(), 200);
        assert_eq!(confirmed.deposit(), 3_000);
        assert!(!cache.has_pending_funding(&req).unwrap());
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
    }

    #[tokio::test]
    async fn pending_topup_needs_verified_receipt_or_definite_non_submission() {
        for definitely_not_submitted in [false, true] {
            let (cache, req, prototype, operator) = uncertain_fixture(0, 1_000);
            cache.reject_submission(&req, &prototype).unwrap();
            let mut top_up = prototype.clone();
            if let Submission::Authorization {
                attempt,
                confirmed_deposit,
                authorization,
            } = &mut top_up
            {
                *attempt = Attempt::default();
                *confirmed_deposit = Some(2_000);
                authorization.request_id = "top-up".to_string();
            }
            cache.register_submission(&req, &top_up).unwrap();
            cache
                .reserve_authorization_without_receipt(&req, &top_up)
                .unwrap();
            let channel = cache.get(&req).unwrap().unwrap();
            assert!(cache.has_pending_funding(&req).unwrap());
            assert_eq!(channel.deposit(), 1_000);
            if definitely_not_submitted {
                cache.reject_submission(&req, &top_up).unwrap();
                cache.reject_submission(&req, &top_up.clone()).unwrap();
                assert!(cache.register_submission(&req, &top_up).is_err());
                assert_eq!(cache.get(&req).unwrap().unwrap().deposit(), 1_000);
            } else {
                let (_, _, _, stranger) = uncertain_fixture(0, 1_000);
                let invalid = signed_receipt(&stranger, &channel, 100, 100).await;
                assert!(cache.apply_settlement(&req, &top_up, &invalid).is_err());
                assert!(cache.has_pending_funding(&req).unwrap());
                assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 1_000);
                let valid = signed_receipt(&operator, &channel, 100, 100).await;
                assert_eq!(cache.apply_settlement(&req, &top_up, &valid).unwrap(), 100);
                assert_eq!(cache.get(&req).unwrap().unwrap().deposit(), 2_000);
                cache
                    .reserve_authorization_without_receipt(&req, &top_up)
                    .unwrap();
            }
            assert!(!cache.has_pending_funding(&req).unwrap());
            assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
        }
    }

    #[tokio::test]
    async fn dropped_ambiguous_attempt_retains_budget_but_definite_rejection_releases_it() {
        let (cache, req, prototype, operator) = uncertain_fixture(0, 1_000);
        cache.reject_submission(&req, &prototype).unwrap();
        let ambiguous = new_attempt(&cache, &req, &prototype, "ambiguous", 100);
        drop(ambiguous);
        let rejected = new_attempt(&cache, &req, &prototype, "rejected", 100);
        cache.reject_submission(&req, &rejected).unwrap();
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 100);
        let channel = cache.get(&req).unwrap().unwrap();
        let proof = corrective(&req, &operator, &channel, 100).await;
        assert_eq!(cache.adopt_corrective(&proof).unwrap(), Some(100));
        let next = new_attempt(&cache, &req, &prototype, "next", 100);
        let receipt = signed_receipt(&operator, &channel, 300, 100).await;
        assert!(cache.apply_settlement(&req, &next, &receipt).is_err());
    }

    #[tokio::test]
    async fn native_exact_delta_cannot_reuse_budget_already_consumed_by_corrective() {
        let (cache, req, prototype, operator) = uncertain_fixture(0, 1_000);
        cache.reject_submission(&req, &prototype).unwrap();
        let a = new_attempt(&cache, &req, &prototype, "A", 100);
        let channel = cache.get(&req).unwrap().unwrap();
        let proof = corrective(&req, &operator, &channel, 100).await;
        cache.adopt_corrective(&proof).unwrap();
        let receipt = signed_receipt(&operator, &channel, 200, 100).await;
        assert!(cache.apply_settlement(&req, &a, &receipt).is_err());
        assert_eq!(
            cache
                .get(&req)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            100
        );
        assert_eq!(cache.lock().unwrap()[&cache_key(&req)].remaining, 0);
    }

    fn new_attempt(
        cache: &BatchChannelCache,
        requirements: &BatchRequirements,
        prototype: &Submission,
        id: &str,
        ceiling: u64,
    ) -> Submission {
        let Submission::Authorization { authorization, .. } = prototype else {
            unreachable!()
        };
        let mut authorization = authorization.clone();
        authorization.request_id = id.to_string();
        authorization.authorized_amount = ceiling.to_string();
        let attempt = Submission::Authorization {
            authorization,
            confirmed_deposit: None,
            attempt: Attempt::default(),
        };
        cache.register_submission(requirements, &attempt).unwrap();
        attempt
    }

    #[test]
    fn funded_channel_signer_is_reused_only_for_the_same_payer() {
        const KEYPAIR: [u8; 64] = [
            41, 99, 180, 88, 51, 57, 48, 80, 61, 63, 219, 75, 176, 49, 116, 254, 227, 176, 196,
            204, 122, 47, 166, 133, 155, 252, 217, 0, 253, 17, 49, 143, 47, 94, 121, 167, 195, 136,
            72, 22, 157, 48, 77, 88, 63, 96, 57, 122, 181, 243, 236, 188, 241, 134, 174, 224, 100,
            246, 17, 170, 104, 17, 151, 48,
        ];
        let memory = pay_kit::solana_keychain::MemorySigner::from_bytes(&KEYPAIR).unwrap();
        let signer = Arc::new(crate::signer::ResolvedSigner::local(
            &crate::backend::Ephemeral,
            memory,
        ));
        let cache = BatchChannelCache::new();
        let requirements = requirements("payTo1", "feePayer1");

        cache
            .insert_signer(&requirements, Arc::clone(&signer))
            .unwrap();
        cache
            .insert(
                &requirements,
                BatchChannel::new(
                    solana_pubkey::Pubkey::new_unique(),
                    BatchChannelConfig {
                        payer: signer.pubkey().to_string(),
                        payer_authorizer: signer.pubkey().to_string(),
                        receiver: requirements.pay_to.clone(),
                        receiver_authorizer: None,
                        token: requirements.asset.clone(),
                        withdraw_delay: requirements.extra.withdraw_delay,
                        salt: "1".to_string(),
                        open_slot: 1,
                        voucher_signer: None,
                    },
                    0,
                    10_000,
                ),
            )
            .unwrap();
        let reused = cache.signer(&requirements).unwrap().unwrap();
        assert!(Arc::ptr_eq(&signer, &reused));

        assert!(
            cache
                .get_for_payer(&requirements, &solana_pubkey::Pubkey::new_unique())
                .unwrap()
                .is_none()
        );
        assert!(cache.get(&requirements).unwrap().is_none());
        assert!(cache.signer(&requirements).unwrap().is_none());
    }
}
