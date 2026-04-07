//! Axum handlers for the debugger API.

use std::collections::HashMap;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use tokio::sync::broadcast;

use crate::PdbState;
use crate::types::{LogEntry, SseMessage};

/// SSE stream of flow events (`/__debugger/logs/stream`).
pub async fn sse_stream(State(state): State<PdbState>) -> Response {
    let mut rx = state.tx.subscribe();

    let snapshot = {
        let engine = state.correlation.lock().unwrap();
        engine.snapshot()
    };

    let init_data = serde_json::to_string(&SseMessage::Init {
        viewer_ip: "unknown".into(),
    })
    .unwrap();
    let snapshot_data = serde_json::to_string(&SseMessage::Snapshot { flows: snapshot }).unwrap();

    let stream = async_stream::stream! {
        // Initial events
        yield Ok::<_, std::convert::Infallible>(format!("data: {init_data}\n\n"));
        yield Ok(format!("data: {snapshot_data}\n\n"));

        // Live events
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    let data = serde_json::to_string(&msg).unwrap();
                    yield Ok(format!("data: {data}\n\n"));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// JSON snapshot of all flows (`/__debugger/logs`).
pub async fn logs_snapshot(State(state): State<PdbState>) -> impl IntoResponse {
    let engine = state.correlation.lock().unwrap();
    Json(engine.snapshot())
}

/// Sidebar config (`/__debugger/api/config`).
pub async fn config_handler(State(state): State<PdbState>) -> impl IntoResponse {
    Json(state.config.clone())
}

/// Debug: inject a fake MPP flow with splits, for UI testing.
/// `POST /__debugger/debug/fake-flow`
pub async fn inject_fake_flow(State(state): State<PdbState>) -> impl IntoResponse {
    let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Build a fake MPP challenge payload with splits.
    // Total: 10 USDC = 10_000_000 (6 decimals)
    // Primary gets the remainder after all splits
    let challenge_json = serde_json::json!({
        "amount": "10000000",
        "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        "recipient": "BQG92Aos6y8A78BkWN33SzM5T5Q1t98wKj2rxaWz1t7Y",
        "methodDetails": {
            "decimals": 6,
            "feePayer": true,
            "feePayerKey": "4FEnLHBHFeePayerxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "network": "mainnet-beta",
            "recentBlockhash": "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x",
            "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "splits": [
                { "recipient": "CreatorxWalletxxxxxxxxxxxxxxxxxxxxxxxxxx1", "amount": "1500000", "label": "Creator", "memo": "Creator royalty" },
                { "recipient": "PlatformxFeexxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "amount": "500000", "label": "Platform", "memo": "Platform fee" },
                { "recipient": "TreasuryxDAOxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "amount": "300000", "label": "DAO Treasury", "memo": "DAO treasury contribution" },
                { "recipient": "ReferrerxWalletxxxxxxxxxxxxxxxxxxxxxxxxxxx", "amount": "200000", "label": "Referrer", "memo": "Referral bonus" },
                { "recipient": "InsurancexFundxxxxxxxxxxxxxxxxxxxxxxxxxxx1", "amount": "100000", "label": "Insurance Fund", "memo": "Insurance reserve" },
                { "recipient": "MarketingxPoolxxxxxxxxxxxxxxxxxxxxxxxxxxx1", "amount": "100000", "label": "Marketing", "memo": "Marketing pool" },
                { "recipient": "BurnxAddressxxxxxxxxxxxxxxxxxxxxxxxxxxxxx1", "amount": "50000", "label": "Burn", "memo": "Token burn" }
            ]
        }
    });
    let challenge_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_json.to_string());
    let www_auth = format!(
        "Payment id=\"fake-challenge-id\", realm=\"MPP Payment\", method=\"solana\", intent=\"charge\", request=\"{challenge_b64}\", expires=\"{ts}\""
    );

    let mut res_headers = HashMap::new();
    res_headers.insert("www-authenticate".to_string(), www_auth);
    res_headers.insert("content-type".to_string(), "application/json".to_string());

    let challenge_entry = LogEntry {
        id: state.next_log_id(),
        ts: ts.clone(),
        method: "POST".to_string(),
        path: "/api/v1/mint-nft".to_string(),
        status: 402,
        ms: 12,
        req_headers: HashMap::new(),
        res_headers,
        res_body: None,
        client_ip: "192.168.1.42".to_string(),
    };

    // Ingest challenge, then a matching successful retry.
    {
        let mut engine = state.correlation.lock().unwrap();
        engine.ingest(challenge_entry);

        let mut retry_req = HashMap::new();
        retry_req.insert(
            "authorization".to_string(),
            "Payment <signed-tx>".to_string(),
        );
        let mut retry_res = HashMap::new();
        retry_res.insert(
            "payment-receipt".to_string(),
            "eyJjaGFsbGVuZ2VJZCI6ImZha2UtY2hhbGxlbmdlLWlkIiwic3RhdHVzIjoic3VjY2VzcyJ9".to_string(),
        );
        let retry_entry = LogEntry {
            id: state.next_log_id(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            method: "POST".to_string(),
            path: "/api/v1/mint-nft".to_string(),
            status: 200,
            ms: 842,
            req_headers: retry_req,
            res_headers: retry_res,
            res_body: Some(r#"{"mint":"FakeNFTMintxxxxxxxxxxxxxxxxxxxxxxxx"}"#.to_string()),
            client_ip: "192.168.1.42".to_string(),
        };
        engine.ingest(retry_entry);
    }

    // Also inject a failed flow
    {
        let failed_challenge = serde_json::json!({
            "amount": "500000",
            "currency": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "recipient": "FailedRecipientxxxxxxxxxxxxxxxxxxxxxxxx1",
            "methodDetails": {
                "decimals": 6,
                "feePayer": true,
                "feePayerKey": "4FEnLHBHFeePayerxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "network": "mainnet-beta",
                "recentBlockhash": "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x",
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
            }
        });
        let failed_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(failed_challenge.to_string());
        let failed_auth = format!(
            "Payment id=\"failed-challenge\", realm=\"MPP Payment\", method=\"solana\", intent=\"charge\", request=\"{failed_b64}\", expires=\"{ts}\""
        );
        let mut failed_res_headers = HashMap::new();
        failed_res_headers.insert("www-authenticate".to_string(), failed_auth);
        failed_res_headers.insert("content-type".to_string(), "application/json".to_string());

        let mut engine = state.correlation.lock().unwrap();

        // 402 challenge
        engine.ingest(LogEntry {
            id: state.next_log_id(),
            ts: ts.clone(),
            method: "POST".to_string(),
            path: "/api/v1/transfer".to_string(),
            status: 402,
            ms: 8,
            req_headers: HashMap::new(),
            res_headers: failed_res_headers,
            res_body: None,
            client_ip: "10.0.0.5".to_string(),
        });

        // Failed retry — 500 from upstream
        let mut retry_req = HashMap::new();
        retry_req.insert(
            "authorization".to_string(),
            "Payment <bad-signature>".to_string(),
        );
        engine.ingest(LogEntry {
            id: state.next_log_id(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            method: "POST".to_string(),
            path: "/api/v1/transfer".to_string(),
            status: 500,
            ms: 1204,
            req_headers: retry_req,
            res_headers: HashMap::new(),
            res_body: Some(
                r#"{"error":"Transaction simulation failed: insufficient funds for fee"}"#
                    .to_string(),
            ),
            client_ip: "10.0.0.5".to_string(),
        });
    }

    Json(serde_json::json!({"status": "ok", "message": "Fake flows injected (success + failure)"}))
}
