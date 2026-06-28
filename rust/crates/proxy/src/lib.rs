//! `pay-proxy` — the production data plane, as a library `pay` relies on.
//!
//! [`Http402Gate`] is a Pingora [`ProxyHttp`](pingora::proxy::ProxyHttp) that
//! runs the framework-agnostic `pay_core::server::gate::PaymentGate` and proxies
//! natively: metered traffic to the endpoint's real upstream, control-plane
//! paths to an internal axum service.
//!
//! Callers ([`run`]) build their own `PaymentState` + an internal axum
//! control-plane service, then hand the public bind to Pingora.

pub mod http402;

pub use http402::Http402Gate;

use pay_core::PaymentState;
use pingora::proxy::http_proxy_service;
use pingora::server::Server;

/// Build and run the Pingora gateway on `bind`, fronting `state`'s
/// [`PaymentGate`] and forwarding control-plane traffic to `control_plane`
/// (the `host:port` of an internal axum service).
///
/// **Blocks forever** — Pingora owns its own runtimes, so this MUST be called
/// from a thread with no ambient tokio runtime (e.g. the main thread after the
/// caller's `block_on` has returned, with the axum control-plane already spawned
/// on its own thread/runtime).
pub fn run<S: PaymentState>(
    state: S,
    bind: &str,
    control_plane: String,
    threads: Option<usize>,
) -> anyhow::Result<()> {
    // rustls 0.23 requires a process-default CryptoProvider. The dependency tree
    // enables BOTH ring (pingora) and aws-lc-rs (reqwest), so rustls can't pick
    // one automatically and pingora's TLS init panics. Install ring (what
    // pingora-rustls uses) once, before any pingora TLS setup. Idempotent — the
    // Err just means a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut server = Server::new(None).map_err(|e| anyhow::anyhow!("pingora server: {e}"))?;
    server.bootstrap();
    let gate = Http402Gate::new(state, control_plane);
    let mut svc = http_proxy_service(&server.configuration, gate);
    // Pingora services default to a single worker thread — match the core count
    // so it spreads across cores like the tokio proxy did.
    let cores = threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    });
    svc.threads = Some(cores);
    svc.add_tcp(bind);
    server.add_service(svc);
    tracing::info!(bind, threads = cores, "pingora gateway up (Http402Gate)");
    server.run_forever();
}
