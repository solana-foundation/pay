//! HTTP fixtures exercise the real driver and CRUD through Firestore CAS.

use super::*;
use crate::payment_policy::{PaymentPolicy, PaymentPolicySpec, WalletReference};
use crate::payment_service::{DeletePaymentPolicyRequest, PaymentService, SetPaymentPolicyRequest};
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use std::sync::Mutex;
use tokio::sync::Notify;

const OWNER: &str = "0123456789abcdef";
const CREATED: &str = "2025-01-01T00:00:00Z";
const COLLECTION: &str =
    "projects/project/databases/policies/documents/pay_compute_payment_policies";

#[derive(Clone, Default)]
struct Pause {
    reached: Arc<Notify>,
    resume: Arc<Notify>,
}

impl Pause {
    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.reached.notified())
            .await
            .unwrap();
    }
}

#[derive(Default)]
struct FixtureState {
    documents: BTreeMap<String, Value>,
    functions: BTreeMap<String, Value>,
    listed_functions: Option<Vec<Value>>,
    provider_get_error: Option<StatusCode>,
    provider_delete_error: Option<StatusCode>,
    deleting: bool,
    fail_policy_writes: usize,
    fail_after_provider_delete: bool,
    conflict_purge: bool,
    revision: u64,
    actions: Vec<String>,
    pause_wallet: Option<Pause>,
    pause_policy_patch: Option<Pause>,
}

