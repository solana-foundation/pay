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
use serde::{Deserialize, Serialize};
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

#[derive(Clone)]
struct ResolverState {
    drivers: DriverRegistry,
    proof: Vec<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipientRequest {
    owner_key: String,
    wallets: Vec<WalletRequest>,
}

/// Read-only service boundary. This proof is distinct from the public wallet
/// proxy's proof: a paid consumer must never select another owner's wallets.
pub fn recipient_router(drivers: DriverRegistry, proof: Vec<u8>) -> Router {
    Router::new()
        .route("/__402/resolve-recipients", post(resolve_recipients))
        .with_state(ResolverState { drivers, proof })
}

async fn resolve_recipients(
    State(state): State<ResolverState>,
    headers: HeaderMap,
    Json(request): Json<RecipientRequest>,
) -> Response {
    if state.proof.len() < 32
        || !headers
            .get("x-pay-wallet-resolver-proof")
            .is_some_and(|value| constant_time_eq(value.as_bytes(), &state.proof))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request.owner_key.len() != 16
        || !request
            .owner_key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || request.wallets.is_empty()
        || request.wallets.len() > 8
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // This is ownership evidence from compute, not a payment session. The
    // receive-wallet driver uses only the canonical key for external-ID lookup.
    let tenant = Tenant {
        payer: String::new(),
        key: request.owner_key,
        channel_id: String::new(),
    };
    let mut wallets = Vec::new();
    for reference in request.wallets {
        if reference.driver != crate::privy::DRIVER_ID
            || crate::privy::validate_name(&reference.name).is_err()
        {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let driver = match state.drivers.get(&reference.driver) {
            Ok(driver) => driver,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        match driver.get(&tenant, reference).await {
            Ok(wallet) => wallets.push(serde_json::json!({
                "owner_key": tenant.key,
                "reference": {"driver": wallet.driver, "name": wallet.name},
                "chain": wallet.chain,
                "address": wallet.address,
            })),
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    }
    Json(wallets).into_response()
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    #[tokio::test]
    async fn health_is_public_while_mcp_requires_verified_identity() {
        let app = router(
            DriverRegistry::default(),
            b"wallet-proxy-proof-at-least-32-bytes".to_vec(),
            Vec::new(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        for (path, expected) in [
            ("/__402/health", StatusCode::OK),
            ("/mcp", StatusCode::UNAUTHORIZED),
        ] {
            let response = client.get(format!("{origin}{path}")).send().await.unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn public_proxy_proof_cannot_resolve_another_owners_wallets() {
        let resolver_proof = "resolver-only-proof-at-least-32-bytes";
        let app = recipient_router(
            DriverRegistry::default(),
            resolver_proof.as_bytes().to_vec(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/__402/resolve-recipients",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "owner_key": "0123456789abcdef",
            "wallets": [{"driver": "privy", "name": "tax"}]
        });
        for proof in [None, Some("public-proxy-proof-at-least-32-bytes")] {
            let mut request = client
                .post(&url)
                .json(&body)
                .header("x-pay-proxy-proof", "public-proxy-proof-at-least-32-bytes")
                .header("x-pay-verified-payer", bs58::encode([1; 32]).into_string())
                .header("x-pay-verified-channel", "channel");
            if let Some(proof) = proof {
                request = request.header("x-pay-wallet-resolver-proof", proof);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let response = client
            .post(&url)
            .json(&body)
            .header("x-pay-wallet-resolver-proof", resolver_proof)
            .send()
            .await
            .unwrap();
        // Authentication succeeded; there is deliberately no driver configured.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let mut invalid = body;
        invalid["owner_key"] = serde_json::json!("../another-owner");
        assert_eq!(
            client
                .post(&url)
                .json(&invalid)
                .header("x-pay-wallet-resolver-proof", resolver_proof)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        task.abort();
        let _ = task.await;
    }
}
