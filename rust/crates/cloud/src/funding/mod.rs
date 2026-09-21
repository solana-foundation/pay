//! Buying the first stablecoins with a card, through Coinflow.
//!
//! pay is the Coinflow merchant and the user is the customer. This module
//! owns the merchant API key and does the three server-side steps a
//! purchase needs: a session key for the payer, a fee quote, and a hosted
//! checkout link whose amount, settlement and destination are fixed by the
//! server. The `/fund` page embeds that link in an iframe; card entry never
//! touches pay. Coinflow reports progress through webhooks, which the page
//! and the CLI read back through [`status`].
//!
//! Settlement goes to the customer's own address when
//! [`Config::settle_to_customer`] is on. Coinflow enables that per merchant
//! (it is off on the sandbox merchant today); until then purchases settle to
//! pay's merchant wallet, and the start response says so.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::AppState;
use crate::onboard::{ApiError, validate_callback, validate_state};

/// Smallest purchase Coinflow accepts.
pub const MIN_CENTS: u64 = 200;
/// Largest purchase this page offers. A first top-up, not a treasury move.
pub const MAX_CENTS: u64 = 100_000;
/// How long a minted checkout link stays valid.
pub const LINK_TTL_MINUTES: u32 = 30;
/// How long a payment record is kept for [`status`] polling.
pub const PAYMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Most payment records held at once; expired ones are swept when full.
pub const MAX_PAYMENTS: usize = 8192;
/// Provider-heavy checkout starts allowed at once across the service.
pub const MAX_CONCURRENT_CHECKOUTS: usize = 8;
/// Minimum interval between checkout starts for the same destination.
pub const CHECKOUT_COOLDOWN: Duration = Duration::from_secs(5);
/// Most destination throttles retained at once.
pub const MAX_CHECKOUT_CLIENTS: usize = 8192;

/// Card rails offered on the page. Bank rails need a payer identity and
/// take days; PayPal and Venmo are not configured on the merchant.
pub const PAYMENT_METHODS: &[&str] = &["card", "applePay", "googlePay"];
/// Short card-statement label used when the issuing bank supports dynamic descriptors.
pub const STATEMENT_DESCRIPTOR: &str = "PAY.SH";

pub const API_KEY_ENV: &str = "COINFLOW_API_KEY";
pub const ENV_ENV: &str = "COINFLOW_ENV";
pub const MERCHANT_ID_ENV: &str = "COINFLOW_MERCHANT_ID";
pub const WEBHOOK_KEY_ENV: &str = "COINFLOW_WEBHOOK_KEY";
pub const SETTLE_TO_CUSTOMER_ENV: &str = "COINFLOW_SETTLE_TO_CUSTOMER";
pub const API_URL_ENV: &str = "COINFLOW_API_URL";
/// Card-entry mode. Only `hosted` is accepted until the pages app implements
/// Coinflow's direct SDK and the deployment meets its PCI requirements.
pub const CARD_ENTRY_ENV: &str = "COINFLOW_CARD_ENTRY";

/// Coinflow environment. Decides the API host and the origin of the hosted
/// checkout page the browser must trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Env {
    Sandbox,
    Production,
}

impl Env {
    fn api_url(self) -> &'static str {
        match self {
            Env::Sandbox => "https://api-sandbox.coinflow.cash",
            Env::Production => "https://api.coinflow.cash",
        }
    }

    fn checkout_origin(self) -> &'static str {
        match self {
            Env::Sandbox => "https://sandbox.coinflow.cash",
            Env::Production => "https://coinflow.cash",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sandbox" => Some(Env::Sandbox),
            "prod" | "production" => Some(Env::Production),
            _ => None,
        }
    }
}

/// Merchant configuration. Built from the environment by [`Config::from_env`]
/// or directly in tests.
#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub env: Env,
    pub merchant_id: String,
    /// The `Authorization` value Coinflow sends with webhooks, set in their
    /// dashboard. Without it the webhook endpoint refuses every delivery.
    pub webhook_key: Option<String>,
    /// Lock the customer's address as the USDC settlement destination.
    /// Requires Coinflow to enable third-party settlement on the merchant.
    pub settle_to_customer: bool,
    /// Coinflow API base; the environment's host unless overridden for tests.
    pub api_url: String,
    /// How pages collect the card.
    pub card_entry: CardEntry,
}

/// Where card details are typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CardEntry {
    /// Coinflow's hosted checkout page in an iframe; works from any origin.
    Hosted,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{ENV_ENV} must be `sandbox` or `production`, got `{0}`")]
    BadEnv(String),
    #[error("{MERCHANT_ID_ENV} is required when {API_KEY_ENV} is set")]
    MissingMerchant,
    #[error("{CARD_ENTRY_ENV} must be `hosted`, got `{0}`")]
    BadCardEntry(String),
    #[error("{CARD_ENTRY_ENV}=direct is not supported yet; use `hosted`")]
    DirectCardEntryUnsupported,
}

