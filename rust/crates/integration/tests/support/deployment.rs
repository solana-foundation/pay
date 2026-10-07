//! Local-only transport fixture. The runner owns Redis and a Surfpool validator
//! loaded with the real payment-channels program; this module never starts a
//! remote service, substitutes a verifier, or seeds channel state.
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pay_core::{
    PaymentState,
    server::{
        deployment_policy::DeploymentPolicyResolver,
        session::{DeploymentSessionStore, SessionLifecycleReconciliation, SessionMpp},
    },
};
use pay_kit::mpp::{
    server::session::SessionConfig,
    solana_keychain::TransactionSigner,
    store::{ChannelState, ChannelStore, RedisChannelStore},
};
use pay_types::metering::ApiSpec;
use pingora::{
    proxy::http_proxy_service,
    server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch},
};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

pub const DOMAIN: &str = "gateway.example";
pub const AUDIENCE: &str = "https://policy.example";
pub const HOST_A: &str = "gcf-0123456789abcdef-alpha.gateway.example";
pub const HOST_B: &str = "gcf-fedcba9876543210-beta.gateway.example";
pub const BODY: &str = "generic paid deployment response";
pub const SECRET: &str = "local-deployment-e2e-challenge-binding-secret";

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub struct LocalEndpoints {
    pub rpc: String,
    pub redis: String,
}

impl LocalEndpoints {
    pub fn from_env() -> Self {
        Self {
            rpc: required_loopback_url("PAY_DEPLOYMENT_TEST_RPC_URL", "http"),
            redis: required_loopback_url("PAY_DEPLOYMENT_TEST_REDIS_URL", "redis"),
        }
    }
}

fn required_loopback_url(name: &str, scheme: &str) -> String {
    let value = std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required when deployment-e2e is selected"));
    let url = reqwest::Url::parse(&value).unwrap_or_else(|_| panic!("{name} must be a valid URL"));
    // Reject DNS names (including localhost), credentials, and implicit ports.
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
    let loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    assert!(
        url.scheme() == scheme
            && loopback
            && url.port().is_some_and(|port| port != 0)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{name} must name an explicit literal-loopback {scheme} endpoint"
    );
    value
}

pub fn policy(host: &str, price: u64, recipient: &str, version: u64) -> Value {
    let id = host.split('.').next().unwrap();
    let owner = id.strip_prefix("gcf-").unwrap().split('-').next().unwrap();
    json!({
        "deployment": {
            "owner_key": owner,
            "resource_name": format!("projects/local/locations/local/functions/{id}"),
            "created_at": "2026-01-01T00:00:00Z",
            "hostname": host
        },
        "version": version,
        "price_micro_usd": price,
        "expires_at": now() + 3600,
        "schemes": ["mpp-session"],
        "allocations": [{
            "recipient": recipient,
            "amount_micro_usd": price,
            "basis_points": 10000
        }]
    })
}

#[derive(Clone)]
pub struct Controls {
    pub policies: Arc<RwLock<HashMap<String, Value>>>,
    pub policy_status: Arc<RwLock<StatusCode>>,
    pub metadata_expiry: Arc<RwLock<u64>>,
    pub upstream_status: Arc<RwLock<StatusCode>>,
    pub upstream_content_type: Arc<RwLock<Option<String>>>,
}

impl Controls {
    pub fn set_policy(&self, host: &str, policy: Value) {
        self.policies.write().unwrap().insert(host.into(), policy);
    }
}

fn token(expiry: u64) -> String {
    format!(
        "{}.{}.local-fixture-signature",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"aud": AUDIENCE, "exp": expiry})).unwrap())
    )
}

async fn metadata(
    State(controls): State<Controls>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if query.get("audience").map(String::as_str) != Some(AUDIENCE)
        || headers.get("metadata-flavor").and_then(|v| v.to_str().ok()) != Some("Google")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    token(*controls.metadata_expiry.read().unwrap()).into_response()
}

