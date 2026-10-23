//! Stateful REST fixture: all lifecycle mutations go through the public driver.
use super::*;
use axum::{Router, body::Body, http::Request, response::Response};
use std::{collections::BTreeMap, sync::Mutex};
use tokio::sync::Notify;

/// A Firestore-shaped creation time that orders like the fixture revision.
fn created_at(revision: u64) -> Value {
    json!(format!("2026-01-01T00:00:00.{revision:06}Z"))
}

#[derive(Default)]
struct State {
    documents: BTreeMap<String, Value>,
    revision: u64,
    pause: Option<&'static str>,
    fail_listing: bool,
}

struct Fixture {
    state: Mutex<State>,
    paused: Notify,
    resume: Notify,
}

impl Fixture {
    fn arm(&self, point: &'static str) {
        self.state.lock().unwrap().pause = Some(point);
    }

    fn process(&self, method: &str, path: &str, query: &str, body: Value) -> (Response, bool) {
        let mut state = self.state.lock().unwrap();
        let name = path.strip_prefix("/v1/").unwrap();
        let params: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).collect();
        let point = if path.ends_with(":runQuery") {
            "query"
        } else if method == "GET" && path.ends_with("/documents") {
            "children"
        } else if method == "GET" && path.contains("/documentStores/") {
            "read"
        } else if method == "PATCH" && params.contains_key("updateMask.fieldPaths") {
            "mark"
        } else {
            ""
        };
        let pause = state.pause == Some(point);
        if pause {
            state.pause = None;
        }
        let ok = |value| super::tests::json_response(StatusCode::OK, value);
        let conflict = || {
            super::tests::json_response(
                StatusCode::CONFLICT,
                json!({"error": {"message": "precondition failed"}}),
            )
        };
        let matches = |documents: &BTreeMap<String, Value>, name: &str, condition: &Value| {
            let existing = documents.get(name);
            condition
                .get("updateTime")
                .is_none_or(|time| existing.and_then(|doc| doc.get("updateTime")) == Some(time))
                && condition
                    .get("exists")
                    .and_then(Value::as_bool)
                    .is_none_or(|exists| exists == existing.is_some())
        };
        let response = if path.ends_with(":runQuery") {
            let lease = &body["structuredQuery"]["where"]["fieldFilter"]["value"];
            ok(json!(
                state
                    .documents
                    .values()
                    .filter(|doc| { doc["fields"].get("payChannel") == Some(lease) })
                    .map(|doc| json!({"document": doc}))
                    .collect::<Vec<_>>()
            ))
        } else if path.ends_with(":batchGet") {
            ok(json!(
                body["documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|name| match state.documents.get(name.as_str().unwrap()) {
                        Some(doc) => json!({"found": doc}),
                        None => json!({"missing": name}),
                    })
                    .collect::<Vec<_>>()
            ))
        } else if path.ends_with(":commit") {
            let writes = body["writes"].as_array().unwrap();
            if writes.iter().any(|write| {
                let target = write
                    .get("verify")
                    .or_else(|| write.get("delete"))
                    .and_then(Value::as_str)
                    .or_else(|| write["update"]["name"].as_str())
                    .unwrap();
                !matches(&state.documents, target, &write["currentDocument"])
            }) {
                conflict()
            } else {
                for write in writes {
                    if let Some(name) = write["delete"].as_str() {
                        state.documents.remove(name);
                    } else if let Some(update) = write.get("update") {
                        let mut update = update.clone();
                        state.revision += 1;
                        update["updateTime"] = json!(format!("revision-{}", state.revision));
                        let name = update["name"].as_str().unwrap();
                        update["createTime"] = state
                            .documents
                            .get(name)
                            .and_then(|existing| existing.get("createTime").cloned())
                            .unwrap_or_else(|| created_at(state.revision));
                        state
                            .documents
                            .insert(update["name"].as_str().unwrap().to_owned(), update);
                    }
                }
                ok(json!({}))
            }
        } else if method == "GET" && path.ends_with("/documents") {
            if std::mem::take(&mut state.fail_listing) {
                super::tests::json_response(StatusCode::SERVICE_UNAVAILABLE, json!({}))
            } else {
                ok(json!({"documents": state.documents.iter()
                    .filter(|(key, _)| key.starts_with(&format!("{name}/")))
                    .map(|(_, doc)| doc).collect::<Vec<_>>()}))
            }
        } else if method == "GET" && path.ends_with("/documentStores") {
            ok(json!({"documents": state.documents.iter()
                .filter(|(key, _)| key.strip_prefix(&format!("{name}/"))
                    .is_some_and(|rest| !rest.contains('/')))
                .map(|(_, doc)| doc).collect::<Vec<_>>()}))
        } else if method == "GET" {
            match state.documents.get(name) {
                Some(doc) => ok(doc.clone()),
                None => super::tests::json_response(StatusCode::NOT_FOUND, json!({})),
            }
        } else {
            let mut condition = json!({});
            if let Some(time) = params.get("currentDocument.updateTime") {
                condition["updateTime"] = json!(time);
            }
            if let Some(exists) = params.get("currentDocument.exists") {
                condition["exists"] = json!(exists == "true");
            }
            if !matches(&state.documents, name, &condition) {
                conflict()
            } else if method == "DELETE" {
                state.documents.remove(name);
                ok(json!({}))
            } else if method == "PATCH" {
                let mut document = if params.contains_key("updateMask.fieldPaths") {
                    let mut current = state.documents.get(name).cloned().unwrap_or(json!({}));
                    current["fields"]["phase"] = body["fields"]["phase"].clone();
                    current
                } else {
                    body
                };
                state.revision += 1;
                document["name"] = json!(name);
                document["updateTime"] = json!(format!("revision-{}", state.revision));
                document["createTime"] = state
                    .documents
                    .get(name)
                    .and_then(|existing| existing.get("createTime").cloned())
                    .unwrap_or_else(|| created_at(state.revision));
                state.documents.insert(name.to_owned(), document.clone());
                ok(document)
            } else {
                panic!("unexpected request {method} {path}");
            }
        };
        (response, pause)
    }
}

