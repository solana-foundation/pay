//! The paywall behind a `sell_inference` endpoint.
//!
//! A seller's endpoint on connect.pay.sh is an ordinary gated API: one
//! OpenAI-shaped chat-completions route under `endpoints/<id>/`, priced by
//! the seller, proxied to the queue that parks each request until the
//! seller's agent answers it. This module produces the [`ApiSpec`] the
//! payment gate reads, the same document the Gemini and Qwen fleets ship as
//! `paywall.yml`, so verification, metering and settlement are the gate's.
//!
//! Which schemes an endpoint offers follows from how it is priced. A flat
//! price per request settles identically whether it is paid up front
//! (`mpp-charge`), reserved and committed after serving
//! (`x402-batch-settlement`), or metered (`mpp-session`, `x402-upto`), so all
//! four are offered. A per-token price can only be billed by schemes that
//! read the response: the session meter rates streamed usage chunk by
//! chunk and `x402-upto` settles the observed token counts under a ceiling,
//! while charge and batch would take the first dimension's per-token price
//! as the whole amount. Those two are therefore left out, as the Qwen spec
//! does.

use std::collections::{BTreeMap, HashMap};

use pay_types::metering::{
    AccountingMode, ApiCategory, ApiSpec, BatchSettlementSpec, BillingUnit, Endpoint, HttpMethod,
    MeterDimension, MeterDirection, MeterVariant, Metering, MissingUsagePolicy, OperatorConfig,
    PriceTier, RoutingConfig, Scheme, SessionSpec, SessionVoucherSigner, UptoMetering,
    UptoResponseBody, UptoResponseBodyMode, UsageMeter, UsageMeterSource,
};

use crate::pricing::{PricingConfig, TokenRate};

/// Route prefix for one endpoint: `endpoints/<id>`.
pub const PATH_PREFIX: &str = "endpoints";
/// The paid route, relative to the endpoint prefix.
pub const CHAT_COMPLETIONS_PATH: &str = "v1/chat/completions";
/// The free model listing, relative to the endpoint prefix.
pub const MODELS_PATH: &str = "v1/models";
/// Gate preset reading `/usage/prompt_tokens` and `/usage/completion_tokens`.
pub const USAGE_PRESET: &str = "openai-compatible";
/// Upper bound on a buffered response body for usage extraction.
pub const RESPONSE_BODY_LIMIT: usize = 32 * 1024 * 1024;

const TOKENS_PER_SCALE: u64 = 1_000_000;
const PROMPT_TOKENS_PATH: &str = "/usage/prompt_tokens";
const COMPLETION_TOKENS_PATH: &str = "/usage/completion_tokens";

/// How a seller prices the endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum SellPricing {
    /// The same USD amount for every request, whatever it returns.
    PerRequest { usd: f64 },
    /// USD per 1M input and output tokens, with per-model rates and a
    /// default. `max_usd` is the most one request may cost: the ceiling an
    /// `x402-upto` client deposits before the turn runs.
    PerToken { rates: PricingConfig, max_usd: f64 },
}

impl SellPricing {
    /// The payment schemes this pricing can be billed under.
    pub fn schemes(&self) -> Vec<Scheme> {
        match self {
            Self::PerRequest { .. } => vec![
                Scheme::MppCharge,
                Scheme::MppSession,
                Scheme::X402Upto,
                Scheme::X402BatchSettlement,
            ],
            Self::PerToken { .. } => vec![Scheme::MppSession, Scheme::X402Upto],
        }
    }

    /// The most one request can cost, in USD.
    pub fn max_usd(&self) -> f64 {
        match self {
            Self::PerRequest { usd } => *usd,
            Self::PerToken { max_usd, .. } => *max_usd,
        }
    }

    fn validate(&self, errs: &mut Vec<String>) {
        match self {
            Self::PerRequest { usd } => check_positive("price per request", *usd, errs),
            Self::PerToken { rates, max_usd } => {
                check_positive("max_usd", *max_usd, errs);
                if rates.default.is_none() && rates.per_model.is_empty() {
                    errs.push(
                        "per-token pricing needs a default rate or at least one model".into(),
                    );
                }
                for (label, rate) in rates
                    .default
                    .iter()
                    .map(|r| ("default".to_string(), r))
                    .chain(
                        rates
                            .per_model
                            .iter()
                            .map(|(m, r)| (format!("model \"{m}\""), r)),
                    )
                {
                    check_positive(&format!("{label} input rate"), rate.input_per_1m, errs);
                    check_positive(&format!("{label} output rate"), rate.output_per_1m, errs);
                }
            }
        }
    }
}

