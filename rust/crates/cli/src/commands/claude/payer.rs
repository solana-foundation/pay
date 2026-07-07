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
//!
//! **Dialects.** When the upstream speaks OpenAI chat completions
//! ([`Dialect::OpenAiCompat`] — vLLM, LM Studio, llama.cpp, Alibaba Model
//! Studio's compatible mode), `POST /v1/messages` requests are translated
//! to OpenAI shape (see [`super::translate`]), sent to the upstream's
//! chat-completions path, and the response — buffered JSON or incremental
//! SSE — is translated back to an Anthropic envelope. The 402 pay-retry
//! composes with translation: the challenge fires on the OpenAI-side
//! request, so the retry replays the *translated* body. All other
//! requests and dialects pass through untouched.

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

use super::translate;
use crate::commands::server::inference::providers::Dialect;

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

/// Where the payer forwards to, and how to talk to it.
#[derive(Clone)]
pub struct PayerUpstream {
    /// Upstream base URL, e.g. `http://127.0.0.1:11434` or
    /// `https://modelstudio.alibaba.gateway-402.com`.
    pub base_url: String,
    /// Chat-API wire dialect of the upstream. [`Dialect::Anthropic`]
    /// passes through; [`Dialect::OpenAiCompat`] translates
    /// `POST /v1/messages`; anything else passes through untranslated
    /// (the launcher warns about those picks).
    pub dialect: Dialect,
    /// Chat-completions path (gate convention, no leading slash) that
    /// translated requests are sent to, e.g. `v1/chat/completions` or
    /// Alibaba's `compatible-mode/v1/chat/completions`.
    pub chat_path: String,
}

/// Shared state for the proxy handlers.
struct PayerState {
    /// Upstream base URL without a trailing slash.
    upstream: String,
    dialect: Dialect,
    /// Chat-completions path without a leading slash (translation target).
    chat_path: String,
    client: reqwest::Client,
    store: Arc<dyn AccountsStore>,
    /// Forced network slug (`--sandbox` → `localnet`, `--mainnet` →
    /// `mainnet`). `None` trusts the challenge's `methodDetails.network`.
    network_override: Option<String>,
    /// `--account` override, same semantics as the curl/fetch retry path.
    account_override: Option<String>,
    /// Optional per-request spend cap (base units of the challenge asset). When
    /// set, an x402-upto ceiling above this is refused and the 402 passes
    /// through. `None` (today's default) authorizes whatever the challenge
    /// advertises, matching the MPP path's unbudgeted behavior.
    per_request_cap_base_units: Option<u128>,
}

impl PayerState {
    fn new(
        upstream: PayerUpstream,
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
            upstream: upstream.base_url.trim_end_matches('/').to_string(),
            dialect: upstream.dialect,
            chat_path: upstream.chat_path.trim_start_matches('/').to_string(),
            client,
            store,
            network_override,
            account_override,
            per_request_cap_base_units: None,
        })
    }
}

