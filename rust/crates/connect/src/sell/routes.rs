//! HTTP surface of `sell_inference`.
//!
//! Two sides. Sellers (their MCP tool and worker) use `/v1/endpoints`: create
//! an endpoint, long-poll its queue, stream answers back, adjust pricing,
//! delete it. Buyers use `/endpoints/<id>/v1/chat/completions`, which is
//! gated by the seller's own payment backends before it reaches the queue.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, middleware};
use base64::Engine;
use futures_util::StreamExt;
use pay_core::pricing::{PricingConfig, TokenRate};
use pay_core::sell_inference::{SellInference, SellPricing};
use pay_core::server::payment::payment_middleware;
use pay_types::metering::Scheme;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tower::ServiceExt;

use super::backends::{EndpointBackends, Operator};
use super::queue::{EndpointQueue, Event, QueueError};
use crate::protocol::ApiError;

/// Most endpoints one process holds.
pub const MAX_ENDPOINTS: usize = 256;
/// How long a buyer waits for a worker to claim the request.
pub const CLAIM_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a non-streaming buyer waits for the whole answer.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest silence inside a stream before it is closed.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest a worker's long-poll may ask to wait.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(60);
const MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;
const OWNER_TOKEN_PREFIX: &str = "pso_";

/// Who may create endpoints: the bearer of a connector or CLI token.
pub trait CreatorAuth: Send + Sync {
    fn authenticate(&self, bearer: &str) -> Option<Creator>;
}

/// An authenticated creator and, when known, the wallet to pay by default.
#[derive(Debug, Clone)]
pub struct Creator {
    pub subject: String,
    pub wallet: Option<String>,
}

/// Shared state of the sell routes.
pub struct SellState {
    pub registry: Arc<EndpointRegistry>,
    pub operator: Arc<Operator>,
    /// Origin buyers reach endpoints at, e.g. `https://connect.pay.sh`.
    pub public_url: String,
    pub creators: Arc<dyn CreatorAuth>,
}

impl SellState {
    pub fn new(
        operator: Arc<Operator>,
        public_url: impl Into<String>,
        creators: Arc<dyn CreatorAuth>,
    ) -> Self {
        Self {
            registry: Arc::default(),
            operator,
            public_url: public_url.into().trim_end_matches('/').to_string(),
            creators,
        }
    }
}

/// Every live endpoint.
#[derive(Default)]
pub struct EndpointRegistry {
    endpoints: Mutex<HashMap<String, Arc<EndpointEntry>>>,
}

impl EndpointRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<EndpointEntry>>> {
        self.endpoints.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Arc<EndpointEntry>> {
        self.lock().get(id).cloned()
    }

