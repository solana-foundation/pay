//! One seller's payment backends: the gate's [`PaymentState`] for one endpoint.
//!
//! `pay server start` builds one set of MPP and x402 servers per process,
//! all paying one recipient. Here every endpoint pays a different seller, so
//! each gets its own set, built from its paywall with the deployment's
//! [`Operator`] as fee payer, session operator and settlement signer. The
//! gate's verification and settlement code then runs unchanged.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use pay_core::PaymentState;
use pay_core::sell_inference::SellInference;
use pay_core::server::session::SessionMpp;
use pay_kit::core::store::ChannelStore;
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

fn validate_session_storage(channel_version: Option<u8>) -> Result<(), String> {
    if channel_version == Some(2) {
        return Err(
            "V2 MPP sessions require durable storage; Connect's memory-backed session \
             backend does not support V2"
                .into(),
        );
    }
    Ok(())
}

/// An RPC endpoint URL. Providers carry credentials in its path or query,
/// so it shows as its origin only; [`RpcUrl::expose`] is for the client that
/// dials it, never for a log line.
#[derive(Clone)]
pub struct RpcUrl(String);

impl RpcUrl {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    /// The full URL, credentials included.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` (an RPC error, say) with the URL replaced by its origin.
    ///
    /// Clients print the URL they parsed, not the string they were given:
    /// `reqwest` lowercases the host, adds a `/` path and percent-encodes.
    /// So every form of the URL is redacted, and so are its secret-bearing
    /// parts on their own (credentials, path segments, query values), raw,
    /// percent-encoded and decoded.
    pub fn redact_in(&self, text: &str) -> String {
        // Shorter fragments are not credentials, and redacting them would
        // mangle ordinary words in the message.
        const MIN_SECRET_LEN: usize = 8;
        const PLACEHOLDER: &str = "\u{0}rpc-url\u{0}";
        // Every byte but alphanumerics, and only what a URL cannot carry raw.
        const STRICT: &percent_encoding::AsciiSet = percent_encoding::NON_ALPHANUMERIC;
        const MINIMAL: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
            .add(b' ')
            .add(b'"')
            .add(b'#')
            .add(b'<')
            .add(b'>')
            .add(b'`')
            .add(b'{')
            .add(b'}');
        let mut whole = vec![self.0.clone()];
        let mut parts = Vec::new();
        if let Ok(url) = url::Url::parse(&self.0) {
            whole.push(url.as_str().to_string());
            let tail = &url[url::Position::BeforePath..];
            if tail != "/" {
                parts.push(tail.to_string());
            }
            parts.push(url.username().to_string());
            parts.extend(url.password().map(str::to_string));
            parts.extend(
                url.path_segments()
                    .into_iter()
                    .flatten()
                    .map(str::to_string),
            );
            parts.extend(url.query().map(str::to_string));
            for (key, value) in url.query_pairs() {
                parts.push(key.into_owned());
                parts.push(value.into_owned());
            }
        }
        let spellings = |forms: Vec<String>| {
            let mut all: Vec<String> = forms
                .into_iter()
                .flat_map(|form| {
                    let decoded = percent_encoding::percent_decode_str(&form)
                        .decode_utf8_lossy()
                        .into_owned();
                    let encode =
                        |set| percent_encoding::utf8_percent_encode(&form, set).to_string();
                    let (strict, minimal) = (encode(STRICT), encode(MINIMAL));
                    let form_encoded: String =
                        url::form_urlencoded::byte_serialize(form.as_bytes()).collect();
                    [form.clone(), decoded, strict, minimal, form_encoded]
                })
                .filter(|form| form.len() >= MIN_SECRET_LEN)
                .collect();
            // Longest first, so a form is never split by one of its parts.
            all.sort_by_key(|form| std::cmp::Reverse(form.len()));
            all.dedup();
            all
        };
        let mut redacted = text.to_string();
        for form in spellings(whole) {
            redacted = redacted.replace(&form, PLACEHOLDER);
        }
        for part in spellings(parts) {
            redacted = redacted.replace(&part, "…");
        }
        redacted.replace(PLACEHOLDER, &self.to_string())
    }
}

