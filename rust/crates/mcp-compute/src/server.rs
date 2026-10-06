use std::collections::BTreeMap;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
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

use crate::binding::DataBindingClient;
use crate::driver::{ComputeError, DriverRegistry};
use crate::types::{
    DeployRequest, GatewayInvokeRequest, InvokeRequest, ListRequest, OperationRequest,
    ResourceRequest, Tenant,
};

pub const VERIFIED_PAYER_HEADER: &str = "x-pay-verified-payer";
pub const ORIGINAL_HOST_HEADER: &str = "x-pay-original-host";
pub const USAGE_HEADER: &str = "x-pay-gcp-cpu-microusd";
const MAX_GATEWAY_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub struct ComputeMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    drivers: DriverRegistry,
    bindings: Option<DataBindingClient>,
}

impl ComputeMcp {
    pub fn new(drivers: DriverRegistry, bindings: Option<DataBindingClient>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            drivers,
            bindings,
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
impl ComputeMcp {
    #[tool(
        description = "List configured serverless compute drivers and their capabilities. Call this before deploying when provider, runtime, timeout, or source support is uncertain."
    )]
    async fn providers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Self::result(Ok(self.drivers.capabilities()))
    }

    #[tool(
        description = "Create or update a payer-owned serverless deployment. Source may be inline UTF-8 files or a base64 ZIP. Optional provider-neutral schedule triggers invoke private worker paths, managed service bindings inject scoped data access, and gateway exposure creates a stable paid public hostname. The returned operation is asynchronous; poll operation_status until it succeeds."
    )]
    async fn deploy(
        &self,
        Parameters(mut request): Parameters<DeployRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let binding_result = async {
            if request.service_bindings.is_empty() {
                return Ok(());
            }
            if request
                .environment
                .keys()
                .any(|key| key.starts_with("PAY_BINDING_"))
            {
                return Err(ComputeError::InvalidRequest(
                    "environment keys beginning with `PAY_BINDING_` are reserved for managed service bindings"
                        .into(),
                ));
            }
            let resolver = self.bindings.as_ref().ok_or_else(|| {
                ComputeError::Configuration(
                    "managed service bindings are not configured for this compute service".into(),
                )
            })?;
            let workload_id = format!("{}-{}", tenant.key, request.name);
            let resolved = resolver
                .resolve(&tenant, &workload_id, &request.service_bindings)
                .await?;
            request.environment.extend(resolved.environment);
            Ok(())
        }
        .await;
        if let Err(error) = binding_result {
            return Self::result::<serde_json::Value>(Err(error));
        }
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.deploy(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Get one payer-owned compute deployment by its logical name or provider resource ID."
    )]
    async fn get(
        &self,
        Parameters(request): Parameters<ResourceRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.get(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "List only the compute deployments owned by the current verified payer. Provider resources belonging to other payers are never returned."
    )]
    async fn list(
        &self,
        Parameters(request): Parameters<ListRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.list(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Delete a payer-owned compute deployment. The returned operation is asynchronous; poll operation_status until it succeeds."
    )]
    async fn delete(
        &self,
        Parameters(request): Parameters<ResourceRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.delete(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Poll a deployment or deletion operation returned by deploy or delete. Operations are checked against the current payer before details are returned."
    )]
    async fn operation_status(
        &self,
        Parameters(request): Parameters<OperationRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.operation(&tenant, &request.id).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Invoke a payer-owned deployment and wait for its HTTP response. For public paid callers, use the deployment's compute gateway URL instead."
    )]
    async fn invoke(
        &self,
        Parameters(request): Parameters<InvokeRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.drivers.get(&request.provider) {
            Ok(driver) => driver.invoke(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }
}

#[tool_handler]
impl ServerHandler for ComputeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_06_18)
            .with_server_info(
                Implementation::new("pay-gcp-cpu", env!("CARGO_PKG_VERSION"))
                    .with_title("Pay GCP CPU"),
            )
            .with_instructions("Deploy and invoke payer-isolated serverless compute through pluggable provider drivers.")
    }
}

#[derive(Clone)]
struct AppState {
    drivers: DriverRegistry,
    gateway_domain: String,
}

pub fn router(
    drivers: DriverRegistry,
    gateway_domain: String,
    allowed_hosts: Vec<String>,
    bindings: Option<DataBindingClient>,
) -> Router {
    let transport = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    let mcp_drivers = drivers.clone();
    let mcp_bindings = bindings.clone();
    let service: StreamableHttpService<ComputeMcp, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ComputeMcp::new(mcp_drivers.clone(), mcp_bindings.clone())),
            Default::default(),
            transport,
        );
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(verified_tenant));
    Router::new()
        .route("/__402/health", get(health))
        .merge(mcp)
        .fallback(gateway_invoke)
        .with_state(AppState {
            drivers,
            gateway_domain,
        })
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn verified_tenant(mut request: Request, next: Next) -> Response {
    let payer = request
        .headers()
        .get(VERIFIED_PAYER_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| tenant_from_payer(value).ok());
    match payer {
        Some(tenant) => {
            request.extensions_mut().insert(tenant);
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "verified_payer_required"
            })),
        )
            .into_response(),
    }
}