impl Config {
    /// `None` when `COINFLOW_API_KEY` is unset: funding is off and the
    /// endpoints answer 503. The merchant id has no default on purpose.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        let Some(api_key) = non_empty(std::env::var(API_KEY_ENV).ok()) else {
            return Ok(None);
        };
        let env = match non_empty(std::env::var(ENV_ENV).ok()) {
            None => Env::Sandbox,
            Some(raw) => Env::parse(&raw).ok_or(ConfigError::BadEnv(raw))?,
        };
        let merchant_id =
            non_empty(std::env::var(MERCHANT_ID_ENV).ok()).ok_or(ConfigError::MissingMerchant)?;
        let settle_to_customer = std::env::var(SETTLE_TO_CUSTOMER_ENV)
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"));
        let card_entry = match non_empty(std::env::var(CARD_ENTRY_ENV).ok()) {
            None => CardEntry::Hosted,
            Some(raw) => match raw.to_ascii_lowercase().as_str() {
                "hosted" => CardEntry::Hosted,
                "direct" => return Err(ConfigError::DirectCardEntryUnsupported),
                _ => return Err(ConfigError::BadCardEntry(raw)),
            },
        };
        Ok(Some(Self {
            api_key,
            env,
            merchant_id,
            webhook_key: non_empty(std::env::var(WEBHOOK_KEY_ENV).ok()),
            settle_to_customer,
            api_url: non_empty(std::env::var(API_URL_ENV).ok())
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| env.api_url().to_string()),
            card_entry,
        }))
    }

    pub fn checkout_origin(&self) -> &'static str {
        self.env.checkout_origin()
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ── Coinflow client ────────────────────────────────────────────────────────

/// The Coinflow merchant API calls a purchase needs.
struct Coinflow {
    http: reqwest::Client,
    cfg: Config,
}

#[derive(Debug, thiserror::Error)]
pub enum CoinflowError {
    #[error("Coinflow is unreachable: {0}")]
    Unreachable(#[from] reqwest::Error),
    #[error("Coinflow answered {status} to {call}: {body}")]
    Rejected {
        call: &'static str,
        status: u16,
        body: String,
    },
    #[error("Coinflow's {call} response is missing `{field}`")]
    Shape {
        call: &'static str,
        field: &'static str,
    },
}

/// Fee quote for one card purchase, in USD cents. What the customer is
/// charged is `total_cents`; what settles is `subtotal_cents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Quote {
    pub subtotal_cents: u64,
    pub card_fee_cents: u64,
    pub protection_fee_cents: u64,
    /// Gas, network and FX fees, normally zero for USDC.
    pub other_fee_cents: u64,
    pub total_cents: u64,
}

impl Coinflow {
    fn new(cfg: Config) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            cfg,
        }
    }

    async fn check(
        call: &'static str,
        response: reqwest::Response,
    ) -> Result<serde_json::Value, CoinflowError> {
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(CoinflowError::Rejected {
                call,
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        serde_json::from_str(&body).map_err(|_| CoinflowError::Shape {
            call,
            field: "json body",
        })
    }

    /// A 24-hour payer session, identified by the destination address.
    async fn session_key(&self, user_id: &str) -> Result<String, CoinflowError> {
        let response = self
            .http
            .get(format!("{}/api/auth/session-key", self.cfg.api_url))
            .header("Authorization", &self.cfg.api_key)
            .header("x-coinflow-auth-user-id", user_id)
            .header("accept", "application/json")
            .send()
            .await?;
        let json = Self::check("session-key", response).await?;
        json["key"]
            .as_str()
            .map(str::to_string)
            .ok_or(CoinflowError::Shape {
                call: "session-key",
                field: "key",
            })
    }

    async fn totals(&self, session_key: &str, cents: u64) -> Result<Quote, CoinflowError> {
        let response = self
            .http
            .post(format!(
                "{}/api/checkout/totals/{}",
                self.cfg.api_url, self.cfg.merchant_id
            ))
            .header("x-coinflow-auth-session-key", session_key)
            .json(&serde_json::json!({
                "subtotal": { "cents": cents, "currency": "USD" },
                "settlementType": "USDC",
                "blockchain": "solana",
            }))
            .send()
            .await?;
        let json = Self::check("totals", response).await?;
        let card = &json["card"];
        let cents_of = |field: &'static str| -> Result<u64, CoinflowError> {
            card[field]["cents"].as_u64().ok_or(CoinflowError::Shape {
                call: "totals",
                field,
            })
        };
        let subtotal_cents = cents_of("subtotal")?;
        let card_fee_cents = cents_of("creditCardFees")?;
        let protection_fee_cents = cents_of("chargebackProtectionFees")?;
        let total_cents = cents_of("total")?;
        Ok(Quote {
            subtotal_cents,
            card_fee_cents,
            protection_fee_cents,
            other_fee_cents: total_cents
                .saturating_sub(subtotal_cents + card_fee_cents + protection_fee_cents),
            total_cents,
        })
    }

    /// A hosted checkout link with every commercial parameter fixed here.
    async fn checkout_link(
        &self,
        user_id: &str,
        body: &serde_json::Value,
    ) -> Result<String, CoinflowError> {
        let response = self
            .http
            .post(format!("{}/api/checkout/link", self.cfg.api_url))
            .header("Authorization", &self.cfg.api_key)
            .header("x-coinflow-auth-user-id", user_id)
            .json(body)
            .send()
            .await?;
        let json = Self::check("checkout-link", response).await?;
        json["link"]
            .as_str()
            .map(str::to_string)
            .ok_or(CoinflowError::Shape {
                call: "checkout-link",
                field: "link",
            })
    }
}

