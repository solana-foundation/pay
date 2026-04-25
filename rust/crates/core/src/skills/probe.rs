//! Endpoint probing — verify that provider endpoints return valid Solana 402 challenges.
//!
//! Used by `pay skills probe` CLI and CI to verify that every listed endpoint
//! actually accepts payment via the expected stablecoins on Solana.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use serde::Serialize;

use crate::client::fetch::fetch_request;
use crate::client::runner::RunOutcome;

// ── Currency normalization ───────────────────────────────────────────────────

/// Known Solana mint addresses → symbol mappings.
const MINT_MAP: &[(&str, &str)] = &[
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC"),
    ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT"),
];

/// Normalize a currency identifier to its symbol (uppercase).
/// Recognizes known mint addresses and maps them to symbols.
fn normalize_currency(raw: &str) -> String {
    for (mint, symbol) in MINT_MAP {
        if raw == *mint {
            return symbol.to_string();
        }
    }
    raw.to_uppercase()
}

// ── Types ────────────────────────────────────────────────────────────────────

/// Configuration for a probe run.
pub struct ProbeConfig {
    /// Accepted currency symbols (e.g. ["USDC", "USDT"]).
    pub accepted_currencies: Vec<String>,
    /// Per-endpoint timeout in seconds.
    pub timeout_secs: u64,
    /// Max concurrent provider probes.
    pub concurrency: usize,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            accepted_currencies: vec!["USDC".into(), "USDT".into()],
            timeout_secs: 10,
            concurrency: 5,
        }
    }
}

/// Result of probing a single endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct EndpointProbeResult {
    pub method: String,
    pub path: String,
    pub url: String,
    pub status: ProbeStatus,
    pub duration_ms: u64,
}

/// Outcome of a single endpoint probe.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Valid 402 challenge with accepted currency on Solana.
    Ok {
        protocol: String,
        currency: String,
        network: String,
        recipient: String,
    },
    /// 402 returned but only for non-Solana chains.
    WrongChain { details: String },
    /// 402 returned with a currency not in the accepted set.
    WrongCurrency { got: String, accepted: Vec<String> },
    /// 402 returned but no recognized payment protocol.
    UnknownProtocol,
    /// Endpoint did not return 402 (e.g. 200, 401, 500).
    NotPaywalled { status_code: u16 },
    /// Free endpoint (no pricing in the spec) — skipped.
    Free,
    /// Connection error or timeout.
    Error { message: String },
}

impl ProbeStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. } | Self::Free)
    }
}

/// Result of probing all endpoints for a single provider.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderProbeResult {
    pub fqn: String,
    pub service_url: String,
    pub endpoints: Vec<EndpointProbeResult>,
    pub pass: bool,
}

/// Aggregate result of probing multiple providers.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub providers: Vec<ProviderProbeResult>,
    pub total_endpoints: usize,
    pub passed: usize,
    pub failed: usize,
}

// ── Probing ──────────────────────────────────────────────────────────────────