async fn gateway_invoke(State(state): State<AppState>, request: Request) -> Response {
    match gateway_invoke_inner(state, request).await {
        Ok(response) => response,
        Err(error) => {
            let status = match error {
                ComputeError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
                ComputeError::ProviderNotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_GATEWAY,
            };
            (
                status,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

async fn gateway_invoke_inner(
    state: AppState,
    request: Request,
) -> crate::driver::Result<Response> {
    let host = request
        .headers()
        .get(ORIGINAL_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split(':').next());
    let (deployment_id, gateway_path) = if let Some(host) = host {
        let suffix = format!(".{}", state.gateway_domain);
        let deployment_id = host
            .strip_suffix(&suffix)
            .filter(|label| !label.is_empty() && !label.contains('.'))
            .ok_or_else(|| {
                ComputeError::InvalidRequest(
                    "host is not a deployment below the compute gateway domain".into(),
                )
            })?
            .to_string();
        let path = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .to_string();
        (deployment_id, path)
    } else {
        gateway_path_target(request.uri())?
    };
    let driver = state.drivers.for_gateway(&deployment_id)?;
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_GATEWAY_BODY_BYTES)
        .await
        .map_err(|error| {
            ComputeError::InvalidRequest(format!("invocation body is too large: {error}"))
        })?;
    let headers: BTreeMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            let lower = name.as_str();
            (!matches!(
                lower,
                VERIFIED_PAYER_HEADER
                    | ORIGINAL_HOST_HEADER
                    | "host"
                    | "authorization"
                    | "content-length"
            ) && !lower.starts_with("payment-")
                && !lower.starts_with("x-payment"))
            .then(|| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (lower.to_string(), v.to_string()))
            })
            .flatten()
        })
        .collect();
    let invocation = driver
        .invoke_gateway(GatewayInvokeRequest {
            deployment_id,
            method: parts.method.to_string(),
            path_and_query: gateway_path,
            headers,
            body,
        })
        .await?;
    let mut builder = Response::builder().status(invocation.status);
    for (name, value) in invocation.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            builder = builder.header(name, value);
        }
    }
    builder = builder
        .header(USAGE_HEADER, invocation.billed_micro_usd)
        .header("x-pay-gcp-cpu-elapsed-ms", invocation.elapsed_ms);
    builder.body(Body::from(invocation.body)).map_err(|error| {
        ComputeError::Provider(format!("failed to build invocation response: {error}"))
    })
}

fn gateway_path_target(uri: &axum::http::Uri) -> crate::driver::Result<(String, String)> {
    let path = uri.path().trim_start_matches('/');
    let (deployment_id, remainder) = path.split_once('/').unwrap_or((path, ""));
    if deployment_id.is_empty() || deployment_id.contains('.') {
        return Err(ComputeError::InvalidRequest(
            "gateway path must begin with a deployment ID".into(),
        ));
    }
    let mut upstream_path = format!("/{remainder}");
    if let Some(query) = uri.query() {
        upstream_path.push('?');
        upstream_path.push_str(query);
    }
    Ok((deployment_id.to_string(), upstream_path))
}

fn tenant_from_payer(payer: &str) -> crate::driver::Result<Tenant> {
    let decoded = bs58::decode(payer)
        .into_vec()
        .map_err(|_| ComputeError::InvalidRequest("verified payer is not base58".into()))?;
    if decoded.len() != 32 {
        return Err(ComputeError::InvalidRequest(
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

    #[test]
    fn gateway_path_selects_deployment_and_preserves_query() {
        let uri: axum::http::Uri = "/gcf-tenant-weather/latest?units=metric".parse().unwrap();
        assert_eq!(
            gateway_path_target(&uri).unwrap(),
            (
                "gcf-tenant-weather".to_string(),
                "/latest?units=metric".to_string()
            )
        );
    }

    #[test]
    fn gateway_path_rejects_an_empty_selector() {
        let uri: axum::http::Uri = "/".parse().unwrap();
        assert!(gateway_path_target(&uri).is_err());
    }

    #[test]
    fn tool_results_are_structured_without_json_text_duplication() {
        let result = ComputeMcp::result::<serde_json::Value>(Ok(serde_json::json!({
            "value": 42
        })))
        .unwrap();

        assert!(result.content.is_empty());
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "success": { "value": 42 },
                "error": null
            }))
        );
    }

    #[test]
    fn tenant_key_is_stable_and_not_the_pubkey() {
        let payer = bs58::encode([7_u8; 32]).into_string();
        let tenant = tenant_from_payer(&payer).unwrap();
        assert_eq!(tenant.key.len(), 16);
        assert_ne!(tenant.key, payer);
        assert_eq!(tenant, tenant_from_payer(&payer).unwrap());
    }
}
