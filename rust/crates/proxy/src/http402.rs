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
//! ## Buffered paths
//! - **Body-signing auth**: auth schemes that digest the body (HMAC /
//!   `AccessToken` `body_digest`) need the complete request before the upstream
//!   connect. Those requests use the bounded buffered path; all other auth keeps
//!   using Pingora's native streaming path.
//! - **Response metering**: delegated MPP SSE/NDJSON responses are rated and
//!   authorized incrementally before each chunk is released. Responses whose
//!   pricing cannot be observed safely in-stream, including x402 `upto`, stay
//!   on the bounded response-metering path.

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use pay_core::PaymentState;
use pay_core::server::gate::{
    BatchForward, GateDecision, GateRequest, GateResponse, MAX_BATCH_CACHED_RESPONSE_BYTES,
    PaidRequestTelemetry, PaymentGate, ReceiptAnnotation, SessionForward, UpstreamRoute,
    UptoPaymentTelemetry, batch_cached_response, cache_batch_response, commit_batch, release_batch,
    resolve_api, settle_batch, settle_delegated_session as settle_delegated_session_forward,
    settle_upto, settle_upto_metered,
};
use pay_core::server::metering::{self, UptoSettlementPlan};
use pay_core::server::payment::{TrustedPaymentIdentity, strip_internal_identity_headers};
use pay_core::server::proxy::{
    ClientVoucherMeter, STRIP_HEADERS, URL_RESPONSE_HEADERS, UpstreamPlan, UrlCredentials,
    collect_upstream_body, prepare_upstream_with_identity, routing_signs_request_body,
    upstream_http_client,
};
use pay_core::server::session_stream::{
    BoxError, DelegatedSessionStreamMeter, SessionStreamContext,
};
use pay_core::server::telemetry;
use pay_kit::x402::server::VerifiedUptoOpen;
use pay_types::metering::ApiSpec;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;

const BUFFERED_REQUEST_BODY_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_RESPONSE_BODY_LIMIT: usize = 1024 * 1024;

/// Where a non-`Respond` request should be proxied.
// One per request, and the API target is the common case: boxing it would
// only add an allocation.
#[allow(clippy::large_enum_variant)]
enum Target {
    /// Real API upstream resolved from the endpoint spec via `prepare_upstream`.
    Api {
        addr: String,
        tls: bool,
        sni: String,
        host_header: String,
        path_and_query: String,
        subdomain: String,
        upstream: String,
        /// Forwarded client headers (minus stripped) + injected auth headers.
        headers: Vec<(String, String)>,
        /// Credentials injected into the upstream query, scrubbed from the
        /// response before it reaches the caller.
        url_credentials: UrlCredentials,
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
    /// responded). Taken when settled, so it's never double-settled.
    upto: Option<PendingUpto>,
    batch: Option<BatchForward>,
    /// A successfully committed streaming response whose body is still being
    /// captured for idempotent replay. Keeping the outcome until `logging`
    /// lets pay-kit attach the complete representation after end-of-stream.
    batch_cache: Option<BatchForward>,
    batch_response: Option<BatchResponseCapture>,
    /// A delegated MPP session opened pre-serve. Observable streams authorize
    /// cumulative usage before each chunk is released; other responses are
    /// buffered and settled before the body is returned.
    session: Option<SessionForward>,
    /// A client-voucher MPP session whose endpoint prices response usage:
    /// the response is released only as the client's vouchers cover it.
    client_meter: Option<ClientVoucherMeter>,
    /// Present only for payment-backed forwards. Consumed by `logging`, where
    /// Pingora exposes the final downstream status for every forwarding path.
    paid_request: Option<PaidRequestTelemetry>,
    /// Captured at `request_filter` for the Payment Debugger exchange emitted
    /// in `logging` (the data plane is Pingora, so the old axum logging
    /// middleware never sees proxied traffic).
    log: Option<LogStart>,
    /// Inference telemetry scanner over the upstream response body. Only
    /// created when the host opted into in-flight tracking
    /// (`record_request_start` returned a log id) — otherwise the body
    /// filter is a no-op.
    observer: Option<crate::observer::StreamObserver>,
    /// Final inference telemetry collected by the buffered x402-upto path.
    /// Buffered response-body metering settles before writing headers, so the
    /// streaming body observer is not used for those requests.
    buffered_usage: Option<pay_core::InferenceUsage>,
    /// Payment receipt headers generated by the gate/proxy and written to the
    /// downstream response. Pingora's recorded response can omit headers added
    /// by filters, so keep these explicitly for PDB logging.
    logged_payment_headers: Vec<(HeaderName, HeaderValue)>,
    /// Normalized request path retained for paid-request and upstream-error
    /// metrics even when debugger exchange logging is disabled.
    request_path: String,
    trusted_identity: Option<TrustedPaymentIdentity>,
}

struct PendingUpto {
    open: VerifiedUptoOpen,
    settle_amount: u64,
    settlement: Option<UptoSettlementPlan>,
    telemetry: UptoPaymentTelemetry,
}

struct BatchResponseCapture {
    cached: Option<pay_kit::core::store::CachedUpstreamResponse>,
    complete: bool,
}

impl BatchResponseCapture {
    fn start(status: StatusCode, headers: &HeaderMap) -> Option<Self> {
        let declared = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());
        if declared.is_some_and(|length| length > MAX_BATCH_CACHED_RESPONSE_BYTES) {
            return None;
        }
        Some(Self {
            cached: Some(batch_cached_response(status, headers, &[])),
            complete: declared == Some(0),
        })
    }

    fn push(&mut self, chunk: &[u8], end_of_stream: bool) {
        if let Some(cached) = &mut self.cached {
            if cached.body.len().saturating_add(chunk.len()) <= MAX_BATCH_CACHED_RESPONSE_BYTES {
                cached.body.extend_from_slice(chunk);
            } else {
                self.cached = None;
            }
        }
        self.complete = end_of_stream;
    }
}

/// Request-side facts captured up front for the PDB exchange.
struct LogStart {
    method: String,
    path: String,
    req_headers: Vec<(String, String)>,
    client_ip: String,
    start: std::time::Instant,
    /// In-flight tracking id from `PaymentState::record_request_start`.
    log_id: Option<u64>,
}

/// Pingora payment gate over [`PaymentGate`], parameterized over the host's
/// [`PaymentState`].
pub struct Http402Gate<S: PaymentState> {
    state: S,
    /// `host:port` of the internal axum service handling the control plane.
    control_plane: String,
    deployment_policy_resolver:
        Option<std::sync::Arc<pay_core::server::deployment_policy::DeploymentPolicyResolver>>,
}

impl<S: PaymentState> Http402Gate<S> {
    pub fn new(state: S, control_plane: impl Into<String>) -> Self {
        Self {
            state,
            control_plane: control_plane.into(),
            deployment_policy_resolver: None,
        }
    }

    pub fn with_deployment_policy_resolver(
        mut self,
        resolver: Option<
            std::sync::Arc<pay_core::server::deployment_policy::DeploymentPolicyResolver>,
        >,
    ) -> Self {
        self.deployment_policy_resolver = resolver;
        self
    }

