//! Stateful REST fixture: all lifecycle mutations go through the public driver.
use super::*;
use axum::{Router, body::Body, http::Request, response::Response};
use std::{collections::BTreeMap, sync::Mutex};
use tokio::sync::Notify;

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
