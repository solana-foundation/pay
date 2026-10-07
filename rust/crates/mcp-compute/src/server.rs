use std::collections::BTreeMap;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
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

use crate::binding::DataBindingClient;
use crate::driver::{ComputeError, DriverRegistry};
use crate::payment_service::{
    DeletePaymentPolicyRequest, GetPaymentPolicyRequest, PaymentService, SetPaymentPolicyRequest,
};
use crate::trigger_driver::TriggerDriverRegistry;
use crate::trigger_types::{
    CreateTriggerRequest, ExecuteTriggerRequest, ListTriggersRequest, TriggerRequest,
};
use crate::types::{
    DeployRequest, GatewayInvokeRequest, InvokeRequest, ListRequest, OperationRequest,
    ResourceRequest, Tenant,
};

pub const VERIFIED_PAYER_HEADER: &str = "x-pay-verified-payer";
pub const VERIFIED_CHANNEL_HEADER: &str = "x-pay-verified-channel";
pub const ORIGINAL_HOST_HEADER: &str = "x-pay-original-host";
pub const USAGE_HEADER: &str = "x-pay-gcp-cpu-microusd";
const MAX_GATEWAY_BODY_BYTES: usize = 10 * 1024 * 1024;
const TRIGGER_EXECUTOR_PROOF_HEADER: &str = "x-compute-trigger-proof";

#[derive(Clone)]
pub struct ComputeMcp {
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    drivers: DriverRegistry,
    trigger_drivers: TriggerDriverRegistry,
    bindings: Option<DataBindingClient>,
    payments: Option<PaymentService>,
}

impl ComputeMcp {
    pub fn new(
        drivers: DriverRegistry,
        trigger_drivers: TriggerDriverRegistry,
        bindings: Option<DataBindingClient>,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            drivers,
            trigger_drivers,
            bindings,
            payments: None,
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

    fn result<T: Serialize, E: std::fmt::Display>(
        result: Result<T, E>,
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
        description = "Delete a deployment payment policy using its observed version. Retains a revision tombstone; subsequent public resolution fails closed. Exact delete retries are idempotent."
    )]
    async fn delete_payment_policy(
        &self,
        Parameters(request): Parameters<DeletePaymentPolicyRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match &self.payments {
            Some(service) => service.delete(&tenant, request).await,
            None => Err(ComputeError::Configuration(
                "deployment payment policies are not configured".into(),
            )),
        };
        Self::result(result)
    }