struct Fixture {
    driver: GoogleCloudFunctionsDriver,
    service: PaymentService,
    state: Arc<Mutex<FixtureState>>,
    server: tokio::task::JoinHandle<()>,
    identity: DeploymentIdentity,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn identity(name: &str, created: &str) -> DeploymentIdentity {
    DeploymentIdentity {
        owner_key: OWNER.into(),
        resource_name: format!(
            "projects/project/locations/us-central1/functions/gcf-{OWNER}-{name}"
        ),
        hostname: format!("gcf-{OWNER}-{name}.compute.example"),
        created_at: created.into(),
    }
}

fn function(identity: &DeploymentIdentity) -> Value {
    json!({
        "name": identity.resource_name, "createTime": identity.created_at, "state": "ACTIVE",
        "labels": {
            "managed-by": "mcp-compute", "pay-tenant": identity.owner_key,
            "pay-exposure": "gateway", "pay-channel": channel_lease_key("channel").unwrap()
        },
        "serviceConfig": {"environmentVariables": {"PAY_INTERNAL_PUBLIC_PATHS": "[\"/summary\"]"}}
    })
}

fn policy(identity: &DeploymentIdentity) -> PaymentPolicy {
    PaymentPolicy {
        deployment: identity.clone(),
        version: 1,
        deleted: false,
        spec: PaymentPolicySpec {
            price_micro_usd: 50_000,
            schemes: vec!["mpp-session".into()],
            primary_recipient: WalletReference {
                driver: "privy".into(),
                name: "seller".into(),
            },
            splits: vec![],
            expires_at: policy_now().unwrap() + 3600,
        },
    }
}

fn document_name(identity: &DeploymentIdentity) -> String {
    format!("{COLLECTION}/{}", PolicyRepository::key(identity))
}

fn seed(state: &mut FixtureState, policy: &PaymentPolicy) {
    let name = document_name(&policy.deployment);
    state.revision += 1;
    state.documents.insert(
        name.clone(),
        json!({
            "name": name,
            "updateTime": format!("revision{}", state.revision),
            // Existing documents do not have the new lifecycle fields.
            "fields": {"policy": {"stringValue": serde_json::to_string(policy).unwrap()}}
        }),
    );
}

fn response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

async fn serve_fixture(state: Arc<Mutex<FixtureState>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query: BTreeMap<String, String> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let body = axum::body::to_bytes(request.into_body(), 128 * 1024)
        .await
        .unwrap();
    let body: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let pause = {
        let mut state = state.lock().unwrap();
        if path == "/__402/resolve-recipients" {
            state.pause_wallet.take()
        } else if path.contains("/pay_compute_payment_policies/") && method == Method::PATCH {
            state.pause_policy_patch.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.resume.notified().await;
    }
    let mut state = state.lock().unwrap();
    state.actions.push(format!("{method} {path}"));
    if path == "/__402/resolve-recipients" {
        let wallets: Vec<Value> = body["wallets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| {
                json!({
                    "owner_key": OWNER, "reference": reference, "chain": "solana",
                    "address": bs58::encode([1u8; 32]).into_string(),
                })
            })
            .collect();
        return Json(wallets).into_response();
    }
    if let Some(resource) = path.strip_prefix("/v2/") {
        if resource == "projects/project/locations/-/functions" {
            let functions = state
                .listed_functions
                .clone()
                .unwrap_or_else(|| state.functions.values().cloned().collect());
            return Json(json!({"functions": functions})).into_response();
        }
        if method == Method::GET {
            if let Some(status) = state.provider_get_error {
                return response(status, json!({"error": {"status": "PERMISSION_DENIED"}}));
            }
            return state
                .functions
                .get(resource)
                .cloned()
                .map(|value| Json(value).into_response())
                .unwrap_or_else(|| response(StatusCode::NOT_FOUND, json!({})));
        }
        if method == Method::DELETE {
            // This assertion is in the transport: the real driver must have
            // persisted its fence before a provider deletion can be observed.
            let value = state.functions.get(resource).unwrap();
            let identity = DeploymentIdentity {
                owner_key: OWNER.into(),
                resource_name: resource.into(),
                created_at: value["createTime"].as_str().unwrap().into(),
                hostname: format!("{}.compute.example", resource.rsplit('/').next().unwrap()),
            };
            let document = state.documents.get(&document_name(&identity)).unwrap();
            assert!(PolicyRepository::decode(document).unwrap().retired);
            if let Some(status) = state.provider_delete_error {
                return response(status, json!({"error": {"status": "UNAVAILABLE"}}));
            }
            if state.deleting {
                state.functions.get_mut(resource).unwrap()["state"] = json!("DELETING");
            } else {
                state.functions.remove(resource);
            }
            if state.fail_after_provider_delete {
                state.fail_policy_writes += 1;
                state.fail_after_provider_delete = false;
            }
            return Json(
                json!({"name": "projects/project/locations/us-central1/operations/deletion"}),
            )
            .into_response();
        }
    }
    let name = path
        .strip_prefix("/v1/")
        .expect("fixture received unexpected URL");
    if method == Method::GET && name == COLLECTION {
        let after = query.get("pageToken").map(String::as_str).unwrap_or("");
        let page_size: usize = query["pageSize"].parse().unwrap();
        let all: Vec<_> = state
            .documents
            .iter()
            .filter(|(name, _)| {
                name.starts_with(&format!("{COLLECTION}/")) && name.as_str() > after
            })
            .collect();
        let documents: Vec<_> = all
            .iter()
            .take(page_size)
            .map(|(_, value)| (*value).clone())
            .collect();
        let next = (all.len() > page_size).then(|| all[page_size - 1].0.clone());
        let mut result = json!({"documents": documents});
        if let Some(next) = next {
            result["nextPageToken"] = json!(next);
        }
        return Json(result).into_response();
    }
    if method == Method::GET {
        return state
            .documents
            .get(name)
            .cloned()
            .map(|value| Json(value).into_response())
            .unwrap_or_else(|| response(StatusCode::NOT_FOUND, json!({})));
    }
    if method == Method::PATCH || method == Method::DELETE {
        let is_policy = name.starts_with(&format!("{COLLECTION}/"));
        if is_policy && method == Method::PATCH && state.fail_policy_writes > 0 {
            state.fail_policy_writes -= 1;
            return response(StatusCode::SERVICE_UNAVAILABLE, json!({}));
        }
        if is_policy && method == Method::DELETE && state.conflict_purge {
            state.conflict_purge = false;
            state.documents.get_mut(name).unwrap()["updateTime"] = json!("concurrent-revision");
        }
        let matches = match state.documents.get(name) {
            Some(value) => {
                query.get("currentDocument.updateTime").map(String::as_str)
                    == value.get("updateTime").and_then(Value::as_str)
            }
            None => query.get("currentDocument.exists").map(String::as_str) == Some("false"),
        };
        if !matches {
            return response(
                StatusCode::BAD_REQUEST,
                json!({"error": {"status": "FAILED_PRECONDITION"}}),
            );
        }
        if method == Method::DELETE {
            assert!(query.contains_key("currentDocument.updateTime"));
            state.documents.remove(name);
            return Json(json!({})).into_response();
        }
        state.revision += 1;
        let mut value = body;
        value["name"] = json!(name);
        value["updateTime"] = json!(format!("revision{}", state.revision));
        state.documents.insert(name.into(), value.clone());
        return Json(value).into_response();
    }
    panic!("unexpected fixture request: {method} {path}");
}

impl Fixture {
    async fn new(with_policy: bool) -> Self {
        let identity = identity("weather", CREATED);
        let mut initial = FixtureState::default();
        initial
            .functions
            .insert(identity.resource_name.clone(), function(&identity));
        if with_policy {
            seed(&mut initial, &policy(&identity));
        }
        let state = Arc::new(Mutex::new(initial));
        let handler_state = state.clone();
        let app = Router::new()
            .fallback(move |request: Request| serve_fixture(handler_state.clone(), request));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut driver = GoogleCloudFunctionsDriver::new(GoogleConfig {
            project: "project".into(),
            default_region: "us-central1".into(),
            api_base: base.clone(),
            metadata_base: "http://metadata.invalid".into(),
            access_token: Some("fixture".into()),
            identity_token: Some("fixture".into()),
            allow_unauthenticated_invoke: false,
            function_service_account: None,
            build_service_account: None,
            gateway_domain: "compute.example".into(),
            payment_policy_database: Some("policies".into()),
        })
        .unwrap();
        driver.policies = Some(PolicyRepository::new(format!("{base}/v1/{COLLECTION}")).unwrap());
        let service = PaymentService::for_test(driver.clone(), base);
        Self {
            driver,
            service,
            state,
            server,
            identity,
        }
    }

