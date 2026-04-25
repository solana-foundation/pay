//! Types for the pay-skills registry — provider, affiliate, and aggregator specs.
//!
//! These represent the YAML frontmatter in `.md` files submitted to the
//! pay-skills registry. Used by:
//! - `pay skills build` (validation + index generation)
//! - `pay skills create` MCP tool (schema generation + validation)

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const KNOWN_CATEGORIES: &[&str] = &[
    "ai_ml",
    "analytics",
    "cloud",
    "compute",
    "data",
    "devtools",
    "finance",
    "identity",
    "iot",
    "maps",
    "media",
    "messaging",
    "other",
    "productivity",
    "search",
    "security",
    "storage",
    "translation",
];

pub const AFFILIATE_TYPES: &[&str] = &["agent", "cli", "platform"];

/// Common metadata shared across all service representations (frontmatter,
/// index entries, runtime catalog, search results, detail views).
///
/// Embed with `#[serde(flatten)]` to avoid repeating these fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServiceMeta {
    /// Human-readable title.
    #[serde(default)]
    pub title: String,
    /// One-sentence description (max 255 chars). Powers search.
    #[serde(default)]
    pub description: String,
    /// Hint for LLMs: when should this skill be used? (e.g. "looking for data analytics, market research")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_case: Option<String>,
    /// Category. One of: ai_ml, data, compute, maps, search, translation,
    /// productivity, finance, identity, storage, messaging, media, iot,
    /// security, analytics, devtools, cloud, other.
    #[serde(default)]
    pub category: String,
    /// Live URL where the API is reachable (production).
    #[serde(default)]
    pub service_url: String,
    /// Optional sandbox/testnet URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_service_url: Option<String>,
}

/// Provider frontmatter — the YAML block in a provider `.md` file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderFrontmatter {
    /// API name — must match the filename (without `.md`).
    pub name: String,
    #[serde(flatten)]
    pub meta: ServiceMeta,
    /// API version (e.g. "v1", "v2").
    #[serde(default)]
    pub version: String,
    /// Pointer to full OpenAPI spec (not auto-expanded).
    #[serde(default)]
    pub openapi_url: Option<String>,
    /// Opt-in to affiliate referrals.
    #[serde(default)]
    pub affiliate_policy: Option<AffiliatePolicy>,
    /// API endpoints — at least one required.
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
}

/// Affiliate referral policy on a provider.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AffiliatePolicy {
    pub enabled: bool,
    #[serde(default)]
    pub default_percent: Option<f64>,
    /// Restrict to specific affiliate slugs. Omit to accept all.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
}

/// A single API endpoint in the registry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EndpointSpec {
    /// HTTP method (GET, POST, PUT, PATCH, DELETE).
    pub method: String,
    /// URL path (e.g. "v1/search").
    pub path: String,
    /// What this endpoint does (max 120 chars, start with a verb).
    pub description: String,
    /// Resource group for organizing endpoints (e.g. "jobs", "datasets").
    #[serde(default)]
    pub resource: Option<String>,
    /// Pricing config. Omit for free endpoints.
    #[serde(default)]
    pub pricing: Option<serde_json::Value>,
}

/// Affiliate frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AffiliateFrontmatter {
    pub name: String,
    pub title: String,
    /// One of: agent, cli, platform.
    #[serde(rename = "type")]
    pub affiliate_type: String,
    /// Solana wallet address (base58 pubkey).
    pub account: String,
    /// Contact email or URL — required because money is involved.
    pub contact: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Solana network: mainnet or devnet.
    #[serde(default = "default_network")]
    pub network: String,
}

fn default_network() -> String {
    "mainnet".to_string()
}

/// Aggregator frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AggregatorFrontmatter {
    pub name: String,
    pub title: String,
    pub url: String,
    pub contact: String,
    #[serde(default)]
    pub description: Option<String>,
    /// URL to their skills.json equivalent (metadata only).
    #[serde(default)]
    pub catalog_url: Option<String>,
}

// ── Probe types ───────────────────────────────────────────────────────────

/// An endpoint to probe: method, path, and whether it's metered.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProbeEndpoint {
    pub method: String,
    pub path: String,
    pub metered: bool,
}

/// A provider with its service URL and endpoints, ready for probing.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProbeProvider {
    pub fqn: String,
    pub service_url: String,
    pub endpoints: Vec<ProbeEndpoint>,
}

// ── Schema ─────────────────────────────────────────────────────────────────