    /// Resolve the API spec for a host — subdomain match, single-API fallback —
    /// the same resolution [`PaymentGate::evaluate`] uses.
    fn resolve_api<'a>(&'a self, host: &str) -> Option<&'a ApiSpec> {
        resolve_api(self.state.apis(), Some(host))
    }

    /// Prepare a request whose upstream authentication depends on its body.
    /// Kept on the gate so the buffered route has a focused regression seam:
    /// tests can prove the exact bytes read from downstream reach auth prep.
    // The response-shaped error is consumed immediately by `forward_buffered`;
    // boxing it would add allocation and obscure the existing response path.
    #[allow(clippy::result_large_err)]
    #[allow(clippy::too_many_arguments)]
    async fn prepare_buffered_request(
        &self,
        api: &ApiSpec,
        route: &UpstreamRoute,
        method: &http::Method,
        uri: &Uri,
        headers: &http::HeaderMap,
        body: &[u8],
        trusted_identity: Option<&TrustedPaymentIdentity>,
    ) -> Result<UpstreamPlan, axum::response::Response> {
        prepare_upstream_with_identity(api, route, method, uri, headers, body, trusted_identity)
            .await
    }

    /// Plan the upstream for a gate decision: control-plane → the internal axum
    /// service; otherwise resolve + `prepare_upstream` under the gate's
    /// `route`, or 404 when the gate admitted none. Returns
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
        route: Option<&UpstreamRoute>,
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
        let (Some(route), Some(api)) = (route, self.resolve_api(host.unwrap_or(""))) else {
            let _ = session
                .respond_error_with_body(404, Bytes::from_static(b"{\"error\":\"not_found\"}"))
                .await;
            return Ok(true);
        };
        // Body-digest auth needs the complete request before auth preparation.
        // Keep the common case on Pingora's native streaming path and route only
        // these endpoints through the bounded reqwest adapter.
        if routing_signs_request_body(api, route) {
            return self
                .forward_buffered(session, ctx, path, route, host, method, uri, headers)
                .await;
        }
        // No body-signing auth: an empty placeholder body is safe for prep.
        match prepare_upstream_with_identity(
            api,
            route,
            method,
            uri,
            headers,
            &[],
            ctx.trusted_identity.as_ref(),
        )
        .await
        {
            Ok(UpstreamPlan::Forward(prepared)) => {
                ctx.target = Some(target_from_prepared(prepared, api.subdomain.clone()));
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
        let status = resp.status();
        match self
            .drain_payment_headers_with_response(
                ctx,
                status.is_success(),
                Some(status),
                &HeaderMap::new(),
                None,
            )
            .await
        {
            Ok(extra) => write_axum_response(session, resp, extra).await,
            Err(extra) => write_axum_response(session, batch_commit_failed_response(), extra).await,
        }
    }

    /// Settle/refund pending payment side-effects and collect the headers that
    /// must ride the response: refund an open `upto` channel and surface the
    /// x402 `exact` `PAYMENT-RESPONSE` receipt. Used by every terminal path that
    /// `response_filter` doesn't reach (respond-mode and connect/proxy failures)
    /// so a settled payment always returns
    /// its proof. `served_ok` decides debit vs full refund for `upto`.
    async fn drain_payment_headers(
        &self,
        ctx: &mut Ctx,
        served_ok: bool,
    ) -> Vec<(HeaderName, HeaderValue)> {
        match self
            .drain_payment_headers_with_response(ctx, served_ok, None, &HeaderMap::new(), None)
            .await
        {
            Ok(extra) | Err(extra) => extra,
        }
    }

    /// `Err` means a served payment (batch commit or upto charge) did not
    /// settle: the caller must replace the successful response with an error. The returned headers
    /// (e.g. an x402 `exact` receipt) still ride that error response.
    async fn drain_payment_headers_with_response(
        &self,
        ctx: &mut Ctx,
        served_ok: bool,
        response_status: Option<StatusCode>,
        response_headers: &HeaderMap,
        response_body: Option<&[u8]>,
    ) -> Result<Vec<(HeaderName, HeaderValue)>, Vec<(HeaderName, HeaderValue)>> {
        let mut extra: Vec<(HeaderName, HeaderValue)> = Vec::new();
        let mut payment_committed = true;
        if !served_ok {
            // Dropping the forwarded session drops its capacity lease. Keep
            // this in the shared terminal-path drain so request buffering,
            // upstream setup/send, response buffering, and proxy failures all
            // release immediately.
            ctx.session.take();
        }
        if let Some(pending) = ctx.upto.take() {
            match self
                .settle_pending_upto(pending, served_ok, response_headers, response_body)
                .await
            {
                Ok(Some(header)) => extra.push(header),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "x402 upto settlement failed; withholding response");
                    payment_committed = false;
                }
            }
        }
        if let Some(pending) = ctx.batch.take() {
            match settle_batch(
                &self.state,
                pending,
                response_status,
                response_status
                    .zip(response_body)
                    .filter(|(_, body)| body.len() <= MAX_BATCH_CACHED_RESPONSE_BYTES)
                    .map(|(status, body)| batch_cached_response(status, response_headers, body)),
            )
            .await
            {
                Ok(Some(header)) => extra.push(header),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "x402 batch commit failed; withholding response");
                    payment_committed = false;
                }
            }
        }
        if let Some(receipt) = ctx.receipt.take() {
            extra.extend(receipt.headers);
            if let Some(reference) = &receipt.reference {
                tracing::Span::current().record("tx_sig", reference.as_str());
            }
        }
        ctx.logged_payment_headers.extend(extra.iter().cloned());
        if payment_committed {
            Ok(extra)
        } else {
            Err(extra)
        }
    }

    async fn settle_pending_upto(
        &self,
        pending: PendingUpto,
        served_ok: bool,
        response_headers: &HeaderMap,
        response_body: Option<&[u8]>,
    ) -> Result<Option<(HeaderName, HeaderValue)>, pay_core::Error> {
        match pending.settlement {
            Some(plan) => {
                settle_upto_metered(
                    &self.state,
                    pending.open,
                    plan,
                    served_ok,
                    response_headers,
                    response_body,
                    pending.telemetry,
                )
                .await
            }
            None => {
                settle_upto(
                    &self.state,
                    pending.open,
                    pending.settle_amount,
                    served_ok,
                    pending.telemetry,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_buffered(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        path: &str,
        route: &UpstreamRoute,
        host: Option<&str>,
        method: &http::Method,
        uri: &Uri,
        headers: &http::HeaderMap,
    ) -> pingora::Result<bool> {
        let deadline = ctx.session.as_ref().and_then(|forward| forward.deadline());
        let forwarding =
            self.forward_buffered_inner(session, ctx, path, route, host, method, uri, headers);
        match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, forwarding).await {
                Ok(result) => result,
                Err(_) => {
                    ctx.session.take();
                    Err(pingora::Error::explain(
                        pingora::ErrorType::HTTPStatus(504),
                        "deployment request lifetime exceeded",
                    ))
                }
            },
            None => forwarding.await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_buffered_inner(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        path: &str,
        route: &UpstreamRoute,
        host: Option<&str>,
        method: &http::Method,
        uri: &Uri,
        headers: &http::HeaderMap,
    ) -> pingora::Result<bool> {
        let Some(api) = self.resolve_api(host.unwrap_or("")) else {
            let extra = self.drain_payment_headers(ctx, false).await;
            write_buffered_response(
                session,
                StatusCode::NOT_FOUND,
                HeaderMap::new(),
                Bytes::from_static(b"{\"error\":\"not_found\"}"),
                extra,
            )
            .await?;
            return Ok(true);
        };

        let body = match read_downstream_body(session, BUFFERED_REQUEST_BODY_LIMIT).await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "failed to buffer request body");
                let extra = self.drain_payment_headers(ctx, false).await;
                write_buffered_response(
                    session,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    HeaderMap::new(),
                    Bytes::from_static(b"{\"error\":\"request_body_too_large\"}"),
                    extra,
                )
                .await?;
                return Ok(true);
            }
        };

        // Price the forward from the exact body it sends, as the axum adapter
        // does: the body's model selects the variant the meter rates.
        let mut forwarded_headers = headers.clone();
        let body = match pay_core::server::payment::prepare_metered_forward_body(
            path,
            headers,
            &body,
            ctx.session.as_mut(),
            ctx.upto
                .as_mut()
                .and_then(|pending| pending.settlement.as_mut()),
        ) {
            Ok(Some(prepared)) if prepared[..] != body[..] => {
                forwarded_headers.insert(http::header::CONTENT_LENGTH, prepared.len().into());
                Bytes::from(prepared)
            }
            Ok(_) => body,
            Err(rejection) => {
                self.finish_buffered_axum_response(session, ctx, rejection.into_response())
                    .await?;
                return Ok(true);
            }
        };
        let headers = &forwarded_headers;

        let prepared = match self
            .prepare_buffered_request(
                api,
                route,
                method,
                uri,
                headers,
                body.as_ref(),
                ctx.trusted_identity.as_ref(),
            )
            .await
        {
            Ok(UpstreamPlan::Respond(resp)) => {
                self.finish_buffered_axum_response(session, ctx, resp)
                    .await?;
                return Ok(true);
            }
            Ok(UpstreamPlan::Forward(prepared)) => prepared,
            Err(resp) => {
                self.finish_buffered_axum_response(session, ctx, resp)
                    .await?;
                return Ok(true);
            }
        };

        let response_limit = ctx
            .upto
            .as_ref()
            .and_then(|pending| pending.settlement.as_ref())
            .or_else(|| {
                ctx.session
                    .as_ref()
                    .and_then(|pending| pending.metered_plan())
            })
            .map(|plan| metering::upto_response_body_limit(&plan.metering))
            // Body-signing endpoints are buffered for request preparation, not
            // response metering. Allow their normal API responses up to the same
            // bounded size as their requests.
            .unwrap_or(BUFFERED_REQUEST_BODY_LIMIT);

        let client = upstream_http_client();
        let mut upstream_req = client.request(
            reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
            prepared.url.clone(),
        );
        for (name, value) in buffered_upstream_headers(&prepared.headers) {
            upstream_req = upstream_req.header(name.as_str(), value);
        }
        if !body.is_empty() {
            upstream_req = upstream_req.body(body.to_vec());
        } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            upstream_req = upstream_req.header("content-length", "0");
        }

        if let Some(forward) = &ctx.session {
            forward.require_active().await.map_err(|error| {
                pingora::Error::explain(pingora::ErrorType::HTTPStatus(503), error)
            })?;
        }
        let upstream = match upstream_req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                let error = prepared.error_for_logging(&e);
                let upstream = prepared.url_for_logging();
                telemetry::record_upstream_error(&api.subdomain, uri.path(), &upstream, &error);
                tracing::error!(%error, %upstream, "buffered upstream request failed");
                let extra = self.drain_payment_headers(ctx, false).await;
                write_buffered_response(
                    session,
                    StatusCode::BAD_GATEWAY,
                    HeaderMap::new(),
                    Bytes::from_static(b"{\"error\":\"upstream_unavailable\"}"),
                    extra,
                )
                .await?;
                return Ok(true);
            }
        };

        let status = StatusCode::from_u16(upstream.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status.is_server_error() {
            telemetry::record_upstream_error(
                &api.subdomain,
                uri.path(),
                &prepared.url_for_logging(),
                &format!("upstream returned {status}"),
            );
        }
        let mut response_headers = filtered_response_headers(upstream.headers());
        prepared
            .url_credentials
            .scrub_response_headers(&mut response_headers);
        // A client-voucher session pays as it reads: its whole response goes
        // through the meter that waits on the client's commits.
        let mut client_meter = ctx
            .client_meter
            .take()
            .map(|meter| meter.with_request_body_bytes(body.len() as u64));
        let client_voucher = client_meter.is_some();
        if let Some(meter) = client_meter.take_if(|_| !is_streamed_response(&response_headers)) {
            return self
                .serve_client_voucher_body(session, ctx, status, response_headers, upstream, meter)
                .await;
        }
        if is_streamed_response(&response_headers)
            && ctx
                .session
                .as_ref()
                .and_then(|forward| forward.deadline())
                .is_some()
        {
            ctx.session.take();
            return Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(502),
                "streaming is unsupported for fixed deployment charging",
            ));
        }
        let delegated_stream = ctx
            .session
            .as_ref()
            .is_some_and(DelegatedSessionStreamMeter::supports);
        if client_voucher
            || (is_streamed_response(&response_headers)
                && ctx.upto.is_none()
                && (ctx.session.is_none() || delegated_stream)
                && ctx.batch.is_none())
        {
            let extra = self
                .drain_payment_headers_with_response(
                    ctx,
                    status.is_success(),
                    Some(status),
                    &response_headers,
                    None,
                )
                .await
                .map_err(|_| batch_commit_failed_error())?;
            self.stream_buffered_upstream(
                session,
                ctx,
                status,
                response_headers,
                upstream,
                extra,
                body.len() as u64,
                client_meter,
            )
            .await?;
            tracing::debug!(
                path,
                "served streaming response via body-signing proxy path"
            );
            return Ok(true);
        }
        let body = match collect_upstream_body(upstream, response_limit).await {
            Ok(body) => body,
            Err(e) => {
                let error = prepared.redact_for_logging(&e.to_string());
                telemetry::record_upstream_error(
                    &api.subdomain,
                    uri.path(),
                    &prepared.url_for_logging(),
                    &error,
                );
                tracing::warn!(%error, "failed to buffer upstream response body");
                let extra = self.drain_payment_headers(ctx, false).await;
                write_buffered_response(
                    session,
                    StatusCode::BAD_GATEWAY,
                    HeaderMap::new(),
                    Bytes::from_static(b"{\"error\":\"response_metering_failed\"}"),
                    extra,
                )
                .await?;
                return Ok(true);
            }
        };

        self.observe_buffered_metered_response(ctx, &response_headers, &body);
        if pay_core::server::gate::handler_completed(status) {
            match self
                .settle_delegated_session(ctx, &response_headers, &body)
                .await
            {
                Ok(Some(receipt)) => ctx.receipt = Some(receipt),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "failed to settle delegated MPP session usage");
                    let extra = self.drain_payment_headers(ctx, false).await;
                    write_buffered_response(
                        session,
                        StatusCode::BAD_GATEWAY,
                        HeaderMap::new(),
                        Bytes::from_static(b"{\"error\":\"session_settlement_failed\"}"),
                        extra,
                    )
                    .await?;
                    return Ok(true);
                }
            }
        }
        match self
            .drain_payment_headers_with_response(
                ctx,
                status.is_success(),
                Some(status),
                &response_headers,
                Some(&body),
            )
            .await
        {
            Ok(extra) => {
                write_buffered_response(session, status, response_headers, body, extra).await?
            }
            Err(extra) => write_batch_commit_failed(session, extra).await?,
        }
        tracing::debug!(path, "served via buffered proxy path");
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_buffered_upstream(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        status: StatusCode,
        headers: HeaderMap,
        upstream: reqwest::Response,
        extra: Vec<(HeaderName, HeaderValue)>,
        request_body_bytes: u64,
        client_meter: Option<ClientVoucherMeter>,
    ) -> pingora::Result<()> {
        let is_sse = is_sse_response(&headers);
        let mut delegated_meter = match ctx.session.take() {
            Some(forward) => Some(
                DelegatedSessionStreamMeter::from_forward(forward)
                    .map_err(|error| {
                        pingora::Error::because(
                            pingora::ErrorType::InternalError,
                            "configure delegated response stream metering",
                            error,
                        )
                    })?
                    .with_request_body_bytes(request_body_bytes),
            ),
            None => None,
        };
        let mut out = ResponseHeader::build(status.as_u16(), None)?;
        for (name, value) in headers {
            let Some(name) = name else {
                continue;
            };
            out.append_header(name, value)?;
        }
        for (name, value) in extra {
            let _ = out.append_header(name, value);
        }
        session.write_response_header(Box::new(out), false).await?;

        let request_start = ctx
            .log
            .as_ref()
            .map(|log| log.start)
            .unwrap_or_else(std::time::Instant::now);
        let mut observer = crate::observer::StreamObserver::new(true);
        let upstream = upstream.bytes_stream();
        let mut stream: std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<Bytes, BoxError>> + Send>,
        > = match client_meter {
            Some(meter) => Box::pin(meter.meter_stream(upstream, is_sse)),
            None => Box::pin(upstream.map(|chunk| chunk.map_err(BoxError::from))),
        };
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    observer.on_chunk(&chunk, request_start);
                    if let Some(meter) = delegated_meter.as_mut() {
                        meter
                            .authorize_chunk(&chunk, is_sse)
                            .await
                            .map_err(|error| {
                                pingora::Error::because(
                                    pingora::ErrorType::ReadError,
                                    "authorize delegated response stream chunk",
                                    error,
                                )
                            })?;
                    }
                    session.write_response_body(Some(chunk), false).await?;
                }
                Err(error) => {
                    tracing::warn!(%error, "streaming body-signing upstream response failed");
                    observer.finish();
                    if let Some(log_id) = ctx.log.as_ref().and_then(|log| log.log_id) {
                        self.state.record_exchange_update(log_id, &observer.usage);
                    }
                    ctx.buffered_usage = Some(observer.usage);
                    // Headers and possibly body bytes are already downstream,
                    // so a replacement HTTP error is impossible. Return a
                    // read error without writing the final body marker; Pingora
                    // will terminate the downstream stream instead of making a
                    // truncated response look like a clean EOF.
                    return Err(pingora::Error::because(
                        pingora::ErrorType::ReadError,
                        "body-signing upstream stream failed",
                        error,
                    ));
                }
            }
        }
        if let Some(meter) = delegated_meter.as_mut() {
            meter.finish_stream(is_sse).await.map_err(|error| {
                pingora::Error::because(
                    pingora::ErrorType::ReadError,
                    "finish delegated response stream metering",
                    error,
                )
            })?;
        }
        observer.finish();
        if let Some(log_id) = ctx.log.as_ref().and_then(|log| log.log_id) {
            self.state.record_exchange_update(log_id, &observer.usage);
        }
        ctx.buffered_usage = Some(observer.usage);
        session.write_response_body(None, true).await?;
        Ok(())
    }

    /// Serve a complete client-voucher session response only once the
    /// client's vouchers cover its usage.
    async fn serve_client_voucher_body(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        status: StatusCode,
        headers: HeaderMap,
        upstream: reqwest::Response,
        meter: ClientVoucherMeter,
    ) -> pingora::Result<bool> {
        let body = match collect_upstream_body(upstream, meter.body_limit()).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%error, "failed to buffer session-metered response");
                let extra = self.drain_payment_headers(ctx, false).await;
                write_buffered_response(
                    session,
                    StatusCode::BAD_GATEWAY,
                    HeaderMap::new(),
                    Bytes::from_static(b"{\"error\":\"response_metering_failed\"}"),
                    extra,
                )
                .await?;
                return Ok(true);
            }
        };
        if let Err(error) = meter.settle_body(&body).await {
            tracing::warn!(%error, "session-metered response usage not covered");
            self.finish_buffered_axum_response(
                session,
                ctx,
                pay_core::server::proxy::session_voucher_required_response(),
            )
            .await?;
            return Ok(true);
        }
        let extra = self.drain_payment_headers(ctx, status.is_success()).await;
        write_buffered_response(session, status, headers, body, extra).await?;
        Ok(true)
    }

    fn observe_buffered_metered_response(&self, ctx: &mut Ctx, headers: &HeaderMap, body: &[u8]) {
        let Some(plan) = ctx
            .upto
            .as_mut()
            .and_then(|pending| pending.settlement.as_mut())
            .or_else(|| {
                ctx.session
                    .as_mut()
                    .and_then(|pending| pending.metered_plan_mut())
            })
        else {
            return;
        };
        let request_start = ctx
            .log
            .as_ref()
            .map(|log| log.start)
            .unwrap_or_else(std::time::Instant::now);
        let mut observer = crate::observer::StreamObserver::new(is_streamed_response(headers));
        if !body.is_empty() {
            observer.on_chunk(body, request_start);
        }
        observer.finish();
        let usage = observer.usage;
        plan.inferred_usage = Some(usage.clone());
        if let Some(log_id) = ctx.log.as_ref().and_then(|log| log.log_id) {
            self.state.record_exchange_update(log_id, &usage);
        }
        ctx.buffered_usage = Some(usage);
    }

    async fn settle_delegated_session(
        &self,
        ctx: &mut Ctx,
        response_headers: &HeaderMap,
        response_body: &[u8],
    ) -> Result<Option<ReceiptAnnotation>, String> {
        let Some(pending) = ctx.session.take() else {
            return Ok(None);
        };
        settle_delegated_session_forward(pending, response_headers, Some(response_body)).await
    }

    async fn finish_buffered_axum_response(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
        resp: axum::response::Response,
    ) -> pingora::Result<()> {
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = match axum::body::to_bytes(resp.into_body(), DEFAULT_RESPONSE_BODY_LIMIT).await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "failed to buffer inline response for x402 upto");
                let extra = self.drain_payment_headers(ctx, false).await;
                return write_buffered_response(
                    session,
                    StatusCode::BAD_GATEWAY,
                    HeaderMap::new(),
                    Bytes::from_static(b"{\"error\":\"upstream_body_read_failed\"}"),
                    extra,
                )
                .await;
            }
        };
        if pay_core::server::gate::handler_completed(status) {
            match self.settle_delegated_session(ctx, &headers, &body).await {
                Ok(Some(receipt)) => ctx.receipt = Some(receipt),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(%error, "failed to settle delegated MPP session usage");
                    let extra = self.drain_payment_headers(ctx, false).await;
                    return write_buffered_response(
                        session,
                        StatusCode::BAD_GATEWAY,
                        HeaderMap::new(),
                        Bytes::from_static(b"{\"error\":\"session_settlement_failed\"}"),
                        extra,
                    )
                    .await;
                }
            }
        }
        match self
            .drain_payment_headers_with_response(
                ctx,
                status.is_success(),
                Some(status),
                &headers,
                Some(&body),
            )
            .await
        {
            Ok(extra) => write_buffered_response(session, status, headers, body, extra).await,
            Err(extra) => write_batch_commit_failed(session, extra).await,
        }
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
            batch: None,
            batch_cache: None,
            batch_response: None,
            session: None,
            client_meter: None,
            paid_request: None,
            log: None,
            observer: None,
            buffered_usage: None,
            logged_payment_headers: Vec::new(),
            request_path: String::new(),
            trusted_identity: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> pingora::Result<bool> {
        let rh = session.req_header();
        let method = rh.method.clone();
        let uri = rh.uri.clone();
        let mut headers = rh.headers.clone();

        let path = match pay_core::server::gate::gate_path(uri.path()) {
            Ok(path) => path.to_string(),
            Err(response) => {
                write_gate_response(session, response).await?;
                return Ok(true);
            }
        };
        ctx.request_path = format!("/{path}");
        // Cloud Run's serverless NEG rewrites Host to run.app. In deployment
        // mode require the original Host inserted by the CPU load balancer,
        // which overwrites any client-supplied header of the same name.
        let host = if self.deployment_policy_resolver.is_some() {
            gateway_host(&headers, &uri)
        } else {
            request_host(&headers, &uri)
        };
        // Discard internal headers before payment evaluation and observability
        // capture. Only authenticated identity may be restored upstream.
        strip_internal_identity_headers(&mut headers);
        let str_h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());

        // Capture request-side facts for the PDB exchange emitted in `logging`.
        // Skip the control plane's own paths (`/__402/*`, `/openapi.json`,
        // `/.well-known/*`, `/`) so the debugger only shows real traffic —
        // e.g. the ephemeral `/.well-known/pay-skills.json` that the MCP
        // catalog tools poll must not show up as a failed OLLAMA request.
        if !is_control_plane(&path) && self.state.records_http_exchanges() {
            let client_ip = str_h("x-forwarded-for")
                .and_then(|v| v.split(',').next())
                .map(|s| s.trim().to_string())
                .or_else(|| host.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let mpp_payment = str_h("authorization")
                .map(|a| a.len() >= 8 && a[..8].eq_ignore_ascii_case("payment "))
                .unwrap_or(false);
            let x402_payment = str_h(pay_kit::x402::PAYMENT_SIGNATURE_HEADER).is_some()
                || str_h(pay_kit::x402::X402_V1_PAYMENT_HEADER).is_some();
            let log_id = self.state.record_request_start(&pay_core::RequestStart {
                method: method.to_string(),
                path: format!("/{path}"),
                host: host.clone(),
                client_ip: client_ip.clone(),
                payment: mpp_payment || x402_payment,
            });
            ctx.log = Some(LogStart {
                method: method.to_string(),
                path: format!("/{path}"),
                req_headers: header_pairs(&headers),
                client_ip,
                start: std::time::Instant::now(),
                log_id,
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
            .with_deployment_policy_resolver(self.deployment_policy_resolver.clone())
            .evaluate(&gate_req)
            .await;
        match decision {
            GateDecision::Respond(r) => {
                write_gate_response(session, r).await?;
                Ok(true)
            }
            GateDecision::Forward {
                route,
                session: session_forward,
                receipt,
                upto,
                batch,
                paid_request,
            } => {
                ctx.trusted_identity = Some(TrustedPaymentIdentity {
                    payer: session_forward
                        .as_ref()
                        .and_then(|pending| pending.verified_payer.clone()),
                    channel_id: session_forward
                        .as_ref()
                        .map(|pending| pending.channel_id.clone()),
                    original_host: host.clone(),
                    payment_policy: session_forward
                        .as_ref()
                        .and_then(|pending| pending.payment_policy.clone()),
                });
                ctx.receipt = receipt;
                ctx.paid_request = paid_request;
                // x402 `batch-settlement`: the voucher is verified and the
                // channel reserved, but nothing is charged until the upstream
                // has actually served.
                ctx.batch = batch.map(|b| *b);
                // x402 `upto`: the channel is open; hold it for post-response
                // settlement (response_filter on success, logging on failure).
                ctx.upto = upto.map(|u| {
                    let u = *u;
                    PendingUpto {
                        open: *u.open,
                        settle_amount: u.settle_amount,
                        settlement: u.settlement,
                        telemetry: u.telemetry,
                    }
                });
                ctx.session = session_forward.map(|pending| *pending);
                // Delegated sessions always settle before releasing a
                // successful response, including fixed-price endpoints, and a
                // client-voucher session's response is withheld until the
                // client's vouchers cover its usage; both need the metered
                // adapter. x402 `upto` only needs this path when pricing
                // consumes response usage; its fixed-price path can settle in
                // response_filter.
                let delegated_session = ctx
                    .session
                    .as_ref()
                    .is_some_and(|pending| pending.settlement.is_some());
                if let Some(forward) = ctx.session.take_if(|pending| pending.settlement.is_none()) {
                    let Some(api) = self.resolve_api(host.as_deref().unwrap_or("")) else {
                        return self
                            .plan_upstream(
                                ctx,
                                &path,
                                host.as_deref(),
                                &method,
                                &uri,
                                &headers,
                                session,
                                Some(&route),
                                false,
                            )
                            .await;
                    };
                    let context = SessionStreamContext::new(
                        std::sync::Arc::clone(&forward.handle),
                        forward.channel_id.clone(),
                        forward.committed_base_units,
                    );
                    match ClientVoucherMeter::for_route(api, &route, &uri, &headers, context) {
                        Ok(meter) => ctx.client_meter = meter,
                        Err(error) => {
                            tracing::error!(%error, path, "session response metering unavailable");
                            write_buffered_response(
                                session,
                                StatusCode::BAD_GATEWAY,
                                HeaderMap::new(),
                                Bytes::from_static(b"{\"error\":\"session_metering_failed\"}"),
                                Vec::new(),
                            )
                            .await?;
                            return Ok(true);
                        }
                    }
                    // Request-only pricing: the admitting voucher covers it.
                    if ctx.client_meter.is_none() {
                        ctx.session = Some(forward);
                    }
                }
                if settles_on_completed_body(
                    delegated_session,
                    ctx.client_meter.is_some(),
                    ctx.upto.is_some(),
                ) {
                    return self
                        .forward_buffered(
                            session,
                            ctx,
                            &path,
                            &route,
                            host.as_deref(),
                            &method,
                            &uri,
                            &headers,
                        )
                        .await;
                }
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
                    Some(&route),
                    false,
                )
                .await
            }
            GateDecision::Passthrough(route) => {
                // Free requests may be served by the control plane.
                self.plan_upstream(
                    ctx,
                    &path,
                    host.as_deref(),
                    &method,
                    &uri,
                    &headers,
                    session,
                    Some(&route),
                    true,
                )
                .await
            }
            GateDecision::Local => {
                // Control plane only; never the API upstream.
                self.plan_upstream(
                    ctx,
                    &path,
                    host.as_deref(),
                    &method,
                    &uri,
                    &headers,
                    session,
                    None,
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
        if let Some(Target::Api {
            url_credentials, ..
        }) = &ctx.target
        {
            scrub_url_credentials(url_credentials, upstream_response);
        }
        if upstream_response.status.is_server_error()
            && let Some(Target::Api {
                subdomain,
                upstream,
                ..
            }) = &ctx.target
        {
            telemetry::record_upstream_error(
                subdomain,
                &ctx.request_path,
                upstream,
                &format!("upstream returned {}", upstream_response.status),
            );
        }
        if let Some(receipt) = &ctx.receipt {
            for (name, value) in &receipt.headers {
                // Pass the HeaderValue through unchanged (a receipt can carry
                // non-ASCII bytes that `to_str()` would blank).
                let _ = upstream_response.insert_header(name.clone(), value.clone());
                ctx.logged_payment_headers
                    .push((name.clone(), value.clone()));
            }
            if let Some(reference) = &receipt.reference {
                tracing::Span::current().record("tx_sig", reference.as_str());
            }
        }
        // Start the inference telemetry observer for hosts that opted into
        // in-flight tracking. x402-upto requests never reach this path: they
        // are buffered so they settle on a completed body, before downstream
        // headers carry the PAYMENT-RESPONSE receipt.
        let observe_inference = ctx.log.as_ref().is_some_and(|log| log.log_id.is_some());
        if observe_inference {
            ctx.observer = Some(crate::observer::StreamObserver::new(is_streamed_response(
                &upstream_response.headers,
            )));
        }

        // x402 `upto` settles only once its body completed, on the buffered
        // path. Charging here, at headers, would keep the payment of a
        // response that then fails mid-body: refund and withhold instead.
        if let Some(pending) = ctx.upto.take() {
            tracing::error!("x402 upto reached the native response path; refunding");
            let _ = self
                .settle_pending_upto(pending, false, &upstream_response.headers, None)
                .await;
            return Err(batch_commit_failed_error());
        }
        // Batch authorization is also post-response. Unlike `upto`, its hot
        // path commits only Redis state; on-chain claim/distribution belongs
        // to pay-worker. This must happen here (before headers are sent) so a
        // successful proxied response cannot drop its reservation uncharged.
        if let Some(pending) = ctx.batch.take() {
            if pay_core::server::gate::handler_completed(upstream_response.status) {
                match commit_batch(&self.state, &pending).await {
                    Ok(Some((name, value))) => {
                        let _ = upstream_response.insert_header(name.clone(), value.clone());
                        ctx.logged_payment_headers.push((name, value));
                    }
                    Ok(None) => {}
                    // Headers are not yet sent: fail the request instead of
                    // releasing an unpaid response. The handler-succeeded
                    // authorization is retained for a resumed commit.
                    Err(error) => {
                        tracing::error!(%error, "x402 batch commit failed; withholding response");
                        return Err(batch_commit_failed_error());
                    }
                }
                if let Some(capture) = BatchResponseCapture::start(
                    upstream_response.status,
                    &upstream_response.headers,
                ) {
                    ctx.batch_cache = Some(pending);
                    ctx.batch_response = Some(capture);
                }
            } else {
                release_batch(&self.state, pending).await;
            }
        }
        Ok(())
    }

    /// Scan upstream body chunks for inference telemetry (model, token usage,
    /// TTFT) without buffering the stream. Emits throttled live updates while
    /// the response runs. No-op unless `response_filter` armed the observer.
    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> pingora::Result<Option<std::time::Duration>> {
        if let Some(capture) = ctx.batch_response.as_mut() {
            capture.push(body.as_deref().unwrap_or_default(), end_of_stream);
        }
        let Some(observer) = ctx.observer.as_mut() else {
            return Ok(None);
        };
        let Some(log) = ctx.log.as_ref() else {
            return Ok(None);
        };
        let Some(log_id) = log.log_id else {
            return Ok(None);
        };

        if let Some(chunk) = body.as_ref()
            && !chunk.is_empty()
        {
            observer.on_chunk(chunk, log.start);
        }
        if end_of_stream {
            observer.finish();
            self.state.record_exchange_update(log_id, &observer.usage);
        } else if observer.should_emit() {
            self.state.record_exchange_update(log_id, &observer.usage);
        }
        Ok(None)
    }

    /// Emit the completed exchange to the Payment Debugger. Pingora is the data
    /// plane, so the old axum `logging_middleware` never sees proxied traffic —
    /// this is what keeps PDB populated. Fires for every request (short-circuit
    /// 402s included); control-plane paths were filtered out at capture time.
    async fn logging(&self, session: &mut Session, error: Option<&pingora::Error>, ctx: &mut Ctx)
    where
        Self::CTX: Send + Sync,
    {
        // An x402 `upto` channel still held here means a terminal hook did not
        // drain it (for example, response_filter never ran because upstream
        // connect/response failed). Refund rather than stranding funds. A
        // successful channel is settled on the buffered path before headers
        // are written, so its receipt reaches the client as a normal
        // PAYMENT-RESPONSE header.
        let mut deferred_payment_headers = Vec::new();
        if let Some(forward) = ctx.batch_cache.take()
            && let Some(capture) = ctx.batch_response.take()
            && capture.complete
            && let Some(cached) = capture.cached
        {
            cache_batch_response(&self.state, &forward, cached).await;
        }
        // Covers cancellation/disconnect paths that bypassed all explicit
        // drains. Dropping the session releases its capacity lease.
        ctx.session.take();
        if let Some(pending) = ctx.upto.take()
            && let Ok(Some(header)) = self
                .settle_pending_upto(pending, false, &HeaderMap::new(), None)
                .await
        {
            deferred_payment_headers.push(header);
        }
        if let Some(paid_request) = ctx.paid_request.take() {
            let status = session
                .response_written()
                .and_then(|response| StatusCode::from_u16(response.status.as_u16()).ok())
                .unwrap_or_else(|| {
                    if error.is_some() {
                        StatusCode::BAD_GATEWAY
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                });
            telemetry::record_paid_request_completed(
                paid_request.protocol,
                &paid_request.subdomain,
                &ctx.request_path,
                status,
                paid_request.payment.as_ref(),
            );
        }
        let Some(log) = ctx.log.take() else {
            return;
        };
        let (status, mut res_headers) = match session.response_written() {
            Some(resp) => (resp.status.as_u16(), header_pairs(&resp.headers)),
            None => (0, Vec::new()),
        };
        let logged_payment_headers = std::mem::take(&mut ctx.logged_payment_headers);
        res_headers.extend(header_pairs_from_owned(deferred_payment_headers));
        res_headers.extend(header_pairs_from_owned(logged_payment_headers));
        // Flush the observer if the body filter never saw end_of_stream
        // (upstream error / client disconnect) so partial telemetry survives.
        let usage = ctx.buffered_usage.take().or_else(|| {
            ctx.observer.take().map(|mut observer| {
                observer.finish();
                observer.usage
            })
        });
        self.state.record_exchange(pay_core::HttpExchange {
            method: log.method,
            path: log.path,
            status,
            ms: log.start.elapsed().as_millis() as u64,
            req_headers: log.req_headers,
            res_headers,
            client_ip: log.client_ip,
            log_id: log.log_id,
            usage,
        });
    }

    /// Don't log downstream disconnects as proxy errors — a client closing a
    /// long-lived stream (e.g. the PDB `/__402/pdb/logs/stream` SSE on a tab
    /// close/refresh) is benign, not a gateway failure.
    fn suppress_error_log(&self, _session: &Session, _ctx: &Ctx, e: &pingora::Error) -> bool {
        matches!(e.esource(), pingora::ErrorSource::Downstream)
    }

    /// Write the downstream error response when proxying fails. `response_filter`
    /// only runs on a real upstream response, so a connect/TLS failure (or any
    /// pre-response proxy error) skips it. Recover the pending payment
    /// side-effects here — refund an open `upto` channel and re-attach the x402
    /// `exact` receipt — so a client whose payment already settled on-chain
    /// still receives its PAYMENT-RESPONSE instead of a bare 502 (mirrors
    /// `finish_inline` and `response_filter`). Falls back to Pingora's canned
    /// error when there is no receipt to preserve.
    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        e: &pingora::Error,
        ctx: &mut Ctx,
    ) -> pingora::proxy::FailToProxy
    where
        Self::CTX: Send + Sync,
    {
        let extra = self.drain_payment_headers(ctx, false).await;

        // Mirror Pingora's default downstream error-code derivation so behavior
        // is unchanged except for the preserved payment headers.
        let code = match e.etype() {
            pingora::ErrorType::HTTPStatus(code) => *code,
            _ => match e.esource() {
                pingora::ErrorSource::Upstream => 502,
                pingora::ErrorSource::Downstream => match e.etype() {
                    pingora::ErrorType::WriteError
                    | pingora::ErrorType::ReadError
                    | pingora::ErrorType::ConnectionClosed => 0, // conn already dead
                    _ => 400,
                },
                pingora::ErrorSource::Internal | pingora::ErrorSource::Unset => 500,
            },
        };
        if code >= 500
            && let Some(Target::Api {
                subdomain,
                upstream,
                ..
            }) = &ctx.target
        {
            let path = ctx.log.as_ref().map_or("", |log| log.path.as_str());
            telemetry::record_upstream_error(subdomain, path, upstream, &e.to_string());
        }

        if code > 0 {
            if extra.is_empty() {
                let _ = session.respond_error(code).await;
            } else {
                let body = Bytes::from_static(b"{\"error\":\"upstream_unavailable\"}");
                match ResponseHeader::build(code, None) {
                    Ok(mut resp) => {
                        for (n, v) in extra {
                            let _ = resp.insert_header(n, v);
                        }
                        let _ = resp.insert_header("content-type", "application/json");
                        let _ = resp.insert_header("content-length", body.len().to_string());
                        if session
                            .write_response_header(Box::new(resp), false)
                            .await
                            .is_ok()
                        {
                            let _ = session.write_response_body(Some(body), true).await;
                        }
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "failed to build payment-preserving error response");
                        let _ = session.respond_error(code).await;
                    }
                }
            }
        }

        pingora::proxy::FailToProxy {
            error_code: code,
            can_reuse_downstream: false,
        }
    }
}

/// Collect an `http::HeaderMap` into `(name, value)` string pairs for PDB.
///
/// Uses lossy UTF-8 rather than `to_str()` — the latter rejects any non-visible-
/// ASCII byte, which would silently drop a `WWW-Authenticate` whose challenge
/// description carries a non-ASCII char (e.g. an em-dash). PDB's correlation
/// engine keys on that header, so dropping it means the flow never appears.
/// Whether a payment-backed forward must read its whole upstream body before
/// it settles and releases the response: delegated sessions and metered
/// client vouchers rate the body, and x402 `upto` (fixed price included)
/// charges only for a response that was delivered complete.
fn settles_on_completed_body(delegated_session: bool, client_metered: bool, upto: bool) -> bool {
    delegated_session || client_metered || upto
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            let value = String::from_utf8_lossy(v.as_bytes()).into_owned();
            (k.as_str().to_string(), value)
        })
        .collect()
}

fn header_pairs_from_owned(headers: Vec<(HeaderName, HeaderValue)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|(k, v)| {
            let value = String::from_utf8_lossy(v.as_bytes()).into_owned();
            (k.as_str().to_string(), value)
        })
        .collect()
}