    fn tenant(&self) -> Tenant {
        Tenant {
            payer: "payer".into(),
            key: OWNER.into(),
            channel_id: "channel".into(),
        }
    }

    fn request(&self) -> ResourceRequest {
        ResourceRequest {
            provider: DRIVER_ID.into(),
            id: self.identity.resource_name.clone(),
            region: None,
        }
    }

    fn set_request(&self, expected_version: u64) -> SetPaymentPolicyRequest {
        SetPaymentPolicyRequest {
            hostname: self.identity.hostname.clone(),
            expected_version,
            policy: policy(&self.identity).spec,
        }
    }

    fn stored(&self) -> Option<StoredPolicy> {
        self.state
            .lock()
            .unwrap()
            .documents
            .get(&document_name(&self.identity))
            .map(|value| PolicyRepository::decode(value).unwrap())
    }

    fn expire(&self, identity: &DeploymentIdentity) {
        self.state
            .lock()
            .unwrap()
            .documents
            .get_mut(&document_name(identity))
            .unwrap()["fields"]["absent_since"] =
            json!({"integerValue": (policy_now().unwrap() - RETENTION_SECONDS - 1).to_string()});
    }
}

#[tokio::test]
async fn explicit_delete_retires_before_provider_and_retry_is_idempotent() {
    let fixture = Fixture::new(true).await;
    let before = fixture.stored().unwrap().policy.unwrap();
    fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    let retired = fixture.stored().unwrap();
    assert!(retired.retired);
    assert!(retired.absent_since.is_some());
    let tombstone = retired.policy.unwrap();
    assert!(tombstone.deleted);
    assert_eq!(tombstone.version, 2);
    assert_eq!(tombstone.deployment, before.deployment);
    assert_eq!(tombstone.spec, before.spec);
    let retry = fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    assert!(matches!(retry.state, OperationState::Succeeded));
    assert_eq!(fixture.stored().unwrap().policy.unwrap().version, 2);
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|action| action.contains("resolve-recipients"))
    );
}