/// Start the payer proxy on an ephemeral 127.0.0.1 port, on a dedicated
/// runtime in a background thread (the `pay claude` main thread stays
/// sync and blocks on the `claude` child process). Returns once the
/// listener is bound.
pub fn start_background(
    upstream: PayerUpstream,
    network_override: Option<&str>,
    account_override: Option<&str>,
) -> pay_core::Result<PayerProxy> {
    let store: Arc<dyn AccountsStore> = Arc::new(FileAccountsStore::default_path());
    let state = Arc::new(PayerState::new(
        upstream,
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
    let path = req.uri().path().to_string();
    let path_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

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

    // Anthropic → OpenAI request translation for OpenAI-compatible
    // upstreams. Happens BEFORE the send/402 loop so the pay-retry
    // replays the translated body. Everything else passes through.
    let (url, body, translated) = match translate_request(&state, &method, &path, &body) {
        Some(openai_body) => (
            format!("{}/{}", state.upstream, state.chat_path),
            openai_body,
            true,
        ),
        None => (format!("{}{}", state.upstream, path_query), body, false),
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
        return deliver(first, translated).await;
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

    // Scheme precedence: MPP charge first (the established path), then x402
    // `upto` (per-token gateway). MPP wins when both are advertised — it is
    // an exact, single-shot charge, whereas `upto` opens a spending channel
    // for a ceiling; prefer the tighter commitment when the server offers a
    // choice.
    let payment = if !challenges.is_empty() {
        // `select_challenge_by_balance` / `build_credential` spin their own
        // runtimes and may block on RPC + signing — keep them off the async
        // workers.
        let state = state.clone();
        let resource_url = url.clone();
        tokio::task::spawn_blocking(move || {
            build_payment_authorization(&state, &challenges, &resource_url)
        })
        .await
    } else if let Some(upto) = parse_upto_challenge(&resp_headers, &resp_body) {
        // x402 `upto`: open a channel for the advertised ceiling; the gateway
        // settles the actual per-token cost after serving and refunds the rest.
        let state = state.clone();
        let resource_url = url.clone();
        tokio::task::spawn_blocking(move || build_upto_authorization(&state, &upto, &resource_url))
            .await
    } else {
        tracing::warn!(%url, "payer proxy: 402 without an MPP or x402-upto challenge — passing through");
        return buffered_response(status, &resp_headers, resp_body);
    };

    let payment = match payment {
        Ok(Ok(payment)) => payment,
        Ok(Err(e)) => {
            tracing::warn!(%url, error = %e, "payer proxy: could not pay 402 — passing it through");
            return buffered_response(status, &resp_headers, resp_body);
        }
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: payment task failed — passing the 402 through");
            return buffered_response(status, &resp_headers, resp_body);
        }
    };

    tracing::info!(%url, "payer proxy: 402 paid — retrying once with payment credential");
    match send_upstream(&state, &method, &url, &headers, body, Some(&payment)).await {
        Ok(retry) => deliver(retry, translated).await,
        Err(e) => {
            tracing::warn!(%url, error = %e, "payer proxy: paid retry failed — returning the original 402");
            buffered_response(status, &resp_headers, resp_body)
        }
    }
}

/// Parse an x402 `upto` challenge off the buffered 402 response. `parse_upto`
/// wants `(name, value)` string pairs and an optional body string; build them
/// from the response's headers and (UTF-8) body.
fn parse_upto_challenge(
    resp_headers: &HeaderMap,
    resp_body: &Bytes,
) -> Option<pay_core::client::x402::UptoChallenge> {
    let headers: Vec<(String, String)> = resp_headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    let body = std::str::from_utf8(resp_body).ok();
    pay_core::client::x402::parse_upto(&headers, body)
}

/// When the upstream is OpenAI-compatible and the inbound request is
/// Claude Code's `POST /v1/messages`, return the translated OpenAI body.
/// `None` means pass through untranslated (wrong dialect/path, or a body
/// that isn't JSON — the upstream will produce the error).
fn translate_request(
    state: &PayerState,
    method: &Method,
    path: &str,
    body: &Bytes,
) -> Option<Bytes> {
    if state.dialect != Dialect::OpenAiCompat || method != Method::POST || path != "/v1/messages" {
        return None;
    }
    let anthropic: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "payer proxy: /v1/messages body is not JSON — passing through untranslated");
            return None;
        }
    };
    let openai = translate::anthropic_to_openai_request(&anthropic);
    match serde_json::to_vec(&openai) {
        Ok(bytes) => Some(Bytes::from(bytes)),
        Err(e) => {
            tracing::warn!(error = %e, "payer proxy: failed to serialize translated request — passing through");
            None
        }
    }
}

/// Hand an upstream response back to Claude Code: translated (SSE or
/// buffered JSON) when the request was translated and succeeded,
/// streamed passthrough otherwise.
async fn deliver(resp: reqwest::Response, translated: bool) -> Response {
    if !translated || !resp.status().is_success() {
        return stream_response(resp);
    }
    let is_sse = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));
    if is_sse {
        translate_stream_response(resp)
    } else {
        translate_json_response(resp).await
    }
}

