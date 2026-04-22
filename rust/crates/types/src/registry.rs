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

/// Provider frontmatter — the YAML block in a provider `.md` file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderFrontmatter {
    /// API name — must match the filename (without `.md`).
    pub name: String,
    /// Human-readable title.
    pub title: String,
    /// One-sentence description (max 120 chars). Powers search.
    pub description: String,
    /// Category. One of: ai_ml, data, compute, maps, search, translation,
    /// productivity, finance, identity, storage, messaging, media, iot,
    /// security, analytics, devtools, cloud, other.
    pub category: String,
    /// Live URL where the API is reachable.
    pub service_url: String,
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

    if !KNOWN_CATEGORIES.contains(&spec.category.as_str()) {
        errs.push(format!(
            "{fqn}: unknown category `{}` (valid: {})",
            spec.category,
            KNOWN_CATEGORIES.join(", ")
        ));
    }
    if spec.description.len() > 120 {
        errs.push(format!(
            "{fqn}: description is {} chars (max 120)",
            spec.description.len()
        ));
    }
    if !spec.service_url.starts_with("https://") && !spec.service_url.starts_with("http://") {
        errs.push(format!(
            "{fqn}: service_url must start with https:// (got `{}`)",
            spec.service_url
        ));
    }
    if spec.endpoints.is_empty() {
        errs.push(format!("{fqn}: must have at least one endpoint"));
    }
    for (i, ep) in spec.endpoints.iter().enumerate() {
        if ep.method.is_empty() {
            errs.push(format!("{fqn}: endpoint[{i}] missing `method`"));
        }
        if ep.path.is_empty() {
            errs.push(format!("{fqn}: endpoint[{i}] missing `path`"));
        }
        if ep.description.is_empty() {
            errs.push(format!("{fqn}: endpoint[{i}] missing `description`"));
        }
    }
    errs
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