fn request_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let mut hosts = headers.get_all(http::header::HOST).iter();
    let host = hosts.next().and_then(|value| value.to_str().ok());
    if hosts.next().is_some() {
        return None;
    }
    let authority = uri.authority().map(|authority| authority.as_str());
    match (host, authority) {
        (Some(host), Some(authority)) if host != authority => None,
        (Some(host), _) => Some(host.to_string()),
        (None, Some(authority)) if !headers.contains_key(http::header::HOST) => {
            Some(authority.to_string())
        }
        _ => None,
    }
}

fn gateway_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    request_host(headers, uri)?;
    let mut values = headers.get_all("x-pay-gateway-host").iter();
    let host = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(host.to_string())
}

fn is_control_plane(path: &str) -> bool {
    path.is_empty()
        || path.starts_with("__402/")
        || path == "openapi.json"
        || path.starts_with(".well-known/")
}

/// Remove gateway query credentials from a natively proxied response's
/// URL-bearing headers before they are written downstream.
fn scrub_url_credentials(credentials: &UrlCredentials, response: &mut ResponseHeader) {
    let mut scrubbed = response.headers.clone();
    credentials.scrub_response_headers(&mut scrubbed);
    for name in URL_RESPONSE_HEADERS {
        let before: Vec<HeaderValue> = response.headers.get_all(&name).iter().cloned().collect();
        let after: Vec<HeaderValue> = scrubbed.get_all(&name).iter().cloned().collect();
        if before == after {
            continue;
        }
        response.remove_header(&name);
        for value in after {
            let _ = response.append_header(name.clone(), value);
        }
    }
}

