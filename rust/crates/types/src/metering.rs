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
    #[serde(alias = "base_url")]
    pub forward_url: String,
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
}

// =============================================================================
// Operator config
// =============================================================================

/// Operator-level configuration for a proxy instance.
/// Controls signing, payment recipient, and currency.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OperatorConfig {
    /// Signing backend for fee sponsorship and settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<SignerConfig>,
    /// Payment recipient wallet address (base58).
    /// Overrides --recipient CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Payment currency (SOL, USDC, etc.).
    /// Overrides --currency CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Solana RPC URL. Overrides --rpc-url CLI flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// Solana network (mainnet-beta, devnet, localnet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// Whether the operator sponsors transaction fees.
    #[serde(default)]
    pub fee_payer: bool,
}

/// Signing backend configuration.
/// When specified in the YAML, the proxy uses this signer directly —
/// bypassing the keystore. For production use GCP KMS; for dev use file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "kebab-case")]
pub enum SignerConfig {
    /// GCP Cloud KMS — Ed25519 HSM key. Private key never leaves the HSM.
    GcpKms {
        /// Full KMS key version resource name.
        key_name: String,
        /// Solana public key (base58) derived from the KMS key.
        pubkey: String,
    },
}

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
                    }],
                }],
            }],
            sku_tiers: vec![],
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
            forward_url: "https://vision.googleapis.com".to_string(),
            accounting: AccountingMode::PerAgent,
            endpoints: vec![Endpoint {
                method: HttpMethod::Post,
                path: "v1/images:annotate".to_string(),
                description: Some("Annotate images".to_string()),
                resource: Some("images".to_string()),
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
                        }],
                    }],
                    variants: vec![],
                    sku_tiers: vec![],
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
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ApiSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "vision");
        assert_eq!(back.endpoints.len(), 1);
        assert!(back.endpoints[0].metering.is_some());
        assert!(back.free_tier.is_some());
        assert_eq!(back.free_tier.unwrap().amount, Some(1000));
    }
}
