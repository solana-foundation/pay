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
//! When a successful streaming response has no receipt, server-signed channels
//! conservatively reserve the full payer-approved ceiling so the host tops up
//! before it unknowingly exhausts escrow.

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
        /// Total escrow after a payer-signed top-up. This is intentionally
        /// derived from the submitted transaction rather than server-reported
        /// channel state, which is optional in BlockRun receipts.
        confirmed_deposit: Option<u64>,
    },
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
            }),
            _ => Err(Error::Mpp(
                "batch-settlement paid request carries no credential".to_string(),
            )),
        }
    }

    pub(crate) fn with_confirmed_deposit(self, confirmed_deposit: u64) -> Self {
        match self {
            Self::Authorization { authorization, .. } => Self::Authorization {
                authorization,
                confirmed_deposit: Some(confirmed_deposit),
            },
            submission => submission,
        }
    }
}

/// Process-lifetime cache of open `batch-settlement` channels.
#[derive(Clone, Default)]
pub struct BatchChannelCache {
    channels: Arc<Mutex<HashMap<String, BatchChannel>>>,
    /// Signers for funded channels remain resident for the host process. The
    /// escrow open/top-up is still approval-gated; requests spending capacity
    /// that was already approved must not reopen the keystore every time.
    signers: Arc<Mutex<HashMap<String, Arc<crate::signer::ResolvedSigner>>>>,
}

impl BatchChannelCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The channel already open for this offer, if any.
    pub fn get(&self, requirements: &BatchRequirements) -> Result<Option<BatchChannel>> {
        let channels = self.lock()?;
        Ok(channels.get(&cache_key(requirements)).cloned())
    }

    /// Remember a channel opened for this offer.
    pub fn insert(&self, requirements: &BatchRequirements, channel: BatchChannel) -> Result<()> {
        let mut channels = self.lock()?;
        channels.insert(cache_key(requirements), channel);
        Ok(())
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
    /// Returns the new cumulative watermark. A response that confirms a
    /// different commitment, or a charge other than the advertised price, is
    /// rejected and the watermark is left alone: the next request then re-signs
    /// the same cumulative amount, which the server treats as the idempotent
    /// retry it is.
    pub fn apply_settlement(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
        response: &BatchSettlementResponse,
    ) -> Result<u64> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let channel = channels.get_mut(&key).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        match submitted {
            Submission::Voucher(voucher) => channel
                .apply_payment_response(response, requirements, voucher)
                .map_err(|e| Error::Mpp(format!("batch-settlement settlement rejected: {e}")))?,
            Submission::Authorization {
                authorization,
                confirmed_deposit,
            } => channel
                .apply_authorization_response_with_deposit(
                    response,
                    authorization,
                    *confirmed_deposit,
                )
                .map_err(|e| Error::Mpp(format!("batch-settlement settlement rejected: {e}")))?,
        }
        Ok(channel.charged_cumulative_amount())
    }

    /// Resynchronize from a corrective 402.
    ///
    /// The server proves how much it has charged with a voucher this client
    /// signed; the channel refuses anything it cannot verify against its own
    /// authorizer key. A channel the server no longer knows about is dropped so
    /// the next attempt opens a fresh one.
    pub fn adopt_corrective(&self, requirements: &BatchRequirements) -> Result<Option<u64>> {
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let Some(channel) = channels.get_mut(&key) else {
            return Ok(None);
        };
        match channel.adopt_corrective_state(requirements) {
            Ok(cumulative) => Ok(Some(cumulative)),
            Err(e) => {
                // Unverifiable: the safe move is to forget the channel rather
                // than keep signing against a watermark neither side agrees on.
                channels.remove(&key);
                drop(channels);
                let mut signers = self.signers.lock().map_err(|_| {
                    Error::Mpp("batch-settlement signer cache lock poisoned".to_string())
                })?;
                signers.remove(&key);
                Err(Error::Mpp(format!(
                    "batch-settlement corrective state rejected: {e}"
                )))
            }
        }
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

    /// Conservatively reserve a server-signed authorization after a successful
    /// response that carried no settlement receipt.
    ///
    /// Streaming providers may publish the exact charge only after the initial
    /// headers. A proxy cannot safely keep treating that capacity as unused:
    /// doing so postpones top-up until the server rejects a later request. We
    /// therefore advance by the payer-approved ceiling. A later corrective
    /// proof can replace this conservative watermark with the exact value.
    pub fn reserve_authorization_without_receipt(
        &self,
        requirements: &BatchRequirements,
        submitted: &Submission,
    ) -> Result<Option<u64>> {
        let Submission::Authorization {
            authorization,
            confirmed_deposit,
        } = submitted
        else {
            return Ok(None);
        };
        let authorized = authorization
            .authorized_amount
            .parse::<u64>()
            .map_err(|_| {
                Error::Mpp("batch-settlement authorization amount is invalid".to_string())
            })?;
        let mut channels = self.lock()?;
        let key = cache_key(requirements);
        let channel = channels.get_mut(&key).ok_or_else(|| {
            Error::Mpp("no cached batch-settlement channel for this offer".to_string())
        })?;
        let cumulative = channel
            .charged_cumulative_amount()
            .checked_add(authorized)
            .ok_or_else(|| Error::Mpp("batch-settlement cumulative overflow".to_string()))?;
        let deposit = confirmed_deposit.unwrap_or(channel.deposit());
        if cumulative > deposit {
            return Err(Error::Mpp(
                "batch-settlement authorization exceeds confirmed escrow".to_string(),
            ));
        }
        *channel = BatchChannel::new(
            *channel.channel_id(),
            channel.config().clone(),
            cumulative,
            deposit,
        );
        Ok(Some(cumulative))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, BatchChannel>>> {
        self.channels
            .lock()
            .map_err(|_| Error::Mpp("batch-settlement channel cache lock poisoned".to_string()))
    }
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
        };

        assert_eq!(
            cache
                .reserve_authorization_without_receipt(&requirements, &submitted)
                .unwrap(),
            Some(4_000)
        );
        assert_eq!(
            cache
                .get(&requirements)
                .unwrap()
                .unwrap()
                .charged_cumulative_amount(),
            4_000
        );
    }

    #[test]
    fn funded_channel_signer_is_reused_until_the_channel_is_removed() {
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
        let reused = cache.signer(&requirements).unwrap().unwrap();
        assert!(Arc::ptr_eq(&signer, &reused));

        cache.remove(&requirements).unwrap();
        assert!(cache.signer(&requirements).unwrap().is_none());
    }
}