async fn resolve_policy(
    State(controls): State<Controls>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let expected = format!(
        "Bearer {}",
        token(*controls.metadata_expiry.read().unwrap())
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let status = *controls.policy_status.read().unwrap();
    if status != StatusCode::OK {
        return status.into_response();
    }
    if query.get("path").map(String::as_str) != Some("/invoke") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let policies = controls.policies.read().unwrap();
    match query.get("hostname").and_then(|host| policies.get(host)) {
        Some(policy) => axum::Json(policy.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn upstream(State(controls): State<Controls>) -> Response {
    let mut response = (*controls.upstream_status.read().unwrap(), BODY).into_response();
    if let Some(content_type) = controls.upstream_content_type.read().unwrap().as_ref() {
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            content_type.parse().unwrap(),
        );
    }
    response
}

struct HttpServer {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HttpServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .expect("fixture HTTP server");
        });
        Self {
            addr,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct GatewayState {
    pub apis: Vec<ApiSpec>,
    pub session: Arc<SessionMpp>,
}

impl PaymentState for GatewayState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
        None
    }
    fn session_mpp(&self) -> Option<&SessionMpp> {
        Some(&self.session)
    }
    fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
        Some(self.session.clone())
    }
}

struct Stop(watch::Receiver<bool>);

#[async_trait::async_trait]
impl ShutdownSignalWatch for Stop {
    async fn recv(&self) -> ShutdownSignal {
        let mut stop = self.0.clone();
        while !*stop.borrow() {
            if stop.changed().await.is_err() {
                break;
            }
        }
        ShutdownSignal::FastShutdown
    }
}

pub struct Gateway {
    pub url: String,
    stop: watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Gateway {
    async fn start(state: GatewayState, resolver: DeploymentPolicyResolver) -> Self {
        pay_proxy::install_crypto_provider();
        // Pingora owns its listener. Reserve then release an ephemeral port and
        // surface startup failure rather than retrying on a different address.
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reservation.local_addr().unwrap();
        let (stop, stopped) = watch::channel(false);
        drop(reservation);
        let thread = std::thread::spawn(move || {
            let mut server = Server::new(None).expect("Pingora server");
            server.bootstrap();
            let gate = pay_proxy::http402::Http402Gate::new(state, "127.0.0.1:1")
                .with_deployment_policy_resolver(Some(Arc::new(resolver)));
            let mut service = http_proxy_service(&server.configuration, gate);
            service.threads = Some(1);
            service.add_tcp(&addr.to_string());
            server.add_service(service);
            server.run(RunArgs {
                shutdown_signal: Box::new(Stop(stopped)),
            });
        });
        let gateway = Self {
            url: format!("http://{addr}"),
            stop,
            thread: Some(thread),
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    !gateway.thread.as_ref().unwrap().is_finished(),
                    "Pingora exited during startup"
                );
                if tokio::net::TcpStream::connect(addr).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("Pingora did not start");
        gateway
    }

    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().expect("Pingora thread"))
            .await
            .unwrap();
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Harness {
    pub endpoints: LocalEndpoints,
    pub redis_prefix: String,
    pub controls: Controls,
    pub config: SessionConfig,
    pub operator: Arc<dyn TransactionSigner>,
    pub state: GatewayState,
    pub gateway: Gateway,
    pub client: reqwest::Client,
    servers: Vec<HttpServer>,
    resolver: DeploymentPolicyResolver,
}

impl Harness {
    pub async fn start(
        endpoints: LocalEndpoints,
        config: SessionConfig,
        operator: Arc<dyn TransactionSigner>,
    ) -> Self {
        assert_eq!(config.rpc_url.as_deref(), Some(endpoints.rpc.as_str()));
        let controls = Controls {
            policies: Default::default(),
            policy_status: Arc::new(RwLock::new(StatusCode::OK)),
            metadata_expiry: Arc::new(RwLock::new(now() + 3600)),
            upstream_status: Arc::new(RwLock::new(StatusCode::OK)),
            upstream_content_type: Default::default(),
        };
        let metadata = HttpServer::start(
            Router::new()
                .route(
                    "/computeMetadata/v1/instance/service-accounts/default/identity",
                    get(metadata),
                )
                .with_state(controls.clone()),
        )
        .await;
        let policy = HttpServer::start(
            Router::new()
                .route("/__402/payment-policy", get(resolve_policy))
                .with_state(controls.clone()),
        )
        .await;
        let upstream = HttpServer::start(
            Router::new()
                .route("/invoke", get(upstream))
                .with_state(controls.clone()),
        )
        .await;
        let resolver = DeploymentPolicyResolver::new(
            &format!("{AUDIENCE}/__402/payment-policy"),
            AUDIENCE,
            DOMAIN,
        )
        .unwrap()
        .with_loopback_test_endpoints(policy.addr, metadata.addr)
        .unwrap();
        let redis_prefix = format!("pay:deployment-e2e:{}:", uuid::Uuid::new_v4());
        let session =
            connect_session(&endpoints.redis, &redis_prefix, &config, operator.clone()).await;
        let api = serde_json::from_value(json!({
            "name": "deployment", "subdomain": "deployment", "title": "Deployment",
            "description": "Generic deployment fixture", "category": "ai_ml", "version": "v1",
            "routing": {"type": "proxy", "url": format!("http://{}/", upstream.addr)}
        }))
        .unwrap();
        let state = GatewayState {
            apis: vec![api],
            session,
        };
        let gateway = Gateway::start(state.clone(), resolver.clone()).await;
        Self {
            endpoints,
            redis_prefix,
            controls,
            config,
            operator,
            state,
            gateway,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            servers: vec![metadata, policy, upstream],
            resolver,
        }
    }