    fn insert(&self, entry: Arc<EndpointEntry>) -> Result<(), ApiError> {
        let mut endpoints = self.lock();
        if endpoints.len() >= MAX_ENDPOINTS {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "endpoints_full",
                "This server holds as many endpoints as it can; try again later.",
            ));
        }
        endpoints.insert(entry.id.clone(), entry);
        Ok(())
    }

    fn remove(&self, id: &str) -> Option<Arc<EndpointEntry>> {
        self.lock().remove(id)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The parts of an endpoint that change when its pricing does.
struct Live {
    sale: SellInference,
    /// The buyer-facing routes behind the payment gate.
    gated: Router,
}

/// One seller's endpoint.
pub struct EndpointEntry {
    pub id: String,
    pub owner: String,
    token_hash: [u8; 32],
    pub queue: Arc<EndpointQueue>,
    live: RwLock<Arc<Live>>,
}

impl EndpointEntry {
    fn live(&self) -> Arc<Live> {
        self.live.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn sale(&self) -> SellInference {
        self.live().sale.clone()
    }

    fn owns(&self, bearer: &str) -> bool {
        let hash = token_hash(bearer);
        hash.iter()
            .zip(self.token_hash.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn mint_owner_token() -> String {
    let bytes: [u8; 32] = rand::random();
    format!(
        "{OWNER_TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

// ── Wire types ───────────────────────────────────────────────────────────

/// Pricing as the tool sends it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PricingInput {
    /// `{"per_request_usd": 0.02}`
    PerRequestUsd(f64),
    /// `{"per_token": {"default": {"in": 0.1, "out": 0.3}, "models": {...}, "max_usd": 0.25}}`
    PerToken(PerTokenInput),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerTokenInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<RateInput>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models: std::collections::BTreeMap<String, RateInput>,
    pub max_usd: f64,
}

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RateInput {
    #[serde(rename = "in")]
    pub input: f64,
    #[serde(rename = "out")]
    pub output: f64,
}

impl From<PricingInput> for SellPricing {
    fn from(input: PricingInput) -> Self {
        match input {
            PricingInput::PerRequestUsd(usd) => SellPricing::PerRequest { usd },
            PricingInput::PerToken(t) => SellPricing::PerToken {
                rates: PricingConfig {
                    default: t.default.map(TokenRate::from),
                    per_model: t
                        .models
                        .into_iter()
                        .map(|(m, r)| (m, TokenRate::from(r)))
                        .collect(),
                },
                max_usd: t.max_usd,
            },
        }
    }
}

impl From<RateInput> for TokenRate {
    fn from(r: RateInput) -> Self {
        TokenRate {
            input_per_1m: r.input,
            output_per_1m: r.output,
        }
    }
}

impl From<&SellPricing> for PricingInput {
    fn from(pricing: &SellPricing) -> Self {
        match pricing {
            SellPricing::PerRequest { usd } => PricingInput::PerRequestUsd(*usd),
            SellPricing::PerToken { rates, max_usd } => PricingInput::PerToken(PerTokenInput {
                default: rates.default.map(|r| RateInput {
                    input: r.input_per_1m,
                    output: r.output_per_1m,
                }),
                models: rates
                    .per_model
                    .iter()
                    .map(|(m, r)| {
                        (
                            m.clone(),
                            RateInput {
                                input: r.input_per_1m,
                                output: r.output_per_1m,
                            },
                        )
                    })
                    .collect(),
                max_usd: *max_usd,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateEndpoint {
    pub pricing: PricingInput,
    /// The model id the endpoint answers as.
    pub model: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Payee. Defaults to the creator's connected wallet.
    #[serde(default)]
    pub recipient: Option<String>,
    #[serde(default)]
    pub currencies: Option<Vec<String>>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub session_cap_usd: Option<f64>,
    /// Idle seconds before a buyer's session channel closes and settles.
    #[serde(default)]
    pub session_idle_close_secs: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEndpoint {
    pub pricing: PricingInput,
}

#[derive(Debug, Serialize)]
pub struct EndpointView {
    pub id: String,
    /// OpenAI-compatible base URL: point an SDK's `base_url` here.
    pub base_url: String,
    pub chat_completions_url: String,
    pub model: String,
    pub pricing: PricingInput,
    pub schemes: Vec<Scheme>,
    pub recipient: String,
    pub network: String,
    pub currencies: Vec<String>,
    pub queue: QueueDepth,
    /// Only on creation. Whoever holds it drains the queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueueDepth {
    pub waiting: usize,
    pub claimed: usize,
}

#[derive(Debug, Deserialize)]
pub struct PollQuery {
    /// Seconds to wait for a request before answering 204.
    #[serde(default)]
    pub wait: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct NextRequest {
    pub request_id: String,
    pub stream: bool,
    /// The buyer's chat-completion request.
    pub body: Value,
}

#[derive(Debug, Deserialize)]
pub struct PushChunks {
    pub chunks: Vec<Value>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Complete {
    /// Streaming: a final chunk, usually carrying `usage`. Non-streaming:
    /// the whole `chat.completion` object.
    #[serde(default)]
    pub event: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct Fail {
    #[serde(default)]
    pub status: Option<u16>,
    pub message: String,
}

// ── Router ───────────────────────────────────────────────────────────────

/// Seller and buyer routes.
pub fn router(state: Arc<SellState>) -> Router {
    Router::new()
        .route("/v1/endpoints", post(create))
        .route("/v1/endpoints/{id}", get(view).patch(update).delete(delete))
        .route("/v1/endpoints/{id}/queue/next", get(next_request))
        .route(
            "/v1/endpoints/{id}/requests/{request}/chunks",
            post(push_chunks),
        )
        .route(
            "/v1/endpoints/{id}/requests/{request}/complete",
            post(complete),
        )
        .route("/v1/endpoints/{id}/requests/{request}/fail", post(fail))
        .route("/endpoints/{id}/{*rest}", any(dispatch))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn unauthorized() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "A bearer token is required.",
    )
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "endpoint_not_found",
        "No such endpoint.",
    )
}

/// The endpoint, if `headers` carry its owner token.
fn owned(state: &SellState, id: &str, headers: &HeaderMap) -> Result<Arc<EndpointEntry>, ApiError> {
    let token = bearer(headers).ok_or_else(unauthorized)?;
    let entry = state.registry.get(id).ok_or_else(not_found)?;
    if !entry.owns(token) {
        // The same answer as for a missing endpoint: a token that is not
        // the owner's learns nothing about which ids exist.
        return Err(not_found());
    }
    Ok(entry)
}

fn view_of(state: &SellState, entry: &EndpointEntry, owner_token: Option<String>) -> EndpointView {
    let sale = entry.sale();
    let (waiting, claimed) = entry.queue.depth();
    EndpointView {
        id: entry.id.clone(),
        base_url: format!("{}/{}/v1", state.public_url, sale.path_prefix()),
        chat_completions_url: format!("{}/{}", state.public_url, sale.chat_path()),
        model: sale.model.clone(),
        pricing: PricingInput::from(&sale.pricing),
        schemes: sale.pricing.schemes(),
        recipient: sale.recipient.clone(),
        network: sale.network.clone(),
        currencies: sale.currencies.clone(),
        queue: QueueDepth { waiting, claimed },
        owner_token,
    }
}

fn live_for(
    state: &SellState,
    entry: &Arc<EndpointEntry>,
    sale: SellInference,
) -> Result<Arc<Live>, ApiError> {
    let backends = EndpointBackends::build(&sale, &state.operator, &state.public_url)
        .map_err(|message| ApiError::bad_request("invalid_endpoint", message))?;
    let gated = Router::new()
        .route(&format!("/{}", sale.chat_path()), post(chat_completions))
        .route(&format!("/{}", sale.models_path()), get(models))
        .layer(middleware::from_fn_with_state(
            backends,
            payment_middleware::<EndpointBackends>,
        ))
        .with_state(entry.clone());
    Ok(Arc::new(Live { sale, gated }))
}

async fn create(
    State(state): State<Arc<SellState>>,
    headers: HeaderMap,
    Json(input): Json<CreateEndpoint>,
) -> Result<(StatusCode, Json<EndpointView>), ApiError> {
    let creator = bearer(&headers)
        .and_then(|token| state.creators.authenticate(token))
        .ok_or_else(unauthorized)?;
    let recipient = input.recipient.or(creator.wallet).ok_or_else(|| {
        ApiError::bad_request(
            "recipient_required",
            "Pass `recipient`, or connect a wallet so the endpoint can pay you.",
        )
    })?;
    let id = uuid::Uuid::new_v4().to_string();
    let model = input.model.trim().to_string();
    let sale = SellInference {
        endpoint_id: id.clone(),
        recipient,
        network: input
            .network
            .unwrap_or_else(|| pay_core::accounts::MAINNET_NETWORK.to_string()),
        currencies: input.currencies.unwrap_or_else(|| vec!["USDC".to_string()]),
        pricing: input.pricing.into(),
        title: input.title.unwrap_or_else(|| model.clone()),
        description: input
            .description
            .unwrap_or_else(|| format!("Inference served by {model} through pay.")),
        model,
        upstream_url: String::new(),
        session_cap_usd: input.session_cap_usd.unwrap_or(1.0),
        session_idle_close_secs: input
            .session_idle_close_secs
            .unwrap_or(pay_core::sell_inference::DEFAULT_SESSION_IDLE_CLOSE_SECS),
    };
    let owner_token = mint_owner_token();
    let entry = Arc::new_cyclic(|_| EndpointEntry {
        id: id.clone(),
        owner: creator.subject,
        token_hash: token_hash(&owner_token),
        queue: Arc::default(),
        live: RwLock::new(Arc::new(Live {
            sale: sale.clone(),
            gated: Router::new(),
        })),
    });
    let live = live_for(&state, &entry, sale)?;
    *entry.live.write().unwrap_or_else(|p| p.into_inner()) = live;
    state.registry.insert(entry.clone())?;
    tracing::info!(endpoint = %id, owner = %entry.owner, "sell_inference endpoint created");
    Ok((
        StatusCode::CREATED,
        Json(view_of(&state, &entry, Some(owner_token))),
    ))
}

async fn view(
    State(state): State<Arc<SellState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<EndpointView>, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    Ok(Json(view_of(&state, &entry, None)))
}

async fn update(
    State(state): State<Arc<SellState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<UpdateEndpoint>,
) -> Result<Json<EndpointView>, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    let mut sale = entry.sale();
    sale.pricing = input.pricing.into();
    let live = live_for(&state, &entry, sale)?;
    *entry.live.write().unwrap_or_else(|p| p.into_inner()) = live;
    tracing::info!(endpoint = %id, "sell_inference pricing updated");
    Ok(Json(view_of(&state, &entry, None)))
}

async fn delete(
    State(state): State<Arc<SellState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    owned(&state, &id, &headers)?;
    state.registry.remove(&id);
    tracing::info!(endpoint = %id, "sell_inference endpoint deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn next_request(
    State(state): State<Arc<SellState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PollQuery>,
) -> Result<Response, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    let wait = Duration::from_secs(query.wait.unwrap_or(30)).min(MAX_POLL_WAIT);
    match entry.queue.next(wait).await {
        Some(request) => {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            Ok(Json(NextRequest {
                request_id: request.id,
                stream: request.stream,
                body,
            })
            .into_response())
        }
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

fn queue_error(error: QueueError) -> ApiError {
    match error {
        QueueError::Unknown => ApiError::new(
            StatusCode::NOT_FOUND,
            "request_not_found",
            "No claimed request with that id.",
        ),
        QueueError::Gone => ApiError::new(
            StatusCode::GONE,
            "client_gone",
            "The buyer stopped waiting; drop this request.",
        ),
        QueueError::Full => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "queue_full",
            error.to_string(),
        ),
    }
}

async fn push_chunks(
    State(state): State<Arc<SellState>>,
    Path((id, request)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<PushChunks>,
) -> Result<StatusCode, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    entry
        .queue
        .push(&request, input.chunks)
        .map_err(queue_error)?;
    Ok(StatusCode::ACCEPTED)
}

async fn complete(
    State(state): State<Arc<SellState>>,
    Path((id, request)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Complete>,
) -> Result<StatusCode, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    entry
        .queue
        .complete(&request, input.event)
        .map_err(queue_error)?;
    Ok(StatusCode::OK)
}

async fn fail(
    State(state): State<Arc<SellState>>,
    Path((id, request)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Fail>,
) -> Result<StatusCode, ApiError> {
    let entry = owned(&state, &id, &headers)?;
    entry
        .queue
        .fail(&request, input.status.unwrap_or(502), input.message)
        .map_err(queue_error)?;
    Ok(StatusCode::OK)
}

// ── Buyer side ───────────────────────────────────────────────────────────

/// Hand a buyer's request to the endpoint's gated router.
async fn dispatch(
    State(state): State<Arc<SellState>>,
    Path((id, _rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(entry) = state.registry.get(&id) else {
        return openai_error(
            StatusCode::NOT_FOUND,
            "endpoint_not_found",
            "No such endpoint.",
        );
    };
    let router = entry.live().gated.clone();
    match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// An error in the shape OpenAI clients parse.
fn openai_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "message": message, "type": "server_error", "code": code }
        })),
    )
        .into_response()
}

async fn models(State(entry): State<Arc<EndpointEntry>>) -> Json<Value> {
    let sale = entry.sale();
    Json(json!({
        "object": "list",
        "data": [{
            "id": sale.model,
            "object": "model",
            "owned_by": "pay",
        }],
    }))
}

/// Park the request and relay the worker's answer.
pub async fn chat_completions(State(entry): State<Arc<EndpointEntry>>, body: Bytes) -> Response {
    let stream = match serde_json::from_slice::<Value>(&body) {
        Ok(json) => json.get("stream").and_then(Value::as_bool).unwrap_or(false),
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &format!("request body is not JSON: {e}"),
            );
        }
    };
    let (id, mut rx) = match entry.queue.park(body, stream) {
        Ok(parked) => parked,
        Err(QueueError::Full) => {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "queue_full",
                "This endpoint has too many requests waiting; retry shortly.",
            );
        }
        Err(other) => return openai_error(StatusCode::BAD_GATEWAY, "queue", &other.to_string()),
    };

    // No answer starts until a worker takes the request.
    match tokio::time::timeout(CLAIM_TIMEOUT, rx.recv()).await {
        Ok(Some(Event::Claimed)) => {}
        Ok(Some(other)) => {
            // A worker that skips the claim event is not one of ours.
            tracing::warn!(?other, "unexpected event before claim");
            entry.queue.abandon(&id);
            return openai_error(StatusCode::BAD_GATEWAY, "protocol", "worker protocol error");
        }
        Ok(None) | Err(_) => {
            entry.queue.abandon(&id);
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_worker",
                "No worker picked up the request in time; the seller's agent may be offline.",
            );
        }
    }

    if stream {
        stream_response(rx)
    } else {
        buffered_response(rx).await
    }
}

async fn buffered_response(mut rx: mpsc::UnboundedReceiver<Event>) -> Response {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Event::Chunk(_))) | Ok(Some(Event::Claimed)) => continue,
            Ok(Some(Event::Complete(Some(body)))) => {
                return (StatusCode::OK, Json(body)).into_response();
            }
            Ok(Some(Event::Complete(None))) => {
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    "empty_completion",
                    "The worker completed the request without a response body.",
                );
            }
            Ok(Some(Event::Fail { status, message })) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                return openai_error(status, "worker_failed", &message);
            }
            Ok(None) => {
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    "worker_disconnected",
                    "The worker went away before answering.",
                );
            }
            Err(_) => {
                return openai_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "The worker did not answer in time.",
                );
            }
        }
    }
}

