//! Embedded Payment Debugger — static UI + live flow tracking.
//!
//! The `pdb/dist` directory is compiled into the binary at build time
//! via `include_dir!`. The correlation engine tracks payment flows and
//! broadcasts them to connected SSE clients.

pub mod correlation;
pub mod handlers;
pub mod logging;
pub mod types;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode};
use axum::routing::get;
use include_dir::{Dir, include_dir};
use mime_guess::from_path;
use tokio::sync::broadcast;

use correlation::FlowCorrelation;
use types::SseMessage;

/// Mount path for the PDB debugger UI (no trailing slash).
pub const PDB_PATH: &str = "/__402/pdb";

static ASSETS: Dir<'_> = include_dir!("$OUT_DIR/pdb-dist");

/// Shared state for the PDB debugger.
#[derive(Clone)]
pub struct PdbState {
    pub correlation: Arc<Mutex<FlowCorrelation>>,
    pub tx: broadcast::Sender<SseMessage>,
    pub config: serde_json::Value,
    log_id: Arc<AtomicU64>,
}

impl PdbState {
    pub fn new(config: serde_json::Value) -> Self {
        let (tx, _) = broadcast::channel(256);
        let correlation = FlowCorrelation::new(tx.clone());
        Self {
            correlation: Arc::new(Mutex::new(correlation)),
            tx,
            config,
            log_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn next_log_id(&self) -> u64 {
        self.log_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Spawn the background cleanup task (call once at startup).
    pub fn spawn_cleanup(&self) {
        let correlation = self.correlation.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                correlation.lock().unwrap().cleanup();
            }
        });
    }
}

/// Returns an axum Router serving the complete debugger:
/// - `/logs/stream` — SSE flow events
/// - `/logs` — JSON flow snapshot
/// - `/api/config` — sidebar config
/// - `/*` (fallback) — embedded SPA static files
pub fn debugger_router(state: PdbState) -> Router {
    Router::new()
        .route("/logs/stream", get(handlers::sse_stream))
        .route("/logs", get(handlers::logs_snapshot))
        .route("/api/config", get(handlers::config_handler))
        // Explicit `/` route so axum matches `/__402/pdb/` (with trailing
        // slash) inside the nest.  Without this, `/__402/pdb/` falls through
        // to the outer router's fallback, and relative `./assets/…` paths in
        // index.html break because the browser resolves them against the
        // wrong base.
        .route("/", get(serve_index))
        // .route("/debug/fake-flow", axum::routing::post(handlers::inject_fake_flow))
        .fallback(get(serve_pdb))
        .with_state(state)
}

/// Serve index.html directly (used for the explicit `/` route).
async fn serve_index() -> Response<Body> {
    match ASSETS.get_file("index.html") {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html")
            .header("Cache-Control", "no-cache")
            .body(Body::from(file.contents().to_vec()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

/// Axum handler that serves the embedded PDB static files.
async fn serve_pdb(req: Request) -> Response<Body> {
    let path = req.uri().path().trim_start_matches('/');

    let file = if path.is_empty() {
        ASSETS.get_file("index.html")
    } else {
        ASSETS
            .get_file(path)
            .or_else(|| ASSETS.get_file(format!("{path}.html")))
            .or_else(|| ASSETS.get_file(format!("{path}/index.html")))
            .or_else(|| ASSETS.get_file("index.html"))
    };

    match file {
        Some(file) => {
            let mime = from_path(file.path()).first_or_octet_stream();
            let cache = if mime.type_() == mime_guess::mime::TEXT
                && mime.subtype() == mime_guess::mime::HTML
            {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", mime.as_ref())
                .header("Cache-Control", cache)
                .body(Body::from(file.contents().to_vec()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}