    #[tool(
        description = "Set a deployment-owned MPP selling price and wallet-reference splits. Requires ownership of the active gateway deployment and all recipient wallets. Integer price_micro_usd: 50000 means $0.05; split basis_points: 3000 means 30%. expected_version=0 creates, later updates require the observed version."
    )]
    async fn set_payment_policy(
        &self,
        Parameters(request): Parameters<SetPaymentPolicyRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match &self.payments {
            Some(service) => service.set(&tenant, request).await,
            None => Err(ComputeError::Configuration(
                "deployment payment policies are not configured".into(),
            )),
        };
        Self::result(result)
    }

    #[tool(
        description = "Read the current payment policy and version for a payer-owned gateway deployment."
    )]
    async fn get_payment_policy(
        &self,
        Parameters(request): Parameters<GetPaymentPolicyRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match &self.payments {
            Some(service) => service.get(&tenant, &request.hostname).await,
            None => Err(ComputeError::Configuration(
                "deployment payment policies are not configured".into(),
            )),
        };
        Self::result(result)
    }

    #[tool(
        description = "List configured serverless compute drivers and their capabilities. Call this before deploying when provider, runtime, timeout, or source support is uncertain."
    )]
    async fn providers(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Self::result::<_, std::convert::Infallible>(Ok(serde_json::json!({
            "workloads": self.drivers.capabilities(),
            "triggers": self.trigger_drivers.capabilities(),
        })))
    }

    #[tool(
        description = "Create or update a payer-owned serverless workload. Source may be inline UTF-8 files or a base64 ZIP. Managed service bindings inject scoped data access. Gateway exposure creates a stable paid public hostname and exposes only access.public_paths, so internal trigger paths must be omitted. After deployment succeeds, use create_trigger to attach schedules or other invocation modes."
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
            return Self::result::<serde_json::Value, _>(Err(error));
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
        description = "Delete a payer-owned compute workload and all triggers attached to it. The returned workload operation is asynchronous; poll operation_status until it succeeds."
    )]
    async fn delete(
        &self,
        Parameters(request): Parameters<ResourceRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let driver = match self.drivers.get(&request.provider) {
            Ok(driver) => driver,
            Err(error) => return Self::result::<serde_json::Value, _>(Err(error)),
        };
        let deployment = match driver.get(&tenant, request.clone()).await {
            Ok(deployment) => deployment,
            Err(error) => return Self::result::<serde_json::Value, _>(Err(error)),
        };
        let canonical = ResourceRequest {
            provider: deployment.provider,
            id: deployment.id,
            region: Some(deployment.region),
        };
        if let Err(error) = self
            .trigger_drivers
            .cleanup_target(
                &tenant,
                &canonical.provider,
                &canonical.id,
                canonical.region.as_deref(),
            )
            .await
        {
            return Self::result::<serde_json::Value, _>(Err(error));
        }
        let result = driver.delete(&tenant, canonical).await;
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

    #[tool(
        description = "Attach or update a payer-owned trigger for an existing compute workload. A schedule trigger invokes only the target's private path, strips payment credentials, and prepays a bounded execution budget. Deploy the target workload and wait for operation_status to succeed before calling this tool."
    )]
    async fn create_trigger(
        &self,
        Parameters(request): Parameters<CreateTriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.create(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Get one payer-owned compute trigger.")]
    async fn get_trigger(
        &self,
        Parameters(request): Parameters<TriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.get(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "List compute triggers owned by the verified payer.")]
    async fn list_triggers(
        &self,
        Parameters(request): Parameters<ListTriggersRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.list(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Pause a payer-owned compute trigger.")]
    async fn pause_trigger(
        &self,
        Parameters(request): Parameters<TriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.pause(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(description = "Resume a payer-owned compute trigger with its remaining prepaid budget.")]
    async fn resume_trigger(
        &self,
        Parameters(request): Parameters<TriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.resume(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Run a payer-owned compute trigger immediately without changing its configuration."
    )]
    async fn run_trigger(
        &self,
        Parameters(request): Parameters<TriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.run_now(&tenant, request).await,
            Err(error) => Err(error),
        };
        Self::result(result)
    }

    #[tool(
        description = "Delete a payer-owned compute trigger and its remaining execution budget."
    )]
    async fn delete_trigger(
        &self,
        Parameters(request): Parameters<TriggerRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tenant = Self::tenant(&ctx)?;
        let result = match self.trigger_drivers.get(&request.driver) {
            Ok(driver) => driver.delete(&tenant, request).await,
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
            .with_instructions("Deploy payer-isolated serverless workloads and attach portable invocation triggers through pluggable provider drivers.")
    }
}

#[derive(Clone)]
struct AppState {
    drivers: DriverRegistry,
    trigger_drivers: TriggerDriverRegistry,
    gateway_domain: String,
    executor_proof: Vec<u8>,
}

pub fn router(
    drivers: DriverRegistry,
    trigger_drivers: TriggerDriverRegistry,
    gateway_domain: String,
    allowed_hosts: Vec<String>,
    bindings: Option<DataBindingClient>,
    executor_proof: Vec<u8>,
) -> Router {
    router_with_payments(
        drivers,
        trigger_drivers,
        gateway_domain,
        allowed_hosts,
        bindings,
        executor_proof,
        None,
    )
}

pub fn router_with_payments(
    drivers: DriverRegistry,
    trigger_drivers: TriggerDriverRegistry,
    gateway_domain: String,
    allowed_hosts: Vec<String>,
    bindings: Option<DataBindingClient>,
    executor_proof: Vec<u8>,
    payments: Option<PaymentService>,
) -> Router {
    let transport = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);
    let mcp_drivers = drivers.clone();
    let mcp_trigger_drivers = trigger_drivers.clone();
    let mcp_bindings = bindings.clone();
    let mcp_payments = payments.clone();
    let service: StreamableHttpService<ComputeMcp, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let mut mcp = ComputeMcp::new(
                    mcp_drivers.clone(),
                    mcp_trigger_drivers.clone(),
                    mcp_bindings.clone(),
                );
                mcp.payments = mcp_payments.clone();
                Ok(mcp)
            },
            Default::default(),
            transport,
        );
    let mcp = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(verified_tenant));
    let payment_routes = match payments {
        Some(payments) => payments.router(),
        None => Router::new().route(
            "/__402/payment-policy",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        ),
    };
    Router::new()
        .route("/__402/health", get(health))
        .route("/internal/triggers/run", post(execute_trigger))
        .merge(mcp)
        .fallback(gateway_invoke)
        .with_state(AppState {
            drivers,
            trigger_drivers,
            gateway_domain,
            executor_proof,
        })
        .merge(payment_routes)
}

