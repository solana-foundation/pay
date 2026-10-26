//! What an unpaid request to a sell endpoint costs before the payment gate.
//! Its own test binary: it measures the process heap.
#![cfg(feature = "sell")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use pay_connect::sell::routes::{Creator, CreatorAuth, CreatorError};
use pay_connect::sell::{Operator, SellState, router};
use tower::ServiceExt;

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

struct Seller;

impl CreatorAuth for Seller {
    fn authenticate(&self, _: &str) -> Result<Creator, CreatorError> {
        Ok(Creator {
            subject: "sub_seller".into(),
            wallet: "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU".into(),
        })
    }
}

fn operator() -> Operator {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
    let mut keypair = [0u8; 64];
    keypair[..32].copy_from_slice(sk.as_bytes());
    keypair[32..].copy_from_slice(sk.verifying_key().as_bytes());
    let signer = pay_kit::solana_keychain::MemorySigner::from_bytes(&keypair).unwrap();
    let operator = Operator::new(
        Arc::new(signer),
        "http://127.0.0.1:1",
        "0123456789abcdef0123456789abcdef",
    );
    operator
        .blockhashes
        .set("11111111111111111111111111111111".into(), 1_000, 1);
    operator
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn an_unpaid_chat_request_is_not_built_before_the_gate() {
    let app = router(Arc::new(SellState::new(
        Arc::new(operator()),
        "https://connect.test",
        Arc::new(Seller),
    )));
    let create = serde_json::json!({
        "pricing": { "per_request_usd": 0.02 }, "model": "agent", "network": "devnet",
        "earn_cap_usd": 1.0,
    });
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/endpoints")
                .header(header::AUTHORIZATION, "Bearer seller")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let view: serde_json::Value =
        serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap()).unwrap();
    let path = format!(
        "/endpoints/{}/v1/chat/completions",
        view["id"].as_str().unwrap()
    );

    // 4 MiB of the cheapest JSON values, the shape that costs a parser most.
    let mut body = String::from(r#"{"model":"agent","stream":true,"messages":["#);
    while body.len() < 4 * 1024 * 1024 {
        body.push_str("0,");
    }
    body.push_str("0]}");
    let body = Bytes::from(body);
    let size = body.len();

    let unpaid = |body: Bytes| {
        Request::builder()
            .method(Method::POST)
            .uri(&path)
            .header(header::HOST, "connect.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    // Warm up lazily built state so only the request itself is measured.
    let warm = app.clone().oneshot(unpaid(Bytes::from_static(b"{}"))).await;
    drop(warm);

    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let response = app.clone().oneshot(unpaid(body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    drop(response);
    let grown = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert!(
        grown < size,
        "an unpaid {size}-byte body grew the heap by {grown} bytes"
    );
}