impl std::fmt::Display for RpcUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match url::Url::parse(&self.0) {
            Ok(url) if url.has_host() => {
                let origin = url.origin().ascii_serialization();
                let elided = url.path() != "/" || url.query().is_some();
                write!(f, "{origin}{}", if elided { "/…" } else { "" })
            }
            _ => f.write_str("[unparseable RPC URL]"),
        }
    }
}

impl std::fmt::Debug for RpcUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RpcUrl({self})")
    }
}

/// One endpoint's MPP session backends, one per settlement mint, each over
/// its own channel store.
///
/// Owned by the endpoint, not by a pricing generation: a reprice rebuilds the
/// other backends but must keep serving, metering and settling the channels
/// buyers already funded. Each [`SessionMpp`] runs the only lifecycle runloop
/// over its store, so every generation reuses it rather than starting a
/// second runloop that would settle and close the same channels again.
/// Never shared across endpoints, whose channels pay different sellers.
#[derive(Clone, Default)]
pub struct SessionStores(Arc<std::sync::Mutex<HashMap<String, EndpointSession>>>);

struct EndpointSession {
    /// Everything the session backend was built from. A generation asking
    /// for other terms cannot be served by this backend, and must not start
    /// a second one over the same channels.
    terms: String,
    session: Arc<SessionMpp>,
    #[cfg(test)]
    store: Arc<dyn ChannelStore>,
}

impl SessionStores {
    /// The endpoint's session backend for `mint`: built by `build` over a
    /// fresh channel store the first time, the same instance after that.
    fn session_for(
        &self,
        mint: &str,
        terms: String,
        build: impl FnOnce(Arc<dyn ChannelStore>) -> SessionMpp,
    ) -> Result<Arc<SessionMpp>, String> {
        let mut sessions = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = sessions.get(mint) {
            if existing.terms != terms {
                return Err(
                    "MPP session terms cannot change while the endpoint's channels are live".into(),
                );
            }
            return Ok(existing.session.clone());
        }
        let store: Arc<dyn ChannelStore> =
            Arc::new(pay_kit::core::store::MemoryChannelStore::new());
        let session = Arc::new(build(store.clone()));
        sessions.insert(
            mint.to_string(),
            EndpointSession {
                terms,
                session: session.clone(),
                #[cfg(test)]
                store,
            },
        );
        Ok(session)
    }

    #[cfg(test)]
    fn for_mint(&self, mint: &str) -> Arc<dyn ChannelStore> {
        let sessions = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions[mint].store.clone()
    }
}

/// The deployment's signing identity behind every seller's endpoint.
pub struct Operator {
    pub signer: Arc<dyn TransactionSigner>,
    pub rpc_url: RpcUrl,
    pub challenge_binding_secret: String,
    /// Shared by every backend; refreshed by [`Operator::spawn_blockhash_refresh`].
    pub blockhashes: BlockhashCache,
    /// Durable store for x402 `batch-settlement` vouchers, shared by every
    /// endpoint. Accepted vouchers exist only here, so without one the
    /// scheme is not offered.
    pub batch_store: Option<Arc<dyn ChannelStore>>,
    /// Process-local replay store shared by every endpoint's charge servers.
    pub charge_store: Arc<dyn pay_kit::mpp::store::Store>,
}

impl Operator {
    pub fn new(
        signer: Arc<dyn TransactionSigner>,
        rpc_url: impl Into<String>,
        challenge_binding_secret: impl Into<String>,
    ) -> Self {
        Self {
            signer,
            rpc_url: RpcUrl::new(rpc_url),
            challenge_binding_secret: challenge_binding_secret.into(),
            blockhashes: BlockhashCache::new(),
            batch_store: None,
            charge_store: Arc::new(pay_kit::mpp::store::MemoryStore::new()),
        }
    }

