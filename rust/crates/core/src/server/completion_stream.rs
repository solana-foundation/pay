//! Completion-billed live responses. Partial flat-price answers are not charged.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use axum::body::Body;
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

/// Relay live bytes and commit only at the handler's explicit completion.
/// The terminal success frame follows persistence, not merely a successful
/// HTTP status. Replay capture is bounded and does not delay intermediate data.
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
    let mut capture = capture_replay.then(Vec::new);
    let stream = async_stream::stream! {
        while let Some(chunk) = source.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = delivery.finish(false, None).await;
                    yield Err(std::io::Error::other(error));
                    return;
                }
            };
            if let Some(captured) = &mut capture {
                if captured.len().saturating_add(bytes.len()) <= gate::MAX_BATCH_CACHED_RESPONSE_BYTES {
                    captured.extend_from_slice(&bytes);
                } else {
                    // Existing replay behavior for oversized resources: return
                    // the recorded payment result, never execute the seller twice.
                    capture = None;
                }
            }
            if completion.completed() {
                let cached = capture.map(|bytes| gate::batch_cached_response(status, &headers, &bytes));
                match delivery.finish_observed(true, cached, Some(completion)).await {
                    Ok(_) => yield Ok(bytes),
                    Err(error) => yield Err(std::io::Error::other(error)),
                }
                return;
            }
            yield Ok(bytes);
        }
        // EOF without explicit completion is not a successful answer.
        let _ = delivery.finish(false, None).await;
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
    async fn flat_session_stream_is_live_and_only_explicit_completion_charges() {
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
            assert_eq!(body.next().await.unwrap().unwrap(), "data: partial\n\n");
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