/// The checkout form's look, matched to the pay.sh receipt panel it sits in.
fn checkout_theme() -> serde_json::Value {
    serde_json::json!({
        "style": "rounded",
        "font": "Inter",
        "fontSize": "14px",
        "fontWeight": "500",
        "background": "#0c0c0f",
        "backgroundAccent": "#18181b",
        "backgroundAccent2": "#242428",
        "cardBackground": "#18181b",
        "primary": "#fafafa",
        "ctaColor": "#fafafa",
        "textColor": "#fafafa",
        "textColorAccent": "#a1a1aa",
        "textColorAction": "#09090b",
        "placeholderColor": "#71717a",
        "showCardIcon": true,
    })
}

// ── Payment records ────────────────────────────────────────────────────────

/// Where a payment stands, as told by Coinflow's webhooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    /// Nothing heard yet, or the card is authorized and awaiting settlement.
    Pending,
    /// The card was charged; USDC has not moved yet.
    Authorized,
    /// Coinflow settled the payment into the merchant balance.
    Settled,
    /// USDC is on chain at the destination; `signature` is the transaction.
    Disbursed,
    /// Declined, refused by fraud screening, or charged back.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaymentRecord {
    pub payment_id: String,
    pub status: PaymentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip)]
    pub updated_at: Instant,
}

/// Coinflow event types this module reacts to. Everything else is
/// acknowledged and ignored.
fn status_for_event(event_type: &str) -> Option<PaymentStatus> {
    match event_type {
        "Card Payment Authorized" => Some(PaymentStatus::Authorized),
        "Settled" => Some(PaymentStatus::Settled),
        "Disbursed Funds" => Some(PaymentStatus::Disbursed),
        "Card Payment Declined"
        | "Card Payment Suspected Fraud"
        | "Card Payment Chargeback Opened"
        | "Card Payment Chargeback Lost"
        | "Refund" => Some(PaymentStatus::Failed),
        _ => None,
    }
}

/// Status only moves forward; a late `Authorized` after `Disbursed` (webhooks
/// retry out of order) must not roll the record back.
fn rank(status: PaymentStatus) -> u8 {
    match status {
        PaymentStatus::Pending => 0,
        PaymentStatus::Authorized => 1,
        PaymentStatus::Settled => 2,
        PaymentStatus::Disbursed => 3,
        PaymentStatus::Failed => 4,
    }
}

/// Funding state held by [`AppState`]: the Coinflow client and the payment
/// records fed by webhooks.
pub struct Funding {
    coinflow: Coinflow,
    payments: Mutex<HashMap<String, PaymentRecord>>,
    checkout_slots: Arc<Semaphore>,
    checkout_clients: Mutex<HashMap<String, Instant>>,
}

