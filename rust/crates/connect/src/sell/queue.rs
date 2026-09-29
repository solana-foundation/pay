//! Requests parked for a seller's worker.
//!
//! The public handler parks a paid chat-completion request and waits. The
//! worker long-polls [`EndpointQueue::next`], claims it, and streams the
//! agent's answer back with [`EndpointQueue::push`] until
//! [`EndpointQueue::complete`] or [`EndpointQueue::fail`]. Each parked
//! request is a channel of [`Event`]s from the worker to the waiting client;
//! a client that goes away closes it, and the worker's next push says so.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};

/// Most unclaimed requests one endpoint holds; beyond it, callers get 503.
pub const MAX_WAITING: usize = 64;

/// What the client asked, as the worker receives it.
#[derive(Debug, Clone)]
pub struct ParkedRequest {
    pub id: String,
    /// The OpenAI chat-completion request body, verbatim.
    pub body: Bytes,
    /// Whether the client asked for SSE.
    pub stream: bool,
    pub parked_at: Instant,
}

/// One step of the worker's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A worker took the request.
    Claimed,
    /// One `chat.completion.chunk` (streaming) or ignored (non-streaming).
    Chunk(Value),
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
}

struct Parked {
    request: ParkedRequest,
    tx: mpsc::UnboundedSender<Event>,
}

#[derive(Default)]
struct Inner {
    waiting: VecDeque<Parked>,
    claimed: HashMap<String, Parked>,
}

/// The queue for one endpoint.
#[derive(Default)]
pub struct EndpointQueue {
    inner: Mutex<Inner>,
    /// One permit per parked request, so a waiting `next` wakes once each.
    parked: Notify,
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
            },
            tx,
        });
        drop(inner);
        self.parked.notify_one();
        Ok((id, rx))
    }

    /// The client stopped waiting before a worker claimed the request.
    pub fn abandon(&self, id: &str) {
        let mut inner = self.lock();
        inner.waiting.retain(|p| p.request.id != id);
        inner.claimed.remove(id);
    }

    /// Claim the oldest waiting request, waiting up to `timeout` for one.
    pub async fn next(&self, timeout: Duration) -> Option<ParkedRequest> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut inner = self.lock();
                while let Some(parked) = inner.waiting.pop_front() {
                    // A client that left before being claimed is skipped.
                    if parked.tx.send(Event::Claimed).is_err() {
                        continue;
                    }
                    let request = parked.request.clone();
                    inner.claimed.insert(request.id.clone(), parked);
                    return Some(request);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if tokio::time::timeout(remaining, self.parked.notified())
                .await
                .is_err()
            {
                return None;
            }
        }
    }

    /// Append chunks to a claimed request.
    pub fn push(&self, id: &str, chunks: Vec<Value>) -> Result<(), QueueError> {
        let inner = self.lock();
        let parked = inner.claimed.get(id).ok_or(QueueError::Unknown)?;
        for chunk in chunks {
            parked
                .tx
                .send(Event::Chunk(chunk))
                .map_err(|_| QueueError::Gone)?;
        }
        Ok(())
    }

    /// Finish a claimed request.
    pub fn complete(&self, id: &str, final_event: Option<Value>) -> Result<(), QueueError> {
        self.finish(id, Event::Complete(final_event))
    }

    /// Abort a claimed request with an error for the client.
    pub fn fail(
        &self,
        id: &str,
        status: u16,
        message: impl Into<String>,
    ) -> Result<(), QueueError> {
        self.finish(
            id,
            Event::Fail {
                status,
                message: message.into(),
            },
        )
    }

    fn finish(&self, id: &str, event: Event) -> Result<(), QueueError> {
        let mut inner = self.lock();
        let parked = inner.claimed.remove(id).ok_or(QueueError::Unknown)?;
        parked.tx.send(event).map_err(|_| QueueError::Gone)
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

    #[tokio::test]
    async fn a_worker_claims_streams_and_completes_in_order() {
        let queue = EndpointQueue::default();
        let (id, mut rx) = queue.park(Bytes::from_static(b"{}"), true).unwrap();
        assert_eq!(queue.depth(), (1, 0));

        let claimed = queue.next(Duration::from_secs(1)).await.unwrap();
        assert_eq!(claimed.id, id);
        assert!(claimed.stream);
        assert_eq!(queue.depth(), (0, 1));
        assert_eq!(rx.recv().await, Some(Event::Claimed));

        queue
            .push(&id, vec![json!({"n": 1}), json!({"n": 2})])
            .unwrap();
        queue.complete(&id, Some(json!({"usage": {}}))).unwrap();
        assert_eq!(rx.recv().await, Some(Event::Chunk(json!({"n": 1}))));
        assert_eq!(rx.recv().await, Some(Event::Chunk(json!({"n": 2}))));
        assert_eq!(
            rx.recv().await,
            Some(Event::Complete(Some(json!({"usage": {}}))))
        );
        assert_eq!(rx.recv().await, None, "the channel closes after completion");
        assert_eq!(queue.depth(), (0, 0));
        assert_eq!(queue.push(&id, vec![]), Err(QueueError::Unknown));
    }

    #[tokio::test]
    async fn next_waits_for_a_request_and_gives_up_on_time() {
        let queue = std::sync::Arc::new(EndpointQueue::default());
        assert!(queue.next(Duration::from_millis(20)).await.is_none());

        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.next(Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (id, _rx) = queue.park(Bytes::new(), false).unwrap();
        assert_eq!(waiter.await.unwrap().unwrap().id, id);
    }

    #[tokio::test]
    async fn a_departed_client_is_skipped_and_reported() {
        let queue = EndpointQueue::default();
        let (gone, rx) = queue.park(Bytes::new(), false).unwrap();
        drop(rx);
        let (live, mut live_rx) = queue.park(Bytes::new(), false).unwrap();

        let claimed = queue.next(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            claimed.id, live,
            "the abandoned request is never handed out"
        );
        assert_eq!(queue.push(&gone, vec![]), Err(QueueError::Unknown));

        drop(live_rx.recv().await); // Claimed
        drop(live_rx);
        assert_eq!(
            queue.push(&live, vec![json!(1)]),
            Err(QueueError::Gone),
            "pushing to a client that left says so"
        );
        // Finishing still clears the slot.
        assert_eq!(queue.fail(&live, 502, "x"), Err(QueueError::Gone));
        assert_eq!(queue.depth(), (0, 0));
    }

    #[tokio::test]
    async fn abandon_removes_a_waiting_request_and_the_queue_is_bounded() {
        let queue = EndpointQueue::default();
        let (id, _rx) = queue.park(Bytes::new(), false).unwrap();
        queue.abandon(&id);
        assert_eq!(queue.depth(), (0, 0));

        let mut receivers = Vec::new();
        for _ in 0..MAX_WAITING {
            receivers.push(queue.park(Bytes::new(), false).unwrap());
        }
        assert_eq!(
            queue.park(Bytes::new(), false).map(|_| ()),
            Err(QueueError::Full)
        );
    }
}