/// Buffer an OpenAI `chat.completion` JSON response and return the
/// Anthropic message envelope. Falls back to raw passthrough when the
/// body isn't JSON.
async fn translate_json_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let upstream_headers = resp.headers().clone();
    let bytes = match resp.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "payer proxy: failed to read upstream response body");
            return (
                StatusCode::BAD_GATEWAY,
                format!("payer proxy: upstream error: {e}"),
            )
                .into_response();
        }
    };
    let openai: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "payer proxy: upstream response is not JSON — passing through");
            return buffered_response(status, &upstream_headers, bytes);
        }
    };
    let anthropic = translate::openai_to_anthropic_response(&openai);
    let body = serde_json::to_vec(&anthropic).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "payer proxy: response build error",
            )
                .into_response()
        })
}

/// Wrap an OpenAI SSE response in the incremental Anthropic-SSE
/// translator — chunks flow as they arrive; a carry-over buffer inside
/// [`translate::StreamTranslator`] reassembles SSE lines split across
/// chunk boundaries.
fn translate_stream_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let stream_body = Body::from_stream(async_stream::stream! {
        let mut resp = resp;
        let mut translator = translate::StreamTranslator::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let out = translator.push(&chunk);
                    if !out.is_empty() {
                        yield Ok::<_, std::io::Error>(Bytes::from(out));
                    }
                }
                Ok(None) => {
                    let out = translator.finish();
                    if !out.is_empty() {
                        yield Ok(Bytes::from(out));
                    }
                    break;
                }
                Err(e) => {
                    yield Err(std::io::Error::other(e));
                    break;
                }
            }
        }
    });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(stream_body)
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "payer proxy: response build error",
            )
                .into_response()
        })
}

/// The header(s) a paid retry must carry. MPP charge sets a single
/// `Authorization: Payment <credential>`; x402 `upto` sets `PAYMENT-SIGNATURE`
/// (and never touches `Authorization`, so the caller's upstream key survives).
struct PaidHeaders {
    headers: Vec<(String, String)>,
}

impl PaidHeaders {
    /// The MPP charge credential goes into `Authorization: Payment …`.
    fn mpp(credential: String) -> Self {
        Self {
            headers: vec![(header::AUTHORIZATION.as_str().to_string(), credential)],
        }
    }
}

/// Select a payable MPP challenge and build the `Authorization: Payment …`
/// header value — the same two calls `pay curl`'s
/// `pay_mpp_and_retry` makes (crates/cli/src/commands/mod.rs).
fn build_payment_authorization(
    state: &PayerState,
    challenges: &[pay_core::mpp::Challenge],
    resource_url: &str,
) -> pay_core::Result<PaidHeaders> {
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

    Ok(PaidHeaders::mpp(auth_header))
}

/// Open an x402 `upto` channel for the challenge's advertised ceiling and
/// return the `PAYMENT-SIGNATURE` retry header — the exact
/// [`pay_core::client::x402::build_upto_payment`] call `pay curl`'s
/// `pay_upto_and_retry` makes (crates/cli/src/commands/mod.rs). The authorized
/// deposit is the challenge's own `amount` (the ceiling), so a per-request cap,
/// when the payer grows one, is enforced here before signing.
fn build_upto_authorization(
    state: &PayerState,
    challenge: &pay_core::client::x402::UptoChallenge,
    resource_url: &str,
) -> pay_core::Result<PaidHeaders> {
    let store = state.store.as_ref();
    let network_override = state.network_override.as_deref();
    let account_override = state.account_override.as_deref();

    // The channel deposit the client signs is the challenge's advertised
    // ceiling; authorize exactly that. If the payer ever carries a per-request
    // budget cap, refuse (and pass the 402 through) when the ceiling exceeds
    // it — never silently open a larger channel than the caller allows.
    if let Some(cap) = state.per_request_cap_base_units {
        let ceiling: u128 = challenge.requirements.amount.parse().map_err(|_| {
            pay_core::Error::Mpp(format!(
                "x402-upto challenge advertised a non-numeric ceiling: {}",
                challenge.requirements.amount
            ))
        })?;
        if ceiling > cap {
            return Err(pay_core::Error::Mpp(format!(
                "x402-upto ceiling {ceiling} (base units of {}) exceeds the payer's per-request budget {cap}",
                challenge.requirements.asset
            )));
        }
    }

    let built = pay_core::client::x402::build_upto_payment(
        challenge,
        store,
        network_override,
        account_override,
        Some(resource_url),
    )?;

    if let Some(resolved) = built.ephemeral_notice {
        tracing::info!(
            network = %resolved.network,
            pubkey = resolved.account.pubkey.as_deref().unwrap_or("(unknown)"),
            "payer proxy: generated ephemeral wallet"
        );
    }

    Ok(PaidHeaders {
        headers: built
            .headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    })
}