    pub async fn request(&self, host: &str, authorization: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .get(format!("{}/invoke", self.gateway.url))
            .header("host", host);
        if let Some(auth) = authorization {
            request = request.header("authorization", auth);
        }
        request.send().await.expect("gateway HTTP request")
    }

    /// Read authoritative Redis records through a fresh connection, never the
    /// gateway's in-process cache. No fixture writes are allowed to this store.
    pub async fn channel(&self, id: &str) -> ChannelState {
        RedisChannelStore::connect(&self.endpoints.redis, &self.redis_prefix)
            .await
            .unwrap()
            .list_channels()
            .await
            .unwrap()
            .into_iter()
            .find(|channel| channel.channel_id == id)
            .expect("persisted channel")
    }

    pub async fn reconnect(&mut self) {
        let session = connect_session(
            &self.endpoints.redis,
            &self.redis_prefix,
            &self.config,
            self.operator.clone(),
        )
        .await;
        self.state.session = session;
        let next = Gateway::start(self.state.clone(), self.resolver.clone()).await;
        std::mem::replace(&mut self.gateway, next).shutdown().await;
    }

    pub async fn shutdown(self) {
        self.gateway.shutdown().await;
        for server in self.servers {
            server.shutdown().await;
        }
    }
}

async fn connect_session(
    redis: &str,
    prefix: &str,
    config: &SessionConfig,
    operator: Arc<dyn TransactionSigner>,
) -> Arc<SessionMpp> {
    let store = DeploymentSessionStore::connect(Some(redis), prefix)
        .await
        .expect("deployment Redis connection");
    let session = Arc::new(
        SessionMpp::new_for_deployment(config.clone(), SECRET, &store)
            .expect("deployment session")
            .with_payment_channel_signer(operator),
    );
    session.start_lifecycle_runloop_with_settlement_and_batching(
        Duration::from_secs(2),
        Duration::from_secs(1),
        Duration::ZERO,
        SessionLifecycleReconciliation::External,
    );
    session
}