/// Generate JSON Schema for `ProviderFrontmatter` as a pretty-printed string.
pub fn provider_json_schema() -> String {
    let schema = schemars::schema_for!(ProviderFrontmatter);
    serde_json::to_string_pretty(&schema).unwrap_or_default()
}

// ── Validation ─────────────────────────────────────────────────────────────

const BASE58_ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn valid_base58(s: &str) -> bool {
    (32..=44).contains(&s.len()) && s.bytes().all(|b| BASE58_ALPHABET.contains(&b))
}

pub fn validate_provider(spec: &ProviderFrontmatter, fqn: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let m = &spec.meta;

    // ── Category ──
    if !KNOWN_CATEGORIES.contains(&m.category.as_str()) {
        errs.push(format!(
            "{fqn}: unknown category `{}`\n  valid categories: {}\n",
            m.category,
            KNOWN_CATEGORIES.join(", ")
        ));
    }

    // ── Description (min 64, max 255) ──
    if m.description.len() < 64 {
        errs.push(format!(
            "{fqn}: description too short ({} chars, min 64)\n  got: \"{}\"\n",
            m.description.len(),
            m.description
        ));
    }
    if m.description.len() > 255 {
        errs.push(format!(
            "{fqn}: description too long ({} chars, max 255)\n  got: \"{}...\"\n",
            m.description.len(),
            &m.description[..80]
        ));
    }

    // ── use_case (required, min 32) ──
    match &m.use_case {
        None => {
            errs.push(format!(
                "{fqn}: missing required field `use_case`\n  \
                 add a use_case field (min 32 chars) describing when this API should be used\n"
            ));
        }
        Some(uc) if uc.len() < 32 => {
            errs.push(format!(
                "{fqn}: use_case too short ({} chars, min 32)\n  got: \"{uc}\"\n",
                uc.len()
            ));
        }
        _ => {}
    }

    // ── service_url (HTTPS only, domain names only) ──
    if m.service_url.is_empty() {
        errs.push(format!("{fqn}: missing required field `service_url`\n"));
    } else if !m.service_url.starts_with("https://") {
        errs.push(format!(
            "{fqn}: service_url must start with https://\n  got: `{}`\n",
            m.service_url
        ));
    } else if url_has_ip_address(&m.service_url) {
        errs.push(format!(
            "{fqn}: service_url must use a domain name, not an IP address\n  got: `{}`\n",
            m.service_url
        ));
    }

    // ── Endpoints ──
    if spec.endpoints.is_empty() {
        errs.push(format!(
            "{fqn}: no endpoints defined\n  add at least one endpoint with method, path, and description\n"
        ));
    }
    for (i, ep) in spec.endpoints.iter().enumerate() {
        let label = if ep.path.is_empty() {
            format!("endpoint[{i}]")
        } else {
            format!("endpoint[{i}] {} {}", ep.method, ep.path)
        };

        if ep.method.is_empty() {
            errs.push(format!(
                "{fqn}: {label} — missing `method` (GET, POST, PUT, PATCH, DELETE)\n"
            ));
        }
        if ep.path.is_empty() {
            errs.push(format!("{fqn}: endpoint[{i}] — missing `path`\n"));
        }
        if ep.description.len() < 32 {
            errs.push(format!(
                "{fqn}: {label} — description too short ({} chars, min 32)\n  got: \"{}\"\n",
                ep.description.len(),
                ep.description
            ));
        }
        if ep.description.len() > 255 {
            errs.push(format!(
                "{fqn}: {label} — description too long ({} chars, max 255)\n  got: \"{}...\"\n",
                ep.description.len(),
                &ep.description[..80]
            ));
        }
    }
    errs
}

/// Check if a URL uses an IP address instead of a domain name.
fn url_has_ip_address(url: &str) -> bool {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host_port = after_scheme.split('/').next().unwrap_or("");

    // Bracketed IPv6: [::1] or [::1]:8080
    if host_port.starts_with('[') {
        return true;
    }

    // IPv4 or bare IPv6: strip port suffix
    let host = host_port.split(':').next().unwrap_or("");
    host.parse::<std::net::IpAddr>().is_ok()
}