    /// Offer x402 `batch-settlement`, recording vouchers in `store`.
    pub fn with_batch_store(mut self, store: Arc<dyn ChannelStore>) -> Self {
        self.batch_store = Some(store);
        self
    }

    /// The schemes `pricing` is offered under by this deployment.
    pub fn schemes(&self, pricing: &pay_core::sell_inference::SellPricing) -> Vec<Scheme> {
        pricing
            .schemes()
            .into_iter()
            .filter(|scheme| *scheme != Scheme::X402BatchSettlement || self.batch_store.is_some())
            .collect()
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
            let rpc = RpcClient::new(rpc_url.expose().to_string());
            match pay_kit::mpp::blockhash::fetch_blockhash_with_slot(&rpc, rpc.commitment()) {
                Ok(entry) => cache.set(entry.blockhash, entry.last_valid_block_height, entry.slot),
                Err(error) => tracing::warn!(
                    error = %rpc_url.redact_in(&error.to_string()),
                    "sell operator blockhash refresh failed"
                ),
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
        Self::build_with_session_stores(sale, operator, public_url, &SessionStores::default())
    }

    /// [`Self::build`] over the endpoint's existing session channel records,
    /// so channels opened under an earlier pricing generation keep working.
    pub fn build_with_session_stores(
        sale: &SellInference,
        operator: &Operator,
        public_url: &str,
        session_stores: &SessionStores,
    ) -> Result<Self, String> {
        let problems = sale.validate();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let schemes = operator.schemes(&sale.pricing);
        let mut api = sale.api_spec();
        for meter in api.endpoints.iter_mut().filter_map(|e| e.metering.as_mut()) {
            if let Some(accepted) = &mut meter.schemes {
                accepted.retain(|scheme| schemes.contains(scheme));
            }
        }
        if !schemes.contains(&Scheme::X402BatchSettlement) {
            api.batch_settlement = None;
        }
        let network = sale.network.as_str();
        let rpc_url = Some(operator.rpc_url.expose().to_string());
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
            let channel_charge =
                pay_core::mpp_channel_profile::server_charge_config(Some(operator.signer.clone()))
                    .map_err(|error| error.to_string())?;
            for mint in &mints {
                let mpp = Mpp::new(pay_kit::mpp::server::Config {
                    recipient: sale.recipient.clone(),
                    currency: mint.clone(),
                    decimals: STABLECOIN_DECIMALS,
                    network: network.to_string(),
                    rpc_url: rpc_url.clone(),
                    // Scoped to this endpoint, never to a payout profile a
                    // clone endpoint can copy.
                    challenge_binding_secret: Some(
                        pay_core::server::session::endpoint_challenge_binding_secret(
                            &operator.challenge_binding_secret,
                            &sale.endpoint_id,
                        ),
                    ),
                    realm: Some(sale.path_prefix()),
                    store: Some(operator.charge_store.clone()),
                    fee_payer: true,
                    fee_payer_signer: Some(operator.signer.clone()),
                    channel_charge: channel_charge.clone(),
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
            let (channel_program, channel_version, channel_hooks) =
                pay_core::mpp_channel_profile::server_profile()
                    .map_err(|error| error.to_string())?;
            validate_session_storage(channel_version)?;
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
                    channel_program: Some(channel_program),
                    channel_version,
                    channel_hooks: channel_hooks.clone(),
                    token_program: Some(token_program),
                };
                let terms = format!(
                    "{}|{}|{}|{}|{:?}",
                    sale.recipient,
                    sale.title,
                    operator.pubkey(),
                    cap_base,
                    spec
                );
                let session = session_stores.session_for(mint, terms, |store| {
                    // Endpoints share the operator's root secret; derive one
                    // per endpoint so a clone's session cannot be replayed
                    // into it.
                    let session = SessionMpp::new_with_channel_store(
                        config,
                        pay_core::server::session::endpoint_challenge_binding_secret(
                            &operator.challenge_binding_secret,
                            &sale.endpoint_id,
                        ),
                        store,
                    )
                    .with_realm(sale.title.clone())
                    .with_blockhash_cache(operator.blockhashes.clone())
                    .with_payment_channel_signer(operator.signer.clone());
                    // Embedded reconciliation: this process pushes watermarks
                    // and closes idle channels. A settlement worker takes over
                    // when pay-connect's session state moves to Redis.
                    session.start_lifecycle_runloop_with_settlement_and_batching(
                        std::time::Duration::from_millis(spec.close_delay_ms),
                        std::time::Duration::from_millis(spec.close_batch_interval_ms),
                        std::time::Duration::from_millis(spec.settlement_interval_ms),
                        pay_core::server::session::SessionLifecycleReconciliation::Embedded,
                    );
                    session
                })?;
                sessions.push(session);
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

        let batch = if let (true, Some(store)) = (
            schemes.contains(&Scheme::X402BatchSettlement),
            operator.batch_store.clone(),
        ) {
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
            let batch = X402BatchSettlement::with_store(cfg, store)
                .map_err(|e| format!("x402 batch-settlement backend: {e}"))?;
            Some(Arc::new(batch))
        } else {
            None
        };

        Ok(Self {
            apis: Arc::new(vec![api]),
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

    /// A stream must use a reusable channel. Keep the same backend instances
    /// and channel stores; only the request's advertised/accepted schemes differ.
    pub fn streaming(&self) -> Self {
        let mut streaming = self.clone();
        let mut apis = self.apis.as_ref().clone();
        for endpoint in apis.iter_mut().flat_map(|api| &mut api.endpoints) {
            if let Some(meter) = &mut endpoint.metering {
                meter.schemes = Some(
                    meter
                        .accepted_schemes()
                        .into_iter()
                        .filter(|scheme| {
                            matches!(scheme, Scheme::MppSession | Scheme::X402BatchSettlement)
                        })
                        .collect(),
                );
            }
        }
        streaming.apis = Arc::new(apis);
        streaming.upto = None;
        streaming
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
mod rpc_url_tests {
    use super::RpcUrl;

    /// Credentials ride in the query (Helius) or the path (QuickNode,
    /// Alchemy); neither reaches a log line.
    #[test]
    fn an_rpc_url_shows_only_its_origin() {
        for (raw, shown) in [
            (
                "https://mainnet.helius-rpc.com/?api-key=secret-1",
                "https://mainnet.helius-rpc.com/…",
            ),
            (
                "https://x.solana-mainnet.quiknode.pro/secret-2/",
                "https://x.solana-mainnet.quiknode.pro/…",
            ),
            ("http://127.0.0.1:8899", "http://127.0.0.1:8899"),
            ("not a url secret-3", "[unparseable RPC URL]"),
        ] {
            let url = RpcUrl::new(raw);
            assert_eq!(url.to_string(), shown);
            assert!(!format!("{url:?}").contains("secret"));
            assert_eq!(url.expose(), raw);
            let error = format!("error sending request for url ({raw})");
            assert!(!url.redact_in(&error).contains("secret"), "{raw}");
        }
    }

    /// reqwest prints the URL it parsed: host lowercased, a `/` path added,
    /// unsafe bytes percent-encoded. None of those spellings leaks either.
    #[test]
    fn every_spelling_of_an_rpc_url_is_redacted() {
        let raw = "https://Mainnet.Helius-RPC.com?api-key=secret-1 x&id=7";
        let url = RpcUrl::new(raw);
        let parsed = url::Url::parse(raw).unwrap();
        assert_ne!(parsed.as_str(), raw);
        for leaked in [
            format!("error sending request for url ({parsed})"),
            format!("request to {} failed", &parsed[url::Position::BeforePath..]),
            "upstream rejected api-key secret-1 x".to_string(),
            "upstream rejected api-key secret-1%20x".to_string(),
            "upstream rejected api-key secret-1+x".to_string(),
        ] {
            let redacted = url.redact_in(&leaked);
            assert!(!redacted.contains("secret"), "{leaked} -> {redacted}");
        }
        assert_eq!(
            url.redact_in(&format!("error sending request for url ({parsed})")),
            "error sending request for url (https://mainnet.helius-rpc.com/…)"
        );

        let path = RpcUrl::new("https://user:hunter2-pass@x.quiknode.pro/secret-2/");
        let redacted = path.redact_in("auth hunter2-pass failed at /secret-2/ via secret-2");
        assert!(
            !redacted.contains("secret") && !redacted.contains("hunter2"),
            "{redacted}"
        );
        // Messages that carry no part of the URL are left alone.
        assert_eq!(
            path.redact_in("connection refused (os error 61)"),
            "connection refused (os error 61)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_core::sell_inference::SellPricing;

    const RECIPIENT: &str = "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP";

    #[test]
    fn connect_memory_sessions_reject_v2_and_preserve_v1() {
        assert!(validate_session_storage(None).is_ok());
        assert!(validate_session_storage(Some(1)).is_ok());
        assert!(
            validate_session_storage(Some(2))
                .unwrap_err()
                .contains("Connect")
        );
    }

    #[test]
    fn batch_settlement_is_not_offered_without_a_durable_store() {
        let mut bare = operator();
        bare.batch_store = None;
        let pricing = SellPricing::PerRequest { usd: 0.02 };
        assert!(
            !bare
                .schemes(&pricing)
                .contains(&Scheme::X402BatchSettlement)
        );
        assert!(
            operator()
                .schemes(&pricing)
                .contains(&Scheme::X402BatchSettlement)
        );
        let backends =
            EndpointBackends::build(&sale(pricing), &bare, "https://connect.test").unwrap();
        assert!(backends.x402_batch().is_none());
        assert!(backends.spec().batch_settlement.is_none());
        let meter = backends
            .spec()
            .endpoints
            .iter()
            .find_map(|endpoint| endpoint.metering.as_ref())
            .unwrap();
        assert!(
            !meter
                .accepted_schemes()
                .contains(&Scheme::X402BatchSettlement)
        );
        assert!(meter.accepted_schemes().contains(&Scheme::MppSession));
    }

    #[test]
    fn rebuilt_backends_share_the_operators_batch_store() {
        let operator = operator();
        let pricing = SellPricing::PerRequest { usd: 0.02 };
        let first =
            EndpointBackends::build(&sale(pricing.clone()), &operator, "https://connect.test")
                .unwrap();
        let repriced = EndpointBackends::build(
            &sale(SellPricing::PerRequest { usd: 0.03 }),
            &operator,
            "https://connect.test",
        )
        .unwrap();
        let store = operator.batch_store.as_ref().unwrap();
        assert!(Arc::ptr_eq(first.x402_batch().unwrap().store(), store));
        assert!(Arc::ptr_eq(repriced.x402_batch().unwrap().store(), store));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repriced_backends_keep_the_endpoints_session_channels() {
        let operator = operator();
        let stores = SessionStores::default();
        let build = |usd, stores: &SessionStores| {
            EndpointBackends::build_with_session_stores(
                &sale(SellPricing::PerRequest { usd }),
                &operator,
                "https://connect.test",
                stores,
            )
            .unwrap()
        };
        let first = build(0.02, &stores);
        let session = first.sessions.first().unwrap().clone();
        let channel = solana_pubkey::Pubkey::new_unique().to_string();
        stores
            .for_mint(session.currency())
            .put_channel(
                &channel,
                pay_core::server::session::test_channel_state(
                    &channel,
                    1_000_000,
                    operator.pubkey(),
                    "operator",
                    "opening",
                    solana_pubkey::Pubkey::new_unique().to_string(),
                    None,
                ),
            )
            .await
            .unwrap();
        session.touch_channel(channel.clone()).await.unwrap();

        // A reprice serves the channel the buyer already funded.
        let repriced = build(0.03, &stores);
        repriced
            .sessions
            .first()
            .unwrap()
            .touch_channel(channel.clone())
            .await
            .unwrap();
        // One backend, so one lifecycle runloop, settles the endpoint's channels.
        assert!(Arc::ptr_eq(
            first.sessions.first().unwrap(),
            repriced.sessions.first().unwrap()
        ));
        // Another endpoint's stores never see it.
        let other = build(0.03, &SessionStores::default());
        assert!(
            other
                .sessions
                .first()
                .unwrap()
                .touch_channel(channel)
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_terms_cannot_change_under_live_channels() {
        let operator = operator();
        let stores = SessionStores::default();
        let build = |sale: &SellInference| {
            EndpointBackends::build_with_session_stores(
                sale,
                &operator,
                "https://connect.test",
                &stores,
            )
        };
        let mut terms = sale(SellPricing::PerRequest { usd: 0.02 });
        build(&terms).unwrap();
        // Same session terms at a new price: served by the same backend.
        terms.pricing = SellPricing::PerRequest { usd: 0.05 };
        build(&terms).unwrap();
        terms.session_cap_usd *= 2.0;
        let error = build(&terms).err().unwrap();
        assert!(error.contains("session terms"), "{error}");
    }

    #[test]
    fn streaming_view_keeps_channel_backends_but_removes_upto() {
        let backends = EndpointBackends::build(
            &sale(SellPricing::PerRequest { usd: 0.02 }),
            &operator(),
            "https://connect.test",
        )
        .unwrap();
        let streaming = backends.streaming();
        let meter = streaming
            .spec()
            .endpoints
            .iter()
            .find_map(|endpoint| endpoint.metering.as_ref())
            .unwrap();
        assert_eq!(
            meter.accepted_schemes(),
            vec![Scheme::MppSession, Scheme::X402BatchSettlement]
        );
        assert!(streaming.x402_upto().is_none());
        assert!(backends.x402_upto().is_some());
        assert!(Arc::ptr_eq(
            backends.sessions.first().unwrap(),
            streaming.sessions.first().unwrap()
        ));
        assert!(Arc::ptr_eq(
            backends.batch.as_ref().unwrap(),
            streaming.batch.as_ref().unwrap()
        ));
    }

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
        )
        .with_batch_store(Arc::new(pay_kit::core::store::MemoryChannelStore::new()));
        operator
            .blockhashes
            .set("11111111111111111111111111111111".into(), 1_000, 1);
        operator
    }

    /// A clone endpoint copying another's public profile cannot have its
    /// session challenges accepted by the original.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_challenges_do_not_cross_endpoints() {
        let operator = operator();
        let backends = |endpoint_id: &str| {
            let mut sale = sale(SellPricing::PerRequest { usd: 0.02 });
            sale.endpoint_id = endpoint_id.into();
            EndpointBackends::build(&sale, &operator, "https://connect.test").unwrap()
        };
        let (target, clone) = (backends("e1"), backends("e2"));
        let credential = |backends: &EndpointBackends| {
            pay_kit::mpp::PaymentCredential::new(
                backends.session_mpp_handles()[0]
                    .challenge(None)
                    .unwrap()
                    .to_echo(),
                serde_json::json!({ "action": "mystery" }),
            )
        };
        let session = &target.session_mpp_handles()[0];
        let error = session
            .process_credential(credential(&clone))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not issue"), "{error}");
        let error = session
            .process_credential(credential(&target))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Unrecognized session action"),
            "{error}"
        );
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
    async fn a_flat_price_builds_answered_only_backends_that_can_challenge() {
        let backends = EndpointBackends::build(
            &sale(SellPricing::PerRequest { usd: 0.02 }),
            &operator(),
            "https://connect.test",
        )
        .unwrap();
        assert!(backends.mpps().is_empty());
        assert_eq!(backends.session_mpp_handles().len(), 1);
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
