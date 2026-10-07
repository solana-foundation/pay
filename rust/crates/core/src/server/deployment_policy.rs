//! Authenticated, bounded compute policy lookup before any deployment challenge.
//!
//! Host is only a selector. Compute verifies the active deployment incarnation,
//! ownership, published path, and wallet ownership. No caller header chooses the
//! resolver origin or IAM audience. There is deliberately no stale-policy cache.

use std::{collections::BTreeSet, str::FromStr, sync::Arc, time::Duration};

use pay_kit::mpp::server::session::Split;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

const RESPONSE_LIMIT: usize = 16 * 1024;
const DEADLINE: Duration = Duration::from_secs(5);
const MAX_IN_FLIGHT: usize = 64;

/// Errors intentionally omit URLs, credentials, response bodies, and selectors.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("invalid deployment payment policy configuration")]
    Configuration,
    #[error("invalid deployment hostname")]
    Host,
    #[error("invalid deployment request path")]
    Path,
    #[error("deployment payment policy unavailable")]
    Unavailable,
    #[error("invalid deployment payment policy response")]
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyHost {
    /// Only the exact configured apex retains static control-plane behavior.
    Apex,
    Deployment(String),
}

pub use pay_types::deployment_policy::{DeploymentIdentity, PolicyAllocation};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePolicy {
    deployment: DeploymentIdentity,
    version: u64,
    price_micro_usd: u64,
    expires_at: u64,
    schemes: Vec<String>,
    allocations: Vec<PolicyAllocation>,
}

/// Validated immutable quote. Only Solana USDC with six mint decimals is
/// supported: micro-USD equals USDC base units, with no floating-point conversion.
/// The backend factory must enforce that mint/decimal constraint.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResolvedDeploymentPolicy {
    pub deployment: DeploymentIdentity,
    pub version: u64,
    pub price_micro_usd: u64,
    pub expires_at: u64,
    pub allocations: Vec<PolicyAllocation>,
}

impl ResolvedDeploymentPolicy {
    pub fn durable_policy(&self) -> pay_types::deployment_policy::DurableDeploymentPolicy {
        pay_types::deployment_policy::DurableDeploymentPolicy {
            deployment: self.deployment.clone(),
            version: self.version,
            price_micro_usd: self.price_micro_usd,
            allocations: self.allocations.clone(),
        }
    }

    /// Complete ordered snapshot, including incarnation and version, for durable
    /// channel binding. Expiry gates new invocations, not settlement of already
    /// authorized funds; it is deliberately excluded from the durable identity.
    pub fn binding_identity(&self) -> String {
        serde_json::to_string(&self.durable_policy()).expect("policy serialization is infallible")
    }

    /// Seller payout BEFORE delegated operator conversion. Basis points come
    /// from the policy, never from rounded quote amounts.
    pub fn seller_payout(&self) -> (String, Vec<Split>) {
        (
            self.allocations[0].recipient.clone(),
            self.allocations[1..]
                .iter()
                .map(|allocation| Split {
                    recipient: solana_pubkey::Pubkey::from_str(&allocation.recipient)
                        .expect("validated policy recipient"),
                    bps: allocation.basis_points,
                })
                .collect(),
        )
    }
}

#[derive(Clone)]
pub struct DeploymentPolicyResolver {
    endpoint: reqwest::Url,
    audience: String,
    domain: String,
    client: reqwest::Client,
    in_flight: Arc<Semaphore>,
}

