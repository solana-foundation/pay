//! Axum handlers for the debugger API.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;

use crate::PdbState;
use crate::types::SseMessage;

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
