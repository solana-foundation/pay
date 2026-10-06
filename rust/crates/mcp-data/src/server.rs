use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
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

use crate::binding::{
    BindingIssuer, CAPABILITY_HEADER, CreateBindingRequest, CreateBindingResponse,
    INTERNAL_PROOF_HEADER,
};
use crate::driver::{DataError, DriverRegistry};
use crate::types::{
    CreateDocumentStoreRequest, DocumentRequest, DocumentStoreRequest, GatewayReadRequest,
    ListDocumentStoresRequest, PutDocumentRequest, Tenant,
};

pub const VERIFIED_PAYER_HEADER: &str = "x-pay-verified-payer";
pub const PROXY_PROOF_HEADER: &str = "x-pay-proxy-proof";
pub const ORIGINAL_HOST_HEADER: &str = "x-pay-original-host";
pub const USAGE_HEADER: &str = "x-pay-gcp-data-microusd";

#[derive(Clone)]
pub struct DataMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    drivers: DriverRegistry,
}

impl DataMcp {
    pub fn new(drivers: DriverRegistry) -> Self {
        Self {
            tool_router: Self::tool_router(),
            drivers,
        }
    }

    fn tenant(ctx: &RequestContext<RoleServer>) -> Result<Tenant, rmcp::ErrorData> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Tenant>())
            .cloned()
            .ok_or_else(|| {
                rmcp::ErrorData::internal_error("verified payer context is missing", None)
            })
    }

    fn result<T: Serialize>(
        result: crate::driver::Result<T>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let envelope = match result {
            Ok(value) => serde_json::json!({ "success": value, "error": null }),
            Err(error) => serde_json::json!({ "success": null, "error": error.to_string() }),
        };
        let mut response = CallToolResult::structured(envelope);
        response.content.clear();
        Ok(response)
    }
}

#[tool_router]
impl DataMcp {
    #[tool(
        description = "List portable data classes and configured provider drivers. Call this before creating a data service when the required data model or provider is uncertain."
    )]
    async fn classes(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Self::result(Ok(self.drivers.classes()))
    }

    #[tool(
        description = "Create or reconcile a payer-owned document store claim. Use class `document/serverless` for a portable serverless document API. Set access.gateway_reads=true only when paid public reads should be published."
    )]
    async fn create_document_store(
        &self,
        Parameters(request): Parameters<CreateDocumentStoreRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.create_document_store(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Get one payer-owned document store by logical name or physical ID.")]
    async fn get_document_store(
        &self,
        Parameters(request): Parameters<DocumentStoreRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.get_document_store(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "List only document store claims owned by the current verified payer.")]
    async fn list_document_stores(
        &self,
        Parameters(request): Parameters<ListDocumentStoresRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.list_document_stores(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Delete a payer-owned document store claim. A `delete` reclaim policy removes up to 10,000 documents before the claim; a `retain` policy removes only the claim."
    )]
    async fn delete_document_store(
        &self,
        Parameters(request): Parameters<DocumentStoreRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.delete_document_store(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Read one document from a payer-owned document store.")]
    async fn get_document(
        &self,
        Parameters(request): Parameters<DocumentRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.get_document(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Create or replace one JSON document in a payer-owned document store. The serialized payload is capped at 256 KiB."
    )]
    async fn put_document(
        &self,
        Parameters(request): Parameters<PutDocumentRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.put_document(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Delete one document from a payer-owned document store.")]
    async fn delete_document(
        &self,
        Parameters(request): Parameters<DocumentRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.driver) {
            Ok(driver) => driver.delete_document(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }
}

#[tool_handler]
impl ServerHandler for DataMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_06_18)
            .with_server_info(
                Implementation::new("pay-data", env!("CARGO_PKG_VERSION"))
                    .with_title("Pay Data Services"),
            )
            .with_instructions(
                "Provision payer-scoped typed data services through portable classes and provider drivers.",
            )
    }
}

#[derive(Clone)]
struct AppState {
    drivers: DriverRegistry,
    gateway_domain: Arc<str>,
    bindings: BindingIssuer,
}

pub fn router(
    drivers: DriverRegistry,
    gateway_domain: String,
    allowed_hosts: Vec<String>,
    bindings: BindingIssuer,
) -> Router {
    let transport = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    let mcp_drivers = drivers.clone();
    let service: StreamableHttpService<DataMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(DataMcp::new(mcp_drivers.clone())),
        Default::default(),
        transport,
    );
    let state = AppState {
        drivers,
        gateway_domain: gateway_domain.into(),
        bindings,
    };
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            verified_tenant,
        ));
    Router::new()
        .route("/__402/health", get(health))
        .route("/__402/bind", post(create_binding))
        .route(
            "/__402/bindings/{store_id}/{key}",
            get(binding_get_document)
                .put(binding_put_document)
                .delete(binding_delete_document),
        )
        .merge(mcp)
        .fallback(gateway_read)
        .with_state(state)
}

