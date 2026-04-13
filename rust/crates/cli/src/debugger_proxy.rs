//! Debugger forward proxy — transparent HTTP proxy that logs all traffic
//! to PDB for inspection.
//!
//! Used by `pay --debugger claude` to capture every MCP curl request
//! without requiring a full API spec / payment gateway setup.
//!
//! Routing: the MCP curl tool sends requests to `http://127.0.0.1:1402/`
//! with the original destination in the `X-Pay-Forward-To` header. The
//! proxy reads the header, forwards the request (including method, body,
//! headers), logs the exchange to PDB, and returns the response.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

/// Header carrying the original destination URL.
pub const FORWARD_HEADER: &str = "x-pay-forward-to";

/// Default bind address for the debugger proxy.
pub const DEFAULT_BIND: &str = "127.0.0.1:1402";

/// Start the debugger proxy in the background. Returns the bind address
/// so the caller can set `PAY_DEBUGGER_PROXY` for the MCP server.
///
/// The proxy runs on a dedicated tokio runtime in a background thread
/// so it doesn't interfere with the CLI's sync main function.
pub fn start_background(bind: &str) -> pay_core::Result<String> {
    let bind = bind.to_string();
    let bind_clone = bind.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("debugger proxy runtime");

        rt.block_on(async move {
            let pdb = pay_pdb::PdbState::new(serde_json::json!({
                "recipient": "",
                "network": "proxy",
                "rpcUrl": "",
                "endpoints": {
                    "mpp": [],
                    "x402": [],
                    "oauth": []
                }
            }));
            pdb.spawn_cleanup();

            let pdb_state = pdb.clone();
            let app = Router::new()
                .nest("/__402/pdb", pay_pdb::debugger_router(pdb.clone()))
                .fallback(any(move |req: Request<Body>| {
                    let pdb = pdb_state.clone();
                    forward_and_log(req, pdb)
                }))
                .layer(axum::Extension(Some(pdb)));

            let listener = tokio::net::TcpListener::bind(&bind_clone)
                .await
                .unwrap_or_else(|e| panic!("debugger proxy bind {bind_clone}: {e}"));

            eprintln!(
                "  {} http://{bind_clone}/__402/pdb/",
                owo_colors::OwoColorize::green(&"Debugger"),
            );

            axum::serve(listener, app).await.ok();
        });
    });

    // Give the server a moment to bind.
    std::thread::sleep(std::time::Duration::from_millis(200));

    Ok(format!("http://{bind}"))
}

/// Forward a request to the destination in `X-Pay-Forward-To`, log the
/// exchange to PDB, and return the response.
async fn forward_and_log(req: Request<Body>, pdb: pay_pdb::PdbState) -> Response {
    let forward_to = req
        .headers()
        .get(FORWARD_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let Some(dest_url) = forward_to else {
        // No forward header → this is a browser request, not an MCP
        // curl call. Redirect to the PDB dashboard.
        return axum::response::Redirect::temporary("/__402/pdb").into_response();
    };

    let method = req.method().clone();
    let _path = req.uri().path().to_string();

    // Extract headers to forward (skip hop-by-hop + our internal header).
    let mut fwd_headers = HeaderMap::new();
    for (k, v) in req.headers() {
        let name = k.as_str().to_lowercase();
        if name == FORWARD_HEADER || name == "host" || name == "connection" {
            continue;
        }
        fwd_headers.insert(k.clone(), v.clone());
    }

    // Read body.
    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("read body: {e}")).into_response();
        }
    };

    let log_id = pdb.next_log_id();
    let start = std::time::Instant::now();

    // Forward.
    let client = reqwest::Client::new();
    let upstream_resp = client
        .request(
            reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
            &dest_url,
        )
        .headers(reqwest_headers(&fwd_headers))
        .body(body_bytes.to_vec())
        .send()
        .await;

    let elapsed_ms = start.elapsed().as_millis() as u64;

    match upstream_resp {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let res_headers: std::collections::HashMap<String, String> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let res_body = resp.text().await.unwrap_or_default();

            // Log to PDB.
            let entry = pay_pdb::types::LogEntry {
                id: log_id,
                ts: chrono_now(),
                method: method.to_string(),
                path: dest_url.clone(),
                status,
                ms: elapsed_ms,
                req_headers: fwd_headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
                res_headers: res_headers.clone(),
                res_body: Some(res_body.clone()),
                client_ip: "mcp".to_string(),
            };
            pdb.correlation.lock().unwrap().ingest(entry);

            // Build response.
            let mut builder = Response::builder().status(status);
            for (k, v) in &res_headers {
                if let Ok(hv) = axum::http::HeaderValue::from_str(v) {
                    builder = builder.header(k.as_str(), hv);
                }
            }
            builder.body(Body::from(res_body)).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "response build error").into_response()
            })
        }
        Err(e) => {
            let entry = pay_pdb::types::LogEntry {
                id: log_id,
                ts: chrono_now(),
                method: method.to_string(),
                path: dest_url,
                status: 502,
                ms: elapsed_ms,
                req_headers: Default::default(),
                res_headers: Default::default(),
                res_body: Some(e.to_string()),
                client_ip: "mcp".to_string(),
            };
            pdb.correlation.lock().unwrap().ingest(entry);

            (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response()
        }
    }
}

