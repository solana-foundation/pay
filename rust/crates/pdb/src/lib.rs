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

use correlation::{CorrelationMode, FlowCorrelation};
use types::{ExchangeStart, InferenceInfo, ProviderSummary, SseMessage};

/// Mount path for the PDB debugger UI (no trailing slash).
pub const PDB_PATH: &str = "/__402/pdb";

static ASSETS: Dir<'_> = include_dir!("$OUT_DIR/pdb-dist");

/// Shared state for the PDB debugger.
#[derive(Clone)]
pub struct PdbState {
    pub correlation: Arc<Mutex<FlowCorrelation>>,
    pub tx: broadcast::Sender<SseMessage>,
    pub config: serde_json::Value,
    /// Last known provider status (`AllExchanges` mode) — replayed to new
    /// SSE subscribers after the flow snapshot.
    providers: Arc<Mutex<Vec<ProviderSummary>>>,
    log_id: Arc<AtomicU64>,
}

impl PdbState {
    pub fn new(config: serde_json::Value) -> Self {
        Self::with_mode(config, CorrelationMode::PaymentFlows)
    }

    pub fn with_mode(config: serde_json::Value, mode: CorrelationMode) -> Self {
        let (tx, _) = broadcast::channel(256);
        let correlation = FlowCorrelation::with_mode(tx.clone(), mode);
        Self {
            correlation: Arc::new(Mutex::new(correlation)),
            tx,
            config,
            providers: Arc::new(Mutex::new(Vec::new())),
            log_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn next_log_id(&self) -> u64 {
        self.log_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Open an in-flight flow (`AllExchanges` mode). Returns the log id to
    /// pass back via `update_exchange` and the completing `LogEntry.id`.
    /// Harmless no-op in `PaymentFlows` mode (the id is still valid).
    pub fn begin_exchange(
        &self,
        method: &str,
        path: &str,
        client_ip: &str,
        inference: Option<InferenceInfo>,
    ) -> u64 {
        let id = self.next_log_id();
        let start = ExchangeStart {
            id,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            method: method.to_string(),
            path: path.to_string(),
            client_ip: client_ip.to_string(),
            inference,
        };
        self.correlation.lock().unwrap().begin_exchange(start);
        id
    }

    /// Push live telemetry (running token counts, TTFT) onto an in-flight
    /// exchange opened by `begin_exchange`.
    pub fn update_exchange(&self, log_id: u64, inference: InferenceInfo) {
        self.correlation
            .lock()
            .unwrap()
            .update_exchange(log_id, inference);
    }

    /// Record and broadcast the current provider fleet (discovery/watch task).
    pub fn set_providers(&self, providers: Vec<ProviderSummary>) {
        *self.providers.lock().unwrap() = providers.clone();
        let _ = self.tx.send(SseMessage::ProviderStatus { providers });
    }

    /// Current provider fleet (empty outside inference mode).
    pub fn providers(&self) -> Vec<ProviderSummary> {
        self.providers.lock().unwrap().clone()
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