#[tokio::test]
async fn real_channel_cleanup_retires_policy_without_wallet_resolution() {
    let fixture = Fixture::new(true).await;
    assert_eq!(fixture.driver.cleanup_channel("channel").await.unwrap(), 1);
    assert!(fixture.stored().unwrap().retired);
    assert_eq!(fixture.driver.cleanup_channel("channel").await.unwrap(), 0);
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|action| action.contains("resolve-recipients"))
    );
}

#[tokio::test]
async fn failures_before_and_after_provider_delete_retain_a_permanent_fence() {
    let fixture = Fixture::new(true).await;
    fixture.state.lock().unwrap().fail_policy_writes = 1;
    assert!(
        fixture
            .driver
            .delete(&fixture.tenant(), fixture.request())
            .await
            .is_err()
    );
    assert!(!fixture.stored().unwrap().retired);
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|action| action.starts_with("DELETE /v2/"))
    );
    fixture.state.lock().unwrap().provider_delete_error = Some(StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        fixture
            .driver
            .delete(&fixture.tenant(), fixture.request())
            .await
            .is_err()
    );
    assert!(fixture.stored().unwrap().retired);
    assert!(fixture.stored().unwrap().absent_since.is_none());
    assert!(
        fixture
            .service
            .set(&fixture.tenant(), fixture.set_request(2))
            .await
            .is_err()
    );
    {
        let mut state = fixture.state.lock().unwrap();
        state.provider_delete_error = None;
        state.fail_after_provider_delete = true;
    }
    assert!(
        fixture
            .driver
            .delete(&fixture.tenant(), fixture.request())
            .await
            .is_err()
    );
    assert!(fixture.stored().unwrap().absent_since.is_none());
    fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    fixture.driver.reconcile_orphans(false).await.unwrap();
    assert!(fixture.stored().unwrap().absent_since.is_some());
    assert_eq!(fixture.stored().unwrap().policy.unwrap().version, 2);
}

#[tokio::test]
async fn asynchronous_deletion_does_not_start_retention_until_verified_absence() {
    let fixture = Fixture::new(true).await;
    fixture.state.lock().unwrap().deleting = true;
    fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    assert!(fixture.stored().unwrap().absent_since.is_none());
    assert_eq!(fixture.driver.reconcile_orphans(false).await.unwrap(), 0);
    assert!(fixture.stored().unwrap().absent_since.is_none());
    fixture.state.lock().unwrap().functions.clear();
    assert_eq!(fixture.driver.reconcile_orphans(false).await.unwrap(), 1);
    fixture.expire(&fixture.identity);
    fixture.state.lock().unwrap().conflict_purge = true;
    assert!(fixture.driver.reconcile_orphans(false).await.is_err());
    assert!(fixture.stored().is_some(), "purge must compare updateTime");
    fixture.driver.reconcile_orphans(false).await.unwrap();
    assert!(fixture.stored().is_none());
}

#[tokio::test]
async fn dry_run_validates_and_reports_but_writes_neither_policy_nor_checkpoint() {
    let fixture = Fixture::new(true).await;
    fixture.state.lock().unwrap().functions.clear();
    let before = fixture.state.lock().unwrap().documents.clone();
    assert_eq!(fixture.driver.reconcile_orphans(true).await.unwrap(), 1);
    assert_eq!(fixture.state.lock().unwrap().documents, before);
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .actions
            .iter()
            .all(|action| action.starts_with("GET "))
    );
}

#[tokio::test]
async fn recreated_function_keeps_its_new_policy_while_old_incarnation_is_reclaimed() {
    let fixture = Fixture::new(true).await;
    let newer = identity("weather", "2025-02-01T00:00:00Z");
    {
        let mut state = fixture.state.lock().unwrap();
        state
            .functions
            .insert(newer.resource_name.clone(), function(&newer));
        seed(&mut state, &policy(&newer));
    }
    fixture.driver.reconcile_orphans(false).await.unwrap();
    assert!(fixture.stored().unwrap().retired);
    fixture.expire(&fixture.identity);
    fixture.driver.reconcile_orphans(false).await.unwrap();
    assert!(fixture.stored().is_none());
    let state = fixture.state.lock().unwrap();
    let new_policy = PolicyRepository::decode(&state.documents[&document_name(&newer)]).unwrap();
    assert!(!new_policy.retired);
    assert_eq!(new_policy.policy.unwrap().version, 1);
    assert!(state.functions.contains_key(&newer.resource_name));
    assert!(
        !state
            .actions
            .iter()
            .any(|action| action.starts_with("DELETE /v2/"))
    );
}