impl Funding {
    pub fn new(cfg: Config) -> Self {
        Self {
            coinflow: Coinflow::new(cfg),
            payments: Mutex::default(),
            checkout_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CHECKOUTS)),
            checkout_clients: Mutex::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.coinflow.cfg
    }

    fn admit_checkout(&self, client: &str) -> Result<OwnedSemaphorePermit, ApiError> {
        let permit = self
            .checkout_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "Too many checkouts are starting. Try again in a moment.",
                )
            })?;
        let now = Instant::now();
        let mut clients = self.checkout_clients.lock().unwrap();
        if clients.len() >= MAX_CHECKOUT_CLIENTS {
            clients
                .retain(|_, started| now.saturating_duration_since(*started) < CHECKOUT_COOLDOWN);
            if clients.len() >= MAX_CHECKOUT_CLIENTS && !clients.contains_key(client) {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "Too many checkouts were started recently. Try again in a moment.",
                ));
            }
        }
        if clients
            .get(client)
            .is_some_and(|started| now.saturating_duration_since(*started) < CHECKOUT_COOLDOWN)
        {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "A checkout was just started for this wallet. Try again in a few seconds.",
            ));
        }
        clients.insert(client.to_string(), now);
        Ok(permit)
    }

    /// Apply a webhook event. Returns the record when the event changed
    /// anything.
    pub fn record_event(&self, event: &WebhookEvent) -> Option<PaymentRecord> {
        let status = status_for_event(&event.event_type)?;
        let now = Instant::now();
        let mut payments = self.payments.lock().unwrap();
        if payments.len() >= MAX_PAYMENTS && !payments.contains_key(&event.data.id) {
            payments.retain(|_, p| now.saturating_duration_since(p.updated_at) <= PAYMENT_TTL);
            if payments.len() >= MAX_PAYMENTS {
                tracing::warn!("payment record store is full; dropping webhook");
                return None;
            }
        }
        let record = payments
            .entry(event.data.id.clone())
            .or_insert_with(|| PaymentRecord {
                payment_id: event.data.id.clone(),
                status: PaymentStatus::Pending,
                signature: None,
                wallet: None,
                address: None,
                updated_at: now,
            });
        if rank(status) < rank(record.status) {
            return None;
        }
        record.status = status;
        record.updated_at = now;
        if let Some(signature) = event.data.signature.as_deref().filter(|s| !s.is_empty()) {
            record.signature = Some(signature.to_string());
        }
        if let Some(wallet) = event.data.wallet.as_deref().filter(|s| !s.is_empty()) {
            record.wallet = Some(wallet.to_string());
        }
        if let Some(address) = event
            .data
            .webhook_info
            .as_ref()
            .and_then(|info| info.address.as_deref())
        {
            record.address = Some(address.to_string());
        }
        Some(record.clone())
    }

    pub fn payment(&self, payment_id: &str) -> Option<PaymentRecord> {
        let payments = self.payments.lock().unwrap();
        let record = payments.get(payment_id)?;
        (Instant::now().saturating_duration_since(record.updated_at) <= PAYMENT_TTL)
            .then(|| record.clone())
    }
}

// ── HTTP ──────────────────────────────────────────────────────────────────

/// Body of `POST /api/fund/start`.
#[derive(Debug, Deserialize)]
pub struct StartRequest {
    /// Solana address receiving the USDC.
    pub address: String,
    /// Purchase amount in USD cents, before fees.
    pub cents: u64,
    /// CLI loopback URL the page returns to when the purchase completes.
    #[serde(default)]
    pub callback: Option<String>,
    /// CLI CSRF state echoed on that return.
    #[serde(default)]
    pub state: Option<String>,
}

/// Who receives the settled USDC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Settlement {
    /// The address the customer gave.
    Customer,
    /// pay's merchant wallet, until Coinflow enables customer settlement.
    Merchant,
}

#[derive(Debug, Serialize)]
pub struct StartResponse {
    /// Hosted checkout URL, for an iframe on the page.
    pub link: String,
    /// Origin the iframe's messages must come from.
    pub checkout_origin: &'static str,
    pub env: Env,
    pub quote: Quote,
    pub settlement: Settlement,
    pub payment_methods: &'static [&'static str],
    pub expires_in_minutes: u32,
    /// Whether the page should embed the hosted checkout or render its
    /// own card fields.
    pub card_entry: CardEntry,
}

fn funding_of(state: &AppState) -> Result<&Funding, ApiError> {
    state.funding().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "funding_disabled",
            format!("Card purchases are not configured on this server ({API_KEY_ENV} unset)."),
        )
    })
}

/// A base58 Solana public key: 32 bytes.
pub fn validate_address(address: &str) -> Result<(), ApiError> {
    let ok = bs58::decode(address)
        .into_vec()
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_address",
            "`address` must be a Solana public key.",
        ))
    }
}

pub fn validate_cents(cents: u64) -> Result<(), ApiError> {
    if (MIN_CENTS..=MAX_CENTS).contains(&cents) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_amount",
            format!(
                "`cents` must be between {MIN_CENTS} and {MAX_CENTS} (${:.2} to ${:.2}).",
                MIN_CENTS as f64 / 100.0,
                MAX_CENTS as f64 / 100.0
            ),
        ))
    }
}

