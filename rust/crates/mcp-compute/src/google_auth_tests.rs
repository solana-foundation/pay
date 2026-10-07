//! Transport regressions: these fixtures are not a simulation of Cloud Run's
//! signature stripping, and do not establish live provider token replay.
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request};
use axum::response::Response;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::google::{GoogleCloudFunctionsDriver, GoogleConfig};
use crate::trigger_google::{GoogleTriggerConfig, GoogleTriggerDriver};
use crate::trigger_types::ExecuteTriggerRequest;
use crate::types::Tenant;
use crate::{ComputeDriver, TriggerDriver};

const TENANT: &str = "0123456789abcdef";
const RESOURCE: &str =
    "projects/project/locations/us-central1/functions/gcf-0123456789abcdef-weather";
const TRIGGER: &str = "projects/project/locations/us-central1/jobs/trigger-test";
const TOKEN: &str = "fixture-platform-identity-token";

struct Fixture {
    base: String,
    requests: mpsc::UnboundedReceiver<HeaderMap>,
    server: tokio::task::JoinHandle<()>,
    tls_server: Option<tokio::task::JoinHandle<()>>,
    certificate: Option<reqwest::Certificate>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(server) = &self.tls_server {
            server.abort();
        }
    }
}

struct TlsListener {
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = self.listener.accept().await.unwrap();
        (self.acceptor.accept(stream).await.unwrap(), address)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

impl Fixture {
    async fn new(tls: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut origin = base.clone();
        let mut certificate = None;
        let tls_listener = if tls {
            let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            certificate = Some(reqwest::Certificate::from_der(generated.cert.der()).unwrap());
            let config = tokio_rustls::rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![generated.cert.der().clone()],
                    tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                        generated.signing_key.serialize_der(),
                    )
                    .into(),
                )
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            origin = format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            );
            Some(TlsListener {
                listener,
                acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            })
        } else {
            None
        };
        let (sender, requests) = mpsc::unbounded_channel();
        let app = Router::new().fallback(move |request: Request<Body>| {
            let origin = origin.clone();
            let sender = sender.clone();
            async move {
                match request.uri().path() {
                    "/instance/service-accounts/default/identity" => {
                        assert_eq!(request.headers()["metadata-flavor"], "Google");
                        Response::new(Body::from(TOKEN))
                    }
                    path if path == format!("/v2/{RESOURCE}") => {
                        assert_eq!(
                            request.headers()["authorization"],
                            "Bearer management-token"
                        );
                        Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from(
                                json!({
                                    "name": RESOURCE,
                                    "state": "ACTIVE",
                                    "labels": {
                                        "managed-by": "mcp-compute",
                                        "pay-tenant": TENANT
                                    },
                                    "serviceConfig": {
                                        "uri": origin,
                                        "timeoutSeconds": 30,
                                        "availableMemory": "256M",
                                        "availableCpu": "1"
                                    }
                                })
                                .to_string(),
                            ))
                            .unwrap()
                    }
                    "/invoke" => {
                        sender.send(request.headers().clone()).unwrap();
                        Response::new(Body::from("ok"))
                    }
                    path => panic!("unexpected fixture path: {path}"),
                }
            }
        });
        let tls_server = tls_listener.map(|listener| {
            let app = app.clone();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            requests,
            server,
            tls_server,
            certificate,
        }
    }

    async fn assert_invocation(&mut self, authenticated: bool) {
        let headers = tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!headers.contains_key("authorization"));
        let tokens: Vec<_> = headers
            .get_all("x-serverless-authorization")
            .iter()
            .collect();
        if authenticated {
            assert_eq!(tokens.len(), 1, "caller tokens must not be appended");
            assert_eq!(tokens[0], format!("Bearer {TOKEN}").as_str());
        } else {
            assert!(tokens.is_empty());
        }
        assert_eq!(headers["x-application-header"], "preserved");
    }
}

fn caller_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("AuThOrIzAtIoN".into(), "Bearer caller-ordinary".into()),
        (
            "X-SeRvErLeSs-AuThOrIzAtIoN".into(),
            "Bearer caller-dedicated".into(),
        ),
        (
            "x-serverless-authorization".into(),
            "Bearer caller-duplicate".into(),
        ),
        ("x-application-header".into(), "preserved".into()),
    ])
}

#[tokio::test]
async fn direct_invocation_isolates_platform_authentication() {
    let mut fixture = Fixture::new(false).await;
    for unauthenticated in [false, true] {
        let driver = GoogleCloudFunctionsDriver::new(GoogleConfig {
            project: "project".into(),
            default_region: "us-central1".into(),
            api_base: fixture.base.clone(),
            metadata_base: fixture.base.clone(),
            access_token: Some("management-token".into()),
            identity_token: None,
            allow_unauthenticated_invoke: unauthenticated,
            function_service_account: None,
            build_service_account: None,
            gateway_domain: "cpu.example.invalid".into(),
            payment_policy_database: None,
        })
        .unwrap();
        let tenant = Tenant {
            payer: "payer".into(),
            key: TENANT.into(),
            channel_id: "channel".into(),
        };
        let request = serde_json::from_value(json!({
            "id": RESOURCE,
            "method": "POST",
            "path": "/invoke",
            "headers": caller_headers(),
            "body": {"hello": "world"}
        }))
        .unwrap();
        assert_eq!(driver.invoke(&tenant, request).await.unwrap().status, 200);
        fixture.assert_invocation(!unauthenticated).await;
    }
}

struct RedisProcess(Child);

impl Drop for RedisProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "requires redis-server on PATH; run with --ignored"]
async fn scheduled_invocation_isolates_platform_authentication() {
    let mut fixture = Fixture::new(true).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut redis = RedisProcess(
        Command::new("redis-server")
            .args([
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--save",
                "",
                "--appendonly",
                "no",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("redis-server must be installed"),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                redis.0.try_wait().unwrap().is_none(),
                "Redis exited before startup"
            );
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let redis_url = format!("redis://127.0.0.1:{port}");
    let mut connection = redis::Client::open(redis_url.clone())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let key = format!(
        "pay:compute:trigger-budget:{:x}",
        Sha256::digest(TRIGGER.as_bytes())
    );
    redis::cmd("HSET")
        .arg(&key)
        .arg("channel")
        .arg("lease")
        .arg("remaining")
        .arg(1)
        .arg("expires_at")
        .arg(i64::MAX)
        .query_async::<()>(&mut connection)
        .await
        .unwrap();
    let mut driver = GoogleTriggerDriver::new(GoogleTriggerConfig {
        project: "project".into(),
        default_region: "us-central1".into(),
        scheduler_api_base: fixture.base.clone(),
        functions_api_base: fixture.base.clone(),
        metadata_base: fixture.base.clone(),
        access_token: Some("management-token".into()),
        redis_url,
        executor_url: format!("{}/executor", fixture.base),
        executor_proof: "fixture-proof-long-enough-for-validation".into(),
    })
    .await
    .unwrap();
    driver.client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(fixture.certificate.clone().unwrap())
        .build()
        .unwrap();
    let response = driver
        .execute(ExecuteTriggerRequest {
            driver: crate::trigger_google::DRIVER_ID.into(),
            trigger_resource: TRIGGER.into(),
            channel_lease: "lease".into(),
            target_resource: RESOURCE.into(),
            tenant_key: TENANT.into(),
            path: "/invoke".into(),
            input: json!({"hello": "world"}),
            headers: caller_headers(),
        })
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    fixture.assert_invocation(true).await;
    let remaining: i64 = redis::cmd("HGET")
        .arg(key)
        .arg("remaining")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}