fn sse(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn stream_response(rx: mpsc::UnboundedReceiver<Event>) -> Response {
    let done = Bytes::from_static(b"data: [DONE]\n\n");
    let frames = futures_util::stream::unfold(Some(rx), move |rx| {
        let done = done.clone();
        async move {
            let mut rx = rx?;
            let (frame, next) = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(Event::Claimed)) => (Bytes::new(), Some(rx)),
                Ok(Some(Event::Chunk(chunk))) => (sse(&chunk), Some(rx)),
                Ok(Some(Event::Complete(Some(last)))) => {
                    let mut frame = sse(&last).to_vec();
                    frame.extend_from_slice(&done);
                    (Bytes::from(frame), None)
                }
                Ok(Some(Event::Complete(None))) => (done, None),
                Ok(Some(Event::Fail { message, .. })) => (
                    sse(&json!({ "error": { "message": message, "type": "server_error", "code": "worker_failed" } })),
                    None,
                ),
                Ok(None) => (
                    sse(&json!({ "error": { "message": "The worker went away mid-stream.", "type": "server_error", "code": "worker_disconnected" } })),
                    None,
                ),
                Err(_) => (
                    sse(&json!({ "error": { "message": "The stream went quiet for too long.", "type": "server_error", "code": "timeout" } })),
                    None,
                ),
            };
            Some((Ok::<Bytes, Infallible>(frame), next))
        }
    })
    .filter(|frame: &Result<Bytes, Infallible>| {
        let keep = frame.as_ref().is_ok_and(|b| !b.is_empty());
        async move { keep }
    });
    let mut response = Response::new(Body::from_stream(frames));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