/// `POST /api/fund/start`
pub async fn start(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StartResponse>, ApiError> {
    let funding = funding_of(&state)?;
    let req: StartRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("invalid_json", format!("Invalid JSON body: {e}")))?;
    validate_address(&req.address)?;
    validate_cents(req.cents)?;
    if let Some(callback) = req.callback.as_deref() {
        validate_callback(callback)?;
    }
    if let Some(state) = req.state.as_deref() {
        validate_state(state)?;
    }

    // Admission happens before the first merchant-authenticated request.
    // The permit bounds global provider work; the destination is the stable
    // client key available to both browser and CLI callers.
    let _checkout_permit = funding.admit_checkout(&req.address)?;

    let cfg = funding.config();
    let coinflow = &funding.coinflow;
    let session_key = coinflow
        .session_key(&req.address)
        .await
        .map_err(from_coinflow)?;
    let quote = coinflow
        .totals(&session_key, req.cents)
        .await
        .map_err(from_coinflow)?;

    let mut webhook_info = serde_json::json!({ "address": req.address });
    if let Some(state) = req.state.as_deref() {
        webhook_info["state"] = serde_json::Value::String(state.to_string());
    }
    let mut link_body = serde_json::json!({
        "subtotal": { "cents": req.cents, "currency": "USD" },
        "blockchain": "solana",
        "settlementType": "USDC",
        "allowedPaymentMethods": PAYMENT_METHODS,
        "statementDescriptor": STATEMENT_DESCRIPTOR,
        "webhookInfo": webhook_info,
        "expiresIn": { "minutes": LINK_TTL_MINUTES },
        "theme": checkout_theme(),
    });
    let settlement = if cfg.settle_to_customer {
        link_body["destination"] = serde_json::Value::String(req.address.clone());
        Settlement::Customer
    } else {
        Settlement::Merchant
    };
    let link = coinflow
        .checkout_link(&req.address, &link_body)
        .await
        .map_err(from_coinflow)?;

    tracing::info!(
        address = %req.address,
        cents = req.cents,
        total_cents = quote.total_cents,
        ?settlement,
        "funding checkout minted"
    );
    Ok(Json(StartResponse {
        link,
        checkout_origin: cfg.checkout_origin(),
        env: cfg.env,
        quote,
        settlement,
        payment_methods: PAYMENT_METHODS,
        expires_in_minutes: LINK_TTL_MINUTES,
        card_entry: cfg.card_entry,
    }))
}

fn from_coinflow(err: CoinflowError) -> ApiError {
    tracing::warn!(%err, "coinflow call failed");
    match err {
        CoinflowError::Unreachable(_) => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "provider_unreachable",
            "Coinflow could not be reached. Try again in a moment.",
        ),
        CoinflowError::Rejected { .. } | CoinflowError::Shape { .. } => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "Coinflow could not start this purchase. Try again in a moment.",
        ),
    }
}

/// One Coinflow webhook delivery. Fields beyond these are ignored.
#[derive(Debug, Deserialize)]
pub struct WebhookEvent {
    #[serde(rename = "eventType")]
    pub event_type: String,
    pub data: WebhookData,
}

#[derive(Debug, Deserialize)]
pub struct WebhookData {
    /// Coinflow payment id.
    pub id: String,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default, rename = "webhookInfo")]
    pub webhook_info: Option<WebhookInfo>,
}

/// Our passthrough, set on the checkout link.
#[derive(Debug, Deserialize)]
pub struct WebhookInfo {
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// `POST /api/fund/webhook`: Coinflow's payment events. Authenticated by the
/// `Authorization` value configured in their dashboard.
pub async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let funding = funding_of(&state)?;
    let Some(expected) = funding.config().webhook_key.as_deref() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhooks_disabled",
            format!("Webhook deliveries are refused until {WEBHOOK_KEY_ENV} is set."),
        ));
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Webhook authorization does not match.",
        ));
    }
    let event: WebhookEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("invalid_json", format!("Invalid webhook body: {e}")))?;
    match funding.record_event(&event) {
        Some(record) => tracing::info!(
            payment_id = %record.payment_id,
            status = ?record.status,
            signature = record.signature.as_deref().unwrap_or("-"),
            "funding webhook applied"
        ),
        None => tracing::debug!(event_type = %event.event_type, "funding webhook ignored"),
    }
    Ok(Json(serde_json::json!({ "received": true })))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Response of `GET /api/fund/{payment_id}`. Unknown ids are `pending`:
/// the webhook may simply not have arrived yet.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub payment_id: String,
    pub status: PaymentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
}

pub fn validate_payment_id(id: &str) -> Result<(), ApiError> {
    let ok = (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "invalid_payment_id",
            "`payment_id` must be 1 to 64 letters, digits, `-` or `_`.",
        ))
    }
}

