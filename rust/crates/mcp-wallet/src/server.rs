use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
use crate::types::{CreateWalletRequest, Tenant, WalletRequest};

const PAYER: &str = "x-pay-verified-payer";
const CHANNEL: &str = "x-pay-verified-channel";
const PROOF: &str = "x-pay-proxy-proof";

#[derive(Clone)]
pub struct WalletMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    drivers: DriverRegistry,
}
impl WalletMcp {
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
        let value = match result {
            Ok(v) => serde_json::json!({"success":v,"error":null}),
            Err(e) => serde_json::json!({"success":null,"error":e.to_string()}),
        };
        let mut result = CallToolResult::structured(value);
        result.content.clear();
        Ok(result)
    }
}
#[tool_router]
impl WalletMcp {
    #[tool(description = "List wallet drivers, supported chains, custody model, and operations.")]
    async fn providers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Self::result(Ok(self.drivers.capabilities()))
    }
    #[tool(
        description = "Idempotently create a payer-scoped receive wallet. Use stable names such as tax, profit, and infrastructure. Wallets are retained after the funding channel closes because they may contain earnings."
    )]
    async fn create(
        &self,
        Parameters(request): Parameters<CreateWalletRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.create(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
    #[tool(description = "Get a payer-scoped wallet by its stable logical name.")]
    async fn get(
        &self,
        Parameters(request): Parameters<WalletRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(d) => d.get(&tenant, request).await,
            Err(e) => Err(e),
        };
        Self::result(result)
    }
}
#[tool_handler]
impl ServerHandler for WalletMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_protocol_version(ProtocolVersion::V_2025_06_18).with_server_info(Implementation::new("pay-wallet",env!("CARGO_PKG_VERSION")).with_title("Pay Wallets")).with_instructions("Create stable payer-scoped wallets through portable drivers. Provisioned wallets are durable and are not garbage-collected with payment channels.")
    }
}

#[derive(Clone)]
struct AppState {
    proof: Vec<u8>,
}
pub fn router(drivers: DriverRegistry, proof: Vec<u8>, allowed_hosts: Vec<String>) -> Router {
    let transport = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    let service: StreamableHttpService<WalletMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(WalletMcp::new(drivers.clone())),
        Default::default(),
        transport,
    );
    let state = AppState { proof };
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
        .merge(protected)
        .with_state(state)
}
async fn verified_tenant(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let valid = request
        .headers()
        .get(PROOF)
        .map(|v| constant_time_eq(v.as_bytes(), &state.proof))
        .unwrap_or(false);
    let tenant = tenant_from_headers(request.headers());
    match (valid, tenant) {
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
    let channel = headers
        .get(CHANNEL)
        .and_then(|v| v.to_str().ok())
        .ok_or(())?;
    let decoded = bs58::decode(payer).into_vec().map_err(|_| ())?;
    if decoded.len() != 32
        || channel.is_empty()
        || channel.len() > 128
        || !channel.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(());
    }
    let digest = Sha256::digest(decoded);
    let key = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(Tenant {
        payer: payer.into(),
        key,
        channel_id: channel.into(),
    })
}
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |a, (l, r)| a | (l ^ r)) == 0
}