impl DeploymentPolicyResolver {
    /// All unset preserves static mode; partial, empty, or non-Unicode
    /// configuration fails startup rather than silently disabling policy mode.
    pub fn from_env() -> Result<Option<Self>, PolicyError> {
        let read = |name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(PolicyError::Configuration),
        };
        Self::from_options(
            read("PAY_DEPLOYMENT_POLICY_URL")?.as_deref(),
            read("PAY_DEPLOYMENT_POLICY_AUDIENCE")?.as_deref(),
            read("PAY_DEPLOYMENT_POLICY_DOMAIN")?.as_deref(),
        )
    }

    pub fn from_options(
        endpoint: Option<&str>,
        audience: Option<&str>,
        domain: Option<&str>,
    ) -> Result<Option<Self>, PolicyError> {
        match (endpoint, audience, domain) {
            (None, None, None) => Ok(None),
            (Some(endpoint), Some(audience), Some(domain)) => {
                Self::new(endpoint, audience, domain).map(Some)
            }
            _ => Err(PolicyError::Configuration),
        }
    }

    /// Endpoint is the configured compute URI + `/__402/payment-policy`;
    /// audience must be that exact HTTPS origin (not a caller-controlled URL).
    pub fn new(endpoint: &str, audience: &str, domain: &str) -> Result<Self, PolicyError> {
        let endpoint = reqwest::Url::parse(endpoint).map_err(|_| PolicyError::Configuration)?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.port().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/__402/payment-policy"
            || audience != endpoint.origin().ascii_serialization()
            || !canonical_dns_name(domain)
            || !domain.contains('.')
        {
            return Err(PolicyError::Configuration);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(DEADLINE)
            .build()
            .map_err(|_| PolicyError::Configuration)?;
        Ok(Self {
            endpoint,
            audience: audience.into(),
            domain: domain.into(),
            client,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        })
    }

    /// Pass the single HTTP Host/authority, never x-forwarded-host. Reject
    /// alternate spellings (ports, trailing dots, case) rather than routing and
    /// authorizing different interpretations of one authority.
    pub fn classify_host(&self, host: &str) -> Result<PolicyHost, PolicyError> {
        if !canonical_dns_name(host) {
            return Err(PolicyError::Host);
        }
        if host == self.domain {
            return Ok(PolicyHost::Apex);
        }
        let id = host
            .strip_suffix(&format!(".{}", self.domain))
            .ok_or(PolicyError::Host)?;
        deployment_owner(id).ok_or(PolicyError::Host)?;
        Ok(PolicyHost::Deployment(host.into()))
    }

    /// Re-resolve every invocation, including credential-bearing requests.
    /// Missing, deleted, expired and failed lookups are errors, never static
    /// fallback. Admission is bounded without an unbounded semaphore wait queue.
    pub async fn resolve(
        &self,
        hostname: &str,
        path_and_query: &str,
    ) -> Result<ResolvedDeploymentPolicy, PolicyError> {
        let PolicyHost::Deployment(_) = self.classify_host(hostname)? else {
            return Err(PolicyError::Host);
        };
        validate_path(path_and_query)?;
        let _permit = self
            .in_flight
            .try_acquire()
            .map_err(|_| PolicyError::Unavailable)?;
        tokio::time::timeout(DEADLINE, async {
            let identity =
                super::proxy::fetch_gcp_metadata_identity_token(&self.client, &self.audience)
                    .await
                    .map_err(|_| PolicyError::Unavailable)?;
            if identity.expires_in_secs <= DEADLINE.as_secs() {
                return Err(PolicyError::Unavailable);
            }
            self.fetch_policy(hostname, path_and_query, &identity.access_token)
                .await
        })
        .await
        .map_err(|_| PolicyError::Unavailable)?
    }

    async fn fetch_policy(
        &self,
        hostname: &str,
        path: &str,
        identity: &str,
    ) -> Result<ResolvedDeploymentPolicy, PolicyError> {
        let response = self
            .client
            .get(self.endpoint.clone())
            .query(&[("hostname", hostname), ("path", path)])
            .bearer_auth(identity)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| PolicyError::Unavailable)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(PolicyError::Unavailable);
        }
        let bytes = read_bounded_response(response, RESPONSE_LIMIT).await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| PolicyError::Unavailable)?
            .as_secs();
        self.validate_response(hostname, &bytes, now)
    }

    fn validate_response(
        &self,
        hostname: &str,
        bytes: &[u8],
        now: u64,
    ) -> Result<ResolvedDeploymentPolicy, PolicyError> {
        if bytes.len() > RESPONSE_LIMIT {
            return Err(PolicyError::InvalidResponse);
        }
        let wire: WirePolicy =
            serde_json::from_slice(bytes).map_err(|_| PolicyError::InvalidResponse)?;
        let invalid = || PolicyError::InvalidResponse;
        let PolicyHost::Deployment(_) = self.classify_host(hostname)? else {
            return Err(invalid());
        };
        let id = hostname
            .strip_suffix(&format!(".{}", self.domain))
            .ok_or_else(invalid)?;
        let owner = deployment_owner(id).ok_or_else(invalid)?;
        let resource: Vec<_> = wire.deployment.resource_name.split('/').collect();
        let created = chrono::DateTime::parse_from_rfc3339(&wire.deployment.created_at)
            .map_err(|_| invalid())?;
        if wire.deployment.hostname != hostname
            || wire.deployment.owner_key != owner
            || resource.len() != 6
            || resource[0] != "projects"
            || resource[2] != "locations"
            || resource[4] != "functions"
            || resource[5] != id
            || !resource_segment(resource[1])
            || !resource_segment(resource[3])
            || created.timestamp() < 0
            || created.timestamp() as u64 > now
            || wire.version == 0
            || wire.price_micro_usd == 0
            || wire.expires_at <= now
            || wire.schemes != ["mpp-session"]
            || wire.allocations.is_empty()
            || wire.allocations.len() > 8
        {
            return Err(invalid());
        }
        let mut recipients = BTreeSet::new();
        let mut total_amount = 0u64;
        let mut total_bps = 0u32;
        let mut split_amount = 0u64;
        for (index, allocation) in wire.allocations.iter().enumerate() {
            let recipient =
                solana_pubkey::Pubkey::from_str(&allocation.recipient).map_err(|_| invalid())?;
            if recipient.to_string() != allocation.recipient
                || recipient == solana_pubkey::Pubkey::default()
                || !recipients.insert(recipient)
                || allocation.amount_micro_usd == 0
                || allocation.basis_points == 0
                || allocation.basis_points > 10_000
            {
                return Err(invalid());
            }
            total_amount = total_amount
                .checked_add(allocation.amount_micro_usd)
                .ok_or_else(invalid)?;
            total_bps += u32::from(allocation.basis_points);
            if index != 0 {
                let expected = (u128::from(wire.price_micro_usd)
                    * u128::from(allocation.basis_points)
                    / 10_000) as u64;
                if allocation.amount_micro_usd != expected {
                    return Err(invalid());
                }
                split_amount = split_amount.checked_add(expected).ok_or_else(invalid)?;
            }
        }
        if total_amount != wire.price_micro_usd
            || total_bps != 10_000
            || wire.price_micro_usd.checked_sub(split_amount)
                != Some(wire.allocations[0].amount_micro_usd)
        {
            return Err(invalid());
        }
        Ok(ResolvedDeploymentPolicy {
            deployment: wire.deployment,
            version: wire.version,
            price_micro_usd: wire.price_micro_usd,
            expires_at: wire.expires_at,
            allocations: wire.allocations,
        })
    }
}

