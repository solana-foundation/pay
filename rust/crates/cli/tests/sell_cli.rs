use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn token_authenticated_sell_commands_reach_http_without_an_account() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        captured.lock().unwrap().push((
            request.method().to_string(),
            request.uri().path().to_string(),
        ));
        async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": "reached token-authenticated fixture"})),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for args in [
        vec![
            "sell",
            "create",
            "--model",
            "agent",
            "--price",
            "0.01",
            "--earn-cap",
            "1",
            "--token",
            "fixture",
            "--recipient",
            "11111111111111111111111111111111",
        ],
        vec![
            "sell",
            "serve",
            "endpoint",
            "--owner-token",
            "fixture",
            "--harness",
            "echo",
        ],
    ] {
        let home = tempfile::tempdir().unwrap();
        let url = url.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_pay"))
                .env_clear()
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("PAY_SELL_DIR", home.path().join("sell"))
                .current_dir(home.path())
                .args(args)
                .args(["--connect-url", &url])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("reached token-authenticated fixture"),
            "{stderr}"
        );
        assert!(!stderr.contains("No account configured"), "{stderr}");
    }
    assert_eq!(
        *requests.lock().unwrap(),
        [
            ("POST".into(), "/v1/endpoints".into()),
            ("GET".into(), "/v1/endpoints/endpoint".into()),
        ]
    );
    server.abort();
    let _ = server.await;
}
