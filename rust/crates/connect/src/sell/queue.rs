//! Requests parked for a seller's worker.
//!
//! The public handler parks a paid chat-completion request and waits. The
//! worker long-polls [`EndpointQueue::next`], claims it, and streams the
//! agent's answer back with [`EndpointQueue::push`] until
//! [`EndpointQueue::complete`] or [`EndpointQueue::fail`]. Each parked
//! request is a channel of [`Event`]s from the worker to the waiting client;
//! a client that goes away closes it, and the worker's next push says so.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use pay_core::sell_inference::SellPricing;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

/// Most unclaimed requests one endpoint holds; beyond it, callers get 503.
pub const MAX_WAITING: usize = 64;
/// Most pushed bytes one request holds before its buyer reads them.
pub const MAX_BUFFERED_PER_REQUEST: usize = 1024 * 1024;
/// Most pushed bytes all of one endpoint's requests hold together.
pub const MAX_BUFFERED_PER_ENDPOINT: usize = 4 * 1024 * 1024;
/// Charged per frame on top of its bytes, for the event around it.
const FRAME_OVERHEAD: usize = 64;

/// The terms a request was admitted under. Pricing can change while a
/// request waits; what the buyer paid for is what the endpoint earns.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub pricing: SellPricing,
    /// The model the buyer asked for, from the request body.
    pub model: Option<String>,
    /// The most this request can earn: the flat price, or the per-token
    /// ceiling. Reserved against the earn cap until the request finishes.
    pub expected_usd: f64,
}

/// What the client asked, as the worker receives it.
#[derive(Debug, Clone)]
pub struct ParkedRequest {
    pub id: String,
    /// The OpenAI chat-completion request body, verbatim.
    pub body: Bytes,
    /// Whether the client asked for SSE.
    pub stream: bool,
    pub parked_at: Instant,
    pub admission: Admission,
}

/// How a claimed request ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub admission: Admission,
    /// Streaming requests may keep resource accounting owned by the response.
    pub stream: bool,
    /// Whether the buyer was still there to receive the final event.
    pub delivered: bool,
}

/// Result of one worker poll. Requests whose buyers left before claim retain
/// their delivery mode so the endpoint can identify the reservation owner.
#[derive(Debug, Clone)]
pub struct Next {
    pub request: Option<ParkedRequest>,
    pub abandoned: Vec<ParkedRequest>,
}

/// One push of SSE `data:` lines. It holds its share of the request's and
/// the endpoint's buffer budget until the buyer's stream takes it.
#[derive(Debug)]
pub struct Frame {
    bytes: Bytes,
    _budget: (OwnedSemaphorePermit, OwnedSemaphorePermit),
}

impl Frame {
    /// One `data: <chunk>\n\n` line per chunk. JSON outside strings may
    /// carry raw line breaks, which would split an SSE line; inside strings
    /// they are always escaped, so they become spaces.
    fn sse(chunks: &[&RawValue]) -> Bytes {
        let len = chunks.iter().map(|c| c.get().len() + 8).sum();
        let mut out = Vec::with_capacity(len);
        for chunk in chunks {
            out.extend_from_slice(b"data: ");
            out.extend(chunk.get().bytes().map(|b| match b {
                b'\n' | b'\r' => b' ',
                b => b,
            }));
            out.extend_from_slice(b"\n\n");
        }
        Bytes::from(out)
    }

    /// The bytes for the buyer; the budget they held is released.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

/// One step of the worker's answer.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// A worker took the request.
    Claimed,
    /// Pushed `chat.completion.chunk`s (streaming) or ignored (non-streaming).
    Chunk(Frame),
    /// The end. Streaming: the final chunk (usually carrying `usage`) if
    /// any, then `[DONE]`. Non-streaming: the whole `chat.completion`.
    Complete(Option<Value>),
    /// The worker gave up.
    Fail { status: u16, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    #[error("too many requests waiting for this endpoint")]
    Full,
    #[error("unknown or finished request")]
    Unknown,
    #[error("the client is no longer waiting")]
    Gone,
    #[error("the buyer has not read what was already pushed; retry shortly")]
    Backlogged,
    #[error("one push may carry at most {MAX_BUFFERED_PER_REQUEST} bytes of chunks")]
    TooLarge,
}

struct Parked {
    request: ParkedRequest,
    tx: mpsc::UnboundedSender<Event>,
    /// Bytes this request's buyer has yet to read; see [`Frame`].
    buffered: Arc<Semaphore>,
}

#[derive(Default)]
struct Inner {
    waiting: VecDeque<Parked>,
    claimed: HashMap<String, Parked>,
}

/// The queue for one endpoint.
pub struct EndpointQueue {
    inner: Mutex<Inner>,
    /// One permit per parked request, so a waiting `next` wakes once each.
    parked: Notify,
    /// Bytes all of this endpoint's buyers have yet to read.
    buffered: Arc<Semaphore>,
}

impl Default for EndpointQueue {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            parked: Notify::new(),
            buffered: Arc::new(Semaphore::new(MAX_BUFFERED_PER_ENDPOINT)),
        }
    }
}