/// Capability-authenticated data plane for deployed workloads. This router is
/// deliberately narrow: it cannot issue capabilities, serve MCP, or publish
/// paid gateway reads.
pub fn runtime_router(drivers: DriverRegistry, bindings: BindingIssuer) -> Router {
    Router::new()
        .route("/__402/health", get(health))
        .route(
            "/__402/bindings/{store_id}/{key}",
            get(binding_get_document)
                .put(binding_put_document)
                .delete(binding_delete_document),
        )
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(AppState {
            drivers,
            gateway_domain: Arc::from(""),
            bindings,
        })
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn create_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<CreateBindingRequest>,
) -> Response {
    let result: crate::driver::Result<CreateBindingResponse> = async {
        let proof = headers
            .get(INTERNAL_PROOF_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| DataError::InvalidRequest("binding proof is required".into()))?;
        state.bindings.verify_internal_proof(proof)?;
        let tenant = tenant_from_payer(&request.payer)?;
        let driver = state.drivers.get(&request.driver)?;
        let store = driver
            .get_document_store(
                &tenant,
                DocumentStoreRequest {
                    driver: request.driver.clone(),
                    id: request.store_id.clone(),
                },
            )
            .await?;
        request.store_id = store.id;
        Ok(state.bindings.issue(tenant.key, &request)?.into())
    }
    .await;
    internal_response(result)
}

async fn binding_get_document(
    State(state): State<AppState>,
    Path((store_id, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let capability = binding_capability(&state, &headers, &store_id)?;
        if !capability.read {
            return Err(DataError::InvalidRequest(
                "binding does not grant document reads".into(),
            ));
        }
        let driver = state.drivers.get(&capability.driver)?;
        driver
            .get_document(
                &Tenant {
                    payer: String::new(),
                    key: capability.tenant,
                },
                DocumentRequest {
                    driver: capability.driver,
                    store_id,
                    key,
                },
            )
            .await
    }
    .await;
    internal_response(result)
}

async fn binding_put_document(
    State(state): State<AppState>,
    Path((store_id, key)): Path<(String, String)>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let result = async {
        let capability = binding_capability(&state, &headers, &store_id)?;
        if !capability.write {
            return Err(DataError::InvalidRequest(
                "binding does not grant document writes".into(),
            ));
        }
        let driver = state.drivers.get(&capability.driver)?;
        driver
            .put_document(
                &Tenant {
                    payer: String::new(),
                    key: capability.tenant,
                },
                PutDocumentRequest {
                    driver: capability.driver,
                    store_id,
                    key,
                    value,
                },
            )
            .await
    }
    .await;
    internal_response(result)
}

async fn binding_delete_document(
    State(state): State<AppState>,
    Path((store_id, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let capability = binding_capability(&state, &headers, &store_id)?;
        if !capability.write {
            return Err(DataError::InvalidRequest(
                "binding does not grant document deletes".into(),
            ));
        }
        let driver = state.drivers.get(&capability.driver)?;
        driver
            .delete_document(
                &Tenant {
                    payer: String::new(),
                    key: capability.tenant,
                },
                DocumentRequest {
                    driver: capability.driver,
                    store_id,
                    key,
                },
            )
            .await
    }
    .await;
    internal_response(result)
}

fn binding_capability(
    state: &AppState,
    headers: &HeaderMap,
    store_id: &str,
) -> crate::driver::Result<crate::binding::Capability> {
    let token = headers
        .get(CAPABILITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| DataError::InvalidRequest("data capability is required".into()))?;
    state.bindings.verify(token, store_id)
}

fn internal_response<T: Serialize>(result: crate::driver::Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(DataError::InvalidRequest(message)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response(),
        Err(DataError::DriverNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "data driver not found" })),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "internal data request failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": "data provider request failed" })),
            )
                .into_response()
        }
    }
}

async fn verified_tenant(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let proxy_verified = request
        .headers()
        .get(PROXY_PROOF_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|proof| state.bindings.verify_internal_proof(proof).is_ok());
    let tenant = request
        .headers()
        .get(VERIFIED_PAYER_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| tenant_from_payer(value).ok());
    match (proxy_verified, tenant) {
        (true, Some(tenant)) => {
            request.extensions_mut().insert(tenant);
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "verified_payer_required" })),
        )
            .into_response(),
    }
}

