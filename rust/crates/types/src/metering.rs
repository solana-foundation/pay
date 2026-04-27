use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// =============================================================================
// Provider & API
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderSpec {
    pub provider: String,
    pub generated_at: String,
    pub apis: Vec<ApiSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiSpec {
    pub name: String,
    /// Subdomain for this API: `{subdomain}.agents.solana.com`
    pub subdomain: String,
    pub title: String,
    pub description: String,
    pub category: ApiCategory,
    pub version: String,
    /// Environment variables to set when the spec is loaded.
    /// Static values are set directly; `${VAR}` references the runtime environment.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    /// Routing — how requests are handled (proxied upstream or responded to directly).
    pub routing: RoutingConfig,
    /// How volume tiers are tracked: pooled (shared counter) or per_agent (per wallet).
    #[serde(default)]
    pub accounting: AccountingMode,
    pub endpoints: Vec<Endpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_tier: Option<FreeTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quotas: Option<QuotaSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Operator config — how this proxy instance runs (signer, recipient, currency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<OperatorConfig>,
    /// Named recipient aliases for use in payment splits.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub recipients: std::collections::HashMap<String, RecipientAlias>,
    /// Session channel parameters. When set, the middleware issues a 402
    /// with `intent="session"` and accepts signed vouchers instead of
    /// per-request charges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSpec>,
}

/// How a request is handled after payment verification.
///
/// ```yaml
/// # Proxy — forward to an upstream API
/// routing:
///   type: proxy
///   url: https://generativelanguage.googleapis.com/
///   auth:
///     method: query_param
///     key: "key"
///     value_from_env: GOOGLE_API_KEY
///
/// # Respond — return 200 with verified signature (no upstream)
/// routing:
///   type: respond
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoutingConfig {
    /// Forward request to an upstream API.
    Proxy {
        /// Upstream base URL (e.g. `https://generativelanguage.googleapis.com/`).
        url: String,
        /// Optional path segments prepended to the request path.
        /// Each segment's value is resolved from an environment variable.
        ///
        /// ```yaml
        /// routing:
        ///   type: proxy
        ///   url: https://translation.googleapis.com
        ///   path_rewrites:
        ///     - prefix: "v3/projects/{projectId}"
        ///       env: GOOGLE_PROJECT_ID
        /// ```
        ///
        /// Given `GOOGLE_PROJECT_ID=my-proj`, a request to
        /// `/v3/projects/any-value/locations/global:translateText` is rewritten to
        /// `https://translation.googleapis.com/v3/projects/my-proj/locations/global:translateText`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path_rewrites: Vec<PathRewrite>,
        /// How the proxy injects upstream API credentials after payment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<AuthConfig>,
    },
    /// Respond directly — return 200 with the verified payment signature,
    /// or 401 if the request was denied. No upstream call.
    Respond {},
}

/// A path rewrite rule — matches a prefix pattern in the request path and
/// substitutes `{placeholder}` segments with an env var value.
///
/// Example: prefix `v3/projects/{projectId}` with env `GCP_PROJECT=gateway-402`
/// rewrites `/v3/projects/any-value/locations/global:translateText`
/// to      `/v3/projects/gateway-402/locations/global:translateText`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PathRewrite {
    /// Path prefix template with a `{placeholder}` (e.g. `v3/projects/{projectId}`).
    pub prefix: String,
    /// Environment variable whose value replaces the placeholder.
    pub env: String,
}

impl RoutingConfig {
    /// Build the full upstream URL for a given request path+query.
    /// Returns `None` for the `Respond` variant.
    pub fn upstream_url(&self, path_and_query: &str) -> Option<String> {
        match self {
            Self::Proxy {
                url, path_rewrites, ..
            } => {
                let base = url.trim_end_matches('/');
                if path_rewrites.is_empty() {
                    return Some(format!("{base}{path_and_query}"));
                }
                let (path, query) = match path_and_query.find('?') {
                    Some(i) => (&path_and_query[..i], &path_and_query[i..]),
                    None => (path_and_query, ""),
                };
                let rewritten = rewrite_path(path, path_rewrites);
                Some(format!("{base}{rewritten}{query}"))
            }
            Self::Respond {} => None,
        }
    }

    /// The base URL for display purposes.
    /// Returns `"respond"` for the `Respond` variant.
    pub fn display_url(&self) -> &str {
        match self {
            Self::Proxy { url, .. } => url,
            Self::Respond {} => "respond",
        }
    }

    /// The auth config, if this is a proxy route.
    pub fn auth(&self) -> Option<&AuthConfig> {
        match self {
            Self::Proxy { auth, .. } => auth.as_ref(),
            Self::Respond {} => None,
        }
    }

    /// Returns `true` if this is a proxy route.
    pub fn is_proxy(&self) -> bool {
        matches!(self, Self::Proxy { .. })
    }

    /// Returns `true` if this is a respond route.
    pub fn is_respond(&self) -> bool {
        matches!(self, Self::Respond { .. })
    }
}

/// Apply path rewrite rules to an incoming path.
///
/// Each rule's prefix is split into segments. Literal segments must match
/// exactly; `{placeholder}` segments match any value and are replaced with
/// the env var. The prefix is matched at ANY position in the path — not
/// just the start — so `projects/{projectId}` matches both
/// `/projects/foo/bar` and `/bigquery/v2/projects/foo/bar`.
fn rewrite_path(path: &str, rewrites: &[PathRewrite]) -> String {
    let path_trimmed = path.strip_prefix('/').unwrap_or(path);
    let mut segments: Vec<String> = path_trimmed.split('/').map(String::from).collect();

    for rewrite in rewrites {
        let value = std::env::var(&rewrite.env).unwrap_or_default();
        let prefix_parts: Vec<&str> = rewrite.prefix.split('/').collect();

        if prefix_parts.len() > segments.len() {
            continue;
        }

        // Scan for the prefix at every possible offset in the path.
        let max_start = segments.len() - prefix_parts.len();
        for start in 0..=max_start {
            let mut matched = true;
            for (j, pat) in prefix_parts.iter().enumerate() {
                if pat.starts_with('{') && pat.ends_with('}') {
                    continue;
                }
                if *pat != segments[start + j] {
                    matched = false;
                    break;
                }
            }
            if matched {
                for (j, pat) in prefix_parts.iter().enumerate() {
                    if pat.starts_with('{') && pat.ends_with('}') {
                        segments[start + j] = value.clone();
                    }
                }
                break; // Apply the first match only.
            }
        }
    }

    format!("/{}", segments.join("/"))
}

// =============================================================================
// Operator config
// =============================================================================

/// How the proxy injects upstream API credentials after payment succeeds.
/// All secret values are resolved from environment variables at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthConfig {
    /// Inject as a query parameter (e.g. `?key=API_KEY`).
    QueryParam {
        /// Query parameter name (e.g. "key").
        key: String,
        /// Environment variable holding the value.
        value_from_env: String,
    },
    /// Inject as an HTTP header (e.g. `Authorization: Bearer TOKEN`).
    Header {
        /// Header name (e.g. "Authorization").
        key: String,
        /// Optional prefix (e.g. "Bearer ").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Environment variable holding the value.
        value_from_env: String,
    },
    /// OAuth2 — fetch access token and inject as `Authorization: Bearer`.
    Oauth2 {
        /// Token endpoint URL (e.g. `https://oauth2.googleapis.com/token`).
        /// Special value `"gcp_metadata"` uses the GCP metadata server.
        token_url: String,
        /// OAuth2 scopes to request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
        /// Env var for client_id (for client_credentials grant).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id_from_env: Option<String>,
        /// Env var for client_secret (for client_credentials grant).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret_from_env: Option<String>,
        /// Extra headers to inject, each value resolved from an env var.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        headers: std::collections::HashMap<String, EnvRef>,
    },
}

