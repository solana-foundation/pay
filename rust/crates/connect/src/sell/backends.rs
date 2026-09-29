//! One seller's payment backends: the gate's [`PaymentState`] for one endpoint.
//!
//! `pay server start` builds one set of MPP and x402 servers per process,
//! all paying one recipient. Here every endpoint pays a different seller, so
//! each gets its own set, built from its paywall with the deployment's
//! [`Operator`] as fee payer, session operator and settlement signer. The
//! gate's verification and settlement code then runs unchanged.

use std::str::FromStr;
use std::sync::Arc;

use pay_core::PaymentState;
use pay_core::sell_inference::SellInference;
use pay_core::server::session::SessionMpp;
use pay_kit::mpp::blockhash::BlockhashCache;
use pay_kit::mpp::server::Mpp;
use pay_kit::solana_keychain::TransactionSigner;
use pay_kit::x402::server::{X402BatchSettlement, X402Upto};
use pay_types::Stablecoin;
use pay_types::metering::{ApiSpec, Scheme};

/// Solana keypair material for the operator: a JSON byte array or base58.
pub const KEYPAIR_ENV: &str = "PAY_CONNECT_SELL_KEYPAIR";
/// RPC endpoint the operator verifies opens and settles through.
pub const RPC_URL_ENV: &str = "PAY_CONNECT_SELL_RPC_URL";
/// Challenge-binding secret for MPP charge and session (32 bytes or more).
pub const SECRET_ENV: &str = "PAY_CONNECT_SELL_SECRET";

const STABLECOIN_DECIMALS: u8 = 6;
const BLOCKHASH_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

/// The deployment's signing identity behind every seller's endpoint.
pub struct Operator {
    pub signer: Arc<dyn TransactionSigner>,
    pub rpc_url: String,
    pub challenge_binding_secret: String,
    /// Shared by every backend; refreshed by [`Operator::spawn_blockhash_refresh`].
    pub blockhashes: BlockhashCache,
}

impl Operator {
    pub fn new(
        signer: Arc<dyn TransactionSigner>,
        rpc_url: impl Into<String>,
        challenge_binding_secret: impl Into<String>,
    ) -> Self {
        Self {
            signer,
            rpc_url: rpc_url.into(),
            challenge_binding_secret: challenge_binding_secret.into(),
            blockhashes: BlockhashCache::new(),
        }
    }

    /// `None` when none of the variables is set. All three are required
    /// together: an operator with no RPC or no secret is a misconfiguration,
    /// not a default.
    pub fn from_env() -> Result<Option<Self>, String> {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let (keypair, rpc_url, secret) = (read(KEYPAIR_ENV), read(RPC_URL_ENV), read(SECRET_ENV));
        if keypair.is_none() && rpc_url.is_none() && secret.is_none() {
            return Ok(None);
        }
        let missing: Vec<&str> = [
            (KEYPAIR_ENV, keypair.is_some()),
            (RPC_URL_ENV, rpc_url.is_some()),
            (SECRET_ENV, secret.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| (!set).then_some(name))
        .collect();
        if !missing.is_empty() {
            return Err(format!(
                "sell_inference needs {KEYPAIR_ENV}, {RPC_URL_ENV} and {SECRET_ENV} together; missing {}",
                missing.join(", ")
            ));
        }
        let secret = secret.expect("checked");
        if secret.len() < 32 {
            return Err(format!(
                "{SECRET_ENV} must be at least 32 bytes (`openssl rand -base64 32`)"
            ));
        }
        let signer = parse_keypair(&keypair.expect("checked"))?;
        Ok(Some(Self::new(signer, rpc_url.expect("checked"), secret)))
    }

    /// Prime the blockhash cache and keep it fresh from a background thread,
    /// so challenges never wait on RPC.
    pub fn spawn_blockhash_refresh(&self) {
        use pay_kit::mpp::solana_rpc_client::rpc_client::RpcClient;
        let cache = self.blockhashes.clone();
        let rpc_url = self.rpc_url.clone();
        let refresh = move || {
            let rpc = RpcClient::new(rpc_url.clone());
            match pay_kit::mpp::blockhash::fetch_blockhash_with_slot(&rpc, rpc.commitment()) {
                Ok(entry) => cache.set(entry.blockhash, entry.last_valid_block_height, entry.slot),
                Err(error) => tracing::warn!(%error, "sell operator blockhash refresh failed"),
            }
        };
        refresh();
        std::thread::Builder::new()
            .name("pay-sell-blockhash".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(BLOCKHASH_REFRESH);
                    refresh();
                }
            })
            .expect("spawn blockhash refresh thread");
    }

    pub fn pubkey(&self) -> String {
        self.signer.pubkey().to_string()
    }
}