pub fn validate_affiliate(spec: &AffiliateFrontmatter, name: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if !valid_base58(&spec.account) {
        errs.push(format!(
            "affiliate/{name}: invalid account `{}` (must be base58 Solana pubkey, 32-44 chars)",
            spec.account
        ));
    }
    if !AFFILIATE_TYPES.contains(&spec.affiliate_type.as_str()) {
        errs.push(format!(
            "affiliate/{name}: unknown type `{}` (valid: {})",
            spec.affiliate_type,
            AFFILIATE_TYPES.join(", ")
        ));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> ProviderFrontmatter {
        ProviderFrontmatter {
            name: "test-api".into(),
            meta: ServiceMeta {
                title: "Test API".into(),
                description: "A test API for validating things — long enough to pass the 64-char minimum requirement.".into(),
                use_case: Some("testing validation logic, verifying CI checks work correctly".into()),
                category: "data".into(),
                service_url: "https://api.example.com".into(),
                sandbox_service_url: None,
            },
            version: "v1".into(),
            openapi_url: None,
            affiliate_policy: None,
            endpoints: vec![EndpointSpec {
                method: "POST".into(),
                path: "v1/search".into(),
                description: "Search items by keyword with filtering and pagination support".into(),
                resource: None,
                pricing: None,
            }],
        }
    }

    #[test]
    fn valid_spec_passes() {
        let errs = validate_provider(&valid_spec(), "test/test-api");
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
    }

    #[test]
    fn description_too_short() {
        let mut spec = valid_spec();
        spec.meta.description = "Too short".into();
        let errs = validate_provider(&spec, "t");
        assert!(errs.iter().any(|e| e.contains("min 64")));
    }

    #[test]
    fn description_too_long() {
        let mut spec = valid_spec();
        spec.meta.description = "x".repeat(256);
        let errs = validate_provider(&spec, "t");
        assert!(errs.iter().any(|e| e.contains("max 255")));
    }

    #[test]
    fn use_case_missing() {
        let mut spec = valid_spec();
        spec.meta.use_case = None;
        let errs = validate_provider(&spec, "t");
        assert!(errs.iter().any(|e| e.contains("use_case")));
    }

    #[test]
    fn use_case_too_short() {
        let mut spec = valid_spec();
        spec.meta.use_case = Some("too short".into());
        let errs = validate_provider(&spec, "t");
        assert!(
            errs.iter()
                .any(|e| e.contains("use_case") && e.contains("min 32"))
        );
    }

    #[test]
    fn service_url_http_rejected() {
        let mut spec = valid_spec();
        spec.meta.service_url = "http://api.example.com".into();
        let errs = validate_provider(&spec, "t");
        assert!(errs.iter().any(|e| e.contains("https://")));
    }

    #[test]
    fn service_url_ip_rejected() {
        let mut spec = valid_spec();
        spec.meta.service_url = "https://192.168.1.1/api".into();
        let errs = validate_provider(&spec, "t");
        assert!(errs.iter().any(|e| e.contains("domain name")));
    }

    #[test]
    fn service_url_ipv6_rejected() {
        let mut spec = valid_spec();
        spec.meta.service_url = "https://[::1]/api".into();
        let errs = validate_provider(&spec, "t");
        // [::1] won't parse as IpAddr due to brackets, but it's not a valid domain either
        // The https:// check passes but the IP check handles bare IPs
        assert!(!errs.is_empty());
    }

    #[test]
    fn service_url_domain_accepted() {
        let spec = valid_spec();
        let errs = validate_provider(&spec, "t");
        assert!(!errs.iter().any(|e| e.contains("service_url")));
    }

    #[test]
    fn endpoint_description_too_short() {
        let mut spec = valid_spec();
        spec.endpoints[0].description = "Short".into();
        let errs = validate_provider(&spec, "t");
        assert!(
            errs.iter()
                .any(|e| e.contains("endpoint[0]") && e.contains("min 32"))
        );
    }

    #[test]
    fn endpoint_description_too_long() {
        let mut spec = valid_spec();
        spec.endpoints[0].description = "x".repeat(256);
        let errs = validate_provider(&spec, "t");
        assert!(
            errs.iter()
                .any(|e| e.contains("endpoint[0]") && e.contains("max 255"))
        );
    }

    #[test]
    fn ip_detection() {
        assert!(url_has_ip_address("https://192.168.1.1/api"));
        assert!(url_has_ip_address("https://10.0.0.1:8080/api"));
        assert!(url_has_ip_address("https://127.0.0.1"));
        assert!(!url_has_ip_address("https://api.example.com"));
        assert!(!url_has_ip_address("https://x402.quicknode.com/rpc"));
    }
}
