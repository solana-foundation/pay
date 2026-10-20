//! Completion-billed responses. Partial flat-price answers are not charged,
//! so they are never released either: the body is held until the handler's
//! explicit completion has been committed.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::Response;
use futures_util::StreamExt;
use pay_kit::core::store::CachedUpstreamResponse;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::PaymentState;
use crate::server::gate::{self, BatchForward, SessionForward};

/// Trusted handler signal, carried in response extensions, never HTTP headers.
///
/// Set only while producing the terminal successful body frame. The payment
/// adapter persists that outcome before releasing the frame to the buyer.
#[derive(Clone, Default)]
pub struct StreamCompletion(Arc<CompletionState>);

type SettlementCallback = Box<dyn FnOnce(bool) + Send>;

#[derive(Default)]
struct CompletionState {
    completed: AtomicBool,
    settlement: Mutex<Option<SettlementCallback>>,
}

impl Drop for CompletionState {
    fn drop(&mut self) {
        if let Some(callback) = self
            .settlement
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            callback(false);
        }
    }
}

impl StreamCompletion {
    /// Keep resource accounting reserved until settlement has a definitive
    /// outcome. The callback receives true only after payment commitment; false
    /// on failure or when the last unfinished response owner drops. It runs once.
    pub fn with_settlement_callback(callback: impl FnOnce(bool) + Send + 'static) -> Self {
        Self(Arc::new(CompletionState {
            completed: AtomicBool::new(false),
            settlement: Mutex::new(Some(Box::new(callback))),
        }))
    }

    pub fn complete(&self) {
        self.0.completed.store(true, Ordering::Release);
    }

    fn completed(&self) -> bool {
        self.0.completed.load(Ordering::Acquire)
    }

    fn settled(&self, success: bool) {
        let callback = self
            .0
            .settlement
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(callback) = callback {
            callback(success);
        }
    }
}

/// Trusted handler observer for usage-metered live responses, carried in
/// response extensions. The payment adapter records each committed usage
/// increment; the callback receives the committed USD total exactly once, when
/// the last owner drops (zero when nothing was committed).
#[derive(Clone)]
pub struct MeteredSettlement(Arc<MeteredState>);

type MeteredCallback = Box<dyn FnOnce(f64) + Send>;

struct MeteredState {
    committed_usd: Mutex<f64>,
    callback: Mutex<Option<MeteredCallback>>,
}

impl Drop for MeteredState {
    fn drop(&mut self) {
        let committed = *self
            .committed_usd
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(callback) = self
            .callback
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            callback(committed);
        }
    }
}

impl MeteredSettlement {
    pub fn with_callback(callback: impl FnOnce(f64) + Send + 'static) -> Self {
        Self(Arc::new(MeteredState {
            committed_usd: Mutex::new(0.0),
            callback: Mutex::new(Some(Box::new(callback))),
        }))
    }

    /// Record one durably committed usage increment.
    pub fn record(&self, base_units: u64, decimals: u8) {
        let usd = base_units as f64 / 10f64.powi(i32::from(decimals));
        *self
            .0
            .committed_usd
            .lock()
            .unwrap_or_else(|error| error.into_inner()) += usd;
    }
}

type SettlementResult = Result<Option<(HeaderName, HeaderValue)>, String>;
type Outcome = (
    bool,
    Option<CachedUpstreamResponse>,
    Option<StreamCompletion>,
);

/// One lifecycle task per admitted authorization, not per chunk or drop.
///
/// The task owns the authorization. Dropping its sender before completion makes
/// it release; sending success transfers responsibility for the earned commit.
/// Normal paths await the handle. Cancellation intentionally lets cleanup or an
/// already-earned commit outlive the HTTP request. Process shutdown retains the
/// store's existing fail-closed abandoned/resumable authorization semantics.
pub(super) struct DeliveryTask {
    outcome: oneshot::Sender<Outcome>,
    task: JoinHandle<SettlementResult>,
}

