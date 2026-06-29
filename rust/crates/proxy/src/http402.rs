//! `Http402Gate` — a Pingora [`ProxyHttp`] that runs the framework-agnostic
//! [`PaymentGate`] and then proxies **natively**:
//!
//! - metered / forwarded traffic → the endpoint's real upstream, reusing
//!   [`prepare_upstream`] for URL rewrite, header forwarding/stripping and auth
//!   injection (the exact same path axum's `forward_request` uses);
//! - control-plane paths (`/__402/*`, `/openapi.json`, `/.well-known/*`, `/`) →
//!   an internal axum service (`control_plane` address) that still serves them.
//!
//! This is the production data plane: Pingora fronts everything, axum is demoted
//! to an internal control-plane upstream.
//!
//! ## Deferred (documented)
//! - **Body-signing auth**: request bodies stream to the upstream unbuffered, so
//!   auth schemes that digest the body (HMAC / `AccessToken` `body_digest`) can't
//!   be signed here — those requests are **refused with 501** in
//!   [`Http402Gate::plan_upstream`] (loud + logged) rather than silently forwarded
//!   with a signature computed over an empty body. Header / Bearer / OAuth2 /
//!   QueryParam auth, and HMAC that doesn't digest the body, all work. Lifting
//!   this needs request-body buffering before the upstream connect.
//! - **Response-side session metering**: debiting a push channel as bytes flow
//!   back ([`upstream_response_body_filter`]) is not wired yet; session
//!   open/voucher/close (all request-side, in the gate) work.

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use pay_core::PaymentState;
use pay_core::server::gate::{
    GateDecision, GateRequest, GateResponse, PaymentGate, ReceiptAnnotation, settle_upto,
};
use pay_core::server::proxy::{
    STRIP_HEADERS, UpstreamPlan, prepare_upstream, routing_signs_request_body,
};
use pay_kit::x402::server::VerifiedUptoOpen;
use pay_types::metering::ApiSpec;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;

/// Where a non-`Respond` request should be proxied.
enum Target {
    /// Real API upstream resolved from the endpoint spec via `prepare_upstream`.
    Api {
        addr: String,
        tls: bool,
        sni: String,
        host_header: String,
        path_and_query: String,
        /// Forwarded client headers (minus stripped) + injected auth headers.
        headers: Vec<(String, String)>,
    },
    /// Internal axum service that serves the control plane.
    ControlPlane,
}

/// Per-request state threaded across the Pingora lifecycle hooks.
pub struct Ctx {
    target: Option<Target>,
    receipt: Option<ReceiptAnnotation>,
    /// An x402 `upto` channel opened pre-serve, settled in `response_filter`
    /// (debit on success) or `logging` (refund when the upstream never
    /// responded). Taken when settled, so it's never double-settled. The `u64`
    /// is the success voucher amount (configured `min`, or full ceiling).
    upto: Option<(VerifiedUptoOpen, u64)>,
    /// Captured at `request_filter` for the Payment Debugger exchange emitted
    /// in `logging` (the data plane is Pingora, so the old axum logging
    /// middleware never sees proxied traffic).
    log: Option<LogStart>,
}

/// Request-side facts captured up front for the PDB exchange.
struct LogStart {
    method: String,
    path: String,
    req_headers: Vec<(String, String)>,
    client_ip: String,
    start: std::time::Instant,
}

/// Pingora payment gate over [`PaymentGate`], parameterized over the host's
/// [`PaymentState`].
pub struct Http402Gate<S: PaymentState> {
    state: S,
    /// `host:port` of the internal axum service handling the control plane.
    control_plane: String,
}

impl<S: PaymentState> Http402Gate<S> {
    pub fn new(state: S, control_plane: impl Into<String>) -> Self {
        Self {
            state,
            control_plane: control_plane.into(),
        }
    }