/// Convert axum HeaderMap → reqwest HeaderMap.
fn reqwest_headers(src: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut dst = reqwest::header::HeaderMap::new();
    for (k, v) in src {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes())
            && let Ok(val) = reqwest::header::HeaderValue::from_bytes(v.as_bytes())
        {
            dst.insert(name, val);
        }
    }
    dst
}

fn chrono_now() -> String {
    // Lightweight UTC timestamp without pulling in chrono.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_test_proxy() -> std::net::SocketAddr {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let pdb = pay_pdb::PdbState::new(serde_json::json!({
                    "recipient": "",
                    "network": "proxy",
                    "rpcUrl": "",
                    "endpoints": { "mpp": [], "x402": [], "oauth": [] }
                }));
                let pdb_state = pdb.clone();
                let app = Router::new()
                    .nest("/__402/pdb", pay_pdb::debugger_router(pdb.clone()))
                    .fallback(any(move |req: Request<Body>| {
                        let pdb = pdb_state.clone();
                        forward_and_log(req, pdb)
                    }));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(addr).unwrap();
                axum::serve(listener, app).await.ok();
            });
        });
        let addr = rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        addr
    }

    #[test]
    fn pdb_html_served_with_correct_assets() {
        let addr = start_test_proxy();
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap();

        // PDB index (axum serves at /__402/pdb, redirects /__402/pdb/ there)
        let resp = client
            .get(format!("http://{addr}/__402/pdb"))
            .send()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let html = resp.text().unwrap();
        assert!(html.contains("Payment Debugger"));
        // Asset paths must be absolute so they work regardless of trailing slash
        assert!(html.contains("/__402/pdb/assets/"));
    }

    #[test]
    fn pdb_assets_and_api_served() {
        let addr = start_test_proxy();
        let client = reqwest::blocking::Client::new();

        // API config
        let resp = client
            .get(format!("http://{addr}/__402/pdb/api/config"))
            .send()
            .unwrap();
        assert_eq!(resp.status(), 200);
        let config: serde_json::Value = resp.json().unwrap();
        assert_eq!(config["network"], "proxy");

        // JS asset
        let resp = client
            .get(format!("http://{addr}/__402/pdb/assets/index-2XdDUhE2.js"))
            .send()
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn root_redirects_to_pdb() {
        let addr = start_test_proxy();
        let no_redirect = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = no_redirect.get(format!("http://{addr}/")).send().unwrap();
        assert_eq!(resp.status(), 307);
        let loc = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(loc, "/__402/pdb");
    }
}