use axum::http::HeaderName;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Method, Request};

    const CREATOR_WALLET: &str = "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    struct FixedCreators;

    impl CreatorAuth for FixedCreators {
        fn authenticate(&self, bearer: &str) -> Option<Creator> {
            match bearer {
                "creator" => Some(Creator {
                    subject: "sub_creator".into(),
                    wallet: Some(CREATOR_WALLET.into()),
                }),
                "guest" => Some(Creator {
                    subject: "guest_1".into(),
                    wallet: None,
                }),
                _ => None,
            }
        }
    }

    fn operator() -> Arc<Operator> {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(sk.as_bytes());
        keypair[32..].copy_from_slice(sk.verifying_key().as_bytes());
        let signer = pay_kit::solana_keychain::MemorySigner::from_bytes(&keypair).unwrap();
        let operator = Operator::new(Arc::new(signer), "http://127.0.0.1:1", SECRET);
        // No RPC in tests: challenges read this cached blockhash.
        operator
            .blockhashes
            .set("11111111111111111111111111111111".into(), 1_000, 1);
        Arc::new(operator)
    }

    fn state() -> Arc<SellState> {
        Arc::new(SellState::new(
            operator(),
            "https://connect.test/",
            Arc::new(FixedCreators),
        ))
    }

    async fn send(
        app: &Router,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "connect.test")
            .header(header::ACCEPT, "application/json");
        if let Some(bearer) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let req = match body {
            Some(body) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
    }

    fn per_request(usd: f64) -> Value {
        json!({ "pricing": { "per_request_usd": usd }, "model": "agent", "network": "devnet" })
    }

    async fn create_endpoint(app: &Router, body: Value) -> (String, String) {
        let (status, _, view) = send(
            app,
            Method::POST,
            "/v1/endpoints",
            Some("creator"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{view}");
        (
            view["id"].as_str().unwrap().to_string(),
            view["owner_token"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn creating_needs_a_creator_and_hands_back_the_owner_token_once() {
        let state = state();
        let app = router(state.clone());

        let (status, _, body) = send(
            &app,
            Method::POST,
            "/v1/endpoints",
            None,
            Some(per_request(0.02)),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, _, body) = send(
            &app,
            Method::POST,
            "/v1/endpoints",
            Some("stranger"),
            Some(per_request(0.02)),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

        // A guest has no wallet and names no recipient.
        let (status, _, body) = send(
            &app,
            Method::POST,
            "/v1/endpoints",
            Some("guest"),
            Some(per_request(0.02)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "recipient_required");

        let (status, _, view) = send(
            &app,
            Method::POST,
            "/v1/endpoints",
            Some("creator"),
            Some(per_request(0.02)),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{view}");
        let id = view["id"].as_str().unwrap();
        assert_eq!(
            view["base_url"],
            format!("https://connect.test/endpoints/{id}/v1")
        );
        assert_eq!(view["model"], "agent");
        assert_eq!(
            view["recipient"], CREATOR_WALLET,
            "the creator's wallet is the default payee"
        );
        assert_eq!(view["currencies"], json!(["USDC"]));
        assert_eq!(view["pricing"], json!({ "per_request_usd": 0.02 }));
        assert_eq!(
            view["schemes"],
            json!([
                "mpp-charge",
                "mpp-session",
                "x402-upto",
                "x402-batch-settlement"
            ])
        );
        let token = view["owner_token"].as_str().unwrap();
        assert!(token.starts_with("pso_"), "{token}");
        assert_eq!(state.registry.len(), 1);

        // The token is shown once; later views omit it.
        let (status, _, again) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(again.get("owner_token").is_none());
        assert_eq!(again["queue"], json!({ "waiting": 0, "claimed": 0 }));

        // Anything but the owner token sees no endpoint at all.
        let (status, _, _) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}"),
            Some("pso_x"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_buyer_without_payment_is_challenged_and_the_model_list_is_free() {
        let app = router(state());
        let (id, _) = create_endpoint(&app, per_request(0.02)).await;

        let (status, headers, body) = send(
            &app,
            Method::POST,
            &format!("/endpoints/{id}/v1/chat/completions"),
            None,
            Some(json!({ "model": "agent", "messages": [] })),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{body}");
        let www: Vec<&str> = headers
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert!(
            www.iter().any(|v| v.contains("intent=\"session\"")),
            "{www:?}"
        );
        assert!(
            www.iter().any(|v| v.contains("intent=\"charge\"")),
            "{www:?}"
        );
        // x402 upto challenges from the cached blockhash. Batch-settlement
        // needs RPC for its challenge and is absent offline; the backends
        // test covers that it is built and wired.
        assert_eq!(
            headers
                .get_all(pay_kit::x402::PAYMENT_REQUIRED_HEADER)
                .iter()
                .count(),
            1,
            "x402 upto"
        );

        let (status, _, models) = send(
            &app,
            Method::GET,
            &format!("/endpoints/{id}/v1/models"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{models}");
        assert_eq!(models["data"][0]["id"], "agent");

        let (status, _, body) = send(
            &app,
            Method::POST,
            "/endpoints/nope/v1/chat/completions",
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "endpoint_not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repricing_per_token_narrows_the_schemes_to_metered_ones() {
        let app = router(state());
        let (id, token) = create_endpoint(&app, per_request(0.02)).await;

        let (status, _, view) = send(
            &app,
            Method::PATCH,
            &format!("/v1/endpoints/{id}"),
            Some(&token),
            Some(json!({ "pricing": { "per_token": {
                "default": { "in": 0.1, "out": 0.3 },
                "models": { "agent": { "in": 0.2, "out": 0.6 } },
                "max_usd": 0.25 } } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(view["schemes"], json!(["mpp-session", "x402-upto"]));
        assert_eq!(view["pricing"]["per_token"]["max_usd"], 0.25);
        assert_eq!(view["pricing"]["per_token"]["models"]["agent"]["out"], 0.6);

        let (status, headers, _) = send(
            &app,
            Method::POST,
            &format!("/endpoints/{id}/v1/chat/completions"),
            None,
            Some(json!({ "model": "agent", "messages": [] })),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(headers.get_all(header::WWW_AUTHENTICATE).iter().count(), 1);
        assert_eq!(
            headers
                .get_all(pay_kit::x402::PAYMENT_REQUIRED_HEADER)
                .iter()
                .count(),
            1
        );

        // A price that cannot be billed is refused and the old one stays.
        let (status, _, body) = send(
            &app,
            Method::PATCH,
            &format!("/v1/endpoints/{id}"),
            Some(&token),
            Some(json!({ "pricing": { "per_request_usd": 0.0 } })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_endpoint");
        let (_, _, view) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(view["schemes"], json!(["mpp-session", "x402-upto"]));
    }

    /// A paid buyer's request, as the gate would hand it to the handler.
    fn buyer(state: &SellState, id: &str, body: Value) -> impl Future<Output = Response> + use<> {
        let entry = state.registry.get(id).unwrap();
        chat_completions(State(entry), Bytes::from(body.to_string()))
    }

    use std::future::Future;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_streaming_answer_is_relayed_as_sse_with_done() {
        let state = state();
        let app = router(state.clone());
        let (id, token) = create_endpoint(&app, per_request(0.02)).await;

        let request = json!({ "model": "agent", "stream": true,
            "messages": [{ "role": "user", "content": "hi" }] });
        let buyer = tokio::spawn(buyer(&state, &id, request.clone()));

        // The worker takes it.
        let (status, _, next) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}/queue/next?wait=5"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{next}");
        assert!(next["stream"].as_bool().unwrap());
        assert_eq!(next["body"], request);
        let rid = next["request_id"].as_str().unwrap();

        let chunk = |text: &str| {
            json!({ "id": "c1", "object": "chat.completion.chunk", "model": "agent",
                "choices": [{ "index": 0, "delta": { "content": text } }] })
        };
        let (status, _, _) = send(
            &app,
            Method::POST,
            &format!("/v1/endpoints/{id}/requests/{rid}/chunks"),
            Some(&token),
            Some(json!({ "chunks": [chunk("Hello"), chunk(", world")] })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let usage = json!({ "id": "c1", "object": "chat.completion.chunk", "choices": [],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2 } });
        let (status, _, _) = send(
            &app,
            Method::POST,
            &format!("/v1/endpoints/{id}/requests/{rid}/complete"),
            Some(&token),
            Some(json!({ "event": usage })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let response = buyer.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            HeaderValue::from_static("text/event-stream")
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let events: Vec<&str> = text.split("\n\n").filter(|s| !s.is_empty()).collect();
        assert_eq!(events.len(), 4, "{text}");
        assert_eq!(events[0], format!("data: {}", chunk("Hello")));
        assert_eq!(events[1], format!("data: {}", chunk(", world")));
        assert_eq!(events[2], format!("data: {usage}"));
        assert_eq!(events[3], "data: [DONE]");

        // Once finished, the request id is gone.
        let (status, _, body) = send(
            &app,
            Method::POST,
            &format!("/v1/endpoints/{id}/requests/{rid}/chunks"),
            Some(&token),
            Some(json!({ "chunks": [] })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_buffered_answer_is_the_completion_object_and_failures_are_errors() {
        let state = state();
        let app = router(state.clone());
        let (id, token) = create_endpoint(&app, per_request(0.02)).await;

        let buyer_task = tokio::spawn(buyer(
            &state,
            &id,
            json!({ "model": "agent", "messages": [] }),
        ));
        let (_, _, next) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}/queue/next?wait=5"),
            Some(&token),
            None,
        )
        .await;
        assert!(!next["stream"].as_bool().unwrap());
        let rid = next["request_id"].as_str().unwrap();
        let completion = json!({ "id": "x", "object": "chat.completion",
            "choices": [{ "message": { "role": "assistant", "content": "hi" } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
        send(
            &app,
            Method::POST,
            &format!("/v1/endpoints/{id}/requests/{rid}/complete"),
            Some(&token),
            Some(json!({ "event": completion })),
        )
        .await;
        let response = buyer_task.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), completion);

        // A worker failure becomes an OpenAI-shaped error with its status.
        let buyer_task = tokio::spawn(buyer(
            &state,
            &id,
            json!({ "model": "agent", "messages": [] }),
        ));
        let (_, _, next) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}/queue/next?wait=5"),
            Some(&token),
            None,
        )
        .await;
        let rid = next["request_id"].as_str().unwrap();
        send(
            &app,
            Method::POST,
            &format!("/v1/endpoints/{id}/requests/{rid}/fail"),
            Some(&token),
            Some(json!({ "status": 429, "message": "agent is busy" })),
        )
        .await;
        let response = buyer_task.await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "worker_failed");
        assert_eq!(body["error"]["message"], "agent is busy");

        // Not JSON: refused before parking.
        let entry = state.registry.get(&id).unwrap();
        let response = chat_completions(State(entry.clone()), Bytes::from_static(b"nope")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(entry.queue.depth(), (0, 0));

        // An empty poll answers 204.
        let (status, _, _) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}/queue/next?wait=0"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_removes_the_endpoint_for_everyone() {
        let state = state();
        let app = router(state.clone());
        let (id, token) = create_endpoint(&app, per_request(0.02)).await;
        let (status, _, _) = send(
            &app,
            Method::DELETE,
            &format!("/v1/endpoints/{id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(state.registry.is_empty());
        let (status, _, _) = send(
            &app,
            Method::GET,
            &format!("/v1/endpoints/{id}"),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(
            &app,
            Method::POST,
            &format!("/endpoints/{id}/v1/chat/completions"),
            None,
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn pricing_input_round_trips() {
        let input = PricingInput::PerToken(PerTokenInput {
            default: Some(RateInput {
                input: 0.1,
                output: 0.3,
            }),
            models: [(
                "m".to_string(),
                RateInput {
                    input: 1.0,
                    output: 2.0,
                },
            )]
            .into(),
            max_usd: 0.5,
        });
        let pricing: SellPricing = input.clone().into();
        assert_eq!(PricingInput::from(&pricing), input);
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["per_token"]["default"]["in"], 0.1);
        assert_eq!(json["per_token"]["models"]["m"]["out"], 2.0);
        let flat: PricingInput =
            serde_json::from_value(json!({ "per_request_usd": 0.02 })).unwrap();
        assert_eq!(flat, PricingInput::PerRequestUsd(0.02));
    }
}