async fn fixture() -> (
    Arc<FirestoreDriver>,
    Arc<Fixture>,
    tokio::task::JoinHandle<()>,
) {
    let fixture = Arc::new(Fixture {
        state: Mutex::new(State::default()),
        paused: Notify::new(),
        resume: Notify::new(),
    });
    let handler = fixture.clone();
    let app = Router::new().fallback(move |request: Request<Body>| {
        let fixture = handler.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
            let body = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            let (response, pause) = fixture.process(
                parts.method.as_str(),
                parts.uri.path(),
                parts.uri.query().unwrap_or(""),
                body,
            );
            if pause {
                fixture.paused.notify_one();
                fixture.resume.notified().await;
            }
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let driver = Arc::new(
        FirestoreDriver::new(FirestoreConfig {
            project: "project".into(),
            database: "(default)".into(),
            region: "us-central1".into(),
            api_base: format!("http://{}", listener.local_addr().unwrap()),
            metadata_base: "http://metadata.invalid".into(),
            access_token: Some("token".into()),
        })
        .unwrap(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (driver, fixture, server)
}

fn tenant(channel: &str) -> Tenant {
    Tenant {
        payer: "payer".into(),
        key: "0123456789abcdef".into(),
        channel_id: channel.into(),
        binding: None,
    }
}

fn store_request() -> DocumentStoreRequest {
    DocumentStoreRequest {
        driver: DRIVER_ID.into(),
        id: "weather".into(),
    }
}

async fn create_and_write(driver: &FirestoreDriver, tenant: &Tenant, value: Value) {
    driver
        .create_document_store(
            tenant,
            CreateDocumentStoreRequest {
                driver: DRIVER_ID.into(),
                class: CLASS_ID.into(),
                name: "weather".into(),
                region: None,
                reclaim_policy: ReclaimPolicy::Delete,
                access: Default::default(),
                driver_options: Value::Null,
            },
        )
        .await
        .unwrap();
    driver
        .put_document(
            tenant,
            PutDocumentRequest {
                driver: DRIVER_ID.into(),
                store_id: "weather".into(),
                key: "key".into(),
                value,
            },
        )
        .await
        .unwrap();
}

async fn replacement_survives(point: &'static str, cleanup: bool, empty: bool) {
    for channel in ["channelA", "channelB"] {
        replacement_survives_on_channel(point, cleanup, empty, channel, false).await;
    }
}

async fn replacement_survives_on_channel(
    point: &'static str,
    cleanup: bool,
    empty: bool,
    channel: &str,
    initially_deleting: bool,
) {
    let (driver, fixture, server) = fixture().await;
    let a = tenant("channelA");
    let b = tenant(channel);
    create_and_write(&driver, &a, json!("original")).await;
    if empty {
        driver
            .delete_document(
                &a,
                DocumentRequest {
                    driver: DRIVER_ID.into(),
                    store_id: "weather".into(),
                    key: "key".into(),
                },
            )
            .await
            .unwrap();
    }
    if initially_deleting {
        // A real interrupted deletion leaves the original incarnation retiring.
        fixture.state.lock().unwrap().fail_listing = true;
        assert!(
            driver
                .delete_document_store(&a, store_request())
                .await
                .is_err()
        );
    }
    fixture.arm(point);
    let stale_driver = driver.clone();
    let stale = tokio::spawn(async move {
        if cleanup {
            stale_driver.cleanup_channel("channelA").await.map(|_| ())
        } else {
            stale_driver
                .delete_document_store(&tenant("channelA"), store_request())
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.paused.notified())
        .await
        .unwrap();
    driver
        .delete_document_store(&a, store_request())
        .await
        .unwrap();
    create_and_write(&driver, &b, json!("replacement")).await;
    fixture.resume.notify_one();
    assert!(
        stale.await.unwrap().is_err(),
        "stale fence must fail closed"
    );
    let replacement = driver
        .get_document(
            &b,
            DocumentRequest {
                driver: DRIVER_ID.into(),
                store_id: "weather".into(),
                key: "key".into(),
            },
        )
        .await;
    assert_eq!(
        replacement
            .expect("stale deletion must preserve the replacement")
            .value,
        json!("replacement")
    );
    // The stale operation must not mark the replacement as deleting, either.
    create_and_write(&driver, &b, json!("still writable")).await;
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn cleanup_preserves_replacement_after_selection() {
    replacement_survives("query", true, false).await;
}

#[tokio::test]
async fn cleanup_deletes_the_selected_incarnation_and_its_children() {
    let (driver, fixture, server) = fixture().await;
    create_and_write(&driver, &tenant("channelA"), json!("original")).await;
    assert_eq!(driver.cleanup_channel("channelA").await.unwrap(), 1);
    assert!(fixture.state.lock().unwrap().documents.is_empty());
    assert_eq!(driver.cleanup_channel("channelA").await.unwrap(), 0);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn cleanup_cannot_repurpose_a_selected_retiring_marker() {
    for channel in ["channelA", "channelB"] {
        replacement_survives_on_channel("query", true, false, channel, true).await;
    }
}

#[tokio::test]
async fn cleanup_preserves_replacement_after_marking_deleting() {
    replacement_survives("mark", true, false).await;
}

#[tokio::test]
async fn cleanup_fences_child_deletion() {
    replacement_survives("children", true, false).await;
}

#[tokio::test]
async fn cleanup_fences_parent_deletion_after_empty_listing() {
    replacement_survives("children", true, true).await;
}

#[tokio::test]
async fn normal_delete_cannot_mark_a_replacement_deleting() {
    replacement_survives("read", false, false).await;
}

#[tokio::test]
async fn normal_delete_fences_child_deletion() {
    replacement_survives("children", false, false).await;
}

#[tokio::test]
async fn normal_delete_fences_parent_deletion_after_empty_listing() {
    replacement_survives("children", false, true).await;
}

#[tokio::test]
async fn capabilities_do_not_survive_store_recreation() {
    use crate::binding::{BindingIssuer, CAPABILITY_HEADER, CreateBindingRequest};
    use tower::ServiceExt;
    let (driver, _fixture, server) = fixture().await;
    let owner = tenant("channelA");
    create_and_write(&driver, &owner, json!("original")).await;
    let issuer = BindingIssuer::new(vec![7; 32], vec![9; 32]).unwrap();
    let issue = |store: crate::types::DocumentStore| {
        issuer
            .issue(
                owner.key.clone(),
                crate::types::StoreBinding {
                    generation: store.created_at.unwrap(),
                    channel_lease: channel_lease_key("channelA").unwrap(),
                },
                &CreateBindingRequest {
                    payer: "payer".into(),
                    channel_id: "channelA".into(),
                    binding_name: "weather".into(),
                    driver: DRIVER_ID.into(),
                    store_id: store.id,
                    workload_id: "gcf-0123456789abcdef-weather".into(),
                    read: true,
                    write: true,
                    lease_seconds: Some(300),
                },
            )
            .unwrap()
            .capability
    };
    let stale = issue(
        driver
            .get_document_store(&owner, store_request())
            .await
            .unwrap(),
    );
    let drivers =
        crate::driver::DriverRegistry::new([driver.clone() as Arc<dyn DataDriver>]).unwrap();
    let app = crate::server::runtime_router(drivers, issuer.clone());
    let call = |method: &str, capability: &str, body: Option<Value>| {
        let request = Request::builder()
            .method(method)
            .uri("/__402/bindings/fds-0123456789abcdef-weather/key")
            .header(CAPABILITY_HEADER, capability)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
            .unwrap();
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    let (status, body) = call("GET", &stale, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "original");

    driver
        .delete_document_store(&owner, store_request())
        .await
        .unwrap();
    create_and_write(&driver, &owner, json!("replacement")).await;
    for (method, body) in [
        ("GET", None),
        ("PUT", Some(json!("overwritten"))),
        ("DELETE", None),
    ] {
        let (status, _) = call(method, &stale, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method}");
    }

    let fresh = issue(
        driver
            .get_document_store(&owner, store_request())
            .await
            .unwrap(),
    );
    let (status, body) = call("GET", &fresh, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "replacement");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn store_crud_is_scoped_to_the_funding_channel() {
    let (driver, _fixture, server) = fixture().await;
    let owner = tenant("channelA");
    let sibling = tenant("channelB");
    create_and_write(&driver, &owner, json!("original")).await;
    let document = || DocumentRequest {
        driver: DRIVER_ID.into(),
        store_id: "weather".into(),
        key: "key".into(),
    };
    let put = || PutDocumentRequest {
        driver: DRIVER_ID.into(),
        store_id: "weather".into(),
        key: "key".into(),
        value: json!("overwritten"),
    };
    let list = || ListDocumentStoresRequest {
        driver: DRIVER_ID.into(),
        page_size: 50,
        page_token: None,
    };
    for tenant in [sibling.clone(), tenant("")] {
        assert!(driver.get_document(&tenant, document()).await.is_err());
        assert!(driver.put_document(&tenant, put()).await.is_err());
        assert!(driver.delete_document(&tenant, document()).await.is_err());
        assert!(
            driver
                .get_document_store(&tenant, store_request())
                .await
                .is_err()
        );
        assert!(
            driver
                .delete_document_store(&tenant, store_request())
                .await
                .is_err()
        );
        assert!(
            driver
                .list_document_stores(&tenant, list())
                .await
                .unwrap()
                .stores
                .is_empty()
        );
    }
    // A capability-bound tenant must present the store's own lease.
    let generation = driver
        .get_document_store(&owner, store_request())
        .await
        .unwrap()
        .created_at
        .unwrap();
    let mut bound = tenant("");
    bound.binding = Some(crate::types::StoreBinding {
        generation: generation.clone(),
        channel_lease: channel_lease_key("channelB").unwrap(),
    });
    assert!(driver.get_document(&bound, document()).await.is_err());
    bound.binding = Some(crate::types::StoreBinding {
        generation,
        channel_lease: channel_lease_key("channelA").unwrap(),
    });
    assert_eq!(
        driver.get_document(&bound, document()).await.unwrap().value,
        json!("original")
    );

    assert_eq!(
        driver.get_document(&owner, document()).await.unwrap().value,
        json!("original")
    );
    assert_eq!(
        driver
            .list_document_stores(&owner, list())
            .await
            .unwrap()
            .stores
            .len(),
        1
    );
    driver.put_document(&owner, put()).await.unwrap();
    driver
        .delete_document_store(&owner, store_request())
        .await
        .unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn logical_names_cannot_shadow_a_physical_store_id() {
    let (driver, fixture, server) = fixture().await;
    let owner = tenant("channelA");
    create_and_write(&driver, &owner, json!("weather")).await;
    let before = fixture.state.lock().unwrap().documents.len();
    let error = driver
        .create_document_store(
            &owner,
            CreateDocumentStoreRequest {
                driver: DRIVER_ID.into(),
                class: CLASS_ID.into(),
                name: "fds-0123456789abcdef-weather".into(),
                region: None,
                reclaim_policy: ReclaimPolicy::Delete,
                access: Default::default(),
                driver_options: Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reserved"));
    assert_eq!(fixture.state.lock().unwrap().documents.len(), before);
    // The physical ID still names exactly the `weather` store.
    let store = driver
        .get_document_store(
            &owner,
            DocumentStoreRequest {
                driver: DRIVER_ID.into(),
                id: "fds-0123456789abcdef-weather".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(store.name, "weather");
    server.abort();
    let _ = server.await;
}

fn document_request() -> DocumentRequest {
    DocumentRequest {
        driver: DRIVER_ID.into(),
        store_id: "weather".into(),
        key: "key".into(),
    }
}

#[tokio::test]
async fn a_read_authorized_before_recreation_never_returns_the_replacement() {
    let (driver, fixture, server) = fixture().await;
    let a = tenant("channelA");
    let b = tenant("channelB");
    create_and_write(&driver, &a, json!("original")).await;
    // Channel A's parent check passes, then the store is deleted and
    // recreated for channel B before A's child read.
    fixture.arm("read");
    let stale_driver = driver.clone();
    let stale = tokio::spawn(async move {
        stale_driver
            .get_document(&tenant("channelA"), document_request())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), fixture.paused.notified())
        .await
        .unwrap();
    driver
        .delete_document_store(&a, store_request())
        .await
        .unwrap();
    create_and_write(&driver, &b, json!("replacement")).await;
    fixture.resume.notify_one();
    assert!(stale.await.unwrap().is_err(), "stale read must fail closed");
    // The new owner reads its own document.
    let read = driver.get_document(&b, document_request()).await.unwrap();
    assert_eq!(read.value, json!("replacement"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn retained_documents_are_not_adopted_by_a_recreated_store() {
    let (driver, _fixture, server) = fixture().await;
    let create = |tenant: Tenant| {
        let driver = driver.clone();
        async move {
            driver
                .create_document_store(
                    &tenant,
                    CreateDocumentStoreRequest {
                        driver: DRIVER_ID.into(),
                        class: CLASS_ID.into(),
                        name: "weather".into(),
                        region: None,
                        reclaim_policy: ReclaimPolicy::Retain,
                        access: Default::default(),
                        driver_options: Value::Null,
                    },
                )
                .await
                .unwrap();
        }
    };
    let put = |tenant: Tenant, value: Value| {
        let driver = driver.clone();
        async move {
            driver
                .put_document(
                    &tenant,
                    PutDocumentRequest {
                        driver: DRIVER_ID.into(),
                        store_id: "weather".into(),
                        key: "key".into(),
                        value,
                    },
                )
                .await
                .unwrap();
        }
    };
    let a = tenant("channelA");
    let b = tenant("channelB");
    create(a.clone()).await;
    put(a.clone(), json!("secret")).await;
    let read = driver.get_document(&a, document_request()).await.unwrap();
    assert_eq!(read.value, json!("secret"));
    driver
        .delete_document_store(&a, store_request())
        .await
        .unwrap();

    // Channel B recreates the name; channel A's retained child stays put
    // but is not part of B's store.
    create(b.clone()).await;
    assert!(driver.get_document(&b, document_request()).await.is_err());
    put(b.clone(), json!("fresh")).await;
    let read = driver.get_document(&b, document_request()).await.unwrap();
    assert_eq!(read.value, json!("fresh"));
    server.abort();
    let _ = server.await;
}