/// A value resolved from an environment variable.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvRef {
    pub from_env: String,
}

/// Operator-level configuration for a proxy instance.
/// Controls signing, payment recipient, and currency.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    /// Signing backend for fee sponsorship and settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<SignerConfig>,
    /// Payment recipient wallet address (base58).
    /// Overrides --recipient CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Payment currencies grouped by unit, e.g. `{ usd: [USDC, USDT, CASH] }`.
    /// When present, charge endpoints advertise one challenge per listed currency.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub currencies: std::collections::BTreeMap<String, Vec<String>>,
    /// Solana RPC URL. Overrides --rpc-url CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// Solana network (mainnet, devnet, localnet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// Whether the operator sponsors transaction fees.
    #[serde(default)]
    pub fee_payer: bool,
}

/// Signing backend configuration.
///
/// Tells the server how to load the wallet that co-signs as `fee_payer`.
/// When `operator.fee_payer: true` is set in the YAML, exactly one of
/// these variants must be configured (or the server must be started in
/// `--sandbox` mode, which auto-loads a localnet ephemeral).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "kebab-case")]
pub enum SignerConfig {
    /// GCP Cloud KMS — Ed25519 HSM key. Private key never leaves the HSM.
    /// Recommended for production. Requires the `gcp_kms` build feature.
    GcpKms {
        /// Full KMS key version resource name.
        key_name: String,
        /// Solana public key (base58) derived from the KMS key.
        pubkey: String,
    },
    /// Named account from `~/.config/pay/accounts.yml`. Loaded via the
    /// regular keystore path — for `apple-keychain`/`gnome-keyring`/
    /// `windows-hello`/`1password` entries this triggers the OS auth
    /// prompt **once at server startup** (not per-payment). For
    /// `ephemeral` entries no prompt fires.
    Account {
        /// Account name as it appears under `accounts:` in accounts.yml.
        name: String,
    },
    /// Inline keypair file on disk (Solana CLI's standard JSON format
    /// — a 64-byte u8 array). Bypasses the keystore entirely. Useful
    /// for dev/CI machines where the wallet doesn't need OS-level
    /// protection.
    File {
        /// Path to the keypair JSON file. `~` is expanded.
        path: String,
    },
}

// =============================================================================
// Recipients & Splits
// =============================================================================

/// Session channel parameters — emitted by the server when the API
/// is configured for MPP session payments (off-chain vouchers).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionSpec {
    /// Default channel cap offered to clients (USDC, human-readable).
    /// Clients may request a lower cap; the server will not exceed this.
    pub cap_usdc: f64,
    /// Minimum voucher increment (base units = µUSDC).
    /// Prevents spam vouchers smaller than one API call's cost.
    #[serde(default)]
    pub min_voucher_delta: u64,
    /// Session modes this server accepts.
    ///
    /// Allowed values: `"push"` (Fiber channel, client-funded) and/or
    /// `"pull"` (SPL token delegation, operator fee-pays the approve tx).
    ///
    /// Defaults to `["push"]` when omitted.
    ///
    /// Example YAML:
    /// ```yaml
    /// session:
    ///   cap_usdc: 10.0
    ///   modes: [push, pull]
    /// ```
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    /// Fiber channel-open batch flush interval in milliseconds.
    ///
    /// Defaults to `400` when omitted.
    #[serde(default = "default_session_batch_open_interval_ms")]
    pub batch_open_interval_ms: u64,
}

fn default_session_batch_open_interval_ms() -> u64 {
    400
}

/// Named recipient alias declared at the API spec level.
/// Used in split rules to reference wallet accounts by name.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecipientAlias {
    /// Wallet account — literal base58 pubkey or `${VAR}` for runtime resolution.
    /// Runtime variables are resolved from request query parameters.
    pub account: String,
    /// Human-readable label (shown in debugger UI and receipts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A single split directive — either a fixed USD amount or a percentage of the total.
///
/// Exactly one of `amount` or `percent` must be set.
///
/// **Semantics:**
/// - `amount`: fixed USD value deducted from the charge.
/// - `percent`: percentage of the **original total charge** (not the remaining balance).
///
/// This means reordering splits does not change anyone's payout — both fixed and
/// percentage splits reference the same original total, following the standard
/// payment processing model (Stripe, Adyen).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SplitRule {
    /// Reference to a named recipient alias (key in `ApiSpec.recipients`).
    pub recipient: String,
    /// Fixed USD amount to send to this recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// Percentage of the original total charge to send to this recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    /// Human-readable memo (shown in debugger + on-chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
}