fn parse_keypair(input: &str) -> Result<Arc<dyn TransactionSigner>, String> {
    let bytes: Vec<u8> = if input.starts_with('[') {
        serde_json::from_str(input)
            .map_err(|e| format!("{KEYPAIR_ENV}: invalid keypair JSON: {e}"))?
    } else {
        bs58::decode(input)
            .into_vec()
            .map_err(|e| format!("{KEYPAIR_ENV}: invalid base58 keypair: {e}"))?
    };
    if bytes.len() != 64 {
        return Err(format!(
            "{KEYPAIR_ENV} must decode to exactly 64 bytes, got {}",
            bytes.len()
        ));
    }
    let signer = pay_kit::solana_keychain::MemorySigner::from_bytes(&bytes)
        .map_err(|e| format!("{KEYPAIR_ENV}: not a Solana keypair: {e}"))?;
    Ok(Arc::new(signer))
}

/// The backends one endpoint's paywall asks for, all paying its seller.
#[derive(Clone)]
pub struct EndpointBackends {
    apis: Arc<Vec<ApiSpec>>,
    charge: Arc<Vec<Mpp>>,
    sessions: Arc<Vec<Arc<SessionMpp>>>,
    upto: Option<Arc<X402Upto>>,
    batch: Option<Arc<X402BatchSettlement>>,
    signer: Arc<dyn TransactionSigner>,
}