fn check_positive(label: &str, value: f64, errs: &mut Vec<String>) {
    if !value.is_finite() || value <= 0.0 {
        errs.push(format!(
            "{label} must be a positive USD amount, got {value}"
        ));
    }
}

/// Everything needed to write one endpoint's paywall.
#[derive(Debug, Clone)]
pub struct SellInference {
    /// The endpoint id in its public URL.
    pub endpoint_id: String,
    /// Where the seller is paid (base58).
    pub recipient: String,
    /// Network slug, normally `mainnet`.
    pub network: String,
    /// Stablecoins the endpoint accepts, e.g. `USDC`.
    pub currencies: Vec<String>,
    pub pricing: SellPricing,
    /// The model id the endpoint advertises and answers as.
    pub model: String,
    pub title: String,
    pub description: String,
    /// The queue service the gate proxies paid requests to.
    pub upstream_url: String,
    /// Channel cap offered to `mpp-session` clients, in USD.
    pub session_cap_usd: f64,
    /// Idle time after which a buyer's session channel is closed and its
    /// vouchers settled to the seller. Agent turns are slow and buyers
    /// pause between them, so the default is long.
    pub session_idle_close_secs: u32,
}

/// Default [`SellInference::session_idle_close_secs`]: ten minutes.
pub const DEFAULT_SESSION_IDLE_CLOSE_SECS: u32 = 600;

impl SellInference {
    /// `endpoints/<id>`.
    pub fn path_prefix(&self) -> String {
        format!("{PATH_PREFIX}/{}", self.endpoint_id)
    }

    /// The paid route, as the gate sees it (no leading slash).
    pub fn chat_path(&self) -> String {
        format!("{}/{CHAT_COMPLETIONS_PATH}", self.path_prefix())
    }

    /// The free model listing route.
    pub fn models_path(&self) -> String {
        format!("{}/{MODELS_PATH}", self.path_prefix())
    }