// =============================================================================
// API Categories
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiCategory {
    AiMl,
    Search,
    Maps,
    Data,
    Compute,
    Productivity,
}

// =============================================================================
// Endpoints & Metering
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Endpoint {
    pub method: HttpMethod,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Resource group (e.g. "models", "tunedModels", "files").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Per-endpoint routing override. If set, takes precedence over the
    /// top-level `routing` config for this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingConfig>,
    /// Billing config for this endpoint. None = free / not billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metering: Option<Metering>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Metering {
    /// Direct pricing dimensions (when there's a single pricing model).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<MeterDimension>,
    /// Variant-specific pricing (e.g. different models have different costs).
    /// The proxy matches the variant using a path/body parameter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<MeterVariant>,
    /// Maps Platform SKU tiers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sku_tiers: Vec<SkuTier>,
    /// Payment splits — how the charge is distributed to named recipients.
    /// Applied to all tiers unless overridden at the tier level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub splits: Vec<SplitRule>,
}

/// A variant represents a pricing path selected by a request parameter.
/// The proxy extracts `param` from the URL path or request body and
/// matches it against `value`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MeterVariant {
    /// The parameter to match against (e.g. "model", "voice").
    pub param: String,
    /// The value to match (e.g. "gemini-2.5-pro", "chirp-3-hd").
    pub value: String,
    pub dimensions: Vec<MeterDimension>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MeterDimension {
    pub direction: MeterDirection,
    pub unit: BillingUnit,
    /// Price is quoted per `scale` units. e.g. scale=1000000 → "per 1M tokens".
    pub scale: u64,
    /// Billing period when the unit is time-derived (e.g. GiB billed per_month).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<BillingPeriod>,
    /// Volume tiers. Evaluated in order — first matching tier applies.
    pub tiers: Vec<PriceTier>,
}

/// A volume-based price tier. `up_to: None` means "and above" (final tier).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PriceTier {
    /// Volume ceiling for this tier. None = unlimited (catch-all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to: Option<u64>,
    pub price_usd: f64,
    /// Machine-readable condition that must hold for this tier to apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<MeterCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Per-tier split overrides. If present, these replace the metering-level splits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub splits: Vec<SplitRule>,
}

/// A condition the proxy can evaluate against request properties.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "field")]
pub enum MeterCondition {
    /// Total input token count (from request body or content-length estimation).
    #[serde(rename = "input_tokens")]
    InputTokens { op: CompareOp, value: u64 },
    /// Total input character count.
    #[serde(rename = "input_characters")]
    InputCharacters { op: CompareOp, value: u64 },
    /// Context window size (prompt + history tokens).
    #[serde(rename = "context_length")]
    ContextLength { op: CompareOp, value: u64 },
    /// Request body size in bytes.
    #[serde(rename = "body_size")]
    BodySize { op: CompareOp, value: u64 },
    /// Audio/video duration in seconds.
    #[serde(rename = "duration_seconds")]
    DurationSeconds { op: CompareOp, value: u64 },
    /// Number of items in a batch request.
    #[serde(rename = "batch_size")]
    BatchSize { op: CompareOp, value: u64 },
    /// Image resolution (width * height pixels).
    #[serde(rename = "image_pixels")]
    ImagePixels { op: CompareOp, value: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum CompareOp {
    #[serde(rename = "<=")]
    Lte,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = ">=")]
    Gte,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "==")]
    Eq,
}

// =============================================================================
// Free tier & Quotas
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FreeTier {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<BillingUnit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<BillingPeriod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuotaSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_100_seconds: Option<u64>,
    /// Per-user rate limit (requests per second per wallet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_user_requests_per_second: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_units_per_day: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Maps Platform SKU tier.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkuTier {
    pub sku: String,
    pub level: SkuLevel,
}

// =============================================================================
// Accounting
// =============================================================================

/// How volume tier counters are scoped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountingMode {
    /// All agents share one counter. The Foundation's upstream quota is consumed collectively.
    #[default]
    Pooled,
    /// Each wallet address has its own counter. Volume discounts are per-agent.
    PerAgent,
}