/// Split a prepared upstream request into a connectable [`Target::Api`].
fn target_from_prepared(
    prepared: pay_core::server::proxy::PreparedUpstreamRequest,
    subdomain: String,
) -> Target {
    let upstream = prepared.url_for_logging();
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
        subdomain,
        upstream,
        headers: prepared.headers,
        url_credentials: prepared.url_credentials,
    }
}

fn buffered_upstream_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len() + 1);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out.push(("accept-encoding".to_string(), "identity".to_string()));
    out
}

async fn read_downstream_body(session: &mut Session, limit: usize) -> pingora::Result<Bytes> {
    let mut buf = BytesMut::new();
    while let Some(chunk) = session.read_request_body().await? {
        if buf.len().saturating_add(chunk.len()) > limit {
            return Err(pingora::Error::explain(
                pingora::ErrorType::HTTPStatus(413),
                "buffered request body too large",
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

fn filtered_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let name_lower = name.as_str();
        if matches!(name_lower, "content-length" | "transfer-encoding") {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(n, v);
        }
    }
    out
}

fn is_streamed_response(headers: &HeaderMap) -> bool {
    pay_core::server::gate::is_streaming_response(headers)
}

fn is_sse_response(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .split(';')
                .any(|part| part.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

async fn write_buffered_response(
    session: &mut Session,
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    extra: Vec<(HeaderName, HeaderValue)>,
) -> pingora::Result<()> {
    let mut out = ResponseHeader::build(status.as_u16(), None)?;
    for (n, v) in headers {
        let Some(n) = n else {
            continue;
        };
        if n.as_str().eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.append_header(n, v)?;
    }
    for (n, v) in extra {
        let _ = out.append_header(n, v);
    }
    out.insert_header("content-length", body.len().to_string())?;
    session.write_response_header(Box::new(out), false).await?;
    session.write_response_body(Some(body), true).await?;
    Ok(())
}

/// Write a gate-produced [`GateResponse`] (402 challenge, 404, receipt JSON, …)
/// directly to the downstream and stop. Duplicate `WWW-Authenticate` lines are
/// preserved via `append_header`.
fn batch_commit_failed_error() -> Box<pingora::Error> {
    pingora::Error::explain(
        pingora::ErrorType::HTTPStatus(502),
        "x402 batch payment commitment failed",
    )
}

fn batch_commit_failed_response() -> axum::response::Response {
    axum::response::Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(BATCH_COMMIT_FAILED_BODY))
        .expect("static response must build")
}

const BATCH_COMMIT_FAILED_BODY: &[u8] = b"{\"error\":\"payment_commit_failed\"}";

async fn write_batch_commit_failed(
    session: &mut Session,
    extra: Vec<(HeaderName, HeaderValue)>,
) -> pingora::Result<()> {
    write_buffered_response(
        session,
        StatusCode::BAD_GATEWAY,
        HeaderMap::new(),
        Bytes::from_static(BATCH_COMMIT_FAILED_BODY),
        extra,
    )
    .await
}

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

#[cfg(test)]
#[path = "http402_batch_commit_tests.rs"]
mod batch_commit_tests;

#[cfg(test)]
mod tests {
    use super::{
        BatchResponseCapture, Http402Gate, MAX_BATCH_CACHED_RESPONSE_BYTES,
        buffered_upstream_headers, filtered_response_headers, gateway_host, header_pairs,
        is_control_plane, is_streamed_response, request_host,
    };
    use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
    use pay_core::PaymentState;
    use pay_core::server::gate::delegated_session_receipt_annotation;
    use pay_core::server::proxy::UpstreamPlan;
    use pay_types::metering::{
        AccountingMode, ApiCategory, ApiSpec, AuthConfig, HmacAlgorithm, HmacCanonicalComponent,
        HmacCanonicalConfig, HmacDigestAlgorithm, HmacEncoding, HmacPrepareBinding,
        HmacPrepareValue, HmacSignatureConfig, HmacSignatureDestination, HmacTarget,
        HmacTargetType, RoutingConfig,
    };

    #[derive(Clone)]
    struct BodySigningState {
        apis: Vec<ApiSpec>,
    }

    impl PaymentState for BodySigningState {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }

        fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
            None
        }
    }

    async fn request_session(path: &str) -> (pingora::proxy::Session, tokio::net::TcpStream) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        client
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: invalid-host\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let stream = pingora::protocols::l4::stream::Stream::from(server);
        let mut session = pingora::proxy::Session::new_h1(Box::new(stream));
        assert!(session.read_request().await.unwrap());
        (session, client)
    }

    #[tokio::test]
    async fn pingora_rejects_ambiguous_paths_before_policy_or_upstream() {
        use pingora::proxy::ProxyHttp;
        use tokio::io::AsyncReadExt;
        let resolver = pay_core::server::deployment_policy::DeploymentPolicyResolver::new(
            "https://resolver.example/__402/payment-policy",
            "https://resolver.example",
            "gateway.example",
        )
        .unwrap();
        let gate = Http402Gate::new(BodySigningState { apis: vec![] }, "127.0.0.1:1")
            .with_deployment_policy_resolver(Some(std::sync::Arc::new(resolver)));
        for path in [
            "//foo",
            "///foo?x=1",
            "//.well-known/test",
            "/v1/%2e%2e",
            "/v1/%2E%2E/admin?x=1",
        ] {
            let (mut session, mut client) = request_session(path).await;
            let mut ctx = gate.new_ctx();
            assert!(gate.request_filter(&mut session, &mut ctx).await.unwrap());
            assert!(ctx.target.is_none(), "must not plan upstream");
            assert!(ctx.session.is_none());
            drop(session);
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 400"));
            // The deliberately invalid host would instead fail policy host
            // classification if the resolver were reached.
            assert!(response.contains("invalid_request_path"), "{response}");
        }
    }

    #[derive(Clone)]
    struct AllowlistState {
        apis: Vec<ApiSpec>,
        session: std::sync::Arc<pay_core::server::session::SessionMpp>,
    }

    impl PaymentState for AllowlistState {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&pay_kit::mpp::server::Mpp> {
            None
        }
        fn session_mpp(&self) -> Option<&pay_core::server::session::SessionMpp> {
            Some(&self.session)
        }
        fn session_mpp_handle(
            &self,
        ) -> Option<std::sync::Arc<pay_core::server::session::SessionMpp>> {
            Some(self.session.clone())
        }
    }

    /// One declared root `POST ""` plus a parameterized paid `GET` and a free
    /// `GET`, behind operator header auth.
    fn allowlist_api() -> ApiSpec {
        let metering = serde_json::json!({
            "schemes": ["mpp-session"],
            "dimensions": [{
                "direction": "usage", "unit": "requests", "scale": 1,
                "tiers": [{"price_usd": 0.001}]
            }]
        });
        serde_json::from_value(serde_json::json!({
            "name": "rpc", "subdomain": "rpc", "title": "RPC", "description": "",
            "category": "ai_ml", "version": "v1",
            "routing": {
                "type": "proxy", "url": "http://127.0.0.1:1",
                "auth": {"method": "header", "key": "x-operator-key", "value_from_env": "PATH"}
            },
            "endpoints": [
                {"method": "POST", "path": "", "metering": metering},
                {"method": "GET", "path": "v1/items/{id}", "metering": metering},
                {"method": "GET", "path": "v1/free"}
            ]
        }))
        .unwrap()
    }

    #[derive(Debug, PartialEq)]
    enum Planned {
        Responded(String),
        ControlPlane,
        Api { path: String, operator_key: bool },
    }

    async fn plan<S: PaymentState>(
        gate: &Http402Gate<S>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Planned {
        use pingora::proxy::ProxyHttp;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: rpc.example\r\nContent-Length: 0\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        client.write_all(request.as_bytes()).await.unwrap();
        let stream = pingora::protocols::l4::stream::Stream::from(server);
        let mut session = pingora::proxy::Session::new_h1(Box::new(stream));
        assert!(session.read_request().await.unwrap());
        let mut ctx = gate.new_ctx();
        if gate.request_filter(&mut session, &mut ctx).await.unwrap() {
            assert!(
                ctx.target.is_none(),
                "{method} {path} responded and planned"
            );
            drop(session);
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8_lossy(&response);
            return Planned::Responded(response.split(' ').nth(1).unwrap_or("").to_string());
        }
        match ctx.target {
            Some(super::Target::ControlPlane) => Planned::ControlPlane,
            Some(super::Target::Api {
                path_and_query,
                headers,
                ..
            }) => Planned::Api {
                path: path_and_query,
                operator_key: headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("x-operator-key")),
            },
            None => panic!("{method} {path}: no response and no target"),
        }
    }

    #[tokio::test]
    async fn pingora_forwards_only_gate_routed_requests_upstream() {
        use pay_core::server::gate::{GateDecision, GateRequest, PaymentGate};
        let store: std::sync::Arc<dyn pay_kit::mpp::store::ChannelStore> =
            std::sync::Arc::new(pay_kit::mpp::store::MemoryChannelStore::new());
        let blockhash = pay_kit::mpp::blockhash::BlockhashCache::new();
        blockhash.set(
            "SURFNETxSAFEHASHxxxxxxxxxxxxxxxxxxxxx11x".to_string(),
            42,
            123,
        );
        let session = pay_core::server::session::SessionMpp::new_with_channel_store(
            pay_kit::mpp::server::session::SessionConfig {
                operator: solana_pubkey::Pubkey::new_unique().to_string(),
                recipient: solana_pubkey::Pubkey::new_unique().to_string(),
                suggested_deposit: Some(5_000_000),
                currency: solana_pubkey::Pubkey::new_unique().to_string(),
                network: "localnet".to_string(),
                ..Default::default()
            },
            "test-secret-key-do-not-use-32b-pad",
            store.clone(),
        )
        .with_blockhash_cache(blockhash);
        let state = AllowlistState {
            apis: vec![allowlist_api()],
            session: std::sync::Arc::new(session),
        };
        let gate = Http402Gate::new(state.clone(), "127.0.0.1:1");
        let responded = |status: &str| Planned::Responded(status.to_string());

        for (method, path, headers, expected) in [
            ("POST", "/", vec![], responded("402")),
            ("GET", "/", vec![("Accept", "text/html")], responded("402")),
            ("GET", "/v1/items/42", vec![], responded("402")),
            ("HEAD", "/v1/items/42", vec![], responded("402")),
            ("POST", "/rpc", vec![], responded("404")),
            ("POST", "/anything", vec![], responded("404")),
            ("GET", "/rpc", vec![], responded("404")),
            (
                "POST",
                "/rpc",
                vec![("Authorization", "Payment x")],
                responded("404"),
            ),
            ("GET", "/v1/items/42/extra", vec![], responded("404")),
            ("DELETE", "/v1/items/42", vec![], responded("404")),
            (
                "POST",
                "/v1/free",
                vec![("Authorization", "Payment x")],
                responded("404"),
            ),
            // The root is also the control plane, which re-gates and serves
            // only its own routes; it never injects operator credentials.
            ("GET", "/", vec![], Planned::ControlPlane),
            ("HEAD", "/", vec![], Planned::ControlPlane),
            (
                "GET",
                "/",
                vec![("Authorization", "Payment x")],
                Planned::ControlPlane,
            ),
        ] {
            assert_eq!(
                plan(&gate, method, path, &headers).await,
                expected,
                "{method} {path} {headers:?}"
            );
        }

        // Positive controls: a free declared route, a paid root POST, and a
        // session commit POSTed to a declared GET resource.
        assert_eq!(
            plan(&gate, "GET", "/v1/free", &[]).await,
            Planned::Api {
                path: "/v1/free".into(),
                operator_key: true
            }
        );
        for (challenge_method, path) in [
            (http::Method::POST, "/"),
            (http::Method::GET, "/v1/items/42"),
        ] {
            let decision = PaymentGate::new(state.clone())
                .evaluate(&GateRequest {
                    method: &challenge_method,
                    path: path.trim_start_matches('/'),
                    host: Some("rpc.example"),
                    accept: None,
                    authorization: None,
                    content_length: None,
                    query: None,
                    x402_payment: None,
                })
                .await;
            let GateDecision::Respond(challenge) = decision else {
                panic!("expected a 402 for {path}");
            };
            let www_auth = challenge
                .headers
                .iter()
                .find(|(name, _)| name == header::WWW_AUTHENTICATE)
                .map(|(_, value)| value.to_str().unwrap().to_string())
                .unwrap();
            let challenge = pay_kit::mpp::parse_www_authenticate(&www_auth).unwrap();
            let price: u64 = challenge
                .request
                .decode::<pay_kit::mpp::SessionRequest>()
                .unwrap()
                .amount
                .parse()
                .unwrap();
            let channel = solana_pubkey::Pubkey::new_unique();
            let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
            let mut keypair = [0u8; 64];
            keypair[..32].copy_from_slice(key.as_bytes());
            keypair[32..].copy_from_slice(key.verifying_key().as_bytes());
            let signer: Box<dyn pay_kit::mpp::solana_keychain::TransactionSigner> = Box::new(
                pay_kit::mpp::solana_keychain::memory::MemorySigner::from_bytes(&keypair).unwrap(),
            );
            store
                .put_channel(
                    &channel.to_string(),
                    pay_core::server::session::test_channel_state(
                        channel.to_string(),
                        1_000_000,
                        signer.pubkey().to_string(),
                        "client",
                        &challenge.id,
                        solana_pubkey::Pubkey::new_unique().to_string(),
                        None,
                    ),
                )
                .await
                .unwrap();
            let handle = pay_core::session::SessionHandle::new(channel, signer, challenge)
                .with_voucher_key(key);
            let voucher = handle.voucher_header(price).await.unwrap();
            assert_eq!(
                plan(&gate, "POST", path, &[("Authorization", &voucher)]).await,
                Planned::Api {
                    path: path.into(),
                    operator_key: true
                },
                "paid POST {path}"
            );
        }
    }

    #[tokio::test]
    async fn pingora_rejects_undeclared_proxy_routes_without_planning_upstream() {
        use pingora::proxy::ProxyHttp;
        use tokio::io::AsyncReadExt;
        let mut api = body_signing_api();
        api.routing = RoutingConfig::Proxy {
            url: "http://127.0.0.1:1".into(),
            path_rewrites: vec![],
            auth: Some(Box::new(AuthConfig::Header {
                key: "x-api-key".into(),
                prefix: None,
                value_from_env: "PATH".into(),
            })),
        };
        api.endpoints.push(pay_types::metering::Endpoint {
            method: pay_types::metering::HttpMethod::Get,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
            subscription: None,
        });
        let gate = Http402Gate::new(BodySigningState { apis: vec![api] }, "127.0.0.1:1");

        let (mut session, mut client) = request_session("/admin/users?scope=all").await;
        let mut ctx = gate.new_ctx();
        assert!(gate.request_filter(&mut session, &mut ctx).await.unwrap());
        assert!(ctx.target.is_none(), "must not plan an upstream");
        let mut response = vec![0_u8; 256];
        let read = client.read(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 404"));

        let (mut session, _client) = request_session("/v1/search").await;
        let mut ctx = gate.new_ctx();
        assert!(!gate.request_filter(&mut session, &mut ctx).await.unwrap());
        assert!(matches!(ctx.target, Some(super::Target::Api { .. })));
    }

    #[tokio::test]
    async fn pingora_preserves_normal_path_and_query() {
        use pingora::proxy::ProxyHttp;
        let mut api = body_signing_api();
        api.routing = RoutingConfig::Proxy {
            url: "http://127.0.0.1:1".into(),
            path_rewrites: vec![],
            auth: None,
        };
        let gate = Http402Gate::new(BodySigningState { apis: vec![api] }, "127.0.0.1:1");
        for path in ["/foo", "/foo?x=%2F&x=2", "/foo//bar?x=1"] {
            let (mut session, _client) = request_session(path).await;
            let mut ctx = gate.new_ctx();
            assert!(!gate.request_filter(&mut session, &mut ctx).await.unwrap());
            match &ctx.target {
                Some(super::Target::Api { path_and_query, .. }) => assert_eq!(path_and_query, path),
                _ => panic!("expected real upstream target"),
            }
            assert_eq!(
                session.req_header().uri.path_and_query().unwrap().as_str(),
                path
            );
            assert_eq!(ctx.request_path, path.split('?').next().unwrap());
        }
    }

    #[tokio::test]
    async fn inline_response_body_error_fails_closed() {
        use pingora::proxy::ProxyHttp;
        use tokio::io::AsyncReadExt;
        let gate = Http402Gate::new(BodySigningState { apis: vec![] }, "127.0.0.1:1");
        let (mut session, mut client) = request_session("/foo").await;
        let mut ctx = gate.new_ctx();
        let body = axum::body::Body::from_stream(futures_util::stream::once(async {
            Err::<bytes::Bytes, _>(std::io::Error::other("broken upstream body"))
        }));
        let response = axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-length", "15")
            .body(body)
            .unwrap();
        gate.finish_buffered_axum_response(&mut session, &mut ctx, response)
            .await
            .unwrap();
        drop(session);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("upstream_body_read_failed"));
        assert!(!response.contains("payment-receipt"));
    }

    #[test]
    fn request_host_falls_back_to_http2_authority() {
        let headers = HeaderMap::new();
        let uri: Uri = "https://worker.cpu.gcp.gateway-402.com/latest"
            .parse()
            .unwrap();

        assert_eq!(
            request_host(&headers, &uri).as_deref(),
            Some("worker.cpu.gcp.gateway-402.com")
        );
    }

    #[test]
    fn request_host_rejects_conflicting_host_and_authority() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            HeaderValue::from_static("explicit.example"),
        );
        let uri: Uri = "https://authority.example/latest".parse().unwrap();

        assert_eq!(request_host(&headers, &uri), None);
    }

    #[test]
    fn request_host_ignores_caller_forwarded_host() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            HeaderValue::from_static("rewritten-backend.run.app"),
        );
        headers.insert(
            "x-pay-forwarded-host",
            HeaderValue::from_static("worker.cpu.gcp.gateway-402.com"),
        );
        let uri: Uri = "https://rewritten-backend.run.app/latest".parse().unwrap();

        assert_eq!(
            request_host(&headers, &uri).as_deref(),
            Some("rewritten-backend.run.app")
        );
    }

    #[test]
    fn request_host_never_adopts_forwarded_host_without_authority() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("worker.cpu.gcp.gateway-402.com"),
        );
        headers.insert(
            "x-pay-forwarded-host",
            HeaderValue::from_static("worker.cpu.gcp.gateway-402.com"),
        );
        assert_eq!(request_host(&headers, &"/latest".parse().unwrap()), None);
    }

    #[test]
    fn deployment_requires_overwritten_gateway_host() {
        let uri: Uri = "https://rewritten-backend.run.app/latest".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            HeaderValue::from_static("rewritten-backend.run.app"),
        );
        headers.insert(
            "x-pay-forwarded-host",
            HeaderValue::from_static("attacker.cpu.gcp.gateway-402.com"),
        );
        assert_eq!(gateway_host(&headers, &uri), None);
        headers.insert(
            "x-pay-gateway-host",
            HeaderValue::from_static("worker.cpu.gcp.gateway-402.com"),
        );
        assert_eq!(
            gateway_host(&headers, &uri).as_deref(),
            Some("worker.cpu.gcp.gateway-402.com")
        );
        headers.append(
            "x-pay-gateway-host",
            HeaderValue::from_static("attacker.cpu.gcp.gateway-402.com"),
        );
        assert_eq!(gateway_host(&headers, &uri), None);
        headers.remove("x-pay-gateway-host");
        headers.insert(
            "x-pay-gateway-host",
            HeaderValue::from_static("worker.cpu.gcp.gateway-402.com"),
        );
        assert_eq!(
            gateway_host(
                &headers,
                &"https://conflicting.run.app/latest".parse().unwrap()
            ),
            None
        );
    }

    fn body_signing_api() -> ApiSpec {
        ApiSpec {
            name: "body signing test".to_string(),
            subdomain: "signed".to_string(),
            title: "Body signing test".to_string(),
            description: String::new(),
            category: ApiCategory::AiMl,
            version: "1.0".to_string(),
            env: std::collections::HashMap::new(),
            routing: RoutingConfig::Proxy {
                url: "https://upstream.example".to_string(),
                path_rewrites: vec![],
                auth: Some(Box::new(AuthConfig::Hmac {
                    algorithm: HmacAlgorithm::Sha256,
                    // PATH is guaranteed in the test process and its value is
                    // irrelevant; this test asserts the body-derived header.
                    secret_from_env: "PATH".to_string(),
                    secret_suffix: None,
                    key_id_from_env: None,
                    prepare: vec![HmacPrepareBinding {
                        target: HmacTarget {
                            kind: HmacTargetType::Header,
                            name: "Content-MD5".to_string(),
                        },
                        value: HmacPrepareValue::BodyDigest {
                            algorithm: HmacDigestAlgorithm::Md5,
                            encoding: HmacEncoding::Base64,
                        },
                    }],
                    canonical: HmacCanonicalConfig {
                        join_with: "\n".to_string(),
                        components: vec![HmacCanonicalComponent::Header {
                            name: "Content-MD5".to_string(),
                        }],
                    },
                    signature: HmacSignatureConfig {
                        encoding: HmacEncoding::Base64,
                        destination: HmacSignatureDestination {
                            kind: HmacTargetType::Header,
                            name: "Authorization".to_string(),
                            template: "HMAC {signature}".to_string(),
                        },
                    },
                })),
            },
            accounting: AccountingMode::Pooled,
            endpoints: vec![],
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
            session: None,
            batch_settlement: None,
            recipients: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn control_plane_paths_are_not_tracked() {
        // These bypass PDB tracking (request_filter) as well as payment.
        // The ephemeral skills poll must not surface as a failed request.
        for path in [
            "",
            "__402/health",
            "__402/pdb/api/config",
            "openapi.json",
            ".well-known/pay-skills.json",
        ] {
            assert!(is_control_plane(path), "{path} must be control-plane");
        }
        // Real upstream traffic still tracks.
        for path in ["v1/messages", "v1/chat/completions", "api/chat"] {
            assert!(!is_control_plane(path), "{path} must be tracked");
        }
    }

    #[test]
    fn batch_response_capture_requires_complete_bounded_body() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("6"));
        let mut capture =
            BatchResponseCapture::start(StatusCode::CREATED, &headers).expect("bounded");

        capture.push(b"abc", false);
        assert!(!capture.complete);
        capture.push(b"def", true);

        assert!(capture.complete);
        assert_eq!(
            capture.cached.as_ref().map(|cached| cached.body.as_slice()),
            Some(b"abcdef".as_slice())
        );
    }

    #[test]
    fn batch_response_capture_rejects_or_drops_oversized_bodies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&(MAX_BATCH_CACHED_RESPONSE_BYTES + 1).to_string()).unwrap(),
        );
        assert!(BatchResponseCapture::start(StatusCode::OK, &headers).is_none());

        let mut capture = BatchResponseCapture::start(StatusCode::OK, &HeaderMap::new())
            .expect("unknown length starts bounded");
        capture.push(&vec![0; MAX_BATCH_CACHED_RESPONSE_BYTES + 1], true);
        assert!(capture.complete);
        assert!(capture.cached.is_none());
    }

    #[test]
    fn buffered_upstream_headers_force_identity_encoding() {
        let headers = buffered_upstream_headers(&[
            ("accept-encoding".to_string(), "br, gzip".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]);

        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
                .count(),
            1
        );
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("accept-encoding") && value == "identity"
        }));
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type") && value == "application/json"
        }));
    }

    #[test]
    fn debugger_header_capture_preserves_credentials_for_safe_projection() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(
            "payment-signature",
            HeaderValue::from_static("signed-payment"),
        );
        headers.insert(
            "mcp-session-id",
            HeaderValue::from_static("session-capability"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let captured = header_pairs(&headers);

        assert!(
            captured
                .iter()
                .any(|(name, value)| { name == "authorization" && value == "Bearer secret" })
        );
        assert!(
            captured
                .iter()
                .any(|(name, value)| { name == "payment-signature" && value == "signed-payment" })
        );
        assert!(
            captured
                .iter()
                .any(|(name, value)| { name == "mcp-session-id" && value == "session-capability" })
        );
        assert!(
            captured
                .iter()
                .any(|(name, value)| { name == "content-type" && value == "application/json" })
        );
    }

    #[test]
    fn filtered_response_headers_preserve_content_encoding() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_ENCODING,
            reqwest::header::HeaderValue::from_static("gzip"),
        );
        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("123"),
        );
        headers.insert(
            reqwest::header::TRANSFER_ENCODING,
            reqwest::header::HeaderValue::from_static("chunked"),
        );

        let filtered = filtered_response_headers(&headers);

        assert_eq!(
            filtered
                .get(http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
        assert!(!filtered.contains_key(http::header::CONTENT_LENGTH));
        assert!(!filtered.contains_key(http::header::TRANSFER_ENCODING));
    }

    #[test]
    fn streamed_response_detection_accepts_sse_and_ndjson() {
        let mut sse = http::HeaderMap::new();
        sse.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_streamed_response(&sse));

        let mut ndjson = http::HeaderMap::new();
        ndjson.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-ndjson"),
        );
        assert!(is_streamed_response(&ndjson));

        let mut json = http::HeaderMap::new();
        json.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        assert!(!is_streamed_response(&json));
    }

    #[tokio::test]
    async fn http402_gate_prepares_body_digest_from_buffered_body() {
        let gate = Http402Gate::new(
            BodySigningState {
                apis: vec![body_signing_api()],
            },
            "127.0.0.1:1",
        );
        let api = gate.resolve_api("signed.example").expect("API resolves");
        let decision = pay_core::server::gate::PaymentGate::new(gate.state.clone())
            .evaluate(&pay_core::server::gate::GateRequest {
                method: &http::Method::POST,
                path: "v1/generate",
                host: Some("signed.example"),
                accept: None,
                authorization: None,
                content_length: None,
                query: None,
                x402_payment: None,
            })
            .await;
        let pay_core::server::gate::GateDecision::Passthrough(route) = decision else {
            panic!("an API without endpoints is a passthrough proxy");
        };
        let plan = gate
            .prepare_buffered_request(
                api,
                &route,
                &http::Method::POST,
                &"/v1/generate".parse().unwrap(),
                &HeaderMap::new(),
                b"hello",
                None,
            )
            .await
            .expect("request prepares");
        let UpstreamPlan::Forward(prepared) = plan else {
            panic!("free body-signing route should forward");
        };

        assert!(prepared.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-md5") && value == "XUFAKrxLKna5cZ2REBfFkg=="
        }));
    }

    #[tokio::test]
    async fn forwarding_follows_the_endpoint_the_gate_selected() {
        use pay_types::metering::{Endpoint, HttpMethod};
        let endpoint = |method, path: &str, routing| Endpoint {
            method,
            path: path.to_string(),
            description: None,
            resource: None,
            routing,
            metering: None,
            subscription: None,
        };
        let mut api = body_signing_api();
        api.routing = RoutingConfig::Proxy {
            url: "https://upstream.example".to_string(),
            path_rewrites: vec![],
            auth: None,
        };
        api.endpoints = vec![
            // Same path, other method: must not lend its override to POST.
            endpoint(
                HttpMethod::Get,
                "v1/projects/query",
                Some(RoutingConfig::Respond {}),
            ),
            // First POST pattern: the gate selects it; it has no override.
            endpoint(HttpMethod::Post, "v1/{scope}/query", None),
            // Later overlapping pattern with an override.
            endpoint(
                HttpMethod::Post,
                "v1/projects/{name}",
                Some(RoutingConfig::Respond {}),
            ),
        ];
        let state = BodySigningState { apis: vec![api] };
        let gate = pay_core::server::gate::PaymentGate::new(state.clone());
        let decision = gate
            .evaluate(&pay_core::server::gate::GateRequest {
                method: &http::Method::POST,
                path: "v1/projects/query",
                host: Some("signed.example"),
                accept: None,
                authorization: None,
                content_length: None,
                query: None,
                x402_payment: None,
            })
            .await;
        let pay_core::server::gate::GateDecision::Passthrough(route) = decision else {
            panic!("a free declared endpoint is a passthrough");
        };
        let plan = pay_core::server::proxy::prepare_upstream(
            &state.apis[0],
            &route,
            &http::Method::POST,
            &"/v1/projects/query".parse().unwrap(),
            &HeaderMap::new(),
            b"",
        )
        .await
        .expect("request prepares");
        let UpstreamPlan::Forward(prepared) = plan else {
            panic!("the selected endpoint proxies; another endpoint's override must not apply");
        };
        assert_eq!(prepared.url.host_str(), Some("upstream.example"));
    }

    /// Serves one client-voucher session request on `path` through the
    /// buffered adapter and returns what reached the client within `wait`.
    async fn client_voucher_response(path: &str, wait: std::time::Duration) -> String {
        use pay_types::metering::{
            BillingUnit, Endpoint, HttpMethod, MeterDimension, MeterDirection, Metering, PriceTier,
            Scheme, UsageMeter, UsageMeterSource,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = axum::Router::new().fallback(|| async {
            (
                [(http::header::CONTENT_TYPE, "application/json")],
                r#"{"text":"protected-content","usage":{"completion_tokens":50}}"#,
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, upstream).await });

        let dimension = |direction, unit, meter| MeterDimension {
            direction,
            unit,
            scale: 1,
            period: None,
            tiers: vec![PriceTier {
                up_to: None,
                price_usd: 0.000001,
                condition: None,
                notes: None,
                splits: vec![],
            }],
            meter,
        };
        let endpoint = |path: &str, metering| Endpoint {
            method: HttpMethod::Post,
            path: path.to_string(),
            description: None,
            resource: None,
            routing: None,
            metering,
            subscription: None,
        };
        let mut api = body_signing_api();
        api.routing = RoutingConfig::Proxy {
            url: format!("http://{upstream_addr}"),
            path_rewrites: vec![],
            auth: None,
        };
        api.endpoints = vec![endpoint("v1/metered", None), endpoint("v1/flat", None)];
        // The gate only mints a route for an endpoint it admitted. Admit the
        // free variant, then serve the same endpoint index with its metering.
        let route = match pay_core::server::gate::PaymentGate::new(BodySigningState {
            apis: vec![api.clone()],
        })
        .evaluate(&pay_core::server::gate::GateRequest {
            method: &http::Method::POST,
            path,
            host: Some("signed.example"),
            accept: None,
            authorization: None,
            content_length: None,
            query: None,
            x402_payment: None,
        })
        .await
        {
            pay_core::server::gate::GateDecision::Passthrough(route) => route,
            _ => panic!("a free endpoint is a passthrough"),
        };
        let metering = |dimension| Metering {
            dimensions: vec![dimension],
            schemes: Some(vec![Scheme::MppSession]),
            ..Default::default()
        };
        api.endpoints[0].metering = Some(metering(dimension(
            MeterDirection::Output,
            BillingUnit::Tokens,
            Some(UsageMeter {
                source: UsageMeterSource::ResponseJson,
                path: Some("/usage/completion_tokens".to_string()),
                header: None,
            }),
        )));
        api.endpoints[1].metering = Some(metering(dimension(
            MeterDirection::Usage,
            BillingUnit::Requests,
            None,
        )));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        client
            .write_all(
                format!(
                    "POST /{path} HTTP/1.1\r\nHost: signed.example\r\nContent-Length: 0\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let stream = pingora::protocols::l4::stream::Stream::from(server);
        let mut session = pingora::proxy::Session::new_h1(Box::new(stream));
        assert!(session.read_request().await.unwrap());

        let key = || solana_pubkey::Pubkey::new_unique().to_string();
        let handle = std::sync::Arc::new(pay_core::server::session::SessionMpp::new(
            pay_kit::mpp::server::session::SessionConfig {
                operator: key(),
                recipient: key(),
                currency: key(),
                network: "localnet".to_string(),
                ..Default::default()
            },
            "test-secret",
        ));
        let headers = session.req_header().headers.clone();
        let uri = session.req_header().uri.clone();
        // What request_filter installs for a client voucher admitted at 1_000.
        let client_meter = super::ClientVoucherMeter::for_route(
            &api,
            &route,
            &uri,
            &headers,
            super::SessionStreamContext::new(handle, key(), 1_000),
        )
        .unwrap();
        let gate = Http402Gate::new(BodySigningState { apis: vec![api] }, "127.0.0.1:1");
        let mut ctx = pingora::proxy::ProxyHttp::new_ctx(&gate);
        ctx.client_meter = client_meter;
        let serve = gate.forward_buffered(
            &mut session,
            &mut ctx,
            path,
            &route,
            Some("signed.example"),
            &http::Method::POST,
            &uri,
            &headers,
        );
        // The meter waits for a client commit that never comes; only what is
        // released within `wait` reaches the client.
        let _ = tokio::time::timeout(wait, serve).await;
        let mut raw = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(Ok(n)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), client.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    #[tokio::test]
    async fn client_voucher_response_usage_waits_for_the_client_voucher() {
        let served =
            client_voucher_response("v1/metered", std::time::Duration::from_millis(500)).await;
        assert!(
            !served.contains("protected-content"),
            "usage beyond the admitting voucher must be withheld: {served}"
        );
    }

    #[tokio::test]
    async fn client_voucher_request_priced_response_is_served() {
        let served = client_voucher_response("v1/flat", std::time::Duration::from_secs(5)).await;
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        assert!(served.contains("protected-content"), "{served}");
    }

    #[tokio::test]
    async fn native_responses_never_relay_url_credentials() {
        let mut api = body_signing_api();
        api.routing = RoutingConfig::Proxy {
            url: "https://upstream.example".to_string(),
            path_rewrites: vec![],
            // PATH is always set; its value stands in for the API key.
            auth: Some(Box::new(AuthConfig::QueryParam {
                key: "key".to_string(),
                value_from_env: "PATH".to_string(),
            })),
        };
        let state = BodySigningState { apis: vec![api] };
        let pay_core::server::gate::GateDecision::Passthrough(route) =
            pay_core::server::gate::PaymentGate::new(state.clone())
                .evaluate(&pay_core::server::gate::GateRequest {
                    method: &http::Method::GET,
                    path: "v1/data",
                    host: Some("signed.example"),
                    accept: None,
                    authorization: None,
                    content_length: None,
                    query: None,
                    x402_payment: None,
                })
                .await
        else {
            panic!("an endpoint-less API passes through");
        };
        let UpstreamPlan::Forward(prepared) = pay_core::server::proxy::prepare_upstream(
            &state.apis[0],
            &route,
            &http::Method::GET,
            &"/v1/data?page=2".parse().unwrap(),
            &HeaderMap::new(),
            b"",
        )
        .await
        .expect("request prepares") else {
            panic!("expected a proxied upstream");
        };
        let echoed = format!("/landing?{}", prepared.url.query().unwrap());
        let super::Target::Api {
            url_credentials, ..
        } = super::target_from_prepared(prepared, "signed".to_string())
        else {
            panic!("expected an API target");
        };

        let mut response = super::ResponseHeader::build(302, None).unwrap();
        response.insert_header("location", echoed.as_str()).unwrap();
        response.insert_header("x-other", "kept").unwrap();
        super::scrub_url_credentials(&url_credentials, &mut response);

        assert_eq!(response.headers["location"], "/landing?page=2");
        assert_eq!(response.headers["x-other"], "kept");
    }

    #[test]
    fn every_upto_settles_on_a_completed_body() {
        // Fixed-price upto included: charging at headers keeps the payment of
        // a body that then fails.
        assert!(super::settles_on_completed_body(false, false, true));
        assert!(super::settles_on_completed_body(true, false, false));
        assert!(super::settles_on_completed_body(false, true, false));
        // Request-priced client vouchers, exact and batch stay native.
        assert!(!super::settles_on_completed_body(false, false, false));
    }

    #[test]
    fn delegated_session_receipt_reports_the_accepted_voucher() {
        let annotation = delegated_session_receipt_annotation(
            "mainnet",
            pay_types::stablecoin_mints::USDC_MAINNET,
            "JAxsw27vxSNVSTEpgvnGPsGvfVmDX6jMqezbYU7D673Z",
            1,
            1,
            100_000,
            300,
        )
        .unwrap();
        let headers = annotation
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let decoded = pay_core::client::receipt::decode_response_receipt(&headers).unwrap();

        assert_eq!(decoded.decoded["intent"], "session");
        assert_eq!(decoded.decoded["amount"], "1");
        assert_eq!(decoded.decoded["acceptedCumulative"], "1");
        assert_eq!(decoded.decoded["spent"], "1");
        assert_eq!(decoded.decoded["idleTimeoutSeconds"], 300);
        assert_eq!(decoded.decoded["remaining"], "99999");
        assert!(headers.iter().any(|(name, value)| {
            name == "payment-receipt-url"
                && value == "https://pay.sh/receipt/JAxsw27vxSNVSTEpgvnGPsGvfVmDX6jMqezbYU7D673Z"
        }));
    }
}