#[tokio::test]
async fn concurrent_first_create_and_update_cannot_bypass_driver_retirement() {
    for existing in [false, true] {
        let fixture = Fixture::new(existing).await;
        let pause = Pause::default();
        fixture.state.lock().unwrap().pause_wallet = Some(pause.clone());
        let service = fixture.service.clone();
        let tenant = fixture.tenant();
        let mut request = fixture.set_request(u64::from(existing));
        request.policy.price_micro_usd += 1;
        let setter = tokio::spawn(async move { service.set(&tenant, request).await });
        pause.wait().await;
        fixture
            .driver
            .delete(&fixture.tenant(), fixture.request())
            .await
            .unwrap();
        pause.resume.notify_one();
        assert!(setter.await.unwrap().is_err());
        let stored = fixture.stored().unwrap();
        assert!(stored.retired);
        assert_eq!(stored.policy.is_some(), existing);
    }
}

#[tokio::test]
async fn explicit_policy_delete_cannot_overwrite_concurrent_retirement() {
    let fixture = Fixture::new(true).await;
    let pause = Pause::default();
    fixture.state.lock().unwrap().pause_policy_patch = Some(pause.clone());
    let service = fixture.service.clone();
    let tenant = fixture.tenant();
    let request = DeletePaymentPolicyRequest {
        hostname: fixture.identity.hostname.clone(),
        expected_version: 1,
    };
    let deleting = tokio::spawn(async move { service.delete(&tenant, request).await });
    pause.wait().await;
    fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    pause.resume.notify_one();
    assert!(deleting.await.unwrap().is_err());
    assert!(fixture.stored().unwrap().retired);
    assert_eq!(fixture.stored().unwrap().policy.unwrap().version, 2);
}

#[tokio::test]
async fn channel_cleanup_rejects_recreated_or_refunded_function_after_listing() {
    for change_incarnation in [true, false] {
        let fixture = Fixture::new(true).await;
        {
            let mut state = fixture.state.lock().unwrap();
            state.listed_functions = Some(vec![function(&fixture.identity)]);
            let current = state
                .functions
                .get_mut(&fixture.identity.resource_name)
                .unwrap();
            if change_incarnation {
                current["createTime"] = json!("2025-02-01T00:00:00Z");
            } else {
                current["labels"]["pay-channel"] = json!("different-channel");
            }
        }
        assert!(fixture.driver.cleanup_channel("channel").await.is_err());
        assert!(!fixture.stored().unwrap().retired);
        assert!(
            !fixture
                .state
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|action| action.starts_with("DELETE "))
        );
    }
}

#[tokio::test]
async fn provider_errors_and_future_policy_incarnations_never_imply_absence() {
    for status in [StatusCode::FORBIDDEN, StatusCode::SERVICE_UNAVAILABLE] {
        let fixture = Fixture::new(true).await;
        fixture.state.lock().unwrap().provider_get_error = Some(status);
        assert!(fixture.driver.reconcile_orphans(false).await.is_err());
        assert!(!fixture.stored().unwrap().retired);
    }
    let fixture = Fixture::new(false).await;
    let future = identity("weather", "2099-01-01T00:00:00Z");
    let newer_than_provider = identity("weather", "2025-02-01T00:00:00Z");
    {
        let mut state = fixture.state.lock().unwrap();
        seed(&mut state, &policy(&future));
        seed(&mut state, &policy(&newer_than_provider));
    }
    assert!(fixture.driver.reconcile_orphans(false).await.is_err());
    let state = fixture.state.lock().unwrap();
    for identity in [future, newer_than_provider] {
        assert!(
            !PolicyRepository::decode(&state.documents[&document_name(&identity)])
                .unwrap()
                .retired
        );
    }
}

