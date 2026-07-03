//! 402-paying local reverse proxy for `pay claude`.
//!
//! Claude Code cannot handle HTTP 402 payment challenges, so it can never
//! talk to a priced inference gateway directly. This proxy sits between
//! Claude Code and the upstream (the `pay serve inference` gateway on
//! 127.0.0.1:1402, or a bare provider like Ollama in direct mode):
//!
//! 1. Every request is forwarded upstream, preserving method, path+query,
//!    and headers (minus hop-by-hop). The request body is buffered (capped
//!    at [`MAX_BODY_BYTES`]) so it can be replayed.
//! 2. When the upstream answers `402 Payment Required` with an MPP
//!    `Payment` challenge, the proxy builds a signed charge credential
//!    with the exact client machinery that backs `pay curl` / `pay fetch`
//!    ([`pay_core::mpp::select_challenge_by_balance`] +
//!    [`pay_core::mpp::build_credential`]), swaps the request's
//!    `Authorization` header for `Payment <credential>`, and retries the
//!    buffered request exactly once.
//! 3. Response bodies stream back ([`Body::from_stream`]) — Claude Code
//!    streams every completion over SSE.
//! 4. If payment fails for any reason (mainnet challenge under
//!    `--sandbox`, insufficient funds, signer errors, …) the original 402
//!    is passed through untouched, with a `tracing::warn!` explaining why.
//!
//! Sandbox/network intent is enforced by the reused `build_credential`
//! path: with `--sandbox` the CLI forces `network_override = localnet`,
//! and `pay_core::mpp::check_client_network_intent` refuses to sign when
//! the challenge advertises a different network (e.g. mainnet).

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use pay_core::accounts::{
    AccountChoice, AccountsStore, FileAccountsStore, MAINNET_NETWORK,
    load_or_create_ephemeral_for_network, load_or_create_ephemeral_for_network_as,
    resolve_account_for_network,
};

/// Request bodies are buffered so the paid retry can replay them; cap the
/// buffer so a runaway client cannot exhaust memory.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Handle returned by [`start_background`].
pub struct PayerProxy {
    /// Base URL of the payer proxy, e.g. `http://127.0.0.1:52341`.
    pub base_url: String,
    /// Pubkey of the wallet that funds 402 retries, when resolvable at
    /// startup (in sandbox mode the ephemeral localnet wallet is created
    /// eagerly so the pubkey is always known).
    pub payer_pubkey: Option<String>,
}

/// Shared state for the proxy handlers.
struct PayerState {
    /// Upstream base URL without a trailing slash.
    upstream: String,
    client: reqwest::Client,
    store: Arc<dyn AccountsStore>,
    /// Forced network slug (`--sandbox` → `localnet`, `--mainnet` →
    /// `mainnet`). `None` trusts the challenge's `methodDetails.network`.
    network_override: Option<String>,
    /// `--account` override, same semantics as the curl/fetch retry path.
    account_override: Option<String>,
}

impl PayerState {
    fn new(
        upstream: String,
        store: Arc<dyn AccountsStore>,
        network_override: Option<String>,
        account_override: Option<String>,
    ) -> pay_core::Result<Self> {
        // No request timeout: streamed completions stay open for minutes.
        // `no_proxy` keeps env proxies from hijacking localhost traffic.
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("payer proxy HTTP client: {e}")))?;
        Ok(Self {
            upstream: upstream.trim_end_matches('/').to_string(),
            client,
            store,
            network_override,
            account_override,
        })
    }
}

/// Start the payer proxy on an ephemeral 127.0.0.1 port, on a dedicated
/// runtime in a background thread (the `pay claude` main thread stays
/// sync and blocks on the `claude` child process). Returns once the
/// listener is bound.
pub fn start_background(
    upstream: &str,
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> pay_core::Result<PayerProxy> {
    let store: Arc<dyn AccountsStore> = Arc::new(FileAccountsStore::default_path());
    let state = Arc::new(PayerState::new(
        upstream.to_string(),
        store,
        network_override.map(str::to_string),
        account_override.map(str::to_string),
    )?);
    let payer_pubkey = resolve_payer_pubkey(&state);

    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<u16, String>>();
    let serve_state = state.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(format!("payer proxy runtime: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => listener,
                Err(e) => {
                    let _ = tx.send(Err(format!("payer proxy bind: {e}")));
                    return;
                }
            };
            let port = match listener.local_addr() {
                Ok(addr) => addr.port(),
                Err(e) => {
                    let _ = tx.send(Err(format!("payer proxy local_addr: {e}")));
                    return;
                }
            };
            let _ = tx.send(Ok(port));
            axum::serve(listener, router(serve_state)).await.ok();
        });
    });

    let port = match rx.recv() {
        Ok(Ok(port)) => port,
        Ok(Err(e)) => return Err(pay_core::Error::Config(e)),
        Err(_) => {
            return Err(pay_core::Error::Config(
                "payer proxy thread exited before binding".to_string(),
            ));
        }
    };

    Ok(PayerProxy {
        base_url: format!("http://127.0.0.1:{port}"),
        payer_pubkey,
    })
}