impl DeliveryTask {
    pub(super) fn batch<S: PaymentState>(state: S, forward: BatchForward) -> Self {
        let (outcome, receiver) = oneshot::channel::<Outcome>();
        let task = tokio::spawn(async move {
            let (success, cached, completion) = receiver.await.unwrap_or((false, None, None));
            let result = if success {
                gate::settle_batch_confirmed(&state, forward, cached).await
            } else {
                gate::release_batch(&state, forward).await;
                Ok(None)
            };
            if let Some(completion) = completion {
                completion.settled(success && result.is_ok());
            }
            if let Err(error) = &result {
                tracing::error!(%error, "completed batch stream could not confirm settlement");
            }
            result
        });
        Self { outcome, task }
    }

    pub(super) fn session(forward: SessionForward, headers: HeaderMap) -> Self {
        let (outcome, receiver) = oneshot::channel::<Outcome>();
        let task = tokio::spawn(async move {
            let (success, _, completion) = receiver.await.unwrap_or((false, None, None));
            if success {
                let result = gate::settle_delegated_session(forward, &headers, None).await
                    .map(|_| None)
                    .inspect_err(|error| tracing::error!(%error, "completed session stream settlement failed"));
                if let Some(completion) = completion {
                    completion.settled(result.is_ok());
                }
                result
            } else {
                // The forward owns the capacity lease; dropping it releases it.
                Ok(None)
            }
        });
        Self { outcome, task }
    }

    pub(super) async fn finish(
        self,
        success: bool,
        cached: Option<CachedUpstreamResponse>,
    ) -> SettlementResult {
        self.finish_observed(success, cached, None).await
    }

    async fn finish_observed(
        self,
        success: bool,
        cached: Option<CachedUpstreamResponse>,
        completion: Option<StreamCompletion>,
    ) -> SettlementResult {
        let _ = self.outcome.send((success, cached, completion));
        self.task.await.map_err(|error| {
            tracing::error!(%error, "response settlement task failed");
            error.to_string()
        })?
    }
}