    /// Resolve the API spec for a host — subdomain match, single-API fallback —
    /// the same resolution [`PaymentGate::evaluate`] uses.
    fn resolve_api<'a>(&'a self, host: &str) -> Option<&'a ApiSpec> {
        let apis = self.state.apis();
        let subdomain = host.split('.').next().unwrap_or("");
        apis.iter()
            .find(|a| a.subdomain == subdomain)
            .or_else(|| if apis.len() == 1 { apis.first() } else { None })
    }

    /// Plan the upstream for a Forward/Passthrough decision: control-plane → the
    /// internal axum service; otherwise resolve + `prepare_upstream`. Returns
    /// `Ok(true)` if a response was written here (respond-mode / not-found /
    /// upstream-prep error), `Ok(false)` to continue to `upstream_peer`.
    #[allow(clippy::too_many_arguments)]
    async fn plan_upstream(
        &self,
        ctx: &mut Ctx,
        path: &str,
        host: Option<&str>,
        method: &http::Method,
        uri: &Uri,
        headers: &http::HeaderMap,
        session: &mut Session,
        // Whether this request may be served by the internal control-plane axum.
        // Only `Passthrough` (free / discovery) requests qualify; a `Forward`
        // (verified payment) for a root (`path: ""`) endpoint must reach the real
        // upstream, else the client paid but axum re-checks the stripped auth and
        // returns a 402.
        control_plane_ok: bool,
    ) -> pingora::Result<bool> {
        if control_plane_ok && is_control_plane(path) {
            ctx.target = Some(Target::ControlPlane);
            return Ok(false);
        }
        let Some(api) = self.resolve_api(host.unwrap_or("")) else {
            let _ = session
                .respond_error_with_body(404, Bytes::from_static(b"{\"error\":\"not_found\"}"))
                .await;
            return Ok(true);
        };
        // The body is streamed unbuffered (see module docs), so we can't compute
        // a body-digest signature here. Refuse loudly rather than forward a
        // request signed over an empty body (which the upstream would reject with
        // an opaque 401/403). Header / Bearer / OAuth2 / QueryParam auth, and
        // HMAC that doesn't digest the body, are unaffected.
        if routing_signs_request_body(api, path) {
            tracing::error!(
                path,
                "refusing request: upstream auth signs the request body, which the pingora data \
                 plane does not support (body is streamed, not buffered)"
            );
            write_gate_response(
                session,
                GateResponse::json(
                    StatusCode::NOT_IMPLEMENTED,
                    Bytes::from_static(
                        b"{\"error\":\"unsupported_auth\",\"message\":\"This endpoint's upstream \
                          auth signs the request body, which the gateway does not yet support.\"}",
                    ),
                ),
            )
            .await?;
            return Ok(true);
        }
        // No body-signing auth → an empty placeholder body is safe for prep.
        match prepare_upstream(api, method, uri, headers, &[]).await {
            Ok(UpstreamPlan::Forward(prepared)) => {
                ctx.target = Some(target_from_prepared(prepared));
                Ok(false)
            }
            Ok(UpstreamPlan::Respond(resp)) => {
                self.finish_inline(session, ctx, resp).await?;
                Ok(true)
            }
            // Upstream-prep failure (bad URL, OAuth2 token fetch, body-prep): the
            // request ends here too, so run the same post-payment bookkeeping. An
            // x402 `exact` payment already settled on-chain before `Forward`, so
            // the client must still get its PAYMENT-RESPONSE receipt; any `upto`
            // channel is refunded since the resource was never served.
            Err(resp) => {
                self.finish_inline(session, ctx, resp).await?;
                Ok(true)
            }
        }
    }

    /// Finish a request that ends in `plan_upstream` (respond-mode response or an
    /// upstream-prep error) — `response_filter` never runs for these. Settle or
    /// refund any x402 `upto` channel (refund when the resource wasn't served)
    /// and attach the verified-payment receipt, then write the response.
    ///
    /// Draining `ctx.receipt` here matters on the error path: an x402 `exact`
    /// payment settles on-chain before the gate returns `Forward`, so a client
    /// that then hits an upstream-prep failure must still receive its
    /// PAYMENT-RESPONSE header to prove the charge (mirrors the axum gate, which
    /// appends receipt headers regardless of handler outcome).
    async fn finish_inline(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        resp: axum::response::Response,
    ) -> pingora::Result<()> {
        let served_ok = resp.status().is_success();
        let mut extra: Vec<(HeaderName, HeaderValue)> = Vec::new();
        if let Some((open, amt)) = ctx.upto.take()
            && let Some((n, v)) = settle_upto(&self.state, open, amt, served_ok).await
        {
            extra.push((n, v));
        }
        if let Some(receipt) = ctx.receipt.take() {
            extra.extend(receipt.headers);
            if let Some(reference) = &receipt.reference {
                tracing::Span::current().record("tx_sig", reference.as_str());
            }
        }
        write_axum_response(session, resp, extra).await
    }
}