impl EndpointQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Park a request; the receiver yields the worker's events.
    pub fn park(
        &self,
        body: Bytes,
        stream: bool,
        admission: Admission,
    ) -> Result<(String, mpsc::UnboundedReceiver<Event>), QueueError> {
        let mut inner = self.lock();
        if inner.waiting.len() >= MAX_WAITING {
            return Err(QueueError::Full);
        }
        let id = format!("req_{}", uuid::Uuid::new_v4().simple());
        let (tx, rx) = mpsc::unbounded_channel();
        inner.waiting.push_back(Parked {
            request: ParkedRequest {
                id: id.clone(),
                body,
                stream,
                parked_at: Instant::now(),
                admission,
            },
            tx,
            buffered: Arc::new(Semaphore::new(MAX_BUFFERED_PER_REQUEST)),
        });
        drop(inner);
        self.parked.notify_one();
        Ok((id, rx))
    }

    /// The client stopped waiting before a worker claimed the request.
    /// Returns what it was admitted under, so the reservation can go.
    pub fn abandon(&self, id: &str) -> Option<Admission> {
        let mut inner = self.lock();
        let waiting = inner.waiting.iter().position(|p| p.request.id == id);
        let parked = match waiting {
            Some(index) => inner.waiting.remove(index),
            None => inner.claimed.remove(id),
        };
        parked.map(|p| p.request.admission)
    }

    /// Claim the oldest waiting request, waiting up to `timeout` for one.
    pub async fn next(&self, timeout: Duration) -> Next {
        let deadline = Instant::now() + timeout;
        loop {
            let mut abandoned = Vec::new();
            {
                let mut inner = self.lock();
                while let Some(mut parked) = inner.waiting.pop_front() {
                    // A client that left before being claimed is skipped.
                    if parked.tx.send(Event::Claimed).is_err() {
                        abandoned.push(parked.request);
                        continue;
                    }
                    let request = parked.request.clone();
                    // The worker has the body now; a claimed request that
                    // lingers must not keep it.
                    parked.request.body = Bytes::new();
                    inner.claimed.insert(request.id.clone(), parked);
                    return Next {
                        request: Some(request),
                        abandoned,
                    };
                }
            }
            if !abandoned.is_empty() {
                return Next {
                    request: None,
                    abandoned,
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Next {
                    request: None,
                    abandoned: Vec::new(),
                };
            }
            if tokio::time::timeout(remaining, self.parked.notified())
                .await
                .is_err()
            {
                return Next {
                    request: None,
                    abandoned: Vec::new(),
                };
            }
        }
    }

    /// Append chunks to a claimed request. A buyer that does not read
    /// leaves them buffered, so pushes fail with [`QueueError::Backlogged`]
    /// once the request's or the endpoint's budget is spent.
    pub fn push(&self, id: &str, chunks: &[&RawValue]) -> Result<(), QueueError> {
        let bytes = Frame::sse(chunks);
        let cost = bytes.len() + FRAME_OVERHEAD;
        if cost > MAX_BUFFERED_PER_REQUEST {
            return Err(QueueError::TooLarge);
        }
        let cost = u32::try_from(cost).map_err(|_| QueueError::TooLarge)?;
        let inner = self.lock();
        let parked = inner.claimed.get(id).ok_or(QueueError::Unknown)?;
        if parked.tx.is_closed() {
            return Err(QueueError::Gone);
        }
        if chunks.is_empty() {
            return Ok(());
        }
        let request = parked
            .buffered
            .clone()
            .try_acquire_many_owned(cost)
            .map_err(|_| QueueError::Backlogged)?;
        let endpoint = self
            .buffered
            .clone()
            .try_acquire_many_owned(cost)
            .map_err(|_| QueueError::Backlogged)?;
        parked
            .tx
            .send(Event::Chunk(Frame {
                bytes,
                _budget: (request, endpoint),
            }))
            .map_err(|_| QueueError::Gone)
    }

    /// Finish a claimed request.
    pub fn complete(&self, id: &str, final_event: Option<Value>) -> Result<Completion, QueueError> {
        self.finish(id, Event::Complete(final_event))
    }

    /// Abort a claimed request with an error for the client.
    pub fn fail(
        &self,
        id: &str,
        status: u16,
        message: impl Into<String>,
    ) -> Result<Completion, QueueError> {
        self.finish(
            id,
            Event::Fail {
                status,
                message: message.into(),
            },
        )
    }

    fn finish(&self, id: &str, event: Event) -> Result<Completion, QueueError> {
        let mut inner = self.lock();
        let parked = inner.claimed.remove(id).ok_or(QueueError::Unknown)?;
        let delivered = parked.tx.send(event).is_ok();
        Ok(Completion {
            admission: parked.request.admission,
            stream: parked.request.stream,
            delivered,
        })
    }

    /// (waiting, claimed) counts.
    pub fn depth(&self) -> (usize, usize) {
        let inner = self.lock();
        (inner.waiting.len(), inner.claimed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(values: &[Value]) -> Vec<Box<RawValue>> {
        values
            .iter()
            .map(|v| serde_json::value::to_raw_value(v).unwrap())
            .collect()
    }

    fn push(queue: &EndpointQueue, id: &str, values: &[Value]) -> Result<(), QueueError> {
        let raw = raw(values);
        let chunks: Vec<&RawValue> = raw.iter().map(|r| &**r).collect();
        queue.push(id, &chunks)
    }

    fn frame(rx: &mut mpsc::UnboundedReceiver<Event>) -> Bytes {
        match rx.try_recv() {
            Ok(Event::Chunk(frame)) => frame.into_bytes(),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    fn admission() -> Admission {
        Admission {
            pricing: SellPricing::PerRequest { usd: 0.02 },
            model: Some("agent".into()),
            expected_usd: 0.02,
        }
    }

    #[tokio::test]
    async fn a_worker_claims_streams_and_completes_in_order() {
        let queue = EndpointQueue::default();
        let (id, mut rx) = queue
            .park(Bytes::from_static(b"{}"), true, admission())
            .unwrap();
        assert_eq!(queue.depth(), (1, 0));

        let claimed = queue.next(Duration::from_secs(1)).await.request.unwrap();
        assert_eq!(claimed.id, id);
        assert!(claimed.stream);
        assert_eq!(claimed.admission, admission());
        assert_eq!(queue.depth(), (0, 1));
        assert_eq!(rx.recv().await, Some(Event::Claimed));

        push(&queue, &id, &[json!({"n": 1}), json!({"n": 2})]).unwrap();
        let completion = queue.complete(&id, Some(json!({"usage": {}}))).unwrap();
        assert!(completion.delivered);
        assert_eq!(completion.admission, admission());
        assert_eq!(frame(&mut rx), "data: {\"n\":1}\n\ndata: {\"n\":2}\n\n");
        assert_eq!(
            rx.recv().await,
            Some(Event::Complete(Some(json!({"usage": {}}))))
        );
        assert_eq!(rx.recv().await, None, "the channel closes after completion");
        assert_eq!(queue.depth(), (0, 0));
        assert_eq!(push(&queue, &id, &[]), Err(QueueError::Unknown));
    }

    #[tokio::test]
    async fn next_waits_for_a_request_and_gives_up_on_time() {
        let queue = std::sync::Arc::new(EndpointQueue::default());
        assert!(
            queue
                .next(Duration::from_millis(20))
                .await
                .request
                .is_none()
        );

        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.next(Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (id, _rx) = queue.park(Bytes::new(), false, admission()).unwrap();
        assert_eq!(waiter.await.unwrap().request.unwrap().id, id);
    }

    #[tokio::test]
    async fn a_departed_client_is_skipped_and_reported() {
        let queue = EndpointQueue::default();
        let (gone, rx) = queue.park(Bytes::new(), false, admission()).unwrap();
        drop(rx);
        let (live, mut live_rx) = queue.park(Bytes::new(), false, admission()).unwrap();

        let next = queue.next(Duration::from_secs(1)).await;
        assert_eq!(next.abandoned.len(), 1);
        assert_eq!(next.abandoned[0].admission, admission());
        let claimed = next.request.unwrap();
        assert_eq!(
            claimed.id, live,
            "the abandoned request is never handed out"
        );
        assert_eq!(push(&queue, &gone, &[]), Err(QueueError::Unknown));

        drop(live_rx.recv().await); // Claimed
        drop(live_rx);
        assert_eq!(
            push(&queue, &live, &[json!(1)]),
            Err(QueueError::Gone),
            "pushing to a client that left says so"
        );
        // Finishing still clears the slot and says the buyer never saw it.
        let completion = queue.fail(&live, 502, "x").unwrap();
        assert!(!completion.delivered);
        assert_eq!(queue.depth(), (0, 0));
    }

    async fn claimed(queue: &EndpointQueue) -> (String, mpsc::UnboundedReceiver<Event>) {
        let (id, mut rx) = queue.park(Bytes::new(), true, admission()).unwrap();
        queue.next(Duration::from_secs(1)).await.request.unwrap();
        assert_eq!(rx.recv().await, Some(Event::Claimed));
        (id, rx)
    }

    #[tokio::test]
    async fn an_unread_request_buffers_at_most_its_budget() {
        let queue = EndpointQueue::default();
        let (id, mut rx) = claimed(&queue).await;
        let chunk = json!({ "content": "x".repeat(64 * 1024) });
        let mut accepted = 0;
        while push(&queue, &id, std::slice::from_ref(&chunk)).is_ok() {
            accepted += 1;
            assert!(accepted <= MAX_BUFFERED_PER_REQUEST / (64 * 1024));
        }
        assert_eq!(
            push(&queue, &id, std::slice::from_ref(&chunk)),
            Err(QueueError::Backlogged)
        );
        // Reading a frame frees its share; the next push fits again.
        drop(frame(&mut rx));
        push(&queue, &id, std::slice::from_ref(&chunk)).unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_buffers_at_most_its_budget_across_requests() {
        let queue = EndpointQueue::default();
        let chunk = json!({ "content": "x".repeat(MAX_BUFFERED_PER_REQUEST - 1024) });
        let mut receivers = Vec::new();
        for _ in 0..MAX_BUFFERED_PER_ENDPOINT / MAX_BUFFERED_PER_REQUEST {
            let (id, rx) = claimed(&queue).await;
            push(&queue, &id, std::slice::from_ref(&chunk)).unwrap();
            receivers.push(rx);
        }
        let (id, _rx) = claimed(&queue).await;
        assert_eq!(
            push(&queue, &id, std::slice::from_ref(&chunk)),
            Err(QueueError::Backlogged),
            "a fresh request cannot buffer past the endpoint's budget"
        );
        drop(receivers.pop());
        push(&queue, &id, std::slice::from_ref(&chunk)).unwrap();
    }

    #[tokio::test]
    async fn pushes_are_bounded_single_line_frames() {
        let queue = EndpointQueue::default();
        let (id, mut rx) = claimed(&queue).await;
        let huge = json!({ "content": "x".repeat(MAX_BUFFERED_PER_REQUEST) });
        assert_eq!(push(&queue, &id, &[huge]), Err(QueueError::TooLarge));

        let pretty =
            serde_json::value::RawValue::from_string("{\n  \"a\": \"b\\nc\"\r\n}".into()).unwrap();
        queue.push(&id, &[&pretty]).unwrap();
        assert_eq!(frame(&mut rx), "data: {   \"a\": \"b\\nc\"  }\n\n");
    }

    #[tokio::test]
    async fn a_claimed_request_drops_its_body() {
        let queue = EndpointQueue::default();
        let body = Bytes::from(vec![b' '; 1024]);
        let (_id, _rx) = queue.park(body.clone(), true, admission()).unwrap();
        let claimed = queue.next(Duration::from_secs(1)).await.request.unwrap();
        drop(claimed);
        assert!(
            body.is_unique(),
            "only the worker's copy held the body, and it is gone"
        );
    }

    #[tokio::test]
    async fn abandon_removes_a_waiting_request_and_the_queue_is_bounded() {
        let queue = EndpointQueue::default();
        let (id, _rx) = queue.park(Bytes::new(), false, admission()).unwrap();
        assert_eq!(queue.abandon(&id), Some(admission()));
        assert_eq!(queue.abandon(&id), None);
        assert_eq!(queue.depth(), (0, 0));

        let mut receivers = Vec::new();
        for _ in 0..MAX_WAITING {
            receivers.push(queue.park(Bytes::new(), false, admission()).unwrap());
        }
        assert_eq!(
            queue.park(Bytes::new(), false, admission()).map(|_| ()),
            Err(QueueError::Full)
        );
    }
}