    /// Problems that would make the spec unusable; empty means ok.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.endpoint_id.is_empty() || self.endpoint_id.contains('/') {
            errs.push("endpoint id must be a single non-empty path segment".into());
        }
        if self.recipient.is_empty() {
            errs.push("recipient is required".into());
        }
        if self.currencies.is_empty() {
            errs.push("at least one currency is required".into());
        }
        if self.model.is_empty() {
            errs.push("model is required".into());
        }
        check_positive("session_cap_usd", self.session_cap_usd, &mut errs);
        if self.session_idle_close_secs == 0 {
            errs.push("session_idle_close_secs must be at least 1".into());
        }
        self.pricing.validate(&mut errs);
        if errs.is_empty() {
            errs.extend(pay_types::metering::validate_api_spec(&self.api_spec()));
        }
        errs
    }

    /// The paywall document.
    pub fn api_spec(&self) -> ApiSpec {
        let schemes = self.pricing.schemes();
        let name = format!("sell-{}", self.endpoint_id);
        ApiSpec {
            name: name.clone(),
            subdomain: name,
            title: self.title.clone(),
            description: self.description.clone(),
            category: ApiCategory::AiMl,
            version: "v1".to_string(),
            env: HashMap::new(),
            routing: RoutingConfig::Proxy {
                url: self.upstream_url.clone(),
                path_rewrites: vec![],
                auth: None,
            },
            accounting: AccountingMode::Pooled,
            endpoints: vec![
                Endpoint {
                    method: HttpMethod::Post,
                    path: self.chat_path(),
                    description: Some(format!(
                        "Chat completion answered by {}; OpenAI-compatible, streaming supported.",
                        self.model
                    )),
                    resource: Some("chat.completions".to_string()),
                    routing: None,
                    metering: Some(self.metering(&schemes)),
                    subscription: None,
                },
                Endpoint {
                    method: HttpMethod::Get,
                    path: self.models_path(),
                    description: Some("List the model this endpoint serves (free).".to_string()),
                    resource: Some("models".to_string()),
                    routing: None,
                    metering: None,
                    subscription: None,
                },
            ],
            free_tier: None,
            quotas: None,
            notes: None,
            operator: Some(OperatorConfig {
                signer: None,
                recipient: Some(self.recipient.clone()),
                currencies: BTreeMap::from([("usd".to_string(), self.currencies.clone())]),
                session_benchmark_test_mints: vec![],
                rpc_url: None,
                network: Some(self.network.clone()),
                fee_payer: true,
                challenge_binding_secret: None,
                realm: None,
            }),
            recipients: HashMap::new(),
            session: schemes.contains(&Scheme::MppSession).then(|| {
                let idle_close_secs = self.session_idle_close_secs.max(1);
                let close_delay_ms = u64::from(idle_close_secs) * 1_000;
                // Buyers pick how long their channel may idle, up to a day. A
                // buyer's choice is honored even when the seller's idle close
                // is shorter, so the seller's value leads the list: clients
                // that take the first option close on the seller's schedule.
                let mut idle_options = vec![idle_close_secs, 300, 600, 1800, 3600, 21600, 86400];
                idle_options.sort_unstable();
                idle_options.dedup();
                SessionSpec {
                    cap_usdc: self.session_cap_usd,
                    min_voucher_delta: 1,
                    voucher_signer: SessionVoucherSigner::Operator,
                    idle_timeout_options_seconds: Some(idle_options),
                    close_delay_ms,
                    // Idle closes are grouped per minute; the gate requires
                    // whole minutes here.
                    close_batch_interval_ms: 60_000,
                    // Push accepted watermarks on-chain every few seconds. The
                    // embedded lifecycle also learns of a channel through its
                    // settlement activity, so this must be on for idle closes.
                    settlement_interval_ms: 5_000,
                    splits: vec![],
                    reuse_from_chain: false,
                }
            }),
            batch_settlement: schemes
                .contains(&Scheme::X402BatchSettlement)
                .then(BatchSettlementSpec::default),
        }
    }

    fn metering(&self, schemes: &[Scheme]) -> Metering {
        let (dimensions, variants, missing_usage) = match &self.pricing {
            SellPricing::PerRequest { usd } => (
                vec![MeterDimension {
                    direction: MeterDirection::Usage,
                    unit: BillingUnit::Requests,
                    scale: 1,
                    period: None,
                    tiers: vec![PriceTier {
                        up_to: None,
                        price_usd: *usd,
                        condition: None,
                        notes: None,
                        splits: vec![],
                    }],
                    meter: None,
                }],
                vec![],
                // A flat price is owed for every served request.
                MissingUsagePolicy::Ceiling,
            ),
            SellPricing::PerToken { rates, .. } => (
                token_dimensions(&fallback_rate(rates)),
                rates
                    .per_model
                    .iter()
                    .map(|(model, rate)| MeterVariant {
                        param: "model".to_string(),
                        value: model.clone(),
                        description: None,
                        dimensions: token_dimensions(rate),
                    })
                    .collect(),
                MissingUsagePolicy::Refund,
            ),
        };
        Metering {
            dimensions,
            variants,
            sku_tiers: vec![],
            splits: vec![],
            schemes: Some(schemes.to_vec()),
            min_usd: None,
            upto: Some(UptoMetering {
                max_usd: Some(self.pricing.max_usd()),
                min_usd: None,
                missing_usage,
                response_body: Some(UptoResponseBody {
                    mode: UptoResponseBodyMode::Buffer,
                    max_bytes: Some(RESPONSE_BODY_LIMIT),
                }),
                usage_preset: Some(USAGE_PRESET.to_string()),
            }),
        }
    }
}

/// The rate for a model with no variant of its own: the default when there
/// is one, else the dearest listed rates, so an unlisted or session-priced
/// request never undercharges.
fn fallback_rate(rates: &PricingConfig) -> TokenRate {
    rates.default.unwrap_or_else(|| {
        rates.per_model.values().fold(
            TokenRate {
                input_per_1m: 0.0,
                output_per_1m: 0.0,
            },
            |acc, rate| TokenRate {
                input_per_1m: acc.input_per_1m.max(rate.input_per_1m),
                output_per_1m: acc.output_per_1m.max(rate.output_per_1m),
            },
        )
    })
}