#[tokio::test]
async fn malformed_and_wrong_scope_documents_do_not_block_later_valid_orphans() {
    let fixture = Fixture::new(true).await;
    {
        let mut state = fixture.state.lock().unwrap();
        state.functions.clear();
        let mut invalid = Vec::new();
        let mut owner = identity("wrongowner", CREATED);
        owner.owner_key = "fedcba9876543210".into();
        invalid.push(owner);
        let mut project = identity("wrongproject", CREATED);
        project.resource_name = project
            .resource_name
            .replace("projects/project/", "projects/other/");
        invalid.push(project);
        let mut region = identity("wrongregion", CREATED);
        region.resource_name = region
            .resource_name
            .replace("/us-central1/", "/europe-west1/");
        invalid.push(region);
        let mut host = identity("wronghost", CREATED);
        host.hostname = "outside.example".into();
        invalid.push(host);
        let mut created = identity("badcreation", CREATED);
        created.created_at = "not-a-timestamp".into();
        invalid.push(created);
        for invalid in invalid {
            seed(&mut state, &policy(&invalid));
        }
        state.documents.insert(
            format!("{COLLECTION}/000-malformed"),
            json!({"name": "invalid"}),
        );
        let mut wrong_key = state.documents[&document_name(&fixture.identity)].clone();
        wrong_key["name"] = json!(format!("{COLLECTION}/wrongkey"));
        state
            .documents
            .insert(format!("{COLLECTION}/wrongkey"), wrong_key);
    }
    assert!(fixture.driver.reconcile_orphans(false).await.is_err());
    assert!(fixture.stored().unwrap().retired);
    let state = fixture.state.lock().unwrap();
    let retired = state
        .documents
        .values()
        .filter_map(|value| PolicyRepository::decode(value).ok())
        .filter(|stored| stored.retired)
        .count();
    assert_eq!(retired, 1);
}

#[tokio::test]
async fn bounded_scan_persists_progress_past_a_malformed_first_page() {
    let fixture = Fixture::new(false).await;
    {
        let mut state = fixture.state.lock().unwrap();
        state.functions.clear();
        state.documents.insert(
            format!("{COLLECTION}/000-malformed"),
            json!({"name": "invalid"}),
        );
        for index in 0..105 {
            seed(
                &mut state,
                &policy(&identity(&format!("resource{index}"), CREATED)),
            );
        }
    }
    assert!(fixture.driver.reconcile_orphans(false).await.is_err());
    let retired_count = || {
        fixture
            .state
            .lock()
            .unwrap()
            .documents
            .values()
            .filter_map(|value| PolicyRepository::decode(value).ok())
            .filter(|stored| stored.retired)
            .count()
    };
    assert_eq!(retired_count(), 99);
    assert_eq!(fixture.driver.reconcile_orphans(false).await.unwrap(), 6);
    assert_eq!(retired_count(), 105);
}

#[tokio::test]
async fn retirement_losing_cas_does_not_delete_provider_and_retry_retires_the_winner() {
    let fixture = Fixture::new(true).await;
    let pause = Pause::default();
    fixture.state.lock().unwrap().pause_policy_patch = Some(pause.clone());
    let driver = fixture.driver.clone();
    let tenant = fixture.tenant();
    let request = fixture.request();
    let deletion = tokio::spawn(async move { driver.delete(&tenant, request).await });
    pause.wait().await;
    let mut update = fixture.set_request(1);
    update.policy.price_micro_usd += 1;
    assert_eq!(
        fixture
            .service
            .set(&fixture.tenant(), update)
            .await
            .unwrap()
            .version,
        2
    );
    pause.resume.notify_one();
    assert!(deletion.await.unwrap().is_err());
    assert!(!fixture.stored().unwrap().retired);
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .actions
            .iter()
            .any(|action| action.starts_with("DELETE /v2/"))
    );
    fixture
        .driver
        .delete(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    let tombstone = fixture.stored().unwrap().policy.unwrap();
    assert_eq!(tombstone.version, 3);
    assert_eq!(tombstone.spec.price_micro_usd, 50_001);
}