/// Resolve the pubkey that will fund payments, mirroring the wallet
/// routing of the curl/fetch retry path: `networks.<network>` mapping
/// first (honoring `--account`), then lazy ephemeral creation for
/// throwaway networks (`--sandbox` localnet uses the exact same wallet
/// `pay --sandbox curl` spends from). Never auto-creates a mainnet
/// wallet.
fn resolve_payer_pubkey(state: &PayerState) -> Option<String> {
    let network = state.network_override.as_deref().unwrap_or(MAINNET_NETWORK);
    let file = state.store.load().ok()?;

    if let Some(name) = state.account_override.as_deref() {
        if let Some(pubkey) = file
            .named_account_for_network(network, name)
            .and_then(|account| account.pubkey.clone())
        {
            return Some(pubkey);
        }
    } else if let AccountChoice::Resolved { account, .. } =
        resolve_account_for_network(network, &file)
        && account.pubkey.is_some()
    {
        return account.pubkey;
    }

    if matches!(network, "localnet" | "devnet") {
        let resolved = match state.account_override.as_deref() {
            Some(name) => {
                load_or_create_ephemeral_for_network_as(network, name, state.store.as_ref())
            }
            None => load_or_create_ephemeral_for_network(network, state.store.as_ref()),
        }
        .ok()?;
        return resolved.account.pubkey;
    }

    None
}

fn router(state: Arc<PayerState>) -> Router {
    Router::new().fallback(any(proxy)).with_state(state)
}

async fn proxy(State(state): State<Arc<PayerState>>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let path_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let url = format!("{}{}", state.upstream, path_query);

    // Buffer the request body so a paid retry can replay it verbatim.
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(e) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("payer proxy: request body over {MAX_BODY_BYTES} bytes or unreadable: {e}"),
            )
                .into_response();
        }
    };

    let first = match send_upstream(&state, &method, &url, &headers, body.clone(), None).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: upstream request failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("payer proxy: upstream error: {e}"),
            )
                .into_response();
        }
    };

    if first.status() != StatusCode::PAYMENT_REQUIRED {
        return stream_response(first);
    }

    // Buffer the 402 so it can be passed through untouched when payment
    // is impossible. 402 bodies are small (a JSON error envelope).
    let status = first.status();
    let resp_headers = first.headers().clone();
    let resp_body = match first.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: failed to read 402 body");
            return (
                StatusCode::BAD_GATEWAY,
                format!("payer proxy: upstream error: {e}"),
            )
                .into_response();
        }
    };

    let challenges = pay_core::mpp::parse_all(
        resp_headers
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    if challenges.is_empty() {
        tracing::warn!(%url, "payer proxy: 402 without an MPP Payment challenge — passing through");
        return buffered_response(status, &resp_headers, resp_body);
    }

    // `select_challenge_by_balance` / `build_credential` spin their own
    // runtimes and may block on RPC + signing — keep them off the async
    // workers.
    let auth = {
        let state = state.clone();
        let resource_url = url.clone();
        tokio::task::spawn_blocking(move || {
            build_payment_authorization(&state, &challenges, &resource_url)
        })
        .await
    };
    let auth = match auth {
        Ok(Ok(auth)) => auth,
        Ok(Err(e)) => {
            tracing::warn!(%url, error = %e, "payer proxy: could not pay 402 — passing it through");
            return buffered_response(status, &resp_headers, resp_body);
        }
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: payment task failed — passing the 402 through");
            return buffered_response(status, &resp_headers, resp_body);
        }
    };

    tracing::info!(%url, "payer proxy: 402 paid — retrying once with Payment credential");
    match send_upstream(&state, &method, &url, &headers, body, Some(&auth)).await {
        Ok(retry) => stream_response(retry),
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: paid retry failed — returning the original 402");
            buffered_response(status, &resp_headers, resp_body)
        }
    }
}