fn token_dimensions(rate: &TokenRate) -> Vec<MeterDimension> {
    let dimension = |direction, path: &str, price_usd| MeterDimension {
        direction,
        unit: BillingUnit::Tokens,
        scale: TOKENS_PER_SCALE,
        period: None,
        tiers: vec![PriceTier {
            up_to: None,
            price_usd,
            condition: None,
            notes: None,
            splits: vec![],
        }],
        meter: Some(UsageMeter {
            source: UsageMeterSource::ResponseJson,
            path: Some(path.to_string()),
            header: None,
        }),
    };
    vec![
        dimension(MeterDirection::Input, PROMPT_TOKENS_PATH, rate.input_per_1m),
        dimension(
            MeterDirection::Output,
            COMPLETION_TOKENS_PATH,
            rate.output_per_1m,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPIENT: &str = "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP";

    fn sale(pricing: SellPricing) -> SellInference {
        SellInference {
            endpoint_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7".to_string(),
            recipient: RECIPIENT.to_string(),
            network: "mainnet".to_string(),
            currencies: vec!["USDC".to_string(), "USDT".to_string()],
            pricing,
            model: "ludo-agent".to_string(),
            title: "Ludo's agent".to_string(),
            description: "Answers questions about pay.".to_string(),
            upstream_url: "http://127.0.0.1:8410/".to_string(),
            session_cap_usd: 1.0,
            session_idle_close_secs: DEFAULT_SESSION_IDLE_CLOSE_SECS,
        }
    }

    fn rate(input: f64, output: f64) -> TokenRate {
        TokenRate {
            input_per_1m: input,
            output_per_1m: output,
        }
    }

    fn per_token(default: Option<TokenRate>, models: &[(&str, TokenRate)]) -> SellPricing {
        SellPricing::PerToken {
            rates: PricingConfig {
                default,
                per_model: models.iter().map(|(m, r)| (m.to_string(), *r)).collect(),
            },
            max_usd: 0.25,
        }
    }

    fn chat_metering(spec: &ApiSpec) -> &Metering {
        spec.endpoints[0].metering.as_ref().unwrap()
    }

    #[test]
    fn a_flat_price_offers_every_scheme() {
        let sale = sale(SellPricing::PerRequest { usd: 0.02 });
        assert!(sale.validate().is_empty(), "{:?}", sale.validate());

        let spec = sale.api_spec();
        assert_eq!(
            spec.endpoints[0].path,
            "endpoints/7c9e6679-7425-40de-944b-e07fc1f90ae7/v1/chat/completions"
        );
        assert_eq!(
            spec.endpoints[1].path,
            "endpoints/7c9e6679-7425-40de-944b-e07fc1f90ae7/v1/models"
        );
        assert!(
            spec.endpoints[1].metering.is_none(),
            "model listing is free"
        );

        let metering = chat_metering(&spec);
        assert_eq!(
            metering.schemes.as_deref().unwrap(),
            [
                Scheme::MppCharge,
                Scheme::MppSession,
                Scheme::X402Upto,
                Scheme::X402BatchSettlement,
            ]
        );
        assert_eq!(metering.dimensions.len(), 1);
        assert_eq!(metering.dimensions[0].unit, BillingUnit::Requests);
        assert_eq!(metering.dimensions[0].scale, 1);
        assert_eq!(metering.dimensions[0].tiers[0].price_usd, 0.02);
        assert!(metering.variants.is_empty());
        let upto = metering.upto.as_ref().unwrap();
        assert_eq!(upto.max_usd, Some(0.02));
        assert_eq!(upto.missing_usage, MissingUsagePolicy::Ceiling);

        assert!(spec.session.is_some());
        assert!(spec.batch_settlement.is_some());
        let operator = spec.operator.as_ref().unwrap();
        assert_eq!(operator.recipient.as_deref(), Some(RECIPIENT));
        assert_eq!(operator.currencies["usd"], ["USDC", "USDT"]);
        assert!(operator.fee_payer);
        assert!(
            matches!(spec.routing, RoutingConfig::Proxy { ref url, .. } if url == "http://127.0.0.1:8410/")
        );
    }

    #[test]
    fn per_token_pricing_meters_the_response_and_skips_prepaid_schemes() {
        let sale = sale(per_token(
            Some(rate(0.10, 0.30)),
            &[("fast", rate(0.05, 0.15)), ("deep", rate(1.0, 3.0))],
        ));
        assert!(sale.validate().is_empty(), "{:?}", sale.validate());

        let spec = sale.api_spec();
        let metering = chat_metering(&spec);
        assert_eq!(
            metering.schemes.as_deref().unwrap(),
            [Scheme::MppSession, Scheme::X402Upto]
        );
        assert!(
            spec.batch_settlement.is_none(),
            "no batch lifecycle without the scheme"
        );
        assert!(spec.session.is_some());

        // The default rate is the top-level fallback.
        assert_eq!(metering.dimensions.len(), 2);
        assert_eq!(metering.dimensions[0].direction, MeterDirection::Input);
        assert_eq!(metering.dimensions[0].scale, 1_000_000);
        assert_eq!(metering.dimensions[0].tiers[0].price_usd, 0.10);
        assert_eq!(
            metering.dimensions[0]
                .meter
                .as_ref()
                .unwrap()
                .path
                .as_deref(),
            Some("/usage/prompt_tokens")
        );
        assert_eq!(metering.dimensions[1].direction, MeterDirection::Output);
        assert_eq!(metering.dimensions[1].tiers[0].price_usd, 0.30);
        assert_eq!(
            metering.dimensions[1]
                .meter
                .as_ref()
                .unwrap()
                .path
                .as_deref(),
            Some("/usage/completion_tokens")
        );

        // One variant per listed model, selected by the request's `model`.
        let mut variants: Vec<(&str, f64, f64)> = metering
            .variants
            .iter()
            .map(|v| {
                assert_eq!(v.param, "model");
                (
                    v.value.as_str(),
                    v.dimensions[0].tiers[0].price_usd,
                    v.dimensions[1].tiers[0].price_usd,
                )
            })
            .collect();
        variants.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(variants, [("deep", 1.0, 3.0), ("fast", 0.05, 0.15)]);

        let upto = metering.upto.as_ref().unwrap();
        assert_eq!(upto.max_usd, Some(0.25));
        assert_eq!(upto.missing_usage, MissingUsagePolicy::Refund);
        assert_eq!(upto.usage_preset.as_deref(), Some("openai-compatible"));
        assert_eq!(
            upto.response_body.as_ref().unwrap().max_bytes,
            Some(RESPONSE_BODY_LIMIT)
        );
    }

    #[test]
    fn without_a_default_the_fallback_is_the_dearest_listed_rate() {
        let sale = sale(per_token(
            None,
            &[("fast", rate(0.05, 0.90)), ("deep", rate(1.0, 0.30))],
        ));
        let spec = sale.api_spec();
        let metering = chat_metering(&spec);
        assert_eq!(metering.dimensions[0].tiers[0].price_usd, 1.0);
        assert_eq!(metering.dimensions[1].tiers[0].price_usd, 0.90);
    }

    #[test]
    fn validation_names_each_problem() {
        let mut sale = sale(SellPricing::PerRequest { usd: 0.0 });
        sale.endpoint_id = "a/b".to_string();
        sale.recipient.clear();
        sale.currencies.clear();
        sale.model.clear();
        sale.session_cap_usd = -1.0;
        let errs = sale.validate();
        for needle in [
            "endpoint id",
            "recipient",
            "currency",
            "model",
            "session_cap_usd",
            "price per request",
        ] {
            assert!(
                errs.iter().any(|e| e.contains(needle)),
                "missing `{needle}` in {errs:?}"
            );
        }

        let empty = sale_with_rates(PricingConfig::default());
        assert!(
            empty
                .validate()
                .iter()
                .any(|e| e.contains("default rate or at least one model"))
        );

        let mut bad = PricingConfig::default();
        bad.per_model.insert("x".into(), rate(f64::NAN, 1.0));
        assert!(
            sale_with_rates(bad)
                .validate()
                .iter()
                .any(|e| e.contains("model \"x\" input rate"))
        );
    }

    fn sale_with_rates(rates: PricingConfig) -> SellInference {
        sale(SellPricing::PerToken {
            rates,
            max_usd: 0.25,
        })
    }

    #[test]
    fn the_spec_round_trips_through_yaml() {
        let sale = sale(SellPricing::PerRequest { usd: 0.02 });
        let yaml = serde_yml::to_string(&sale.api_spec()).unwrap();
        let mut parsed: ApiSpec = serde_yml::from_str(&yaml).unwrap();
        parsed.apply_scheme_defaults();
        assert_eq!(chat_metering(&parsed).accepted_schemes().len(), 4, "{yaml}");
        assert!(pay_types::metering::validate_api_spec(&parsed).is_empty());
    }
}

/// The gate itself, offered the spec: every listed scheme must produce its
/// challenge header, with no network in reach.
#[cfg(all(test, feature = "server"))]
mod gate_tests {
    use std::sync::Arc;

    use http::{Method, header};
    use pay_kit::mpp::server::Mpp;
    use pay_kit::solana_keychain::TransactionSigner;
    use pay_kit::x402::server::{X402BatchSettlement, X402Upto};
    use pay_types::metering::ApiSpec;

    use super::*;
    use crate::PaymentState;
    use crate::server::gate::{GateDecision, GateRequest, PaymentGate};
    use crate::server::session::SessionMpp;

    const RECIPIENT: &str = "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP";
    const NETWORK: &str = "devnet";
    const USDC_DEVNET: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    #[derive(Clone)]
    struct AllSchemes {
        apis: Vec<ApiSpec>,
        mpp: Mpp,
        session: Arc<SessionMpp>,
        upto: X402Upto,
        batch: X402BatchSettlement,
        signer: Arc<dyn TransactionSigner>,
    }

    impl PaymentState for AllSchemes {
        fn apis(&self) -> &[ApiSpec] {
            &self.apis
        }
        fn mpp(&self) -> Option<&Mpp> {
            Some(&self.mpp)
        }
        fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
            Some(self.session.clone())
        }
        fn x402_upto(&self) -> Option<&X402Upto> {
            Some(&self.upto)
        }
        fn x402_batch(&self) -> Option<&X402BatchSettlement> {
            Some(&self.batch)
        }
        fn fee_payer_signer(&self) -> Option<Arc<dyn TransactionSigner>> {
            Some(self.signer.clone())
        }
    }

    fn signer() -> Arc<dyn TransactionSigner> {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(sk.as_bytes());
        keypair[32..].copy_from_slice(sk.verifying_key().as_bytes());
        Arc::new(pay_kit::solana_keychain::MemorySigner::from_bytes(&keypair).unwrap())
    }

    fn state(sale: &SellInference) -> AllSchemes {
        let signer = signer();
        let operator = signer.pubkey().to_string();
        // A cached blockhash keeps the charge challenge off the network.
        let blockhashes = pay_kit::core::blockhash::BlockhashCache::new();
        blockhashes.set("11111111111111111111111111111111".to_string(), 1_000, 1);
        let rpc_url = "http://127.0.0.1:1".to_string();

        let mpp = Mpp::new(pay_kit::mpp::server::Config {
            recipient: RECIPIENT.to_string(),
            currency: USDC_DEVNET.to_string(),
            decimals: 6,
            network: NETWORK.to_string(),
            rpc_url: Some(rpc_url.clone()),
            challenge_binding_secret: Some(SECRET.to_string()),
            fee_payer: true,
            fee_payer_signer: Some(signer.clone()),
            html: false,
            ..Default::default()
        })
        .unwrap()
        .with_blockhash_cache(blockhashes.clone());

        let session = SessionMpp::new(
            pay_kit::mpp::server::session::SessionConfig {
                operator: operator.clone(),
                recipient: RECIPIENT.to_string(),
                currency: USDC_DEVNET.to_string(),
                network: NETWORK.to_string(),
                suggested_deposit: Some(1_000_000),
                min_voucher_delta: 1,
                voucher_signer: pay_kit::mpp::server::session::VoucherSigner::Operator,
                fee_payer_signer: Some(signer.clone()),
                rpc_url: Some(rpc_url.clone()),
                ..Default::default()
            },
            SECRET,
        )
        .with_blockhash_cache(blockhashes.clone());

        let upto = X402Upto::new(pay_kit::x402::server::UptoConfig {
            payout: pay_kit::x402::server::UptoPayout::Beneficiary {
                address: RECIPIENT.to_string(),
            },
            currencies: vec![pay_kit::x402::server::CurrencyConfig {
                currency: USDC_DEVNET.to_string(),
                decimals: 6,
                token_program: None,
            }],
            cluster: NETWORK.to_string(),
            rpc_url: Some(rpc_url),
            resource: format!("https://connect.pay.sh/{}", sale.chat_path()),
            description: None,
            max_timeout_seconds: 300,
            program_id: None,
            withdraw_delay: 0,
            fee_payer_signer: signer.clone(),
            receiver_authorizer_signer: None,
        })
        .unwrap()
        .with_blockhash_cache(blockhashes);

        let mut batch_cfg =
            pay_kit::x402::server::BatchConfig::new(RECIPIENT, NETWORK, signer.clone());
        batch_cfg.withdraw_delay = 900;
        let batch = X402BatchSettlement::new(batch_cfg).unwrap();

        AllSchemes {
            apis: vec![sale.api_spec()],
            mpp,
            session: Arc::new(session),
            upto,
            batch,
            signer,
        }
    }

    fn sale(pricing: SellPricing) -> SellInference {
        SellInference {
            endpoint_id: "e1".to_string(),
            recipient: RECIPIENT.to_string(),
            network: NETWORK.to_string(),
            currencies: vec!["USDC".to_string()],
            pricing,
            model: "agent".to_string(),
            title: "Agent".to_string(),
            description: String::new(),
            upstream_url: "http://127.0.0.1:8410/".to_string(),
            session_cap_usd: 1.0,
            session_idle_close_secs: DEFAULT_SESSION_IDLE_CLOSE_SECS,
        }
    }

    /// Challenge the paid route unauthenticated; returns
    /// (WWW-Authenticate values, PAYMENT-REQUIRED values).
    async fn challenge(sale: &SellInference) -> (Vec<String>, Vec<String>) {
        let gate = PaymentGate::new(state(sale));
        let path = sale.chat_path();
        let decision = gate
            .evaluate(&GateRequest {
                method: &Method::POST,
                path: &path,
                host: Some("connect.pay.sh"),
                accept: Some("application/json"),
                authorization: None,
                content_length: Some(128),
                query: None,
                x402_payment: None,
            })
            .await;
        let GateDecision::Respond(resp) = decision else {
            panic!("expected a 402 challenge");
        };
        assert_eq!(
            resp.status,
            http::StatusCode::PAYMENT_REQUIRED,
            "{:?}",
            resp.body
        );
        let values = |name: &str| -> Vec<String> {
            resp.headers
                .iter()
                .filter(|(n, _)| n.as_str().eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_str().unwrap().to_string())
                .collect()
        };
        (
            values(header::WWW_AUTHENTICATE.as_str()),
            values(pay_kit::x402::PAYMENT_REQUIRED_HEADER),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flat_price_challenges_with_all_four_schemes() {
        let (mpp, x402) = challenge(&sale(SellPricing::PerRequest { usd: 0.02 })).await;
        assert!(
            mpp.iter().any(|v| v.contains("intent=\"session\"")),
            "{mpp:?}"
        );
        assert!(
            mpp.iter().any(|v| v.contains("intent=\"charge\"")),
            "{mpp:?}"
        );
        assert_eq!(x402.len(), 2, "upto and batch-settlement: {x402:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_price_challenges_with_session_and_upto_only() {
        let rates = PricingConfig {
            default: Some(TokenRate {
                input_per_1m: 0.10,
                output_per_1m: 0.30,
            }),
            per_model: Default::default(),
        };
        let (mpp, x402) = challenge(&sale(SellPricing::PerToken {
            rates,
            max_usd: 0.25,
        }))
        .await;
        assert_eq!(mpp.len(), 1, "{mpp:?}");
        assert!(mpp[0].contains("intent=\"session\""), "{mpp:?}");
        assert_eq!(x402.len(), 1, "upto only: {x402:?}");
    }
}