impl EndpointBackends {
    /// `public_url` is the origin the endpoint is reached at; x402 `upto`
    /// binds its channel to the full resource URL.
    pub fn build(
        sale: &SellInference,
        operator: &Operator,
        public_url: &str,
    ) -> Result<Self, String> {
        let problems = sale.validate();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let schemes = sale.pricing.schemes();
        let network = sale.network.as_str();
        let rpc_url = Some(operator.rpc_url.clone());
        let mints: Vec<String> = sale
            .currencies
            .iter()
            .map(|symbol| {
                Stablecoin::parse_symbol(symbol)
                    .map(|coin| coin.mint(Some(network)).to_string())
                    .ok_or_else(|| format!("unsupported currency `{symbol}`"))
            })
            .collect::<Result<_, _>>()?;

        let mut charge = Vec::new();
        if schemes.contains(&Scheme::MppCharge) {
            for mint in &mints {
                let mpp = Mpp::new(pay_kit::mpp::server::Config {
                    recipient: sale.recipient.clone(),
                    currency: mint.clone(),
                    decimals: STABLECOIN_DECIMALS,
                    network: network.to_string(),
                    rpc_url: rpc_url.clone(),
                    challenge_binding_secret: Some(operator.challenge_binding_secret.clone()),
                    fee_payer: true,
                    fee_payer_signer: Some(operator.signer.clone()),
                    html: false,
                    ..Default::default()
                })
                .map_err(|e| format!("mpp charge backend: {e}"))?
                .with_tx_v1(pay_kit::core::tx::TxV1Mode::Auto)
                .with_blockhash_cache(operator.blockhashes.clone());
                charge.push(mpp);
            }
        }

        let mut sessions = Vec::new();
        if let Some(spec) = schemes
            .contains(&Scheme::MppSession)
            .then(|| sale.api_spec().session)
            .flatten()
        {
            let cap_base =
                (sale.session_cap_usd * 10f64.powi(i32::from(STABLECOIN_DECIMALS))).round() as u64;
            // The operator signs the vouchers and settles, so the channel pays
            // the operator and the seller's share is a distribution split.
            let (channel_recipient, splits) =
                pay_core::server::session::delegated_session_channel_payout(
                    &sale.recipient,
                    &operator.pubkey(),
                    vec![],
                )
                .map_err(|e| e.to_string())?;
            let idle_timeout_seconds = u32::try_from(spec.close_delay_ms.div_ceil(1_000))
                .unwrap_or(pay_kit::mpp::MAX_IDLE_TIMEOUT_SECONDS)
                .clamp(1, pay_kit::mpp::MAX_IDLE_TIMEOUT_SECONDS);
            for mint in &mints {
                let token_program = solana_pubkey::Pubkey::from_str(
                    pay_kit::mpp::protocol::solana::default_token_program_for_currency(mint, None),
                )
                .map_err(|e| format!("token program: {e}"))?;
                let config = pay_kit::mpp::server::session::SessionConfig {
                    operator: operator.pubkey(),
                    recipient: channel_recipient.clone(),
                    splits: splits.clone(),
                    currency: mint.clone(),
                    decimals: STABLECOIN_DECIMALS,
                    network: network.to_string(),
                    // Per-unit price; the gate sets it per challenge from the
                    // endpoint's resolved metering price.
                    amount: 1,
                    suggested_deposit: Some(cap_base),
                    minimum_deposit: None,
                    min_voucher_delta: spec.min_voucher_delta,
                    voucher_signer: pay_kit::mpp::server::session::VoucherSigner::Operator,
                    operator_signing_key: None,
                    fee_payer_signer: Some(operator.signer.clone()),
                    idle_timeout_options_seconds: spec.idle_timeout_options_seconds.clone(),
                    idle_timeout_seconds,
                    grace_period_seconds:
                        pay_kit::mpp::program::payment_channels::DEFAULT_GRACE_PERIOD_SECONDS,
                    rpc_url: rpc_url.clone(),
                    channel_program: Some(
                        pay_kit::mpp::program::payment_channels::default_program_id(),
                    ),
                    token_program: Some(token_program),
                };
                let session = SessionMpp::new(config, operator.challenge_binding_secret.clone())
                    .with_realm(sale.title.clone())
                    .with_blockhash_cache(operator.blockhashes.clone())
                    .with_payment_channel_signer(operator.signer.clone());
                // Embedded reconciliation: this process pushes watermarks and
                // closes idle channels. A settlement worker takes over when
                // pay-connect's session state moves to Redis.
                session.start_lifecycle_runloop_with_settlement_and_batching(
                    std::time::Duration::from_millis(spec.close_delay_ms),
                    std::time::Duration::from_millis(spec.close_batch_interval_ms),
                    std::time::Duration::from_millis(spec.settlement_interval_ms),
                    pay_core::server::session::SessionLifecycleReconciliation::Embedded,
                );
                sessions.push(Arc::new(session));
            }
        }

        let upto = if schemes.contains(&Scheme::X402Upto) {
            let currencies = mints
                .iter()
                .map(|mint| pay_kit::x402::server::CurrencyConfig {
                    currency: mint.clone(),
                    decimals: STABLECOIN_DECIMALS,
                    token_program: None,
                })
                .collect();
            let upto = X402Upto::new(pay_kit::x402::server::UptoConfig {
                payout: pay_kit::x402::server::UptoPayout::Beneficiary {
                    address: sale.recipient.clone(),
                },
                currencies,
                cluster: network.to_string(),
                rpc_url: rpc_url.clone(),
                resource: format!("{}/{}", public_url.trim_end_matches('/'), sale.chat_path()),
                description: Some(sale.title.clone()),
                max_timeout_seconds: 300,
                program_id: None,
                withdraw_delay: 0,
                fee_payer_signer: operator.signer.clone(),
                receiver_authorizer_signer: None,
            })
            .map_err(|e| format!("x402 upto backend: {e}"))?
            .with_tx_v1(pay_kit::core::tx::TxV1Mode::Auto)
            .with_blockhash_cache(operator.blockhashes.clone());
            Some(Arc::new(upto))
        } else {
            None
        };

        let batch = if schemes.contains(&Scheme::X402BatchSettlement) {
            let mut cfg = pay_kit::x402::server::BatchConfig::new(
                &sale.recipient,
                network,
                operator.signer.clone(),
            );
            cfg.rpc_url = rpc_url.clone();
            cfg.resource = format!("{}/{}", public_url.trim_end_matches('/'), sale.chat_path());
            cfg.currency = mints[0].clone();
            cfg.decimals = STABLECOIN_DECIMALS;
            cfg.max_timeout_seconds = 300;
            // The buyer may force-close and recover unspent escrow after this
            // long; the scheme bounds it to 15 minutes .. 30 days.
            cfg.withdraw_delay = pay_kit::x402::batch_settlement::MIN_WITHDRAW_DELAY_SECONDS;
            let batch = X402BatchSettlement::new(cfg)
                .map_err(|e| format!("x402 batch-settlement backend: {e}"))?;
            Some(Arc::new(batch))
        } else {
            None
        };

        Ok(Self {
            apis: Arc::new(vec![sale.api_spec()]),
            charge: Arc::new(charge),
            sessions: Arc::new(sessions),
            upto,
            batch,
            signer: operator.signer.clone(),
        })
    }

    pub fn spec(&self) -> &ApiSpec {
        &self.apis[0]
    }
}