async fn execute_trigger(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ExecuteTriggerRequest>,
) -> Response {
    let valid_proof = headers
        .get(TRIGGER_EXECUTOR_PROOF_HEADER)
        .is_some_and(|value| constant_time_eq(value.as_bytes(), &state.executor_proof));
    if !valid_proof {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "trigger_executor_proof_required" })),
        )
            .into_response();
    }
    let driver = match state.trigger_drivers.get(&request.driver) {
        Ok(driver) => driver,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    match driver.execute(request).await {
        Ok(execution) => {
            let mut response = Response::builder().status(execution.status);
            for (name, value) in execution.headers {
                response = response.header(name, value);
            }
            response
                .body(Body::from(execution.body))
                .unwrap_or_else(|error| {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({ "error": error.to_string() })),
                    )
                        .into_response()
                })
        }
        Err(error) => (
            StatusCode::PAYMENT_REQUIRED,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn verified_tenant(mut request: Request, next: Next) -> Response {
    let tenant = request
        .headers()
        .get(VERIFIED_PAYER_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|payer| {
            let channel_id = request
                .headers()
                .get(VERIFIED_CHANNEL_HEADER)?
                .to_str()
                .ok()?;
            tenant_from_verified_session(payer, channel_id).ok()
        });
    match tenant {
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

fn tenant_from_verified_session(payer: &str, channel_id: &str) -> crate::driver::Result<Tenant> {
    let mut tenant = tenant_from_payer(payer)?;
    tenant.channel_id = validate_channel_id(channel_id)?.to_string();
    Ok(tenant)
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
        channel_id: String::new(),
    })
}

fn validate_channel_id(channel_id: &str) -> crate::driver::Result<&str> {
    if channel_id.is_empty()
        || channel_id.len() > 128
        || !channel_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(ComputeError::InvalidRequest(
            "verified payment channel is invalid".into(),
        ));
    }
    Ok(channel_id)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

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
        let result = ComputeMcp::result::<serde_json::Value, std::convert::Infallible>(Ok(
            serde_json::json!({ "value": 42 }),
        ))
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

    #[tokio::test]
    async fn trigger_executor_requires_the_internal_proof() {
        let app = router(
            DriverRegistry::default(),
            TriggerDriverRegistry::default(),
            "cpu.example.invalid".into(),
            Vec::new(),
            None,
            b"correct-proof-that-is-at-least-32-bytes".to_vec(),
        );
        let body = serde_json::json!({
            "driver": "missing",
            "trigger_resource": "projects/project/locations/us-central1/jobs/trigger-test",
            "channel_lease": "lease",
            "target_resource": "projects/project/locations/us-central1/functions/gcf-tenant-test",
            "tenant_key": "tenant",
            "path": "/refresh"
        })
        .to_string();

        let unauthorized = app
            .clone()
            .oneshot(
                Request::post("/internal/triggers/run")
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::post("/internal/triggers/run")
                    .header("content-type", "application/json")
                    .header(
                        TRIGGER_EXECUTOR_PROOF_HEADER,
                        "correct-proof-that-is-at-least-32-bytes",
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::BAD_REQUEST);
    }
}
