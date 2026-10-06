use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::driver::DriverRegistry;
use crate::types::{CreateJobRequest, ExecuteJobRequest, JobRequest, ListJobsRequest, Tenant};

const PAYER: &str = "x-pay-verified-payer";
const CHANNEL: &str = "x-pay-verified-channel";
const PROOF: &str = "x-pay-proxy-proof";
const EXECUTOR_PROOF: &str = "x-pay-job-executor-proof";

#[derive(Clone)]
pub struct JobsMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    drivers: DriverRegistry,
}

impl JobsMcp {
    fn new(drivers: DriverRegistry) -> Self {
        Self {
            tool_router: Self::tool_router(),
            drivers,
        }
    }
    fn tenant(ctx: &RequestContext<RoleServer>) -> Result<Tenant, rmcp::ErrorData> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<Tenant>())
            .cloned()
            .ok_or_else(|| {
                rmcp::ErrorData::internal_error("verified payer context is missing", None)
            })
    }
    fn result<T: Serialize>(
        result: crate::driver::Result<T>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let envelope = match result {
            Ok(value) => serde_json::json!({"success":value,"error":null}),
            Err(error) => serde_json::json!({"success":null,"error":error.to_string()}),
        };
        let mut result = CallToolResult::structured(envelope);
        result.content.clear();
        Ok(result)
    }
}

#[tool_router]
impl JobsMcp {
    #[tool(description = "List portable job capabilities and configured provider drivers.")]
    async fn providers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Self::result(Ok(self.drivers.capabilities()))
    }

    #[tool(
        description = "Create or update a payer-owned recurring job. The target is resolved to a private compute origin and invoked with provider workload identity; payment credentials are never persisted."
    )]
    async fn create(
        &self,
        Parameters(request): Parameters<CreateJobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.create(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }

    #[tool(description = "Get one payer-owned recurring job.")]
    async fn get(
        &self,
        Parameters(request): Parameters<JobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.get(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }

    #[tool(description = "List recurring jobs owned by the verified payer.")]
    async fn list(
        &self,
        Parameters(request): Parameters<ListJobsRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.list(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }

    #[tool(description = "Pause a payer-owned recurring job.")]
    async fn pause(
        &self,
        Parameters(request): Parameters<JobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.pause(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
    #[tool(description = "Resume a payer-owned recurring job.")]
    async fn resume(
        &self,
        Parameters(request): Parameters<JobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.resume(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
    #[tool(description = "Run a payer-owned job immediately without changing its schedule.")]
    async fn run_now(
        &self,
        Parameters(request): Parameters<JobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.run_now(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
    #[tool(description = "Delete a payer-owned recurring job.")]
    async fn delete(
        &self,
        Parameters(request): Parameters<JobRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.delete(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
}

#[tool_handler]
impl ServerHandler for JobsMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_protocol_version(ProtocolVersion::V_2025_06_18)
            .with_server_info(Implementation::new("pay-jobs", env!("CARGO_PKG_VERSION")).with_title("Pay Background Jobs"))
            .with_instructions("Create payer-scoped background jobs through portable drivers. Jobs invoke private compute origins with workload identity.")
    }
}

#[derive(Clone)]
struct AppState {
    proof: Vec<u8>,
    executor_proof: Vec<u8>,
    drivers: DriverRegistry,
}

pub fn router(
    drivers: DriverRegistry,
    proof: Vec<u8>,
    executor_proof: Vec<u8>,
    allowed_hosts: Vec<String>,
) -> Router {
    let transport = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    let mcp_drivers = drivers.clone();
    let service: StreamableHttpService<JobsMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(JobsMcp::new(mcp_drivers.clone())),
        Default::default(),
        transport,
    );
    let state = AppState {
        proof,
        executor_proof,
        drivers,
    };
    let protected =
        Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                state.clone(),
                verified_tenant,
            ));
    Router::new()
        .route(
            "/__402/health",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route("/internal/run", post(execute_job))
        .merge(protected)
        .with_state(state)
}

async fn execute_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExecuteJobRequest>,
) -> Response {
    let valid_proof = headers
        .get(EXECUTOR_PROOF)
        .map(|value| constant_time_eq(value.as_bytes(), &state.executor_proof))
        .unwrap_or(false);
    if !valid_proof {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"executor_proof_required"})),
        )
            .into_response();
    }
    let driver = match state.drivers.get(&request.driver) {
        Ok(driver) => driver,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":error.to_string()})),
            )
                .into_response();
        }
    };
    match driver.execute(request).await {
        Ok(execution) => {
            let mut response = axum::response::Response::builder().status(execution.status);
            for (name, value) in execution.headers {
                response = response.header(name, value);
            }
            response
                .body(axum::body::Body::from(execution.body))
                .unwrap()
        }
        Err(error) => (
            StatusCode::PAYMENT_REQUIRED,
            Json(serde_json::json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}

async fn verified_tenant(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let valid_proof = request
        .headers()
        .get(PROOF)
        .map(|v| constant_time_eq(v.as_bytes(), &state.proof))
        .unwrap_or(false);
    let tenant = tenant_from_headers(request.headers());
    match (valid_proof, tenant) {
        (true, Ok(tenant)) => {
            request.extensions_mut().insert(tenant);
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"verified_payer_required"})),
        )
            .into_response(),
    }
}

fn tenant_from_headers(headers: &HeaderMap) -> Result<Tenant, ()> {
    let payer = headers.get(PAYER).and_then(|v| v.to_str().ok()).ok_or(())?;
    let channel_id = headers
        .get(CHANNEL)
        .and_then(|v| v.to_str().ok())
        .ok_or(())?;
    let decoded = bs58::decode(payer).into_vec().map_err(|_| ())?;
    if decoded.len() != 32
        || channel_id.is_empty()
        || channel_id.len() > 128
        || !channel_id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(());
    }
    let digest = Sha256::digest(decoded);
    let key = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(Tenant {
        payer: payer.into(),
        key,
        channel_id: channel_id.into(),
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |a, (l, r)| a | (l ^ r)) == 0
}