/// Select a payable challenge and build the `Authorization: Payment …`
/// header value — the same two calls `pay curl`'s
/// `pay_mpp_and_retry` makes (crates/cli/src/commands/mod.rs).
fn build_payment_authorization(
    state: &PayerState,
    challenges: &[pay_core::mpp::Challenge],
    resource_url: &str,
) -> pay_core::Result<String> {
    let store = state.store.as_ref();
    let network_override = state.network_override.as_deref();
    let account_override = state.account_override.as_deref();

    let challenge = pay_core::mpp::select_challenge_by_balance(
        challenges,
        store,
        network_override,
        account_override,
    )?
    .ok_or_else(|| {
        pay_core::Error::Mpp(format!(
            "no MPP challenge matched the payer's network context (forced: {})",
            network_override.unwrap_or("auto")
        ))
    })?;

    let (auth_header, ephemeral_notice) = pay_core::mpp::build_credential(
        challenge,
        store,
        network_override,
        account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = ephemeral_notice {
        tracing::info!(
            network = %resolved.network,
            pubkey = resolved.account.pubkey.as_deref().unwrap_or("(unknown)"),
            "payer proxy: generated ephemeral wallet"
        );
    }

    Ok(auth_header)
}

/// Forward a request upstream, replaying `body`. When `auth` is `Some`,
/// the request's `Authorization` header is replaced with the MPP
/// credential (the paid retry).
async fn send_upstream(
    state: &PayerState,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    body: Bytes,
    auth: Option<&str>,
) -> reqwest::Result<reqwest::Response> {
    let mut fwd = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop_request_header(name.as_str()) {
            continue;
        }
        fwd.append(name.clone(), value.clone());
    }
    if let Some(auth) = auth {
        fwd.remove(header::AUTHORIZATION);
        if let Ok(value) = HeaderValue::from_str(auth) {
            fwd.insert(header::AUTHORIZATION, value);
        }
    }

    state
        .client
        .request(method.clone(), url)
        .headers(fwd)
        .body(body)
        .send()
        .await
}

/// Stream an upstream response back to the client without buffering —
/// SSE completion streams must flow chunk by chunk.
fn stream_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let headers = resp.headers().clone();

    let mut builder = Response::builder().status(status);
    if let Some(dst) = builder.headers_mut() {
        copy_response_headers(dst, &headers);
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "payer proxy: response build error",
            )
                .into_response()
        })
}

/// Rebuild a fully-buffered upstream response (the 402 passthrough path).
fn buffered_response(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(dst) = builder.headers_mut() {
        copy_response_headers(dst, headers);
    }
    builder.body(Body::from(body)).unwrap_or_else(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "payer proxy: response build error",
        )
            .into_response()
    })
}

fn copy_response_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for (name, value) in src {
        if is_hop_by_hop_response_header(name.as_str()) {
            continue;
        }
        dst.append(name.clone(), value.clone());
    }
}

/// Hop-by-hop request headers (RFC 9110 §7.6.1) plus `host` and
/// `content-length`, which reqwest regenerates for the upstream leg.
fn is_hop_by_hop_request_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("proxy-authenticate")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("upgrade")
        || name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("content-length")
}