/// Upper bound on a completion-billed body held until its commit.
const MAX_HELD_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Hold the body and commit only at the handler's explicit completion, then
/// release it. Releasing any byte earlier would let a buyer read a useful
/// prefix and disconnect before the charge, since partial answers are free.
/// A body that errors or outgrows the bound before completing is withheld;
/// one the handler ends without completing is released uncharged. Either way
/// the authorization is released.
pub(super) fn response(
    response: Response,
    completion: StreamCompletion,
    delivery: DeliveryTask,
    capture_replay: bool,
) -> Response {
    let (mut parts, body) = response.into_parts();
    let headers = parts.headers.clone();
    let status = parts.status;
    parts.headers.remove(header::CONTENT_LENGTH);
    let mut source = body.into_data_stream();
    let stream = async_stream::stream! {
        let mut held: Vec<Bytes> = Vec::new();
        let mut held_bytes = 0usize;
        while let Some(chunk) = source.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = delivery.finish(false, None).await;
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            held_bytes = held_bytes.saturating_add(bytes.len());
            if held_bytes > MAX_HELD_RESPONSE_BYTES {
                let _ = delivery.finish(false, None).await;
                yield Err(std::io::Error::other(format!(
                    "completion-billed response exceeds {MAX_HELD_RESPONSE_BYTES} bytes"
                )));
                return;
            }
            held.push(bytes);
            if completion.completed() {
                // Existing replay behavior for oversized resources: return the
                // recorded payment result, never execute the seller twice.
                let cached = (capture_replay && held_bytes <= gate::MAX_BATCH_CACHED_RESPONSE_BYTES)
                    .then(|| gate::batch_cached_response(status, &headers, &held.concat()));
                match delivery.finish_observed(true, cached, Some(completion)).await {
                    Ok(_) => {
                        for bytes in held {
                            yield Ok(bytes);
                        }
                    }
                    Err(error) => yield Err(std::io::Error::other(error)),
                }
                return;
            }
        }
        // EOF without explicit completion is the handler's own uncharged
        // failure answer (typically an error frame), not a buyer disconnect:
        // a dropped body never reaches here, so nothing leaks to one.
        let _ = delivery.finish(false, None).await;
        for bytes in held {
            yield Ok(bytes);
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        metering,
        session::{SessionMpp, test_channel_state},
    };
    use axum::body::Bytes;
    use pay_kit::core::store::{ChannelStore, MemoryChannelStore};
    use pay_kit::mpp::server::session::{SessionConfig, VoucherSigner};
    use pay_kit::solana_keychain::{MemorySigner, TransactionSigner};

    async fn flat_session() -> (
        SessionForward,
        Arc<SessionMpp>,
        Arc<dyn ChannelStore>,
        String,
    ) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(key.as_bytes());
        bytes[32..].copy_from_slice(key.verifying_key().as_bytes());
        let signer: Arc<dyn TransactionSigner> =
            Arc::new(MemorySigner::from_bytes(&bytes).unwrap());
        let config = SessionConfig {
            operator: signer.pubkey().to_string(),
            recipient: signer.pubkey().to_string(),
            currency: solana_pubkey::Pubkey::new_unique().to_string(),
            network: "localnet".into(),
            decimals: 6,
            voucher_signer: VoucherSigner::Operator,
            ..Default::default()
        };
        let store: Arc<dyn ChannelStore> = Arc::new(MemoryChannelStore::new());
        let cache = pay_kit::mpp::blockhash::BlockhashCache::new();
        cache.set("SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".into(), 42, 123);
        let session = Arc::new(
            SessionMpp::new_with_channel_store(config, "completion-test-secret", store.clone())
                .with_blockhash_cache(cache)
                .with_payment_channel_signer(signer.clone()),
        );
        let challenge = session.challenge(None).unwrap();
        let channel = solana_pubkey::Pubkey::new_unique().to_string();
        store
            .put_channel(
                &channel,
                test_channel_state(
                    &channel,
                    1_000_000,
                    signer.pubkey().to_string(),
                    "operator",
                    &challenge.id,
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        let lease = session
            .reserve_delegated_capacity(&channel, 20_000)
            .await
            .unwrap()
            .unwrap();
        let plan = metering::UptoSettlementPlan {
            metering: serde_json::from_value(serde_json::json!({
                "dimensions":[{"direction":"usage","unit":"requests","scale":1,"tiers":[{"price_usd":0.02}]}],
                "upto":{"max_usd":0.02}
            })).unwrap(),
            variant_hint: None, request_properties: Default::default(), ceiling_usd: 0.02, inferred_usage: None,
        };
        let forward = SessionForward::delegated(
            session.clone(),
            channel.clone(),
            0,
            plan,
            (20_000, 1_000_000),
            "payer".into(),
            lease,
        );
        (forward, session, store, channel)
    }

    #[tokio::test]
    async fn flat_session_stream_releases_output_only_after_completion_commits() {
        for end in ["complete", "fail", "drop"] {
            let (forward, session, store, channel) = flat_session().await;
            let outcomes = Arc::new(Mutex::new(Vec::new()));
            let recorded = outcomes.clone();
            let completion = StreamCompletion::with_settlement_callback(move |paid| {
                recorded.lock().unwrap().push(paid);
            });
            let signal = completion.clone();
            let (sender, mut receiver) =
                tokio::sync::mpsc::unbounded_channel::<(&'static str, bool)>();
            let source = async_stream::stream! {
                while let Some((text, done)) = receiver.recv().await {
                    if done { signal.complete(); }
                    yield Ok::<_, std::convert::Infallible>(Bytes::from_static(text.as_bytes()));
                }
            };
            let mut response = Response::new(Body::from_stream(source));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            response.extensions_mut().insert(completion);
            let response =
                crate::server::payment::settle_axum_delegated_response(forward, response).await;
            let mut body = response.into_body().into_data_stream();
            sender.send(("data: partial\n\n", false)).unwrap();
            // An uncharged prefix is never released: a buyer could read it
            // and disconnect before the completion that would charge it.
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), body.next())
                    .await
                    .is_err(),
                "partial output is held until the charge commits"
            );
            assert_eq!(
                store
                    .get_channel(&channel)
                    .await
                    .unwrap()
                    .unwrap()
                    .cumulative,
                0
            );
            match end {
                "complete" => {
                    sender.send(("data: [DONE]\n\n", true)).unwrap();
                    assert_eq!(body.next().await.unwrap().unwrap(), "data: partial\n\n");
                    assert_eq!(body.next().await.unwrap().unwrap(), "data: [DONE]\n\n");
                    assert_eq!(
                        store
                            .get_channel(&channel)
                            .await
                            .unwrap()
                            .unwrap()
                            .cumulative,
                        20_000
                    );
                }
                "fail" => {
                    sender
                        .send(("data: {\"error\":\"worker_failed\"}\n\n", false))
                        .unwrap();
                    drop(sender);
                    // The handler's own failure answer is released uncharged.
                    assert_eq!(body.next().await.unwrap().unwrap(), "data: partial\n\n");
                    assert!(body.next().await.unwrap().is_ok());
                    assert!(body.next().await.is_none());
                }
                _ => {}
            }
            drop(body);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if session
                        .reserve_delegated_capacity(&channel, 20_000)
                        .await
                        .unwrap()
                        .is_some()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("capacity released on every terminal path");
            assert_eq!(
                store
                    .get_channel(&channel)
                    .await
                    .unwrap()
                    .unwrap()
                    .cumulative,
                if end == "complete" { 20_000 } else { 0 }
            );
            assert_eq!(*outcomes.lock().unwrap(), vec![end == "complete"]);
        }
    }

    #[test]
    fn metered_settlement_reports_committed_usage_once_on_last_drop() {
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let recorded = outcomes.clone();
        let observer = MeteredSettlement::with_callback(move |usd| {
            recorded.lock().unwrap().push(usd);
        });
        let meter = observer.clone();
        meter.record(150_000, 6);
        meter.record(1, 6);
        drop(observer);
        assert!(outcomes.lock().unwrap().is_empty(), "a clone still owns it");
        drop(meter);
        assert_eq!(*outcomes.lock().unwrap(), vec![0.150_001]);

        let recorded = outcomes.clone();
        drop(MeteredSettlement::with_callback(move |usd| {
            recorded.lock().unwrap().push(usd);
        }));
        assert_eq!(*outcomes.lock().unwrap(), vec![0.150_001, 0.0]);
    }

    #[tokio::test]
    async fn cancellation_during_earned_commit_does_not_cancel_settlement() {
        let (outcome, receiver) = oneshot::channel::<Outcome>();
        let (entered, waiting) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let (committed, finished) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (success, _, completion) = receiver.await.unwrap();
            assert!(success);
            entered.send(()).unwrap();
            paused.await.unwrap();
            completion.unwrap().settled(true);
            committed.send(()).unwrap();
            Ok(None)
        });
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let recorded = outcomes.clone();
        let completion = StreamCompletion::with_settlement_callback(move |paid| {
            recorded.lock().unwrap().push(paid);
        });
        let signal = completion.clone();
        let source = futures_util::stream::once(async move {
            signal.complete();
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: [DONE]\n\n"))
        });
        let response = response(
            Response::new(Body::from_stream(source)),
            completion,
            DeliveryTask { outcome, task },
            false,
        );
        let buyer = tokio::spawn(axum::body::to_bytes(response.into_body(), 1024));
        waiting.await.unwrap();
        assert!(
            !buyer.is_finished(),
            "terminal success waits for settlement"
        );
        buyer.abort();
        assert!(buyer.await.unwrap_err().is_cancelled());
        assert!(
            outcomes.lock().unwrap().is_empty(),
            "earned commit still owns the reservation"
        );
        resume.send(()).unwrap();
        finished
            .await
            .expect("earned commit outlives cancelled request");
        assert_eq!(*outcomes.lock().unwrap(), vec![true]);
    }

    #[tokio::test]
    async fn settlement_failure_never_releases_terminal_success() {
        let (outcome, receiver) = oneshot::channel::<Outcome>();
        let task = tokio::spawn(async move {
            assert!(receiver.await.unwrap().0);
            Err("store unavailable".into())
        });
        let completion = StreamCompletion::default();
        let signal = completion.clone();
        let source = futures_util::stream::once(async move {
            signal.complete();
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"data: [DONE]\n\n"))
        });
        let response = response(
            Response::new(Body::from_stream(source)),
            completion,
            DeliveryTask { outcome, task },
            false,
        );
        assert!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .is_err()
        );
    }
}