pub(super) async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, PolicyError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(PolicyError::InvalidResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| PolicyError::Unavailable)?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(PolicyError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn canonical_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn deployment_owner(id: &str) -> Option<&str> {
    if id.len() > 63 || !canonical_dns_name(id) || id.contains('.') {
        return None;
    }
    let (owner, name) = id.strip_prefix("gcf-")?.split_once('-')?;
    (owner.len() == 16 && owner.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty())
        .then_some(owner)
}

fn resource_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate_path(path_and_query: &str) -> Result<(), PolicyError> {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    // Match compute's published-path contract; do not decode or normalize before
    // authorization. Reserved deployment routes cannot escape through discovery.
    if path_and_query.len() > 4096
        || path_and_query
            .bytes()
            .any(|b| !b.is_ascii() || b.is_ascii_control() || b == b' ')
        || path_and_query.contains(['#', '\\'])
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.len() > 256
        || path.contains('%')
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
        || path == "/internal"
        || path.starts_with("/internal/")
        || path == "/__402"
        || path.starts_with("/__402/")
    {
        return Err(PolicyError::Path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, response::Response, routing::get};
    use serde_json::{Value, json};

    const HOST: &str = "gcf-0123456789abcdef-sec.compute.example";
    const OTHER_HOST: &str = "gcf-fedcba9876543210-other.compute.example";
    const NOW: u64 = 1_800_000_000;

    fn resolver() -> DeploymentPolicyResolver {
        DeploymentPolicyResolver::new(
            "https://compute.run.app/__402/payment-policy",
            "https://compute.run.app",
            "compute.example",
        )
        .unwrap()
    }

    fn fixture(host: &str, price: u64, wallet_offset: u8) -> Value {
        let id = host.split('.').next().unwrap();
        let owner = deployment_owner(id).unwrap();
        let tax = price * 3 / 10;
        let profit = price / 10;
        json!({
            "deployment": {
                "owner_key": owner,
                "resource_name": format!("projects/project/locations/us-central1/functions/{id}"),
                "created_at": "2026-01-01T00:00:00Z",
                "hostname": host,
            },
            "version": 1,
            "price_micro_usd": price,
            "expires_at": NOW + 100,
            "schemes": ["mpp-session"],
            "allocations": [
                {"recipient": bs58::encode([wallet_offset;32]).into_string(),
                 "amount_micro_usd": price - tax - profit, "basis_points": 6000},
                {"recipient": bs58::encode([wallet_offset+1;32]).into_string(),
                 "amount_micro_usd": tax, "basis_points": 3000},
                {"recipient": bs58::encode([wallet_offset+2;32]).into_string(),
                 "amount_micro_usd": profit, "basis_points": 1000}
            ]
        })
    }

    fn validate(value: &Value) -> Result<ResolvedDeploymentPolicy, PolicyError> {
        resolver().validate_response(HOST, &serde_json::to_vec(value).unwrap(), NOW)
    }

    #[test]
    fn configuration_is_all_or_nothing_and_fixed_origin() {
        assert!(
            DeploymentPolicyResolver::from_options(None, None, None)
                .unwrap()
                .is_none()
        );
        for values in [
            (Some(""), None, None),
            (None, Some("https://compute.run.app"), None),
            (None, None, Some("compute.example")),
            (
                Some("https://compute.run.app/__402/payment-policy"),
                Some(""),
                Some("compute.example"),
            ),
        ] {
            assert!(DeploymentPolicyResolver::from_options(values.0, values.1, values.2).is_err());
        }
        for endpoint in [
            "http://compute.run.app/__402/payment-policy",
            "https://evil.example/__402/payment-policy",
            "https://user@compute.run.app/__402/payment-policy",
            "https://compute.run.app:8443/__402/payment-policy",
            "https://compute.run.app/__402/payment-policy?hostname=evil",
            "https://compute.run.app/__402/payment-policy#fragment",
            "https://compute.run.app/arbitrary",
        ] {
            assert!(
                DeploymentPolicyResolver::new(
                    endpoint,
                    "https://compute.run.app",
                    "compute.example"
                )
                .is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn only_exact_apex_and_canonical_deployments_are_accepted() {
        let resolver = resolver();
        assert_eq!(
            resolver.classify_host("compute.example").unwrap(),
            PolicyHost::Apex
        );
        assert_eq!(
            resolver.classify_host(HOST).unwrap(),
            PolicyHost::Deployment(HOST.into())
        );
        for host in [
            "",
            "compute.example:443",
            "COMPUTE.example",
            "compute.example.",
            "compute.example.evil",
            "evilcompute.example",
            "compute.example@evil",
            "compute.example,evil",
            " compute.example",
            "compute.example\r\nx:evil",
            "gcf-0123456789abcdef-sec.compute.example:443",
            "sec.compute.example",
            "gcf-0123456789abcdef-sec.extra.compute.example",
            "*.compute.example",
            "gcf-0123456789abcdeg-sec.compute.example",
            "[::1]",
            "é.compute.example",
            "gcf-0123456789abcdef-.compute.example",
        ] {
            assert!(resolver.classify_host(host).is_err(), "{host:?}");
        }
        assert!(
            resolver
                .classify_host(&format!("{}.compute.example", "x".repeat(254)))
                .is_err()
        );
    }

    #[test]
    fn path_is_an_exact_bounded_selector_not_a_url_or_reserved_route() {
        for path in [
            "/summary",
            "/summary?ticker=AAPL&redirect=https%3A%2F%2Fexample.com",
            "/openapi.json",
        ] {
            assert!(validate_path(path).is_ok(), "{path}");
        }
        for path in [
            "",
            "https://evil/summary",
            "//evil/summary",
            "/summary#fragment",
            "/summary\\..\\internal",
            "/a/../summary",
            "/./summary",
            "/%73ummary",
            "/summary%2Fextra",
            "/summary?x=\r\nHeader:x",
            "/summary?x=hello world",
            "/internal",
            "/internal/refresh",
            "/__402",
            "/__402/payment-policy",
        ] {
            assert!(validate_path(path).is_err(), "{path:?}");
        }
        assert!(validate_path(&format!("/{}", "x".repeat(256))).is_err());
        assert!(validate_path(&format!("/summary?x={}", "x".repeat(4096))).is_err());
    }

    #[test]
    fn two_deployments_keep_prices_sellers_and_bindings_separate() {
        let resolver = resolver();
        let first = validate(&fixture(HOST, 50_000, 1)).unwrap();
        let other = fixture(OTHER_HOST, 90_003, 5);
        let second = resolver
            .validate_response(OTHER_HOST, &serde_json::to_vec(&other).unwrap(), NOW)
            .unwrap();
        assert_eq!(first.price_micro_usd, 50_000);
        assert_eq!(second.price_micro_usd, 90_003);
        assert_eq!(
            first
                .allocations
                .iter()
                .map(|a| a.amount_micro_usd)
                .collect::<Vec<_>>(),
            [30_000, 15_000, 5_000]
        );
        assert_eq!(
            second
                .allocations
                .iter()
                .map(|a| a.amount_micro_usd)
                .collect::<Vec<_>>(),
            [54_003, 27_000, 9_000]
        );
        let (seller, splits) = first.seller_payout();
        assert_eq!(seller, first.allocations[0].recipient);
        assert_eq!(
            splits.iter().map(|split| split.bps).collect::<Vec<_>>(),
            [3000, 1000]
        );
        assert_ne!(first.seller_payout().0, second.seller_payout().0);
        assert_ne!(first.binding_identity(), second.binding_identity());
        assert!(
            resolver
                .validate_response(HOST, &serde_json::to_vec(&other).unwrap(), NOW)
                .is_err()
        );
        let mut refreshed = first.clone();
        refreshed.expires_at += 10;
        assert_eq!(first.binding_identity(), refreshed.binding_identity());
        refreshed.version += 1;
        assert_ne!(first.binding_identity(), refreshed.binding_identity());
    }

    #[test]
    fn hostile_missing_deleted_expired_and_mismatched_responses_fail_closed() {
        let valid = fixture(HOST, 50_000, 1);
        for (pointer, value) in [
            ("/deployment/hostname", json!(OTHER_HOST)),
            ("/deployment/owner_key", json!("fedcba9876543210")),
            (
                "/deployment/resource_name",
                json!("projects/p/locations/r/functions/other"),
            ),
            (
                "/deployment/resource_name",
                json!("projects/p/locations/r/functions/../other"),
            ),
            ("/deployment/created_at", json!("")),
            ("/deployment/created_at", json!("2099-01-01T00:00:00Z")),
            ("/version", json!(0)),
            ("/version", json!(-1)),
            ("/version", json!("1")),
            ("/price_micro_usd", json!(0)),
            ("/price_micro_usd", json!(0.05)),
            ("/price_micro_usd", json!(u64::MAX)),
            ("/expires_at", json!(NOW)),
            ("/schemes", json!([])),
            ("/schemes", json!(["x402"])),
            ("/schemes", json!(["mpp-session", "mpp-charge"])),
            ("/allocations", json!([])),
            (
                "/allocations/0/recipient",
                json!("11111111111111111111111111111111"),
            ),
            ("/allocations/0/recipient", json!("not-a-pubkey")),
            (
                "/allocations/1/recipient",
                valid["allocations"][0]["recipient"].clone(),
            ),
            ("/allocations/0/amount_micro_usd", json!(30_001)),
            ("/allocations/1/basis_points", json!(0)),
            ("/allocations/1/basis_points", json!(3001)),
            ("/allocations/1/basis_points", json!(10_001)),
        ] {
            let mut hostile = valid.clone();
            *hostile.pointer_mut(pointer).unwrap() = value;
            assert!(validate(&hostile).is_err(), "{pointer}: {hostile}");
        }
        for field in [
            "deployment",
            "version",
            "price_micro_usd",
            "expires_at",
            "schemes",
            "allocations",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(validate(&missing).is_err(), "{field}");
        }
        let mut deleted = valid.clone();
        deleted["deleted"] = json!(true);
        assert!(validate(&deleted).is_err());
        let mut no_weights = valid.clone();
        no_weights["allocations"][1]
            .as_object_mut()
            .unwrap()
            .remove("basis_points");
        assert!(validate(&no_weights).is_err());
        let mut rounded_incorrectly = fixture(HOST, 50_003, 1);
        rounded_incorrectly["allocations"][0]["amount_micro_usd"] = json!(30_002);
        rounded_incorrectly["allocations"][1]["amount_micro_usd"] = json!(15_001);
        assert!(validate(&rounded_incorrectly).is_err());
        for bytes in [
            b"null".as_slice(),
            b"{}",
            b"not json",
            b"{\"version\":1,\"version\":2}",
        ] {
            assert!(resolver().validate_response(HOST, bytes, NOW).is_err());
        }
        assert!(
            resolver()
                .validate_response(HOST, &vec![b' '; RESPONSE_LIMIT + 1], NOW)
                .is_err()
        );
    }

    async fn server(router: Router) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/__402/payment-policy",
            listener.local_addr().unwrap()
        )
        .parse()
        .unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, task)
    }

    #[tokio::test]
    async fn authenticated_fixed_endpoint_encodes_untrusted_query_as_data() {
        let router = Router::new().route(
            "/__402/payment-policy",
            get(
                |headers: axum::http::HeaderMap,
                 axum::extract::Query(query): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    assert_eq!(headers["authorization"], "Bearer test-token");
                    assert!(!headers.contains_key("metadata-flavor"));
                    assert_eq!(query.len(), 2);
                    assert_eq!(query["hostname"], HOST);
                    assert_eq!(query["path"], "/summary?ticker=AAPL&hostname=evil");
                    axum::Json(fixture(HOST, 50_000, 1))
                },
            ),
        );
        let (url, task) = server(router).await;
        let mut resolver = resolver();
        // Test-only local endpoint: production construction requires HTTPS.
        resolver.endpoint = url;
        let result = resolver
            .fetch_policy(HOST, "/summary?ticker=AAPL&hostname=evil", "test-token")
            .await;
        task.abort();
        let _ = task.await;
        assert_eq!(result.unwrap().price_micro_usd, 50_000);
    }

    #[tokio::test]
    async fn redirects_missing_policies_and_provider_errors_never_fallback() {
        for status in [302, 404, 410, 500] {
            let redirected = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let redirect_seen = redirected.clone();
            let router = Router::new()
                .route(
                    "/__402/payment-policy",
                    get(move || async move {
                        Response::builder()
                            .status(status)
                            .header("location", "/redirect-target")
                            .body(Body::empty())
                            .unwrap()
                    }),
                )
                .route(
                    "/redirect-target",
                    get(move || {
                        let redirected = redirected.clone();
                        async move {
                            redirected.store(true, std::sync::atomic::Ordering::SeqCst);
                            axum::Json(fixture(HOST, 50_000, 1))
                        }
                    }),
                );
            let (url, task) = server(router).await;
            let mut resolver = resolver();
            resolver.endpoint = url;
            assert!(
                resolver
                    .fetch_policy(HOST, "/summary", "test-token")
                    .await
                    .is_err()
            );
            assert!(!redirect_seen.load(std::sync::atomic::Ordering::SeqCst));
            task.abort();
            let _ = task.await;
        }
    }

    #[tokio::test]
    async fn chunked_and_declared_oversize_responses_are_bounded() {
        for chunked in [true, false] {
            let router = Router::new().route(
                "/__402/payment-policy",
                get(move || async move {
                    let bytes = bytes::Bytes::from(vec![b' '; RESPONSE_LIMIT + 1]);
                    if chunked {
                        Body::from_stream(futures_util::stream::iter([
                            Ok::<_, std::io::Error>(bytes.slice(..8000)),
                            Ok(bytes.slice(8000..)),
                        ]))
                    } else {
                        Body::from(bytes)
                    }
                }),
            );
            let (url, task) = server(router).await;
            let mut resolver = resolver();
            resolver.endpoint = url;
            assert!(matches!(
                resolver.fetch_policy(HOST, "/summary", "test-token").await,
                Err(PolicyError::InvalidResponse)
            ));
            task.abort();
            let _ = task.await;
        }
    }

    #[tokio::test]
    async fn deadline_and_admission_are_bounded() {
        let mut resolver = resolver();
        let permit = resolver
            .in_flight
            .clone()
            .acquire_many_owned(MAX_IN_FLIGHT as u32)
            .await
            .unwrap();
        assert!(matches!(
            resolver.resolve(HOST, "/summary").await,
            Err(PolicyError::Unavailable)
        ));
        drop(permit);
        // Invalid selectors fail before any attempt to contact metadata.
        assert!(matches!(
            resolver.resolve("compute.example", "/summary").await,
            Err(PolicyError::Host)
        ));
        assert!(matches!(
            resolver.resolve(HOST, "/__402/session").await,
            Err(PolicyError::Path)
        ));
        let router = Router::new().route(
            "/__402/payment-policy",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                axum::Json(fixture(HOST, 50_000, 1))
            }),
        );
        let (url, task) = server(router).await;
        resolver.endpoint = url;
        resolver.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(matches!(
            resolver.fetch_policy(HOST, "/summary", "test-token").await,
            Err(PolicyError::Unavailable)
        ));
        task.abort();
        let _ = task.await;
    }
}