fn is_hop_by_hop_response_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("proxy-authenticate")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("upgrade")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use pay_core::accounts::MemoryAccountsStore;

    use super::*;

    /// A canned MPP charge challenge that signs fully offline: the
    /// embedded `recentBlockhash` skips the blockhash RPC and the
    /// embedded `tokenProgram` + `decimals` skip the mint-account RPC
    /// (verified against `pay_kit::mpp::client::charge::resolve_blockhash`
    /// / `resolve_token_program`).
    fn challenge_header(network: &str) -> String {
        let request = serde_json::json!({
            "amount": "10000",
            "currency": "USDC",
            "recipient": "So11111111111111111111111111111111111111112",
            "methodDetails": {
                "network": network,
                "recentBlockhash": "9zrUHnA1nCByPksy3aL8tQ47vqdaG2vnFs4HrxgcZj4F",
                "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "decimals": 6,
            },
        });
        let challenge = pay_core::mpp::Challenge::new(
            "USDC",
            "test",
            "solana",
            "charge",
            pay_kit::mpp::Base64UrlJson::from_value(&request).unwrap(),
        );
        pay_kit::mpp::format_www_authenticate(&challenge).unwrap()
    }

    async fn spawn_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    /// Spawn the payer with an in-memory accounts store so tests never
    /// touch (or create) the user's real `~/.config/pay/accounts.yml`.
    async fn spawn_payer(upstream: String, network_override: Option<&str>) -> String {
        let store: Arc<dyn AccountsStore> = Arc::new(MemoryAccountsStore::new());
        let state = Arc::new(
            PayerState::new(upstream, store, network_override.map(str::to_string), None).unwrap(),
        );
        spawn_server(router(state)).await
    }

    #[derive(Default)]
    struct StubSeen {
        calls: usize,
        first_uri: Option<String>,
        retry_auth: Option<String>,
        retry_body: Option<Vec<u8>>,
    }

    async fn stub_402_then_ok(State(seen): State<Arc<Mutex<StubSeen>>>, req: Request) -> Response {
        let uri = req.uri().to_string();
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();

        let mut seen = seen.lock().unwrap();
        seen.calls += 1;
        if seen.calls == 1 {
            seen.first_uri = Some(uri);
            return Response::builder()
                .status(StatusCode::PAYMENT_REQUIRED)
                .header(header::WWW_AUTHENTICATE, challenge_header("localnet"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"payment required"}"#))
                .unwrap();
        }
        seen.retry_auth = auth;
        seen.retry_body = Some(body.to_vec());
        (StatusCode::OK, "paid ok").into_response()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pays_mpp_402_once_and_replays_identical_body() {
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        let app = Router::new()
            .fallback(any(stub_402_then_ok))
            .with_state(seen.clone());
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, None).await;

        let body = r#"{"model":"llama3.2","messages":[{"role":"user","content":"hi"}]}"#;
        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages?beta=true"))
            .header("authorization", "Bearer ollama")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "paid ok");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.calls, 2, "exactly one retry after the 402");
        assert_eq!(
            seen.first_uri.as_deref(),
            Some("/v1/messages?beta=true"),
            "path + query must be preserved"
        );
        let auth = seen
            .retry_auth
            .as_deref()
            .expect("retry must carry Authorization");
        assert!(
            auth.starts_with("Payment "),
            "retry must carry an MPP Payment credential, got: {auth}"
        );
        assert_eq!(
            seen.retry_body.as_deref(),
            Some(body.as_bytes()),
            "retry must replay the identical request body"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passes_non_402_responses_through() {
        let app = Router::new().fallback(any(|| async {
            Response::builder()
                .status(StatusCode::CREATED)
                .header("x-upstream", "yes")
                .body(Body::from("hello upstream"))
                .unwrap()
        }));
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .get(format!("{payer}/v1/models"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(resp.headers().get("x-upstream").unwrap(), "yes");
        assert_eq!(resp.text().await.unwrap(), "hello upstream");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passes_402_through_untouched_when_challenge_is_mainnet_in_sandbox_context() {
        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let app = Router::new().fallback(any(move || {
            let counter = counter.clone();
            async move {
                *counter.lock().unwrap() += 1;
                Response::builder()
                    .status(StatusCode::PAYMENT_REQUIRED)
                    .header(header::WWW_AUTHENTICATE, challenge_header("mainnet"))
                    .body(Body::from("mainnet money required"))
                    .unwrap()
            }
        }));
        let upstream = spawn_server(app).await;
        // `--sandbox` context: network forced to localnet — the payer must
        // refuse to sign for a mainnet challenge and pass the 402 through.
        let payer = spawn_payer(upstream, Some("localnet")).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(
            resp.headers().get(header::WWW_AUTHENTICATE).is_some(),
            "original challenge must survive the passthrough"
        );
        assert_eq!(resp.text().await.unwrap(), "mainnet money required");
        assert_eq!(*calls.lock().unwrap(), 1, "no paid retry may be attempted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passes_402_without_mpp_challenge_through() {
        let app = Router::new().fallback(any(|| async {
            Response::builder()
                .status(StatusCode::PAYMENT_REQUIRED)
                .header(header::WWW_AUTHENTICATE, "Bearer realm=\"nope\"")
                .body(Body::from("not mpp"))
                .unwrap()
        }));
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .get(format!("{payer}/v1/models"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(resp.text().await.unwrap(), "not mpp");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_sse_bodies_incrementally() {
        // The upstream holds the second SSE chunk hostage until the test
        // proves it received the first — impossible if the payer buffers
        // the response instead of streaming it.
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let gate = Arc::new(Mutex::new(Some(gate_rx)));
        let app = Router::new().fallback(any(move || {
            let gate = gate.clone();
            async move {
                let gate_rx = gate.lock().unwrap().take();
                let stream = async_stream::stream! {
                    yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: one\n\n"));
                    if let Some(rx) = gate_rx {
                        let _ = rx.await;
                    }
                    yield Ok(Bytes::from_static(b"data: two\n\n"));
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }));
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, None).await;

        let mut resp = reqwest::Client::new()
            .get(format!("{payer}/v1/messages"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );

        let mut first = Vec::new();
        while !first.ends_with(b"data: one\n\n") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
                .await
                .expect("payer buffered the SSE stream instead of forwarding chunks")
                .unwrap()
                .expect("stream ended before the first SSE event");
            first.extend_from_slice(&chunk);
        }

        gate_tx.send(()).unwrap();

        let mut rest = Vec::new();
        while let Some(chunk) = resp.chunk().await.unwrap() {
            rest.extend_from_slice(&chunk);
        }
        assert_eq!(&rest[..], b"data: two\n\n");
    }
}