/// `GET /api/fund/{payment_id}`
pub async fn status(
    State(state): State<AppState>,
    Path(payment_id): Path<String>,
) -> Result<Json<StatusResponse>, ApiError> {
    let funding = funding_of(&state)?;
    validate_payment_id(&payment_id)?;
    let record = funding.payment(&payment_id);
    Ok(Json(StatusResponse {
        payment_id,
        status: record.as_ref().map_or(PaymentStatus::Pending, |r| r.status),
        signature: record.as_ref().and_then(|r| r.signature.clone()),
        wallet: record.and_then(|r| r.wallet),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(api_url: &str) -> Config {
        Config {
            card_entry: CardEntry::Hosted,
            api_key: "coinflow_sandbox_test".to_string(),
            env: Env::Sandbox,
            merchant_id: "solana-foundation".to_string(),
            webhook_key: Some("whsec-test".to_string()),
            settle_to_customer: false,
            api_url: api_url.to_string(),
        }
    }

    fn event(event_type: &str, id: &str, signature: Option<&str>) -> WebhookEvent {
        WebhookEvent {
            event_type: event_type.to_string(),
            data: WebhookData {
                id: id.to_string(),
                signature: signature.map(str::to_string),
                wallet: Some("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z".to_string()),
                webhook_info: Some(WebhookInfo {
                    address: Some("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z".to_string()),
                    state: None,
                }),
            },
        }
    }

    #[test]
    fn checkout_admission_limits_each_wallet_and_global_concurrency() {
        let funding = Funding::new(test_config("http://127.0.0.1:1"));
        let first = funding.admit_checkout("wallet_0").unwrap();
        let repeated = funding.admit_checkout("wallet_0").unwrap_err();
        assert_eq!(repeated.status, StatusCode::TOO_MANY_REQUESTS);

        let mut permits = vec![first];
        for i in 1..MAX_CONCURRENT_CHECKOUTS {
            permits.push(funding.admit_checkout(&format!("wallet_{i}")).unwrap());
        }
        let busy = funding.admit_checkout("one_more").unwrap_err();
        assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn env_parses_both_names_for_production() {
        assert_eq!(Env::parse("sandbox"), Some(Env::Sandbox));
        assert_eq!(Env::parse("prod"), Some(Env::Production));
        assert_eq!(Env::parse("Production"), Some(Env::Production));
        assert_eq!(Env::parse("staging"), None);
        assert_eq!(
            Env::Sandbox.checkout_origin(),
            "https://sandbox.coinflow.cash"
        );
        assert_eq!(Env::Production.api_url(), "https://api.coinflow.cash");
    }

    #[test]
    fn address_and_amount_validation() {
        assert!(validate_address("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z").is_ok());
        assert!(validate_address("not-an-address").is_err());
        assert!(validate_address("").is_err());
        // 31 bytes decodes but is not a key.
        assert!(validate_address(&bs58::encode([7u8; 31]).into_string()).is_err());

        assert!(validate_cents(MIN_CENTS).is_ok());
        assert!(validate_cents(MAX_CENTS).is_ok());
        assert!(validate_cents(MIN_CENTS - 1).is_err());
        assert!(validate_cents(MAX_CENTS + 1).is_err());

        assert!(validate_payment_id("3f9c-ab_12").is_ok());
        assert!(validate_payment_id("").is_err());
        assert!(validate_payment_id("has space").is_err());
        assert!(validate_payment_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn webhooks_move_a_payment_forward_and_never_back() {
        let funding = Funding::new(test_config("http://127.0.0.1:1"));
        assert!(funding.payment("p1").is_none());

        let record = funding
            .record_event(&event("Card Payment Authorized", "p1", None))
            .unwrap();
        assert_eq!(record.status, PaymentStatus::Authorized);

        let record = funding
            .record_event(&event("Disbursed Funds", "p1", Some("5ig")))
            .unwrap();
        assert_eq!(record.status, PaymentStatus::Disbursed);
        assert_eq!(record.signature.as_deref(), Some("5ig"));
        assert_eq!(
            record.address.as_deref(),
            Some("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z")
        );

        // A retried, out-of-order authorization changes nothing.
        assert!(
            funding
                .record_event(&event("Card Payment Authorized", "p1", None))
                .is_none()
        );
        let current = funding.payment("p1").unwrap();
        assert_eq!(current.status, PaymentStatus::Disbursed);
        assert_eq!(current.signature.as_deref(), Some("5ig"));

        // Unrelated event types are ignored.
        assert!(
            funding
                .record_event(&event("Subscription Created", "p2", None))
                .is_none()
        );
        assert!(funding.payment("p2").is_none());
    }

    #[test]
    fn a_failure_wins_over_everything() {
        let funding = Funding::new(test_config("http://127.0.0.1:1"));
        funding
            .record_event(&event("Disbursed Funds", "p1", Some("5ig")))
            .unwrap();
        let record = funding
            .record_event(&event("Card Payment Chargeback Lost", "p1", None))
            .unwrap();
        assert_eq!(record.status, PaymentStatus::Failed);
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    // ── HTTP, against a mock Coinflow ──────────────────────────────────

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{Method, header};
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    const ADDRESS: &str = "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z";

    /// One request the mock Coinflow saw: path, headers of interest, body.
    type SeenCall = (String, HashMap<String, String>, Value);

    #[derive(Default)]
    struct Seen {
        calls: Mutex<Vec<SeenCall>>,
    }

    fn capture(seen: &Arc<Seen>, req: &Request, body: Value) {
        let mut headers = HashMap::new();
        for name in [
            "authorization",
            "x-coinflow-auth-user-id",
            "x-coinflow-auth-session-key",
        ] {
            if let Some(v) = req.headers().get(name).and_then(|v| v.to_str().ok()) {
                headers.insert(name.to_string(), v.to_string());
            }
        }
        seen.calls
            .lock()
            .unwrap()
            .push((req.uri().path().to_string(), headers, body));
    }

    /// Mock Coinflow on an ephemeral port. Returns its base URL.
    async fn mock_coinflow(seen: Arc<Seen>) -> String {
        let s1 = seen.clone();
        let s2 = seen.clone();
        let s3 = seen;
        let app = Router::new()
            .route(
                "/api/auth/session-key",
                get(move |req: Request| {
                    let seen = s1.clone();
                    async move {
                        capture(&seen, &req, Value::Null);
                        Json(json!({ "key": "sess.jwt" }))
                    }
                }),
            )
            .route(
                "/api/checkout/totals/{merchant}",
                post(move |req: Request| {
                    let seen = s2.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body: Value =
                            serde_json::from_slice(&to_bytes(body, usize::MAX).await.unwrap())
                                .unwrap();
                        capture(&seen, &Request::from_parts(parts, Body::empty()), body);
                        Json(json!({ "card": {
                            "subtotal": { "cents": 2000, "currency": "USD" },
                            "creditCardFees": { "cents": 111, "currency": "USD" },
                            "chargebackProtectionFees": { "cents": 59, "currency": "USD" },
                            "gasFees": { "cents": 0, "currency": "USD" },
                            "total": { "cents": 2170, "currency": "USD" }
                        }}))
                    }
                }),
            )
            .route(
                "/api/checkout/link",
                post(move |req: Request| {
                    let seen = s3.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        let body: Value =
                            serde_json::from_slice(&to_bytes(body, usize::MAX).await.unwrap())
                                .unwrap();
                        capture(&seen, &Request::from_parts(parts, Body::empty()), body);
                        Json(json!({ "link": "https://sandbox.coinflow.cash/solana/purchase-v2/solana-foundation?sessionKey=x" }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
        auth: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(auth) = auth {
            builder = builder.header(header::AUTHORIZATION, auth);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&json).unwrap())
            }
            None => Body::empty(),
        };
        let res = app
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn app_with(cfg: Config) -> Router {
        crate::router(
            AppState::with_drivers("https://cloud.test", Vec::new())
                .with_funding(Funding::new(cfg)),
        )
    }

    #[tokio::test]
    async fn start_mints_a_checkout_fixed_by_the_server() {
        let seen = Arc::new(Seen::default());
        let api_url = mock_coinflow(seen.clone()).await;
        let app = app_with(test_config(&api_url));

        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/start",
            Some(json!({
                "address": ADDRESS, "cents": 2000,
                "callback": "http://127.0.0.1:53211/callback",
                "state": "abcdefghijklmnopqrstuvwxyz012345"
            })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["checkout_origin"], "https://sandbox.coinflow.cash");
        assert_eq!(res["env"], "sandbox");
        assert_eq!(res["settlement"], "merchant");
        assert_eq!(res["card_entry"], "hosted");
        assert_eq!(res["quote"]["total_cents"], 2170);
        assert_eq!(res["quote"]["card_fee_cents"], 111);
        assert_eq!(res["quote"]["other_fee_cents"], 0);
        assert_eq!(
            res["payment_methods"],
            json!(["card", "applePay", "googlePay"])
        );
        assert!(
            res["link"]
                .as_str()
                .unwrap()
                .starts_with("https://sandbox.coinflow.cash/")
        );

        let calls = seen.calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "session key, totals, link");
        let (path, headers, _) = &calls[0];
        assert_eq!(path, "/api/auth/session-key");
        assert_eq!(headers["authorization"], "coinflow_sandbox_test");
        assert_eq!(
            headers["x-coinflow-auth-user-id"], ADDRESS,
            "the payer is the address"
        );
        let (path, headers, body) = &calls[1];
        assert_eq!(path, "/api/checkout/totals/solana-foundation");
        assert_eq!(headers["x-coinflow-auth-session-key"], "sess.jwt");
        assert_eq!(body["subtotal"]["cents"], 2000);
        assert_eq!(body["settlementType"], "USDC");
        let (path, headers, body) = &calls[2];
        assert_eq!(path, "/api/checkout/link");
        assert_eq!(headers["authorization"], "coinflow_sandbox_test");
        assert_eq!(
            body["subtotal"],
            json!({ "cents": 2000, "currency": "USD" })
        );
        assert_eq!(body["blockchain"], "solana");
        assert_eq!(body["settlementType"], "USDC");
        assert_eq!(body["statementDescriptor"], STATEMENT_DESCRIPTOR);
        assert_eq!(
            body["allowedPaymentMethods"],
            json!(["card", "applePay", "googlePay"])
        );
        assert_eq!(body["webhookInfo"]["address"], ADDRESS);
        assert_eq!(
            body["webhookInfo"]["state"],
            "abcdefghijklmnopqrstuvwxyz012345"
        );
        assert_eq!(body["expiresIn"]["minutes"], LINK_TTL_MINUTES);
        assert_eq!(body["theme"]["background"], "#0c0c0f");
        assert_eq!(body["theme"]["cardBackground"], "#18181b");
        assert_eq!(body["theme"]["style"], "rounded");
        assert_eq!(body["theme"]["font"], "Inter");
        assert_eq!(body["theme"]["fontSize"], "14px");
        assert_eq!(body["theme"]["fontWeight"], "500");
        assert!(
            body.get("destination").is_none(),
            "merchant settlement carries no destination"
        );
    }

    #[tokio::test]
    async fn start_locks_the_destination_when_customer_settlement_is_on() {
        let seen = Arc::new(Seen::default());
        let api_url = mock_coinflow(seen.clone()).await;
        let mut cfg = test_config(&api_url);
        cfg.settle_to_customer = true;
        let app = app_with(cfg);

        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/start",
            Some(json!({ "address": ADDRESS, "cents": 1000 })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["settlement"], "customer");
        let calls = seen.calls.lock().unwrap();
        assert_eq!(calls[2].2["destination"], ADDRESS);
        assert!(calls[2].2["webhookInfo"].get("state").is_none());
    }

    #[tokio::test]
    async fn start_validates_before_calling_coinflow() {
        let seen = Arc::new(Seen::default());
        let api_url = mock_coinflow(seen.clone()).await;
        let app = app_with(test_config(&api_url));

        for (body, error) in [
            (
                json!({ "address": "nope", "cents": 2000 }),
                "invalid_address",
            ),
            (
                json!({ "address": ADDRESS, "cents": 100 }),
                "invalid_amount",
            ),
            (
                json!({ "address": ADDRESS, "cents": 2000, "callback": "https://evil.test/cb" }),
                "invalid_callback",
            ),
            (
                json!({ "address": ADDRESS, "cents": 2000, "state": "short" }),
                "invalid_state",
            ),
            (json!({ "cents": 2000 }), "invalid_json"),
        ] {
            let (status, res) = call(&app, Method::POST, "/api/fund/start", Some(body), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
            assert_eq!(res["error"], error, "{res}");
        }
        assert!(seen.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn everything_is_503_until_funding_is_configured() {
        let app = crate::router(AppState::with_drivers("https://cloud.test", Vec::new()));
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/start",
            Some(json!({ "address": ADDRESS, "cents": 2000 })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res["error"], "funding_disabled");
        let (status, _) = call(&app, Method::GET, "/api/fund/pay_1", None, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_coinflow_outage_is_a_502_not_a_panic() {
        // Nothing listens here.
        let app = app_with(test_config("http://127.0.0.1:9"));
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/start",
            Some(json!({ "address": ADDRESS, "cents": 2000 })),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{res}");
        assert_eq!(res["error"], "provider_unreachable");
    }

    #[tokio::test]
    async fn webhooks_need_the_configured_key_and_feed_status() {
        let app = app_with(test_config("http://127.0.0.1:9"));
        let disbursed = json!({
            "eventType": "Disbursed Funds", "category": "Purchase",
            "data": { "id": "pay_1", "signature": "5ig", "wallet": ADDRESS,
                      "webhookInfo": { "address": ADDRESS } }
        });

        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/webhook",
            Some(disbursed.clone()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{res}");
        let (status, _) = call(
            &app,
            Method::POST,
            "/api/fund/webhook",
            Some(disbursed.clone()),
            Some("wrong"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // Unknown payments read as pending, not 404: the webhook may be late.
        let (status, res) = call(&app, Method::GET, "/api/fund/pay_1", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(res["status"], "pending");

        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/webhook",
            Some(disbursed),
            Some("whsec-test"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{res}");
        let (_, res) = call(&app, Method::GET, "/api/fund/pay_1", None, None).await;
        assert_eq!(res["status"], "disbursed");
        assert_eq!(res["signature"], "5ig");
        assert_eq!(res["wallet"], ADDRESS);

        let (status, res) = call(&app, Method::GET, "/api/fund/has%20space", None, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
    }

    #[tokio::test]
    async fn webhooks_are_refused_when_no_key_is_configured() {
        let mut cfg = test_config("http://127.0.0.1:9");
        cfg.webhook_key = None;
        let app = app_with(cfg);
        let (status, res) = call(
            &app,
            Method::POST,
            "/api/fund/webhook",
            Some(json!({ "eventType": "Settled", "data": { "id": "p" } })),
            Some("anything"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{res}");
        assert_eq!(res["error"], "webhooks_disabled");
    }
}