// =============================================================================
// Enums
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MeterDirection {
    Input,
    Output,
    Usage,
    Storage,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingUnit {
    Tokens,
    Characters,
    Requests,
    Minutes,
    Hours,
    Seconds,
    Pages,
    Documents,
    Invocations,
    Bytes,
    #[serde(rename = "GiB")]
    Gibibytes,
    #[serde(rename = "TiB")]
    Tebibytes,
    #[serde(rename = "vCPU")]
    Vcpu,
    #[serde(rename = "quota_units")]
    QuotaUnits,
    Instances,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingPeriod {
    #[serde(rename = "per_second")]
    PerSecond,
    #[serde(rename = "per_hour")]
    PerHour,
    #[serde(rename = "per_day")]
    PerDay,
    #[serde(rename = "per_month")]
    PerMonth,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkuLevel {
    Essentials,
    Pro,
    Enterprise,
}

// =============================================================================
// Payment protocols (x402 / MPP)
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaymentProtocol {
    X402,
    Mpp,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub description: String,
    pub endpoint_url: String,
    pub category: String,
    pub protocol: PaymentProtocol,
    pub facilitator: String,
}

// =============================================================================
// Validation
// =============================================================================

/// Validate an API spec's metering and split configuration.
///
/// Catches configuration errors that would only surface at runtime as
/// `SplitsExceedTotal` or `UnknownRecipient` errors. Run this during
/// `pay skills provider sync` and `pay skills build` to fail fast.
pub fn validate_api_spec(spec: &ApiSpec) -> Vec<String> {
    let mut errs = Vec::new();

    for ep in &spec.endpoints {
        let Some(metering) = &ep.metering else {
            continue;
        };
        let path = &ep.path;

        validate_splits_have_pricing(metering, path, &mut errs);
        validate_splits_within_price(metering, path, &mut errs);
        validate_split_recipients(metering, &spec.recipients, path, &mut errs);
        validate_split_rules(metering, path, &mut errs);
        validate_tier_splits(metering, &spec.recipients, path, &mut errs);
        validate_price_precision(metering, path, &mut errs);
    }

    errs
}

/// Splits require explicit pricing dimensions — `sku_tiers` alone resolves
/// to `price_usd: 0.0`, which always triggers `SplitsExceedTotal`.
fn validate_splits_have_pricing(metering: &Metering, path: &str, errs: &mut Vec<String>) {
    if !metering.splits.is_empty() && metering.dimensions.is_empty() && metering.variants.is_empty()
    {
        errs.push(format!(
            "endpoint `{path}`: has splits but no pricing dimensions — \
             sku_tiers alone resolve to $0.00, causing 'Splits consume the entire amount' at runtime"
        ));
    }
}

/// The sum of all splits must be strictly less than the minimum non-zero
/// per-unit price across all tiers (i.e. `price_usd / scale`).
fn validate_splits_within_price(metering: &Metering, path: &str, errs: &mut Vec<String>) {
    if metering.splits.is_empty() {
        return;
    }

    let min_price = min_nonzero_per_unit_price(&metering.dimensions);
    if min_price == 0.0 {
        return; // No priced tiers — covered by validate_splits_have_pricing.
    }

    let fixed_total: f64 = metering.splits.iter().filter_map(|s| s.amount).sum();
    let percent_total: f64 = metering
        .splits
        .iter()
        .filter_map(|s| s.percent)
        .sum::<f64>()
        / 100.0
        * min_price;
    let splits_total = fixed_total + percent_total;

    if splits_total >= min_price {
        errs.push(format!(
            "endpoint `{path}`: splits total (${splits_total:.6}) >= \
             minimum per-unit price (${min_price:.6}) — primary recipient would receive nothing"
        ));
    }
}

/// Every split recipient alias must exist in the spec-level `recipients` map.
fn validate_split_recipients(
    metering: &Metering,
    recipients: &std::collections::HashMap<String, RecipientAlias>,
    path: &str,
    errs: &mut Vec<String>,
) {
    for split in &metering.splits {
        if !recipients.contains_key(&split.recipient) {
            errs.push(format!(
                "endpoint `{path}`: split references unknown recipient `{}`",
                split.recipient
            ));
        }
    }
}

/// Each split must have exactly one of `amount` or `percent`.
fn validate_split_rules(metering: &Metering, path: &str, errs: &mut Vec<String>) {
    for split in &metering.splits {
        match (split.amount, split.percent) {
            (Some(_), Some(_)) => errs.push(format!(
                "endpoint `{path}`: split for `{}` has both amount and percent — pick one",
                split.recipient
            )),
            (None, None) => errs.push(format!(
                "endpoint `{path}`: split for `{}` has neither amount nor percent",
                split.recipient
            )),
            _ => {}
        }
    }
}

/// Validate per-tier split overrides against their tier's per-unit price.
fn validate_tier_splits(
    metering: &Metering,
    recipients: &std::collections::HashMap<String, RecipientAlias>,
    path: &str,
    errs: &mut Vec<String>,
) {
    for dim in &metering.dimensions {
        let scale = dim.scale.max(1) as f64;
        for tier in &dim.tiers {
            if tier.splits.is_empty() {
                continue;
            }

            let per_unit = tier.price_usd / scale;

            // Recipient existence check.
            for split in &tier.splits {
                if !recipients.contains_key(&split.recipient) {
                    errs.push(format!(
                        "endpoint `{path}` (tier ${per_unit:.6}/unit): split references unknown recipient `{}`",
                        split.recipient
                    ));
                }
                match (split.amount, split.percent) {
                    (Some(_), Some(_)) => errs.push(format!(
                        "endpoint `{path}` (tier ${per_unit:.6}/unit): split for `{}` has both amount and percent",
                        split.recipient
                    )),
                    (None, None) => errs.push(format!(
                        "endpoint `{path}` (tier ${per_unit:.6}/unit): split for `{}` has neither amount nor percent",
                        split.recipient
                    )),
                    _ => {}
                }
            }

            // Splits must be less than the per-unit price.
            if per_unit > 0.0 {
                let fixed: f64 = tier.splits.iter().filter_map(|s| s.amount).sum();
                let pct: f64 =
                    tier.splits.iter().filter_map(|s| s.percent).sum::<f64>() / 100.0 * per_unit;
                let total = fixed + pct;
                if total >= per_unit {
                    errs.push(format!(
                        "endpoint `{path}` (tier ${per_unit:.6}/unit): tier splits total (${total:.6}) >= \
                         per-unit price (${per_unit:.6})"
                    ));
                }
            }
        }
    }
}

/// Per-unit price must be representable with 6 decimal places (USDC/USDT).
/// `price_usd / scale` values like `0.005 / 1099511627776` produce ~30
/// decimals, which overflows the token's precision and crashes at runtime.
fn validate_price_precision(metering: &Metering, path: &str, errs: &mut Vec<String>) {
    const MAX_DECIMALS: u32 = 6; // USDC/USDT = 6 decimals
    let threshold = 10f64.powi(-(MAX_DECIMALS as i32)); // 0.000001

    for dim in &metering.dimensions {
        let scale = dim.scale.max(1) as f64;
        for tier in &dim.tiers {
            if tier.price_usd == 0.0 {
                continue;
            }
            let per_unit = tier.price_usd / scale;
            if per_unit < threshold && per_unit > 0.0 {
                errs.push(format!(
                    "endpoint `{path}`: price ${:.6}/unit (${} / scale {}) is below the \
                     minimum representable amount for 6-decimal tokens (${threshold}) — \
                     reduce scale or increase price_usd",
                    per_unit, tier.price_usd, dim.scale
                ));
            }
        }
    }

    for variant in &metering.variants {
        for dim in &variant.dimensions {
            let scale = dim.scale.max(1) as f64;
            for tier in &dim.tiers {
                if tier.price_usd == 0.0 {
                    continue;
                }
                let per_unit = tier.price_usd / scale;
                if per_unit < threshold && per_unit > 0.0 {
                    errs.push(format!(
                        "endpoint `{path}` (variant {}={}): price ${:.6}/unit (${} / scale {}) \
                         is below the minimum representable amount for 6-decimal tokens",
                        variant.param, variant.value, per_unit, tier.price_usd, dim.scale
                    ));
                }
            }
        }
    }
}

/// Smallest non-zero per-unit price (`price_usd / scale`) across all tiers.
fn min_nonzero_per_unit_price(dimensions: &[MeterDimension]) -> f64 {
    dimensions
        .iter()
        .flat_map(|d| {
            let scale = d.scale.max(1) as f64;
            d.tiers.iter().map(move |t| t.price_usd / scale)
        })
        .filter(|p| *p > 0.0)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_method_serde_roundtrip() {
        for method in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Patch,
            HttpMethod::Delete,
        ] {
            let json = serde_json::to_string(&method).unwrap();
            let back: HttpMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", method));
        }
    }

    #[test]
    fn compare_op_serde() {
        let json = serde_json::to_string(&CompareOp::Lte).unwrap();
        assert_eq!(json, r#""<=""#);
        let json = serde_json::to_string(&CompareOp::Lt).unwrap();
        assert_eq!(json, r#""<""#);
        let json = serde_json::to_string(&CompareOp::Gte).unwrap();
        assert_eq!(json, r#"">=""#);
        let json = serde_json::to_string(&CompareOp::Gt).unwrap();
        assert_eq!(json, r#"">""#);
        let json = serde_json::to_string(&CompareOp::Eq).unwrap();
        assert_eq!(json, r#""==""#);
    }

    #[test]
    fn compare_op_deserialize() {
        let lte: CompareOp = serde_json::from_str(r#""<=""#).unwrap();
        assert!(matches!(lte, CompareOp::Lte));
        let gt: CompareOp = serde_json::from_str(r#"">""#).unwrap();
        assert!(matches!(gt, CompareOp::Gt));
    }

    #[test]
    fn api_category_serde() {
        for cat in [
            ApiCategory::AiMl,
            ApiCategory::Search,
            ApiCategory::Maps,
            ApiCategory::Data,
            ApiCategory::Compute,
            ApiCategory::Productivity,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let back: ApiCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", cat));
        }
    }

    #[test]
    fn accounting_mode_default_is_pooled() {
        let mode = AccountingMode::default();
        assert!(matches!(mode, AccountingMode::Pooled));
    }

    #[test]
    fn accounting_mode_serde() {
        let pooled = serde_json::to_string(&AccountingMode::Pooled).unwrap();
        assert_eq!(pooled, r#""pooled""#);
        let per_agent = serde_json::to_string(&AccountingMode::PerAgent).unwrap();
        assert_eq!(per_agent, r#""per_agent""#);
    }

    #[test]
    fn meter_direction_serde() {
        for dir in [
            MeterDirection::Input,
            MeterDirection::Output,
            MeterDirection::Usage,
            MeterDirection::Storage,
        ] {
            let json = serde_json::to_string(&dir).unwrap();
            let back: MeterDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", dir));
        }
    }

    #[test]
    fn billing_unit_serde() {
        for unit in [
            BillingUnit::Tokens,
            BillingUnit::Characters,
            BillingUnit::Requests,
            BillingUnit::Minutes,
            BillingUnit::Hours,
            BillingUnit::Seconds,
            BillingUnit::Pages,
            BillingUnit::Documents,
            BillingUnit::Invocations,
            BillingUnit::Bytes,
            BillingUnit::Gibibytes,
            BillingUnit::Tebibytes,
            BillingUnit::Vcpu,
            BillingUnit::QuotaUnits,
            BillingUnit::Instances,
        ] {
            let json = serde_json::to_string(&unit).unwrap();
            let back: BillingUnit = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", unit));
        }
    }

    #[test]
    fn billing_period_serde() {
        for period in [
            BillingPeriod::PerSecond,
            BillingPeriod::PerHour,
            BillingPeriod::PerDay,
            BillingPeriod::PerMonth,
        ] {
            let json = serde_json::to_string(&period).unwrap();
            let back: BillingPeriod = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", period));
        }
    }

    #[test]
    fn sku_level_serde() {
        for level in [SkuLevel::Essentials, SkuLevel::Pro, SkuLevel::Enterprise] {
            let json = serde_json::to_string(&level).unwrap();
            let back: SkuLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", level));
        }
    }

    #[test]
    fn payment_protocol_serde() {
        let x402 = serde_json::to_string(&PaymentProtocol::X402).unwrap();
        assert_eq!(x402, r#""x402""#);
        let mpp = serde_json::to_string(&PaymentProtocol::Mpp).unwrap();
        assert_eq!(mpp, r#""mpp""#);
    }

    #[test]
    fn meter_condition_tagged_serde() {
        let cond = MeterCondition::InputTokens {
            op: CompareOp::Lte,
            value: 1000,
        };
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains(r#""field":"input_tokens""#));
        let back: MeterCondition = serde_json::from_str(&json).unwrap();
        match back {
            MeterCondition::InputTokens { op, value } => {
                assert!(matches!(op, CompareOp::Lte));
                assert_eq!(value, 1000);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn price_tier_optional_fields() {
        let tier = PriceTier {
            up_to: None,
            price_usd: 0.01,
            condition: None,
            notes: None,
            splits: vec![],
        };
        let json = serde_json::to_string(&tier).unwrap();
        assert!(!json.contains("up_to"));
        assert!(!json.contains("condition"));
        assert!(!json.contains("notes"));
    }

    #[test]
    fn endpoint_minimal() {
        let ep = Endpoint {
            method: HttpMethod::Get,
            path: "v1/test".to_string(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
        };
        let json = serde_json::to_string(&ep).unwrap();
        let back: Endpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path, "v1/test");
        assert!(back.metering.is_none());
    }

    #[test]
    fn metering_with_variants() {
        let metering = Metering {
            dimensions: vec![],
            variants: vec![MeterVariant {
                param: "model".to_string(),
                value: "gpt-4".to_string(),
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Input,
                    unit: BillingUnit::Tokens,
                    scale: 1_000_000,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.03,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
            }],
            sku_tiers: vec![],
            splits: vec![],
        };
        let json = serde_json::to_string(&metering).unwrap();
        let back: Metering = serde_json::from_str(&json).unwrap();
        assert_eq!(back.variants.len(), 1);
        assert_eq!(back.variants[0].value, "gpt-4");
    }

    #[test]
    fn service_serde_roundtrip() {
        let svc = Service {
            id: "svc-1".to_string(),
            name: "Test Service".to_string(),
            description: "A test".to_string(),
            endpoint_url: "https://api.example.com".to_string(),
            category: "ai".to_string(),
            protocol: PaymentProtocol::Mpp,
            facilitator: "solana".to_string(),
        };
        let json = serde_json::to_string(&svc).unwrap();
        let back: Service = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, svc.id);
        assert_eq!(back.name, svc.name);
    }

    #[test]
    fn full_api_spec_roundtrip() {
        let spec = ApiSpec {
            name: "vision".to_string(),
            subdomain: "vision".to_string(),
            title: "Cloud Vision".to_string(),
            description: "Image analysis".to_string(),
            category: ApiCategory::AiMl,
            version: "v1".to_string(),
            env: std::collections::HashMap::new(),
            routing: RoutingConfig::Proxy {
                url: "https://vision.googleapis.com".to_string(),
                path_rewrites: vec![],
                auth: None,
            },
            accounting: AccountingMode::PerAgent,
            endpoints: vec![Endpoint {
                method: HttpMethod::Post,
                path: "v1/images:annotate".to_string(),
                description: Some("Annotate images".to_string()),
                resource: Some("images".to_string()),
                routing: None,
                metering: Some(Metering {
                    dimensions: vec![MeterDimension {
                        direction: MeterDirection::Usage,
                        unit: BillingUnit::Requests,
                        scale: 1,
                        period: None,
                        tiers: vec![PriceTier {
                            up_to: Some(1000),
                            price_usd: 0.0,
                            condition: None,
                            notes: Some("Free tier".to_string()),
                            splits: vec![],
                        }],
                    }],
                    variants: vec![],
                    sku_tiers: vec![],
                    splits: vec![],
                }),
            }],
            free_tier: Some(FreeTier {
                amount: Some(1000),
                unit: Some(BillingUnit::Requests),
                period: Some(BillingPeriod::PerMonth),
                notes: None,
            }),
            quotas: Some(QuotaSpec {
                requests_per_minute: Some(600),
                requests_per_day: None,
                requests_per_100_seconds: None,
                per_user_requests_per_second: None,
                quota_units_per_day: None,
                notes: None,
            }),
            notes: None,
            operator: None,
            recipients: std::collections::HashMap::new(),
            session: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ApiSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "vision");
        assert_eq!(back.endpoints.len(), 1);
        assert!(back.endpoints[0].metering.is_some());
        assert!(back.free_tier.is_some());
        assert_eq!(back.free_tier.unwrap().amount, Some(1000));
    }

    // ── RoutingConfig / path rewrites ────────────────────────────────────

    // ── rewrite_path ─────────────────────────────────────────────────────

    #[test]
    fn rewrite_path_substitutes_placeholder() {
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_PROJ_1", "gateway-402") };
        let rewrites = vec![PathRewrite {
            prefix: "v3/projects/{projectId}".to_string(),
            env: "_TEST_PROJ_1".to_string(),
        }];
        assert_eq!(
            super::rewrite_path(
                "/v3/projects/user-proj/locations/global:translateText",
                &rewrites
            ),
            "/v3/projects/gateway-402/locations/global:translateText"
        );
        unsafe { std::env::remove_var("_TEST_PROJ_1") };
    }

    #[test]
    fn rewrite_path_no_match_passes_through() {
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_PROJ_2", "gateway-402") };
        let rewrites = vec![PathRewrite {
            prefix: "v3/projects/{projectId}".to_string(),
            env: "_TEST_PROJ_2".to_string(),
        }];
        // Path doesn't start with v3/projects/...
        assert_eq!(
            super::rewrite_path("/v1/translate", &rewrites),
            "/v1/translate"
        );
        unsafe { std::env::remove_var("_TEST_PROJ_2") };
    }

    #[test]
    fn rewrite_path_missing_env_substitutes_empty() {
        // SAFETY: test-only, single-threaded
        unsafe { std::env::remove_var("_TEST_MISSING_2") };
        let rewrites = vec![PathRewrite {
            prefix: "v3/projects/{projectId}".to_string(),
            env: "_TEST_MISSING_2".to_string(),
        }];
        assert_eq!(
            super::rewrite_path("/v3/projects/user-proj/translate", &rewrites),
            "/v3/projects//translate"
        );
    }

    #[test]
    fn rewrite_path_no_match_short_path() {
        // Path is shorter than the prefix — rule is skipped.
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_PROJ_3", "my-proj") };
        let rewrites = vec![PathRewrite {
            prefix: "v3/projects/{projectId}".to_string(),
            env: "_TEST_PROJ_3".to_string(),
        }];
        assert_eq!(super::rewrite_path("/v3", &rewrites), "/v3");
        unsafe { std::env::remove_var("_TEST_PROJ_3") };
    }

    // ── upstream_url ────────────────────────────────────────────────────

    #[test]
    fn upstream_url_no_rewrites() {
        let fwd = RoutingConfig::Proxy {
            url: "https://api.example.com".to_string(),
            path_rewrites: vec![],
            auth: None,
        };
        assert_eq!(
            fwd.upstream_url("/v1/translate?q=hello").unwrap(),
            "https://api.example.com/v1/translate?q=hello"
        );
    }

    #[test]
    fn upstream_url_trailing_slash_on_base() {
        let fwd = RoutingConfig::Proxy {
            url: "https://api.example.com/".to_string(),
            path_rewrites: vec![],
            auth: None,
        };
        assert_eq!(
            fwd.upstream_url("/v1/test").unwrap(),
            "https://api.example.com/v1/test"
        );
    }

    #[test]
    fn upstream_url_with_rewrite() {
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_PROJECT_ID", "my-project-123") };
        let fwd = RoutingConfig::Proxy {
            url: "https://translation.googleapis.com".to_string(),
            path_rewrites: vec![PathRewrite {
                prefix: "v3/projects/{projectId}".to_string(),
                env: "_TEST_PROJECT_ID".to_string(),
            }],
            auth: None,
        };
        assert_eq!(
            fwd.upstream_url("/v3/projects/any-value/locations/global:translateText")
                .unwrap(),
            "https://translation.googleapis.com/v3/projects/my-project-123/locations/global:translateText"
        );
        unsafe { std::env::remove_var("_TEST_PROJECT_ID") };
    }

    #[test]
    fn upstream_url_preserves_query_string() {
        // SAFETY: test-only, single-threaded
        unsafe { std::env::set_var("_TEST_PROJ_QS", "gateway-402") };
        let fwd = RoutingConfig::Proxy {
            url: "https://api.example.com".to_string(),
            path_rewrites: vec![PathRewrite {
                prefix: "v3/projects/{projectId}".to_string(),
                env: "_TEST_PROJ_QS".to_string(),
            }],
            auth: None,
        };
        assert_eq!(
            fwd.upstream_url("/v3/projects/user-proj/translate?lang=fr")
                .unwrap(),
            "https://api.example.com/v3/projects/gateway-402/translate?lang=fr"
        );
        unsafe { std::env::remove_var("_TEST_PROJ_QS") };
    }

    #[test]
    fn upstream_url_rewrite_prefix_not_at_start() {
        // BigQuery case: prefix is `projects/{projectId}` but the path
        // starts with `bigquery/v2/projects/...`. The rewrite must find
        // the prefix at offset 2 in the segment list, not fail because
        // segment[0] != "projects".
        unsafe { std::env::set_var("_TEST_BQ_PROJECT", "gateway-402") };
        let fwd = RoutingConfig::Proxy {
            url: "https://bigquery.googleapis.com".to_string(),
            path_rewrites: vec![PathRewrite {
                prefix: "projects/{projectId}".to_string(),
                env: "_TEST_BQ_PROJECT".to_string(),
            }],
            auth: None,
        };
        assert_eq!(
            fwd.upstream_url("/bigquery/v2/projects/any-user-value/queries")
                .unwrap(),
            "https://bigquery.googleapis.com/bigquery/v2/projects/gateway-402/queries"
        );
        // Also works for nested paths after the project
        assert_eq!(
            fwd.upstream_url(
                "/bigquery/v2/projects/bigquery-public-data/datasets/my_dataset/tables"
            )
            .unwrap(),
            "https://bigquery.googleapis.com/bigquery/v2/projects/gateway-402/datasets/my_dataset/tables"
        );
        unsafe { std::env::remove_var("_TEST_BQ_PROJECT") };
    }

    #[test]
    fn routing_config_json_proxy() {
        let json = r#"{"type":"proxy","url":"https://api.example.com"}"#;
        let rc: RoutingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(rc.display_url(), "https://api.example.com");
        assert!(rc.is_proxy());
    }

    #[test]
    fn routing_config_json_proxy_with_path_rewrites() {
        let json = r#"{
            "type": "proxy",
            "url": "https://translation.googleapis.com",
            "path_rewrites": [
                {"prefix": "v3/projects/{projectId}", "env": "GOOGLE_PROJECT_ID"}
            ]
        }"#;
        let rc: RoutingConfig = serde_json::from_str(json).unwrap();
        assert!(rc.is_proxy());
        if let RoutingConfig::Proxy {
            url, path_rewrites, ..
        } = &rc
        {
            assert_eq!(url, "https://translation.googleapis.com");
            assert_eq!(path_rewrites.len(), 1);
            assert_eq!(path_rewrites[0].prefix, "v3/projects/{projectId}");
            assert_eq!(path_rewrites[0].env, "GOOGLE_PROJECT_ID");
        } else {
            panic!("expected Proxy");
        }
    }

    #[test]
    fn routing_config_json_respond() {
        let json = r#"{"type":"respond"}"#;
        let rc: RoutingConfig = serde_json::from_str(json).unwrap();
        assert!(rc.is_respond());
        assert_eq!(rc.display_url(), "respond");
        assert!(rc.upstream_url("/test").is_none());
    }

    #[test]
    fn routing_config_roundtrip_proxy() {
        let rc = RoutingConfig::Proxy {
            url: "https://api.example.com".to_string(),
            path_rewrites: vec![],
            auth: None,
        };
        let json = serde_json::to_string(&rc).unwrap();
        assert!(json.contains(r#""type":"proxy""#));
        assert!(!json.contains("path_rewrites"));
        let back: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(back.is_proxy());
    }

    #[test]
    fn routing_config_roundtrip_respond() {
        let rc = RoutingConfig::Respond {};
        let json = serde_json::to_string(&rc).unwrap();
        assert!(json.contains(r#""type":"respond""#));
        let back: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(back.is_respond());
    }

    #[test]
    fn endpoint_routing_override_serde() {
        let json = r#"{
            "method": "POST",
            "path": "v1/test",
            "routing": {"type": "respond"}
        }"#;
        let ep: Endpoint = serde_json::from_str(json).unwrap();
        assert!(ep.routing.is_some());
        assert!(ep.routing.unwrap().is_respond());
    }

    #[test]
    fn endpoint_no_routing_override() {
        let json = r#"{"method": "GET", "path": "v1/health"}"#;
        let ep: Endpoint = serde_json::from_str(json).unwrap();
        assert!(ep.routing.is_none());
    }

    // ── validate_api_spec ───────────────────────────────────────────────

    fn test_spec(endpoints: Vec<Endpoint>) -> ApiSpec {
        let mut recipients = std::collections::HashMap::new();
        recipients.insert(
            "operator".into(),
            RecipientAlias {
                account: "OperatorWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                label: Some("Operator".into()),
            },
        );
        recipients.insert(
            "platform".into(),
            RecipientAlias {
                account: "PlatformWaLLetxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".into(),
                label: Some("Platform".into()),
            },
        );
        ApiSpec {
            name: "test".into(),
            subdomain: "test".into(),
            title: "Test".into(),
            description: "Test".into(),
            category: ApiCategory::Maps,
            version: "v1".into(),
            env: Default::default(),
            routing: RoutingConfig::Respond {},
            accounting: AccountingMode::default(),
            endpoints,
            free_tier: None,
            quotas: None,
            notes: None,
            operator: None,
            recipients,
            session: None,
        }
    }

    #[test]
    fn validate_splits_without_dimensions() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![],
                variants: vec![],
                sku_tiers: vec![SkuTier {
                    sku: "search-basic".into(),
                    level: SkuLevel::Essentials,
                }],
                splits: vec![SplitRule {
                    recipient: "operator".into(),
                    amount: Some(0.00025),
                    percent: None,
                    memo: None,
                }],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("no pricing dimensions"));
    }

    #[test]
    fn validate_splits_exceed_price() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.0002,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![SplitRule {
                    recipient: "operator".into(),
                    amount: Some(0.00025),
                    percent: None,
                    memo: None,
                }],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("primary recipient would receive nothing"));
    }

    #[test]
    fn validate_unknown_recipient() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![SplitRule {
                    recipient: "nonexistent".into(),
                    amount: Some(0.001),
                    percent: None,
                    memo: None,
                }],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("unknown recipient `nonexistent`"));
    }

    #[test]
    fn validate_split_both_amount_and_percent() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![SplitRule {
                    recipient: "operator".into(),
                    amount: Some(0.001),
                    percent: Some(5.0),
                    memo: None,
                }],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert!(errs.iter().any(|e| e.contains("both amount and percent")));
    }

    #[test]
    fn validate_split_neither_amount_nor_percent() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![SplitRule {
                    recipient: "operator".into(),
                    amount: None,
                    percent: None,
                    memo: None,
                }],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert!(
            errs.iter()
                .any(|e| e.contains("neither amount nor percent"))
        );
    }

    #[test]
    fn validate_valid_spec_no_errors() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/search".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.001,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![
                    SplitRule {
                        recipient: "operator".into(),
                        amount: Some(0.00025),
                        percent: None,
                        memo: None,
                    },
                    SplitRule {
                        recipient: "platform".into(),
                        amount: None,
                        percent: Some(0.05),
                        memo: None,
                    },
                ],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
    }

    #[test]
    fn validate_free_endpoint_no_errors() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Get,
            path: "v1/health".into(),
            description: None,
            resource: None,
            routing: None,
            metering: None,
        }]);
        let errs = validate_api_spec(&spec);
        assert!(errs.is_empty());
    }

    #[test]
    fn validate_tier_splits_exceed_tier_price() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/compute".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![SplitRule {
                            recipient: "operator".into(),
                            amount: Some(0.01),
                            percent: None,
                            memo: None,
                        }],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("tier splits total"));
    }

    #[test]
    fn validate_tier_split_unknown_recipient_and_bad_rules() {
        let spec = test_spec(vec![Endpoint {
            method: HttpMethod::Post,
            path: "v1/compute".into(),
            description: None,
            resource: None,
            routing: None,
            metering: Some(Metering {
                dimensions: vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: 0.01,
                        condition: None,
                        notes: None,
                        splits: vec![
                            SplitRule {
                                recipient: "missing".into(),
                                amount: Some(0.001),
                                percent: None,
                                memo: None,
                            },
                            SplitRule {
                                recipient: "operator".into(),
                                amount: Some(0.001),
                                percent: Some(10.0),
                                memo: None,
                            },
                            SplitRule {
                                recipient: "platform".into(),
                                amount: None,
                                percent: None,
                                memo: None,
                            },
                        ],
                    }],
                }],
                variants: vec![],
                sku_tiers: vec![],
                splits: vec![],
            }),
        }]);
        let errs = validate_api_spec(&spec);
        assert!(
            errs.iter()
                .any(|e| e.contains("unknown recipient `missing`")),
            "expected unknown recipient error, got: {errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("both amount and percent")),
            "expected both amount and percent error, got: {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("neither amount nor percent")),
            "expected neither amount nor percent error, got: {errs:?}"
        );
    }

    #[test]
    fn validate_price_precision_rejects_dimension_below_token_precision() {
        let yaml = r#"
name: tiny
subdomain: tiny
title: Tiny Prices
description: Tiny prices
category: data
version: v1
routing:
  type: respond
endpoints:
  - method: POST
    path: v1/tiny
    metering:
      dimensions:
        - direction: usage
          unit: requests
          scale: 2000000
          tiers:
            - price_usd: 1.0
"#;
        let spec: ApiSpec = serde_yml::from_str(yaml).unwrap();
        let errs = validate_api_spec(&spec);
        assert!(
            errs.iter()
                .any(|e| e.contains("below the minimum representable amount")),
            "expected precision error, got: {errs:?}"
        );
    }

    #[test]
    fn validate_price_precision_rejects_variant_below_token_precision() {
        let yaml = r#"
name: variants
subdomain: variants
title: Variant Prices
description: Variant prices
category: data
version: v1
routing:
  type: respond
endpoints:
  - method: POST
    path: v1/models
    metering:
      variants:
        - param: model
          value: tiny-model
          dimensions:
            - direction: input
              unit: tokens
              scale: 10000000
              tiers:
                - price_usd: 1.0
"#;
        let spec: ApiSpec = serde_yml::from_str(yaml).unwrap();
        let errs = validate_api_spec(&spec);
        assert!(
            errs.iter().any(|e| e.contains("variant model=tiny-model")),
            "expected variant precision error, got: {errs:?}"
        );
    }

    #[test]
    fn validate_price_precision_allows_zero_and_minimum_prices() {
        let yaml = r#"
name: exact
subdomain: exact
title: Exact Prices
description: Exact prices
category: data
version: v1
routing:
  type: respond
endpoints:
  - method: POST
    path: v1/exact
    metering:
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.0
            - price_usd: 0.000001
"#;
        let spec: ApiSpec = serde_yml::from_str(yaml).unwrap();
        let errs = validate_api_spec(&spec);
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
    }
}