#[tokio::test]
async fn mcp_delete_orchestration_preserves_missing_resource_retries() {
    let fixture = Fixture::new(true).await;
    let registry = crate::driver::DriverRegistry::new([
        Arc::new(fixture.driver.clone()) as Arc<dyn ComputeDriver>
    ])
    .unwrap();
    let mcp = crate::server::ComputeMcp::new(
        registry,
        crate::trigger_driver::TriggerDriverRegistry::default(),
        None,
    );
    mcp.delete_owned(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    let retry = mcp
        .delete_owned(&fixture.tenant(), fixture.request())
        .await
        .unwrap();
    assert!(matches!(retry.state, OperationState::Succeeded));
    fixture.state.lock().unwrap().provider_get_error = Some(StatusCode::FORBIDDEN);
    assert!(
        mcp.delete_owned(&fixture.tenant(), fixture.request())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_checkpoints_cannot_skip_another_runners_unprocessed_page() {
    let fixture = Fixture::new(false).await;
    let repository = fixture.driver.policies.as_ref().unwrap();
    let mut first = repository.checkpoint("fixture", "scope").await.unwrap();
    let mut stale = repository.checkpoint("fixture", "scope").await.unwrap();
    repository
        .advance_checkpoint(
            "fixture",
            "scope",
            &mut first,
            Some("processed-page".into()),
        )
        .await
        .unwrap();
    assert!(
        repository
            .advance_checkpoint(
                "fixture",
                "scope",
                &mut stale,
                Some("unprocessed-page".into())
            )
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .checkpoint("fixture", "scope")
            .await
            .unwrap()
            .page_token
            .as_deref(),
        Some("processed-page")
    );
}

#[tokio::test]
async fn stalled_metadata_headers_and_body_do_not_hang_reconciliation() {
    let fixture = Fixture::new(true).await;
    let before = fixture.state.lock().unwrap().documents.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metadata_base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(|request: Request| async move {
        if request.uri().path().ends_with("/identity") {
            return std::future::pending::<Response>().await;
        }
        let body = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{"))
        })
        .chain(futures_util::stream::pending());
        Response::new(axum::body::Body::from_stream(body))
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut driver = fixture.driver.clone();
    let config = Arc::make_mut(&mut driver.config);
    config.metadata_base = metadata_base;
    config.access_token = None;
    config.identity_token = None;
    let results = tokio::time::timeout(METADATA_REQUEST_TIMEOUT + Duration::from_secs(2), async {
        tokio::join!(
            driver.reconcile_orphans(false),
            driver.identity_token("https://workload.invalid")
        )
    })
    .await;
    server.abort();
    let _ = server.await;
    let (reconcile, identity) = results.expect("metadata acquisition must be bounded");
    for error in [reconcile.unwrap_err(), identity.unwrap_err()] {
        assert!(matches!(error, ComputeError::Transport(error) if error.is_timeout()));
    }
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.documents, before);
    assert!(
        state.actions.is_empty(),
        "no provider or policy access without a token"
    );
}

#[tokio::test]
async fn disabled_policy_storage_is_legacy_compatible_but_explicit_sweeps_fail_closed() {
    let fixture = Fixture::new(false).await;
    let mut config = (*fixture.driver.config).clone();
    for invalid in [
        "",
        "(default)",
        " policies ",
        "policies/other",
        "Policies",
        "abc",
        "policies-",
        "🔥",
    ] {
        config.payment_policy_database = Some(invalid.into());
        assert!(GoogleCloudFunctionsDriver::new(config.clone()).is_err());
    }
    config.payment_policy_database = None;
    let driver = GoogleCloudFunctionsDriver::new(config).unwrap();
    assert!(driver.policy_repository().is_none());
    assert!(matches!(
        driver.reconcile_orphans(true).await,
        Err(ComputeError::Configuration(_))
    ));
}