#[async_trait]
impl<S: PaymentState> ProxyHttp for Http402Gate<S> {
    type CTX = Ctx;
    fn new_ctx(&self) -> Ctx {
        Ctx {
            target: None,
            receipt: None,
            upto: None,
            log: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> pingora::Result<bool> {
        let rh = session.req_header();
        let method = rh.method.clone();
        let uri = rh.uri.clone();
        let headers = rh.headers.clone();

        let path = uri.path().trim_start_matches('/').to_string();
        let str_h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
        let host = str_h("host").map(str::to_string);

        // Capture request-side facts for the PDB exchange emitted in `logging`.
        // Skip the control plane's own paths (`/__402/*`) — same as the old
        // axum logging middleware — so the debugger only shows real traffic.
        if !path.starts_with("__402") {
            let client_ip = str_h("x-forwarded-for")
                .and_then(|v| v.split(',').next())
                .map(|s| s.trim().to_string())
                .or_else(|| host.clone())
                .unwrap_or_else(|| "unknown".to_string());
            ctx.log = Some(LogStart {
                method: method.to_string(),
                path: format!("/{path}"),
                req_headers: header_pairs(&headers),
                client_ip,
                start: std::time::Instant::now(),
            });
        }
        let accept = str_h("accept").map(str::to_string);
        let authorization = str_h("authorization").map(str::to_string);
        let content_length = str_h("content-length").and_then(|v| v.parse::<u64>().ok());
        let query = uri.query().map(str::to_string);
        let x402_payment = str_h(pay_kit::x402::PAYMENT_SIGNATURE_HEADER)
            .or_else(|| str_h(pay_kit::x402::X402_V1_PAYMENT_HEADER))
            .map(str::to_string);

        let gate_req = GateRequest {
            method: &method,
            path: &path,
            host: host.as_deref(),
            accept: accept.as_deref(),
            authorization: authorization.as_deref(),
            content_length,
            query: query.as_deref(),
            x402_payment: x402_payment.as_deref(),
        };

        let decision = PaymentGate::new(self.state.clone())
            .evaluate(&gate_req)
            .await;
        match decision {
            GateDecision::Respond(r) => {
                write_gate_response(session, r).await?;
                Ok(true)
            }
            GateDecision::Forward { receipt, upto, .. } => {
                ctx.receipt = receipt;
                // x402 `upto`: the channel is open; hold it for post-response
                // settlement (response_filter on success, logging on failure).
                ctx.upto = upto.map(|u| (*u.open, u.settle_amount));
                // A verified payment must reach the real upstream — never the
                // control-plane axum (`control_plane_ok = false`), even for a
                // root (`path: ""`) endpoint.
                self.plan_upstream(
                    ctx,
                    &path,
                    host.as_deref(),
                    &method,
                    &uri,
                    &headers,
                    session,
                    false,
                )
                .await
            }
            GateDecision::Passthrough => {
                // Free / discovery requests may be served by the control plane.
                self.plan_upstream(
                    ctx,
                    &path,
                    host.as_deref(),
                    &method,
                    &uri,
                    &headers,
                    session,
                    true,
                )
                .await
            }
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Ctx,
    ) -> pingora::Result<Box<HttpPeer>> {
        match &ctx.target {
            Some(Target::Api { addr, tls, sni, .. }) => {
                Ok(Box::new(HttpPeer::new(addr.clone(), *tls, sni.clone())))
            }
            Some(Target::ControlPlane) => Ok(Box::new(HttpPeer::new(
                self.control_plane.clone(),
                false,
                "localhost".to_string(),
            ))),
            None => Err(pingora::Error::explain(
                pingora::ErrorType::InternalError,
                "no upstream target planned",
            )),
        }
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Ctx,
    ) -> pingora::Result<()> {
        if let Some(Target::Api {
            host_header,
            path_and_query,
            headers,
            ..
        }) = &ctx.target
        {
            if let Ok(uri) = path_and_query.parse::<Uri>() {
                let _ = upstream_request.set_uri(uri);
            }
            for h in STRIP_HEADERS {
                upstream_request.remove_header(*h);
            }
            let _ = upstream_request.insert_header("host", host_header.clone());
            for (k, v) in headers {
                let _ = upstream_request.insert_header(k.clone(), v.clone());
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> pingora::Result<()> {
        if let Some(receipt) = &ctx.receipt {
            for (name, value) in &receipt.headers {
                // Pass the HeaderValue through unchanged (a receipt can carry
                // non-ASCII bytes that `to_str()` would blank).
                let _ = upstream_response.insert_header(name.clone(), value.clone());
            }
            if let Some(reference) = &receipt.reference {
                tracing::Span::current().record("tx_sig", reference.as_str());
            }
        }
        // x402 `upto`: the upstream responded, so settle the channel now (debit
        // on a 2xx, refund otherwise) and attach the PAYMENT-RESPONSE receipt
        // before the response streams downstream. `take` so `logging` won't
        // double-settle.
        if let Some((open, amt)) = ctx.upto.take() {
            let served_ok = upstream_response.status.is_success();
            if let Some((name, value)) = settle_upto(&self.state, open, amt, served_ok).await {
                let _ = upstream_response.insert_header(name, value);
            }
        }
        Ok(())
    }

    /// Emit the completed exchange to the Payment Debugger. Pingora is the data
    /// plane, so the old axum `logging_middleware` never sees proxied traffic —
    /// this is what keeps PDB populated. Fires for every request (short-circuit
    /// 402s included); control-plane paths were filtered out at capture time.
    async fn logging(&self, session: &mut Session, _e: Option<&pingora::Error>, ctx: &mut Ctx)
    where
        Self::CTX: Send + Sync,
    {
        // An x402 `upto` channel still held here means `response_filter` never
        // ran (the upstream connect/response failed) — refund the full deposit
        // so the client's funds aren't stranded in an open channel.
        if let Some((open, amt)) = ctx.upto.take() {
            let _ = settle_upto(&self.state, open, amt, false).await;
        }
        let Some(log) = ctx.log.take() else {
            return;
        };
        let (status, res_headers) = match session.response_written() {
            Some(resp) => (resp.status.as_u16(), header_pairs(&resp.headers)),
            None => (0, Vec::new()),
        };
        self.state.record_exchange(pay_core::HttpExchange {
            method: log.method,
            path: log.path,
            status,
            ms: log.start.elapsed().as_millis() as u64,
            req_headers: log.req_headers,
            res_headers,
            client_ip: log.client_ip,
        });
    }

    /// Don't log downstream disconnects as proxy errors — a client closing a
    /// long-lived stream (e.g. the PDB `/__402/pdb/logs/stream` SSE on a tab
    /// close/refresh) is benign, not a gateway failure.
    fn suppress_error_log(&self, _session: &Session, _ctx: &Ctx, e: &pingora::Error) -> bool {
        matches!(e.esource(), pingora::ErrorSource::Downstream)
    }
}

/// Collect an `http::HeaderMap` into `(name, value)` string pairs for PDB.
///
/// Uses lossy UTF-8 rather than `to_str()` — the latter rejects any non-visible-
/// ASCII byte, which would silently drop a `WWW-Authenticate` whose challenge
/// description carries a non-ASCII char (e.g. an em-dash). PDB's correlation
/// engine keys on that header, so dropping it means the flow never appears.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

fn is_control_plane(path: &str) -> bool {
    path.is_empty()
        || path.starts_with("__402/")
        || path == "openapi.json"
        || path.starts_with(".well-known/")
}

/// Split a prepared upstream request into a connectable [`Target::Api`].
fn target_from_prepared(prepared: pay_core::server::proxy::PreparedUpstreamRequest) -> Target {
    let url = prepared.url;
    let tls = url.scheme() == "https";
    let host = url.host_str().unwrap_or("").to_string();
    let port = url
        .port_or_known_default()
        .unwrap_or(if tls { 443 } else { 80 });
    let default_port = (tls && port == 443) || (!tls && port == 80);
    let host_header = if default_port {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path_and_query = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    Target::Api {
        addr: format!("{host}:{port}"),
        tls,
        sni: host,
        host_header,
        path_and_query,
        headers: prepared.headers,
    }
}

/// Write a gate-produced [`GateResponse`] (402 challenge, 404, receipt JSON, …)
/// directly to the downstream and stop. Duplicate `WWW-Authenticate` lines are
/// preserved via `append_header`.
async fn write_gate_response(session: &mut Session, r: GateResponse) -> pingora::Result<()> {
    // An unread request body breaks HTTP/1.1 keepalive on a short-circuit.
    let _ = session.drain_request_body().await;
    let mut resp = ResponseHeader::build(r.status.as_u16(), None)?;
    for (name, value) in &r.headers {
        // Pass the `http::HeaderValue` through unchanged. The previous
        // `value.to_str().unwrap_or("")` silently blanked any value `to_str()`
        // rejected — dropping the `WWW-Authenticate` challenge on the floor.
        resp.append_header(name.clone(), value.clone())?;
    }
    resp.insert_header("content-length", r.body.len().to_string())?;
    session.write_response_header(Box::new(resp), false).await?;
    session.write_response_body(Some(r.body), true).await?;
    Ok(())
}

/// Collect an axum [`Response`](axum::response::Response) (respond-mode body or
/// an upstream-prep error) and write it to the downstream, appending `extra`
/// headers (e.g. the verified-payment receipt — respond-mode responses never
/// reach `response_filter`, so the receipt is attached here instead).
async fn write_axum_response(
    session: &mut Session,
    resp: axum::response::Response,
    extra: Vec<(HeaderName, HeaderValue)>,
) -> pingora::Result<()> {
    let _ = session.drain_request_body().await;
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(n, v)| {
            v.to_str()
                .ok()
                .map(|v| (n.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body = match axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "write_axum_response: failed to read response body");
            Bytes::new()
        }
    };
    let mut out = ResponseHeader::build(status, None)?;
    for (n, v) in headers {
        if n.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.append_header(n, v)?;
    }
    // Receipt / settlement headers (HeaderValue passed through unchanged).
    for (n, v) in extra {
        let _ = out.append_header(n, v);
    }
    out.insert_header("content-length", body.len().to_string())?;
    session.write_response_header(Box::new(out), false).await?;
    session.write_response_body(Some(body), true).await?;
    Ok(())
}