impl PaymentState for EndpointBackends {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.charge.first()
    }
    fn mpps(&self) -> Vec<&Mpp> {
        self.charge.iter().collect()
    }
    fn session_mpp_handle(&self) -> Option<Arc<SessionMpp>> {
        self.sessions.first().cloned()
    }
    fn session_mpp_handles(&self) -> Vec<Arc<SessionMpp>> {
        self.sessions.to_vec()
    }
    fn x402_upto(&self) -> Option<&X402Upto> {
        self.upto.as_deref()
    }
    fn x402_batch(&self) -> Option<&X402BatchSettlement> {
        self.batch.as_deref()
    }
    fn fee_payer_signer(&self) -> Option<Arc<dyn TransactionSigner>> {
        Some(self.signer.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_core::sell_inference::SellPricing;

    const RECIPIENT: &str = "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP";

    fn operator() -> Operator {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let mut keypair = [0u8; 64];
        keypair[..32].copy_from_slice(sk.as_bytes());
        keypair[32..].copy_from_slice(sk.verifying_key().as_bytes());
        let signer = pay_kit::solana_keychain::MemorySigner::from_bytes(&keypair).unwrap();
        let operator = Operator::new(
            Arc::new(signer),
            "http://127.0.0.1:1",
            "0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        operator
            .blockhashes
            .set("11111111111111111111111111111111".into(), 1_000, 1);
        operator
    }

    fn sale(pricing: SellPricing) -> SellInference {
        SellInference {
            endpoint_id: "e1".into(),
            recipient: RECIPIENT.into(),
            network: "devnet".into(),
            currencies: vec!["USDC".into()],
            pricing,
            model: "agent".into(),
            title: "Agent".into(),
            description: String::new(),
            upstream_url: String::new(),
            session_cap_usd: 1.0,
            session_idle_close_secs: pay_core::sell_inference::DEFAULT_SESSION_IDLE_CLOSE_SECS,
        }
    }

    /// Every backend the paywall asks for exists and can issue its
    /// challenge with no RPC in reach.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_flat_price_builds_all_four_backends_that_can_challenge() {
        let backends = EndpointBackends::build(
            &sale(SellPricing::PerRequest { usd: 0.02 }),
            &operator(),
            "https://connect.test",
        )
        .unwrap();
        assert_eq!(backends.mpps().len(), 1);
        assert_eq!(backends.session_mpp_handles().len(), 1);
        backends.mpps()[0]
            .charge_with_options("0.02", Default::default())
            .expect("charge challenge");
        backends.session_mpp_handles()[0]
            .challenge_header(Some(20_000))
            .expect("session challenge");
        backends
            .x402_upto()
            .expect("upto backend")
            .payment_required_header("0.02")
            .expect("upto challenge");
        // Batch-settlement has no blockhash cache: its challenge asks RPC
        // for one, which is unreachable here. The error proves the backend
        // is built and wired to the operator's RPC.
        let Err(error) = backends
            .x402_batch()
            .expect("batch backend")
            .payment_required_header("0.02", Some("chat.completions"))
        else {
            panic!("the batch challenge must need RPC, which tests do not have");
        };
        assert!(error.to_string().contains("blockhash"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_price_builds_only_metered_backends() {
        let rates = pay_core::pricing::PricingConfig {
            default: Some(pay_core::pricing::TokenRate {
                input_per_1m: 0.1,
                output_per_1m: 0.3,
            }),
            per_model: Default::default(),
        };
        let backends = EndpointBackends::build(
            &sale(SellPricing::PerToken {
                rates,
                max_usd: 0.25,
            }),
            &operator(),
            "https://connect.test",
        )
        .unwrap();
        assert!(backends.mpps().is_empty());
        assert_eq!(backends.session_mpp_handles().len(), 1);
        assert!(backends.x402_upto().is_some());
        assert!(backends.x402_batch().is_none());
    }

    #[test]
    fn unknown_currencies_and_bad_prices_are_refused() {
        let mut bad = sale(SellPricing::PerRequest { usd: 0.02 });
        bad.currencies = vec!["DOGE".into()];
        let error = EndpointBackends::build(&bad, &operator(), "https://connect.test")
            .err()
            .unwrap();
        assert!(error.contains("DOGE"), "{error}");

        let error = EndpointBackends::build(
            &sale(SellPricing::PerRequest { usd: -1.0 }),
            &operator(),
            "https://connect.test",
        )
        .err()
        .unwrap();
        assert!(error.contains("price per request"), "{error}");
    }

    #[test]
    fn the_operator_needs_all_three_variables_or_none() {
        // Env is process-global; this test owns these three names.
        unsafe {
            std::env::remove_var(KEYPAIR_ENV);
            std::env::remove_var(RPC_URL_ENV);
            std::env::remove_var(SECRET_ENV);
        }
        assert!(Operator::from_env().unwrap().is_none());
        unsafe { std::env::set_var(RPC_URL_ENV, "http://127.0.0.1:8899") };
        let error = Operator::from_env().err().unwrap();
        assert!(
            error.contains(KEYPAIR_ENV) && error.contains(SECRET_ENV),
            "{error}"
        );
        unsafe { std::env::remove_var(RPC_URL_ENV) };
    }
}