/// Probe a single endpoint and classify the response.
fn probe_endpoint(
    method: &str,
    url: &str,
    has_pricing: bool,
    config: &ProbeConfig,
) -> EndpointProbeResult {
    if !has_pricing {
        return EndpointProbeResult {
            method: method.to_string(),
            path: String::new(),
            url: url.to_string(),
            status: ProbeStatus::Free,
            duration_ms: 0,
        };
    }

    let start = Instant::now();

    // Use a minimal body for POST/PUT/PATCH to avoid 400 errors before
    // reaching the payment middleware.
    let body = match method.to_uppercase().as_str() {
        "POST" | "PUT" | "PATCH" => Some("{}"),
        _ => None,
    };
    let headers = if body.is_some() {
        vec![("content-type".into(), "application/json".into())]
    } else {
        vec![]
    };

    let status = match fetch_request(method, url, &headers, body) {
        Ok(outcome) => classify_outcome(outcome, &config.accepted_currencies),
        Err(e) => ProbeStatus::Error {
            message: e.to_string(),
        },
    };

    EndpointProbeResult {
        method: method.to_string(),
        path: String::new(), // filled in by caller
        url: url.to_string(),
        status,
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Map a `RunOutcome` to a `ProbeStatus`.
fn classify_outcome(outcome: RunOutcome, accepted: &[String]) -> ProbeStatus {
    match outcome {
        RunOutcome::MppChallenge { challenge, .. } => {
            let request: solana_mpp::ChargeRequest = match challenge.request.decode() {
                Ok(r) => r,
                Err(e) => {
                    return ProbeStatus::Error {
                        message: format!("Failed to decode MPP challenge: {e}"),
                    };
                }
            };

            let currency = normalize_currency(&request.currency);
            let network = request
                .method_details
                .as_ref()
                .and_then(|v| v.get("network"))
                .and_then(|v| v.as_str())
                .unwrap_or("mainnet")
                .to_string();
            let recipient = request.recipient.unwrap_or_default();

            if !accepted.iter().any(|a| a.eq_ignore_ascii_case(&currency)) {
                return ProbeStatus::WrongCurrency {
                    got: currency,
                    accepted: accepted.to_vec(),
                };
            }

            ProbeStatus::Ok {
                protocol: "mpp".into(),
                currency,
                network,
                recipient,
            }
        }

        RunOutcome::SessionChallenge { .. } => {
            // Session challenges are valid Solana endpoints but use a
            // different payment flow. Mark as ok with protocol "mpp-session".
            ProbeStatus::Ok {
                protocol: "mpp-session".into(),
                currency: "session".into(),
                network: "mainnet".into(),
                recipient: String::new(),
            }
        }

        RunOutcome::X402Challenge { challenge, .. } => {
            let currency = normalize_currency(&challenge.requirements.currency);
            let network = challenge
                .requirements
                .cluster
                .clone()
                .unwrap_or_else(|| "mainnet".into());
            let recipient = challenge.requirements.recipient.clone();

            if !accepted.iter().any(|a| a.eq_ignore_ascii_case(&currency)) {
                return ProbeStatus::WrongCurrency {
                    got: currency,
                    accepted: accepted.to_vec(),
                };
            }

            ProbeStatus::Ok {
                protocol: "x402".into(),
                currency,
                network,
                recipient,
            }
        }

        RunOutcome::PaymentRejected { reason, .. } => ProbeStatus::WrongChain { details: reason },

        RunOutcome::UnknownPaymentRequired { .. } => ProbeStatus::UnknownProtocol,

        RunOutcome::Completed { exit_code, .. } => {
            // Non-402 response — could be 200 (free), 401, 403, 500, etc.
            // For paid endpoints, this is unexpected.
            let status_code = if exit_code == 0 { 200 } else { 500 };
            ProbeStatus::NotPaywalled { status_code }
        }
    }
}

/// Probe all endpoints for a single provider.
pub fn probe_provider(
    provider: &pay_types::registry::ProbeProvider,
    config: &ProbeConfig,
) -> ProviderProbeResult {
    let mut results = Vec::with_capacity(provider.endpoints.len());

    for ep in &provider.endpoints {
        let url = format!(
            "{}/{}",
            provider.service_url.trim_end_matches('/'),
            ep.path.trim_start_matches('/')
        );
        let mut result = probe_endpoint(&ep.method, &url, ep.metered, config);
        result.path = ep.path.clone();
        results.push(result);
    }

    let pass = results.iter().all(|r| r.status.is_ok());

    ProviderProbeResult {
        fqn: provider.fqn.clone(),
        service_url: provider.service_url.clone(),
        endpoints: results,
        pass,
    }
}

/// Probe multiple providers concurrently.
pub fn probe_providers(
    providers: Vec<pay_types::registry::ProbeProvider>,
    config: &ProbeConfig,
) -> ProbeReport {
    let total_endpoints: usize = providers.iter().map(|p| p.endpoints.len()).sum();
    let results = std::sync::Mutex::new(Vec::with_capacity(providers.len()));
    let semaphore = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for provider in &providers {
            // Wait for a concurrency slot.
            loop {
                let current = semaphore.load(Ordering::Relaxed);
                if current < config.concurrency
                    && semaphore
                        .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::Relaxed)
                        .is_ok()
                {
                    break;
                }
                std::thread::yield_now();
            }

            let sem = &semaphore;
            let cfg = &config;
            let res = &results;

            scope.spawn(move || {
                let result = probe_provider(provider, cfg);
                res.lock().unwrap().push(result);
                sem.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    let providers = results.into_inner().unwrap();
    let passed = providers
        .iter()
        .flat_map(|p| &p.endpoints)
        .filter(|e| e.status.is_ok())
        .count();

    ProbeReport {
        providers,
        total_endpoints,
        passed,
        failed: total_endpoints - passed,
    }
}