/// Forward a request upstream, replaying `body`. When `payment` is `Some`,
/// its headers are applied to the paid retry: MPP replaces `Authorization`
/// with the `Payment` credential; x402-upto adds `PAYMENT-SIGNATURE` (leaving
/// the caller's own `Authorization` intact).
async fn send_upstream(
    state: &PayerState,
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    body: Bytes,
    payment: Option<&PaidHeaders>,
) -> reqwest::Result<reqwest::Response> {
    let mut fwd = HeaderMap::new();
    for (name, value) in headers {
        if is_hop_by_hop_request_header(name.as_str()) {
            continue;
        }
        fwd.append(name.clone(), value.clone());
    }
    if let Some(payment) = payment {
        for (name, value) in &payment.headers {
            let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                continue;
            };
            // A paid header replaces any inbound copy (MPP's `Authorization`,
            // an echoed `PAYMENT-SIGNATURE`) rather than appending a duplicate.
            fwd.remove(&name);
            fwd.insert(name, value);
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

    /// A canned x402 `upto` challenge that signs fully offline: `network` is a
    /// devnet CAIP-2 (so with no `--sandbox`/`--mainnet` override the payer
    /// lazily mints a throwaway devnet wallet, exactly like the MPP test), the
    /// embedded `recentBlockhash` builds the open transaction with no RPC, and
    /// `payTo == facilitatorAddress` so no distribution split is derived. The
    /// blockhash is *not* Surfpool-prefixed, so no auto-fund network call fires.
    /// Returns the base64 `PAYMENT-REQUIRED` header value the gateway emits.
    fn upto_challenge_header(amount: &str) -> String {
        let payee = "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY";
        let envelope = serde_json::json!({
            "x402Version": 2,
            "accepts": [{
                "scheme": "upto",
                "network": pay_kit::x402::exact::SOLANA_DEVNET,
                "amount": amount,
                "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": payee,
                "maxTimeoutSeconds": 300,
                "extra": {
                    "assetTransferMethod": "payment-channel",
                    "facilitatorAddress": payee,
                    "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                    "recentBlockhash": "9zrUHnA1nCByPksy3aL8tQ47vqdaG2vnFs4HrxgcZj4F",
                },
            }],
        });
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(envelope.to_string().as_bytes())
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
    async fn spawn_payer_with(upstream: PayerUpstream, network_override: Option<&str>) -> String {
        let store: Arc<dyn AccountsStore> = Arc::new(MemoryAccountsStore::new());
        let state = Arc::new(
            PayerState::new(upstream, store, network_override.map(str::to_string), None).unwrap(),
        );
        spawn_server(router(state)).await
    }

    /// Anthropic-dialect payer (pure passthrough — no translation).
    async fn spawn_payer(upstream: String, network_override: Option<&str>) -> String {
        spawn_payer_with(
            PayerUpstream {
                base_url: upstream,
                dialect: Dialect::Anthropic,
                chat_path: "v1/chat/completions".to_string(),
            },
            network_override,
        )
        .await
    }

    /// OpenAI-compat payer targeting Alibaba's compatible-mode chat path.
    async fn spawn_openai_payer(upstream: String, network_override: Option<&str>) -> String {
        spawn_payer_with(
            PayerUpstream {
                base_url: upstream,
                dialect: Dialect::OpenAiCompat,
                chat_path: "compatible-mode/v1/chat/completions".to_string(),
            },
            network_override,
        )
        .await
    }

    const OPENAI_COMPLETION_JSON: &str = r#"{
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "qwen-max",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello!" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 3 }
    }"#;

    fn anthropic_request_body() -> serde_json::Value {
        serde_json::json!({
            "model": "qwen-max",
            "max_tokens": 100,
            "system": "be brief",
            "messages": [{ "role": "user", "content": "hi" }],
        })
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

    /// Stub that answers the first request with an x402 `upto` 402 (a
    /// `PAYMENT-REQUIRED` header) and every retry with 200, recording the
    /// retry's `PAYMENT-SIGNATURE` header and body. `amount` is the advertised
    /// ceiling (base units).
    fn upto_stub(seen: Arc<Mutex<StubSeen>>, amount: &'static str) -> Router {
        Router::new().fallback(any(move |req: Request| {
            let seen = seen.clone();
            async move {
                let uri = req.uri().to_string();
                let sig = req
                    .headers()
                    .get("payment-signature")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
                    .await
                    .unwrap();
                let mut seen = seen.lock().unwrap();
                seen.calls += 1;
                if seen.calls == 1 {
                    seen.first_uri = Some(uri);
                    seen.retry_body = Some(body.to_vec()); // first (buffered) body
                    return Response::builder()
                        .status(StatusCode::PAYMENT_REQUIRED)
                        .header("payment-required", upto_challenge_header(amount))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"error":"payment required"}"#))
                        .unwrap();
                }
                // The upto retry must replay the identical body and must NOT
                // clobber the caller's Authorization (upto uses its own
                // PAYMENT-SIGNATURE header).
                assert_eq!(
                    seen.retry_body.as_deref(),
                    Some(&body[..]),
                    "upto retry must replay the identical body"
                );
                seen.retry_auth = sig;
                (StatusCode::OK, "upto paid ok").into_response()
            }
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pays_x402_upto_402_and_retries_with_payment_header() {
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        // $0.50 ceiling in USDC base units — the gateway's MAX_REQUEST_USD.
        let upstream = spawn_server(upto_stub(seen.clone(), "500000")).await;
        // No override: the devnet challenge lazily mints a throwaway wallet and
        // signs the channel-open offline (embedded, non-Surfpool blockhash).
        let payer = spawn_payer(upstream, None).await;

        let body = r#"{"model":"llama3.2","messages":[{"role":"user","content":"hi"}]}"#;
        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages?beta=true"))
            .header("authorization", "Bearer upstream-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "upto paid ok");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.calls, 2, "exactly one retry after the upto 402");
        assert_eq!(
            seen.first_uri.as_deref(),
            Some("/v1/messages?beta=true"),
            "path + query must be preserved"
        );
        let sig = seen
            .retry_auth
            .as_deref()
            .expect("upto retry must carry a PAYMENT-SIGNATURE header");
        assert!(!sig.is_empty(), "PAYMENT-SIGNATURE must be non-empty");
        assert_eq!(
            seen.retry_body.as_deref(),
            Some(body.as_bytes()),
            "upto retry must replay the identical request body"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pays_x402_upto_402_from_openai_upstream_and_replays_translated_body() {
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        let record = seen.clone();
        let app = Router::new().route(
            "/compatible-mode/v1/chat/completions",
            axum::routing::post(move |req: Request| {
                let record = record.clone();
                async move {
                    let sig = req
                        .headers()
                        .get("payment-signature")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
                        .await
                        .unwrap();
                    let mut seen = record.lock().unwrap();
                    seen.calls += 1;
                    if seen.calls == 1 {
                        seen.retry_body = Some(body.to_vec()); // first (translated) body
                        return Response::builder()
                            .status(StatusCode::PAYMENT_REQUIRED)
                            .header("payment-required", upto_challenge_header("500000"))
                            .body(Body::from(r#"{"error":"payment required"}"#))
                            .unwrap();
                    }
                    assert_eq!(
                        seen.retry_body.as_deref(),
                        Some(&body[..]),
                        "upto retry must replay the exact translated body"
                    );
                    seen.retry_auth = sig;
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(OPENAI_COMPLETION_JSON))
                        .unwrap()
                }
            }),
        );
        let upstream = spawn_server(app).await;
        let payer = spawn_openai_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .json(&anthropic_request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["type"], "message", "paid retry comes back translated");
        assert_eq!(body["content"][0]["text"], "Hello!");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.calls, 2, "exactly one upto retry");
        assert!(
            seen.retry_auth.as_deref().is_some_and(|s| !s.is_empty()),
            "upto retry carries PAYMENT-SIGNATURE"
        );
        // The body the 402 fired on — and the retry replayed — is the
        // translated OpenAI body.
        let translated: serde_json::Value =
            serde_json::from_slice(seen.retry_body.as_deref().unwrap()).unwrap();
        assert_eq!(translated["messages"][0]["role"], "system");
        assert_eq!(translated["messages"][1]["content"], "hi");
        assert!(translated.get("system").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passes_upto_402_through_when_open_fails() {
        // A `--sandbox` (localnet-forced) payer facing a devnet upto challenge:
        // `check_client_network_intent` refuses to sign for a network the user
        // did not opt into, so the channel open fails and the original 402 must
        // pass through untouched with no retry.
        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let app = Router::new().fallback(any(move || {
            let counter = counter.clone();
            async move {
                *counter.lock().unwrap() += 1;
                Response::builder()
                    .status(StatusCode::PAYMENT_REQUIRED)
                    .header("payment-required", upto_challenge_header("500000"))
                    .body(Body::from("upto money required"))
                    .unwrap()
            }
        }));
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, Some("localnet")).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        assert!(
            resp.headers().get("payment-required").is_some(),
            "original upto challenge must survive the passthrough"
        );
        assert_eq!(resp.text().await.unwrap(), "upto money required");
        assert_eq!(*calls.lock().unwrap(), 1, "no paid retry may be attempted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mpp_charge_takes_precedence_over_coexisting_upto_challenge() {
        // A 402 that advertises BOTH an MPP charge (WWW-Authenticate) and an
        // x402 upto (PAYMENT-REQUIRED). Scheme precedence: MPP wins, so the
        // retry carries an `Authorization: Payment …` credential, not a
        // PAYMENT-SIGNATURE.
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        let record = seen.clone();
        let app = Router::new().fallback(any(move |req: Request| {
            let record = record.clone();
            async move {
                let auth = req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let has_sig = req.headers().get("payment-signature").is_some();
                let _ = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await;
                let mut seen = record.lock().unwrap();
                seen.calls += 1;
                if seen.calls == 1 {
                    return Response::builder()
                        .status(StatusCode::PAYMENT_REQUIRED)
                        .header(header::WWW_AUTHENTICATE, challenge_header("localnet"))
                        .header("payment-required", upto_challenge_header("500000"))
                        .body(Body::from(r#"{"error":"payment required"}"#))
                        .unwrap();
                }
                seen.retry_auth = auth;
                seen.first_uri = Some(has_sig.to_string()); // reuse slot: "true"/"false"
                (StatusCode::OK, "mpp paid ok").into_response()
            }
        }));
        let upstream = spawn_server(app).await;
        let payer = spawn_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .body(r#"{"messages":[]}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "mpp paid ok");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.calls, 2, "exactly one paid retry");
        let auth = seen
            .retry_auth
            .as_deref()
            .expect("MPP retry must carry Authorization");
        assert!(
            auth.starts_with("Payment "),
            "MPP charge must take precedence, got: {auth}"
        );
        assert_eq!(
            seen.first_uri.as_deref(),
            Some("false"),
            "the MPP retry must NOT also carry a PAYMENT-SIGNATURE header"
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

    // ── OpenAI-compat dialect loopback ─────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn translates_anthropic_request_and_openai_response_for_openai_upstream() {
        let seen = Arc::new(Mutex::new(Option::<serde_json::Value>::None));
        let record = seen.clone();
        let app = Router::new().route(
            "/compatible-mode/v1/chat/completions",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let record = record.clone();
                async move {
                    *record.lock().unwrap() = Some(body.0);
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        OPENAI_COMPLETION_JSON,
                    )
                }
            }),
        );
        let upstream = spawn_server(app).await;
        let payer = spawn_openai_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages?beta=true"))
            .json(&anthropic_request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(
            body["content"],
            serde_json::json!([{ "type": "text", "text": "Hello!" }])
        );
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 12);
        assert_eq!(body["usage"]["output_tokens"], 3);

        // The upstream must have received the OpenAI shape.
        let sent = seen
            .lock()
            .unwrap()
            .clone()
            .expect("upstream saw the request");
        assert_eq!(
            sent["messages"],
            serde_json::json!([
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
            ])
        );
        assert_eq!(sent["model"], "qwen-max");
        assert!(sent.get("system").is_none(), "system moved into messages");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translates_openai_sse_stream_into_anthropic_events() {
        const OPENAI_SSE: &str = concat!(
            "data: {\"id\":\"chatcmpl-7\",\"model\":\"qwen-max\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n",
        );
        let app = Router::new().route(
            "/compatible-mode/v1/chat/completions",
            axum::routing::post(|| async {
                ([(header::CONTENT_TYPE, "text/event-stream")], OPENAI_SSE)
            }),
        );
        let upstream = spawn_server(app).await;
        let payer = spawn_openai_payer(upstream, None).await;

        let mut request = anthropic_request_body();
        request["stream"] = serde_json::json!(true);
        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .json(&request)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );

        let sse = resp.text().await.unwrap();
        let event_names: Vec<&str> = sse
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect();
        assert_eq!(
            event_names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(
            sse.contains(r#""text":"Hello""#) && sse.contains(r#""text_delta""#),
            "text delta must survive translation: {sse}"
        );
        assert!(
            sse.contains(r#""output_tokens":1"#),
            "final usage must reach message_delta: {sse}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pays_402_from_openai_upstream_and_replays_translated_body() {
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        let record = seen.clone();
        let app = Router::new().route(
            "/compatible-mode/v1/chat/completions",
            axum::routing::post(move |req: Request| {
                let record = record.clone();
                async move {
                    let auth = req
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
                        .await
                        .unwrap();
                    let mut seen = record.lock().unwrap();
                    seen.calls += 1;
                    if seen.calls == 1 {
                        seen.retry_body = Some(body.to_vec()); // first (translated) body
                        return Response::builder()
                            .status(StatusCode::PAYMENT_REQUIRED)
                            .header(header::WWW_AUTHENTICATE, challenge_header("localnet"))
                            .body(Body::from(r#"{"error":"payment required"}"#))
                            .unwrap();
                    }
                    let first_body = seen.retry_body.clone();
                    seen.retry_auth = auth;
                    assert_eq!(
                        first_body.as_deref(),
                        Some(&body[..]),
                        "retry must replay the exact translated body"
                    );
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(OPENAI_COMPLETION_JSON))
                        .unwrap()
                }
            }),
        );
        let upstream = spawn_server(app).await;
        let payer = spawn_openai_payer(upstream, None).await;

        let resp = reqwest::Client::new()
            .post(format!("{payer}/v1/messages"))
            .json(&anthropic_request_body())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["type"], "message",
            "paid retry must come back translated"
        );
        assert_eq!(body["content"][0]["text"], "Hello!");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.calls, 2, "exactly one paid retry");
        let auth = seen
            .retry_auth
            .as_deref()
            .expect("retry carries Authorization");
        assert!(
            auth.starts_with("Payment "),
            "MPP credential expected, got: {auth}"
        );
        // The body the 402 fired on — and that the retry replayed — is
        // the *translated* OpenAI body.
        let translated: serde_json::Value =
            serde_json::from_slice(seen.retry_body.as_deref().unwrap()).unwrap();
        assert_eq!(translated["messages"][0]["role"], "system");
        assert_eq!(translated["messages"][1]["content"], "hi");
        assert!(translated.get("system").is_none());
    }
}