async fn gateway_read(State(state): State<AppState>, request: Request) -> Response {
    match gateway_read_inner(state, request).await {
        Ok(response) => response,
        Err(error) => {
            let (status, message) = match &error {
                DataError::InvalidRequest(message) => (StatusCode::BAD_REQUEST, message.as_str()),
                DataError::DriverNotFound(_) => (StatusCode::NOT_FOUND, "data driver not found"),
                _ => {
                    tracing::warn!(error = %error, "published data read failed");
                    (StatusCode::BAD_GATEWAY, "data provider request failed")
                }
            };
            (status, Json(serde_json::json!({ "error": message }))).into_response()
        }
    }
}

async fn gateway_read_inner(state: AppState, request: Request) -> crate::driver::Result<Response> {
    if request.method() != axum::http::Method::GET {
        return Err(DataError::InvalidRequest(
            "published document stores support GET only".into(),
        ));
    }
    let host = request
        .headers()
        .get(ORIGINAL_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| DataError::InvalidRequest("trusted original host is missing".into()))?
        .split(':')
        .next()
        .unwrap_or_default();
    let suffix = format!(".{}", state.gateway_domain);
    let store_id = host
        .strip_suffix(&suffix)
        .filter(|label| !label.is_empty() && !label.contains('.'))
        .ok_or_else(|| {
            DataError::InvalidRequest(
                "host is not a store below the configured data gateway domain".into(),
            )
        })?
        .to_string();
    let key = request
        .uri()
        .path()
        .strip_prefix('/')
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .ok_or_else(|| {
            DataError::InvalidRequest("document key must be the single URL path segment".into())
        })?
        .to_string();
    let driver = state.drivers.for_gateway(&store_id)?;
    let read = driver
        .read_gateway(GatewayReadRequest { store_id, key })
        .await?;
    let mut response = Json(read.document).into_response();
    response.headers_mut().insert(
        HeaderName::from_static(USAGE_HEADER),
        HeaderValue::from_str(&read.billed_micro_usd.to_string())
            .map_err(|_| DataError::Provider("calculated usage could not be encoded".into()))?,
    );
    Ok(response)
}

fn tenant_from_payer(payer: &str) -> crate::driver::Result<Tenant> {
    let decoded = bs58::decode(payer)
        .into_vec()
        .map_err(|_| DataError::InvalidRequest("verified payer is not base58".into()))?;
    if decoded.len() != 32 {
        return Err(DataError::InvalidRequest(
            "verified payer must decode to 32 bytes".into(),
        ));
    }
    let digest = Sha256::digest(decoded);
    let key = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Tenant {
        payer: payer.to_string(),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[test]
    fn tool_results_are_structured_without_text_duplication() {
        let result =
            DataMcp::result::<serde_json::Value>(Ok(serde_json::json!({ "ok": true }))).unwrap();
        assert!(result.content.is_empty());
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "success": { "ok": true },
                "error": null
            }))
        );
    }

    #[test]
    fn payer_keys_are_stable_and_private() {
        let payer = bs58::encode([11_u8; 32]).into_string();
        let tenant = tenant_from_payer(&payer).unwrap();
        assert_eq!(tenant.key.len(), 16);
        assert_ne!(tenant.key, payer);
        assert_eq!(tenant, tenant_from_payer(&payer).unwrap());
    }

    #[tokio::test]
    async fn runtime_plane_does_not_expose_control_routes() {
        let app = runtime_router(
            DriverRegistry::default(),
            BindingIssuer::new(vec![7; 32], vec![9; 32]).unwrap(),
        );
        for path in ["/mcp", "/__402/bind"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/__402/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn payer_identity_requires_the_proxy_proof() {
        let state = AppState {
            drivers: DriverRegistry::default(),
            gateway_domain: Arc::from("example.invalid"),
            bindings: BindingIssuer::new(vec![7; 32], vec![b'x'; 32]).unwrap(),
        };
        let app = Router::new()
            .route("/", get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                verified_tenant,
            ))
            .with_state(state);
        let payer = bs58::encode([11_u8; 32]).into_string();
        let without_proof = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(VERIFIED_PAYER_HEADER, &payer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(without_proof.status(), StatusCode::UNAUTHORIZED);
        let with_proof = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(VERIFIED_PAYER_HEADER, payer)
                    .header(PROXY_PROOF_HEADER, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(with_proof.status(), StatusCode::OK);
    }
}
