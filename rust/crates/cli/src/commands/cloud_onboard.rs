//! Loopback onboarding: link this terminal to pay-cloud from the browser.
//!
//! Same shape as `gh auth login`:
//!
//! 1. Generate `state` and a PKCE verifier/challenge (RFC 7636 S256).
//! 2. Bind an ephemeral `127.0.0.1` listener with a single `GET /callback`.
//! 3. Open `{cloud_url}/onboard?callback=…&state=…&code_challenge=…` in the
//!    browser (the URL is also printed so it can be copied).
//! 4. pay-cloud redirects the browser to the callback with a one-time
//!    `code`; the handler checks `state` and hands the code back.
//! 5. `POST {cloud_url}/v1/onboard/exchange` with the code and verifier.
//!
//! Milestone 1: the exchange returns a `pending` stub — no wallet material
//! is provisioned yet. This module is not wired into `pay setup`; the hidden
//! `pay cloud-onboard` subcommand drives it for development.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::components;
use owo_colors::OwoColorize;

/// How long to wait for the browser to hit the callback.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Timeout for the code exchange request.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for the pre-flight `/health` probe.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// Never bind the callback listener to a LAN-reachable interface.
const LOOPBACK_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// Set to skip `webbrowser::open` (headless shells, scripted tests).
const NO_BROWSER_ENV: &str = "PAY_NO_BROWSER";
/// Overrides the pay-cloud base URL.
const CLOUD_URL_ENV: &str = "PAY_CLOUD_URL";
/// Set to anything but `0`/`false` to target a `pay-cloud` running locally
/// on its default port. `PAY_CLOUD_URL` wins when both are set.
const CLOUD_LOCAL_ENV: &str = "PAY_CLOUD_LOCAL";
/// Production pay-cloud.
const DEFAULT_CLOUD_URL: &str = "https://cloud.pay.sh";
/// Where `cargo run -p pay-cloud` listens by default.
const LOCAL_CLOUD_URL: &str = "http://127.0.0.1:8402";

/// `--backend` value that selects the browser-linked remote wallet in
/// `pay setup` and `pay account new`.
pub const CLOUD_BACKEND_FLAG: &str = "cloud";
/// Picker name and detail for the remote wallet.
pub const CLOUD_BACKEND_NAME: &str = "Remote wallet";
pub const CLOUD_BACKEND_DETAIL: &str =
    "sign in from your browser; funds and approvals live at cloud.pay.sh";

/// pay-cloud base URL: `PAY_CLOUD_URL` when set, else the local server when
/// `PAY_CLOUD_LOCAL` is on, else production.
pub fn default_cloud_url() -> String {
    cloud_url_from(
        std::env::var(CLOUD_URL_ENV).ok().as_deref(),
        std::env::var(CLOUD_LOCAL_ENV).ok().as_deref(),
    )
}

fn cloud_url_from(url: Option<&str>, local: Option<&str>) -> String {
    if let Some(url) = url
        .map(|v| v.trim().trim_end_matches('/'))
        .filter(|v| !v.is_empty())
    {
        return url.to_string();
    }
    let local_on = local
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"));
    if local_on {
        LOCAL_CLOUD_URL.to_string()
    } else {
        DEFAULT_CLOUD_URL.to_string()
    }
}

/// `pay setup --backend cloud`: link this terminal through the browser.
///
/// A `ready` result is a wallet provisioned by pay-cloud on the user's own
/// custody account; it is registered exactly like `pay account new
/// --backend <provider>` would: credentials in the platform secret store,
/// the account in `accounts.yml`, after the provider confirms the address.
/// Anything else is a failed setup: nothing is saved and the user is told
/// to run it again.
///
/// Returns the account's address so setup can go on to fund it.
pub fn run_setup_onboarding(account: &str, force: bool) -> pay_core::Result<String> {
    let result = run_loopback_onboarding(&OnboardRequest {
        cloud_url: default_cloud_url(),
        account: account.to_string(),
    })?;
    ensure_ready(&result)?;
    store_provisioned_wallet(account, &result, force)
}

/// Only a `ready` exchange carries a wallet to register.
fn ensure_ready(result: &OnboardResult) -> pay_core::Result<()> {
    if result.is_ready() {
        return Ok(());
    }
    let status = if result.status.is_empty() {
        "unknown"
    } else {
        result.status.as_str()
    };
    let detail = result
        .message
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(|m| format!(" {m}"))
        .unwrap_or_default();
    Err(pay_core::Error::Config(format!(
        "pay-cloud did not hand over a wallet (status: {status}).{detail} \
         Nothing was saved; run `pay setup` again."
    )))
}

/// Store a `ready` exchange result as a remote account named `account`.
/// Returns the verified address.
pub fn store_provisioned_wallet(
    account: &str,
    result: &OnboardResult,
    force: bool,
) -> pay_core::Result<String> {
    let provider = pay_core::remote::provider(&result.provider).ok_or_else(|| {
        pay_core::Error::Config(format!(
            "pay-cloud returned a wallet for `{}`, which this build of pay does not support \
             (known: {}). Update pay and run `pay setup` again.",
            result.provider,
            pay_core::remote::provider_ids().join(", ")
        ))
    })?;
    let wallet_id = result
        .wallet_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| pay_core::Error::Config("pay-cloud returned no wallet id.".to_string()))?;
    let expected_pubkey = result
        .pubkey
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            pay_core::Error::Config("pay-cloud returned no wallet address.".to_string())
        })?;
    for field in provider.credential_fields() {
        if !result.credentials.contains_key(field.key) {
            return Err(pay_core::Error::Config(format!(
                "pay-cloud returned no `{}` credential for {}.",
                field.key,
                provider.display_name()
            )));
        }
    }
    provider.validate_wallet_id(wallet_id)?;

    if pay_core::remote::credentials_exist(account) && !force {
        return Err(pay_core::Error::Config(format!(
            "Account `{account}` already has stored credentials. Re-run with --force to replace them."
        )));
    }

    // Confirm the credentials work and resolve to the address the page
    // showed, before anything is written locally.
    eprintln!(
        "  {}",
        format!("Verifying with {}…", provider.display_name()).dimmed()
    );
    let pubkey = pay_core::remote::fetch_wallet_address(provider, &result.credentials, wallet_id)?;
    if pubkey != expected_pubkey {
        return Err(pay_core::Error::Config(format!(
            "{} resolves wallet `{wallet_id}` to {pubkey}, but pay-cloud reported {expected_pubkey}. \
             Nothing was saved.",
            provider.display_name()
        )));
    }

    let ks = super::account::new::platform_credential_keystore()?;
    let intent = pay_core::keystore::AuthIntent::create_account(account);
    pay_core::remote::store_credentials(&ks, account, &result.credentials, &intent)?;
    super::account::new::save_account_remote(account, provider.id(), &pubkey, wallet_id)?;

    let mut body = format!(
        "Account `{account}` signs through {}.\nAddress: {pubkey}\nWallet: {wallet_id}",
        provider.display_name()
    );
    if let Some(project) = result.project_id.as_deref().filter(|p| !p.is_empty()) {
        body.push_str(&format!("\nProject: {project}"));
    }
    components::print_notice(components::NoticeLevel::Info, "Remote wallet ready", &body);
    Ok(pubkey)
}

fn print_result(result: &OnboardResult) {
    let mut body = format!(
        "Provider: {}\nNetwork: {}\nStatus: {}",
        result.provider, result.network, result.status
    );
    if !result.email.is_empty() {
        body.push_str(&format!("\nEmail: {}", result.email));
    }
    if let Some(wallet_id) = result.wallet_id.as_deref() {
        body.push_str(&format!("\nWallet: {wallet_id}"));
    }
    if let Some(pubkey) = result.pubkey.as_deref() {
        body.push_str(&format!("\nAddress: {pubkey}"));
    }
    if let Some(message) = result.message.as_deref().filter(|m| !m.is_empty()) {
        body.push('\n');
        body.push_str(message);
    }
    components::print_notice(components::NoticeLevel::Info, "Terminal linked", &body);
}

pub struct OnboardRequest {
    /// pay-cloud base URL, e.g. `http://127.0.0.1:8402`.
    pub cloud_url: String,
    /// Account name being linked (informational for now).
    pub account: String,
}

/// Response of `POST /v1/onboard/exchange`. Every field defaults so the
/// server can grow the payload without breaking older CLIs.
///
/// `status: "ready"` carries a provisioned wallet: `provider` is a
/// `pay_core::remote` provider id, `credentials` its declared fields,
/// `wallet_id` and `pubkey` the account to register.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OnboardResult {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub network: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub wallet_id: Option<String>,
    #[serde(default)]
    pub pubkey: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub credentials: std::collections::BTreeMap<String, String>,
}

impl OnboardResult {
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }
}

/// Link this terminal to pay-cloud from the browser (preview).
#[derive(clap::Args)]
pub struct CloudOnboardCommand {
    /// pay-cloud base URL. Defaults to `PAY_CLOUD_URL`, else production.
    #[arg(long)]
    pub url: Option<String>,

    /// Account name to link.
    #[arg(long, default_value = "default")]
    pub account: String,
}

impl CloudOnboardCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let result = run_loopback_onboarding(&OnboardRequest {
            cloud_url: self.url.unwrap_or_else(default_cloud_url),
            account: self.account,
        })?;
        print_result(&result);
        Ok(())
    }
}

/// Run the full browser round-trip synchronously and return the exchange
/// result.
pub fn run_loopback_onboarding(req: &OnboardRequest) -> pay_core::Result<OnboardResult> {
    let cloud_url = req.cloud_url.trim_end_matches('/').to_string();
    check_cloud_reachable(&cloud_url)?;
    let pkce = Pkce::generate();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("onboarding runtime: {e}")))?;
    let code = rt.block_on(async {
        let listener = bind_callback_listener(&pkce.state, Expect::Code).await?;
        let url = build_browser_url(&BrowserUrlParams {
            cloud_url: &cloud_url,
            callback: &listener.callback_url(),
            state: &pkce.state,
            code_challenge: &pkce.code_challenge,
            account: &req.account,
            host: &local_hostname(),
            cli: env!("CARGO_PKG_VERSION"),
        });
        eprintln!("Opening your browser to link this terminal…");
        eprintln!("{url}");
        eprintln!();
        open_browser(&url);
        listener.wait_for_code(CALLBACK_TIMEOUT).await
    })?;
    // The blocking reqwest client must not be used inside a tokio context.
    drop(rt);

    exchange_code(&cloud_url, &code, &pkce.code_verifier)
}

/// Confirm pay-cloud answers before binding a listener or opening the
/// browser, so a stopped local server is one clear error instead of a
/// browser tab that cannot connect and a CLI that waits ten minutes.
fn check_cloud_reachable(cloud_url: &str) -> pay_core::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|e| pay_core::Error::Config(format!("http client: {e}")))?;
    let health = format!("{cloud_url}/health");
    match client.get(&health).send() {
        Ok(response) if response.status().is_success() => Ok(()),
        Ok(response) => Err(pay_core::Error::Config(format!(
            "pay-cloud at {cloud_url} answered {} on /health. {}",
            response.status(),
            unreachable_hint(cloud_url)
        ))),
        Err(e) => Err(pay_core::Error::Config(format!(
            "Cannot reach pay-cloud at {cloud_url}: {e}. {}",
            unreachable_hint(cloud_url)
        ))),
    }
}

fn unreachable_hint(cloud_url: &str) -> String {
    if cloud_url == LOCAL_CLOUD_URL {
        "Start the local server first: `cargo run -p pay-cloud` (from rust/), then retry."
            .to_string()
    } else {
        format!("Check the URL, or set {CLOUD_LOCAL_ENV}=1 to use a local pay-cloud.")
    }
}

// ── PKCE ──────────────────────────────────────────────────────────────────

/// Per-attempt CSRF state and PKCE pair.
pub struct Pkce {
    pub state: String,
    pub code_verifier: String,
    pub code_challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let code_verifier = random_token();
        Self {
            state: random_token(),
            code_challenge: pkce_challenge(&code_verifier),
            code_verifier,
        }
    }
}

/// 32 random bytes, base64url without padding (43 chars).
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 7636 S256: `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// ── Browser URL ───────────────────────────────────────────────────────────

pub struct BrowserUrlParams<'a> {
    pub cloud_url: &'a str,
    pub callback: &'a str,
    pub state: &'a str,
    pub code_challenge: &'a str,
    pub account: &'a str,
    pub host: &'a str,
    pub cli: &'a str,
}

/// `{cloud_url}/onboard?callback=…&state=…&code_challenge=…&account=…&host=…&cli=…`
pub fn build_browser_url(p: &BrowserUrlParams<'_>) -> String {
    format!(
        "{}/onboard?callback={}&state={}&code_challenge={}&account={}&host={}&cli={}",
        p.cloud_url.trim_end_matches('/'),
        urlencoding::encode(p.callback),
        urlencoding::encode(p.state),
        urlencoding::encode(p.code_challenge),
        urlencoding::encode(p.account),
        urlencoding::encode(p.host),
        urlencoding::encode(p.cli),
    )
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn open_browser(url: &str) {
    if std::env::var_os(NO_BROWSER_ENV).is_some() {
        return;
    }
    // The URL was already printed; a failure here is not fatal.
    let _ = webbrowser::open(url);
}

// ── Callback listener ─────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub payment_id: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
}

/// What a `/callback` hit must carry to complete the flow that opened the
/// browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// An authorization code from pay-cloud's onboarding exchange.
    Code,
    /// A Coinflow payment id from the funding page.
    Payment,
}

/// What the browser delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Accepted {
    Code(String),
    Payment {
        payment_id: String,
        /// On-chain signature, when pay-cloud already knew it.
        signature: Option<String>,
    },
}

/// Decide whether a `/callback` hit completes the flow. Pure so the state
/// check is unit-testable without a socket.
pub fn accept_callback(
    expect: Expect,
    q: &CallbackQuery,
    expected_state: &str,
) -> Result<Accepted, &'static str> {
    match q.state.as_deref() {
        None => return Err("missing state"),
        Some(s) if s != expected_state => return Err("state mismatch"),
        Some(_) => {}
    }
    let present = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    match expect {
        Expect::Code => present(&q.code).map(Accepted::Code).ok_or("missing code"),
        Expect::Payment => present(&q.payment_id)
            .map(|payment_id| Accepted::Payment {
                payment_id,
                signature: present(&q.signature),
            })
            .ok_or("missing payment_id"),
    }
}

#[derive(Clone)]
struct CallbackState {
    expect: Expect,
    expected_state: Arc<str>,
    result_tx: Arc<Mutex<Option<oneshot::Sender<Accepted>>>>,
}

async fn callback_handler(
    State(st): State<CallbackState>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    match accept_callback(st.expect, &q, &st.expected_state) {
        Ok(accepted) => {
            if let Some(tx) = st.result_tx.lock().unwrap().take() {
                let _ = tx.send(accepted);
            }
            (StatusCode::OK, Html(success_page())).into_response()
        }
        Err(reason) => (StatusCode::BAD_REQUEST, Html(error_page(reason))).into_response(),
    }
}

/// Minimal self-contained page shown in the browser tab after the redirect.
fn page(heading: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Pay</title>\
<style>body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
font-family:Inter,system-ui,-apple-system,sans-serif;background:#fff;color:#111}}\
main{{text-align:center;padding:24px}}h1{{font-size:28px;margin:0 0 12px}}\
p{{color:#666;font-size:18px;margin:0}}</style></head>\
<body><main><h1>{heading}</h1><p>{body}</p></main></body></html>"
    )
}

fn success_page() -> String {
    page("You're all set.", "You can return to your terminal.")
}

fn error_page(reason: &str) -> String {
    page(
        "Something went wrong.",
        &format!(
            "This link could not be verified ({reason}). Return to your terminal and run <code>pay setup</code> again."
        ),
    )
}

/// A bound `/callback` listener on `127.0.0.1:<ephemeral>`.
pub struct CallbackListener {
    pub addr: SocketAddr,
    result_rx: oneshot::Receiver<Accepted>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

/// Bind the listener and start serving. Must be called inside a tokio
/// runtime; the server task lives until [`CallbackListener::wait_for`]
/// returns.
pub async fn bind_callback_listener(
    expected_state: &str,
    expect: Expect,
) -> pay_core::Result<CallbackListener> {
    let listener = tokio::net::TcpListener::bind((LOOPBACK_IP, 0))
        .await
        .map_err(|e| pay_core::Error::Config(format!("loopback callback bind: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| pay_core::Error::Config(format!("loopback callback local_addr: {e}")))?;

    let (result_tx, result_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let state = CallbackState {
        expect,
        expected_state: Arc::from(expected_state),
        result_tx: Arc::new(Mutex::new(Some(result_tx))),
    };
    let app = Router::new()
        .route("/callback", get(callback_handler))
        .with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    Ok(CallbackListener {
        addr,
        result_rx,
        shutdown_tx: Some(shutdown_tx),
        server,
    })
}

impl CallbackListener {
    pub fn callback_url(&self) -> String {
        format!("http://{}/callback", self.addr)
    }

    /// Wait for the browser to come back, then shut the server down
    /// (letting the success page flush first).
    pub async fn wait_for(self, timeout: Duration) -> pay_core::Result<Accepted> {
        let CallbackListener {
            result_rx,
            mut shutdown_tx,
            server,
            ..
        } = self;
        let outcome = tokio::time::timeout(timeout, result_rx).await;
        if let Some(tx) = shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        match outcome {
            Ok(Ok(accepted)) => Ok(accepted),
            Ok(Err(_)) => Err(pay_core::Error::Config(
                "loopback callback listener closed before the browser came back".to_string(),
            )),
            Err(_) => Err(pay_core::Error::Config(
                "Timed out waiting for the browser to finish. Run the command again.".to_string(),
            )),
        }
    }

    /// [`wait_for`](Self::wait_for) for an onboarding listener.
    pub async fn wait_for_code(self, timeout: Duration) -> pay_core::Result<String> {
        match self.wait_for(timeout).await? {
            Accepted::Code(code) => Ok(code),
            Accepted::Payment { .. } => Err(pay_core::Error::Config(
                "loopback callback delivered a payment where a code was expected".to_string(),
            )),
        }
    }
}

// ── Funding page ──────────────────────────────────────────────────────────

/// How long `pay topup` waits for the card purchase; entering a card and
/// clearing 3-D Secure can take a while.
pub const FUNDING_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Query for `{cloud}/fund`, the card-purchase page.
pub struct FundUrlParams<'a> {
    pub cloud_url: &'a str,
    pub address: &'a str,
    /// Loopback URL the page returns to; `None` when nobody is waiting.
    pub callback: Option<&'a str>,
    pub state: Option<&'a str>,
    pub account: &'a str,
    pub cli: &'a str,
}

pub fn build_fund_url(p: &FundUrlParams<'_>) -> String {
    let mut url = reqwest::Url::parse(&format!("{}/fund", p.cloud_url.trim_end_matches('/')))
        .expect("cloud url should parse");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("address", p.address);
        if let Some(callback) = p.callback {
            q.append_pair("callback", callback);
        }
        if let Some(state) = p.state {
            q.append_pair("state", state);
        }
        q.append_pair("account", p.account);
        q.append_pair("cli", p.cli);
    }
    url.into()
}

/// The funding page came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundingOutcome {
    pub payment_id: String,
    pub signature: Option<String>,
}

/// A funding page that was opened: the URL to reopen it, and the browser's
/// eventual answer. The listener lives on its own thread with its own
/// runtime so a synchronous UI can poll [`try_recv`](Self::try_recv).
pub struct FundingSession {
    pub url: String,
    outcome_rx: std::sync::mpsc::Receiver<pay_core::Result<FundingOutcome>>,
}

impl FundingSession {
    /// Bind the loopback listener, build the page URL and open the browser.
    /// Returns once the URL is known; the wait continues in the background.
    pub fn open(cloud_url: &str, address: &str, account: &str) -> pay_core::Result<Self> {
        let cloud_url = cloud_url.trim_end_matches('/').to_string();
        check_cloud_reachable(&cloud_url)?;
        let state = random_token();
        let (url_tx, url_rx) = std::sync::mpsc::channel::<pay_core::Result<String>>();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let (address, account) = (address.to_string(), account.to_string());
        std::thread::Builder::new()
            .name("pay-fund-callback".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = url_tx.send(Err(pay_core::Error::Config(format!(
                            "funding runtime: {e}"
                        ))));
                        return;
                    }
                };
                let outcome =
                    rt.block_on(async {
                        let listener = match bind_callback_listener(&state, Expect::Payment).await {
                            Ok(listener) => listener,
                            Err(e) => {
                                let _ = url_tx.send(Err(e));
                                return None;
                            }
                        };
                        let url = build_fund_url(&FundUrlParams {
                            cloud_url: &cloud_url,
                            address: &address,
                            callback: Some(&listener.callback_url()),
                            state: Some(&state),
                            account: &account,
                            cli: env!("CARGO_PKG_VERSION"),
                        });
                        let _ = url_tx.send(Ok(url));
                        Some(listener.wait_for(FUNDING_TIMEOUT).await.map(
                            |accepted| match accepted {
                                Accepted::Payment {
                                    payment_id,
                                    signature,
                                } => FundingOutcome {
                                    payment_id,
                                    signature,
                                },
                                Accepted::Code(_) => {
                                    unreachable!("listener bound with Expect::Payment")
                                }
                            },
                        ))
                    });
                if let Some(outcome) = outcome {
                    let _ = outcome_tx.send(outcome);
                }
            })
            .map_err(|e| pay_core::Error::Config(format!("funding listener thread: {e}")))?;
        let url = url_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| {
            pay_core::Error::Config("funding listener did not start in time".to_string())
        })??;
        open_browser(&url);
        Ok(Self { url, outcome_rx })
    }

    /// The browser's answer, if it has come back yet.
    pub fn try_recv(&self) -> Option<pay_core::Result<FundingOutcome>> {
        self.outcome_rx.try_recv().ok()
    }
}

// ── Exchange ──────────────────────────────────────────────────────────────

/// `POST {cloud_url}/v1/onboard/exchange`. Must be called outside a tokio
/// runtime (blocking client).
pub fn exchange_code(
    cloud_url: &str,
    code: &str,
    code_verifier: &str,
) -> pay_core::Result<OnboardResult> {
    #[derive(Deserialize, Default)]
    struct ErrorBody {
        #[serde(default)]
        message: Option<String>,
    }

    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(EXCHANGE_TIMEOUT)
        .build()
        .map_err(|e| pay_core::Error::Config(format!("pay-cloud HTTP client: {e}")))?;
    let body = serde_json::json!({ "code": code, "code_verifier": code_verifier });
    let res = client
        .post(format!(
            "{}/v1/onboard/exchange",
            cloud_url.trim_end_matches('/')
        ))
        .header(header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body).expect("static JSON shape"))
        .send()
        .map_err(|e| pay_core::Error::Config(format!("pay-cloud exchange request failed: {e}")))?;
    let status = res.status();
    let bytes = res
        .bytes()
        .map_err(|e| pay_core::Error::Config(format!("pay-cloud exchange read failed: {e}")))?;

    if !status.is_success() {
        let message = serde_json::from_slice::<ErrorBody>(&bytes)
            .ok()
            .and_then(|e| e.message)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| format!("pay-cloud exchange failed: HTTP {status}"));
        return Err(pay_core::Error::Config(message));
    }

    serde_json::from_slice(&bytes).map_err(|e| {
        pay_core::Error::Config(format!(
            "pay-cloud exchange returned an unexpected body: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_only_registers_a_ready_result() {
        let ready = OnboardResult {
            status: "ready".to_string(),
            ..OnboardResult::default()
        };
        assert!(ensure_ready(&ready).is_ok());

        let pending = OnboardResult {
            status: "pending".to_string(),
            message: Some("Wallet provisioning is not available yet.".to_string()),
            ..OnboardResult::default()
        };
        let Err(pay_core::Error::Config(msg)) = ensure_ready(&pending) else {
            panic!("pending is not a linked terminal");
        };
        assert!(msg.contains("status: pending"), "{msg}");
        assert!(msg.contains("not available yet"), "{msg}");
        assert!(msg.contains("Nothing was saved"), "{msg}");

        let Err(pay_core::Error::Config(msg)) = ensure_ready(&OnboardResult::default()) else {
            panic!("an empty status is not ready");
        };
        assert!(msg.contains("status: unknown"), "{msg}");
    }

    #[test]
    fn unreachable_cloud_fails_before_opening_anything() {
        // Bind and immediately drop a port so nothing listens on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        let err = run_loopback_onboarding(&OnboardRequest {
            cloud_url: url.clone(),
            account: "t".into(),
        })
        .err()
        .map(|e| e.to_string())
        .expect("must fail fast");
        assert!(err.contains("Cannot reach pay-cloud"), "{err}");
        assert!(err.contains(&url), "{err}");
        assert!(err.contains("PAY_CLOUD_LOCAL"), "{err}");
        assert_eq!(
            unreachable_hint(LOCAL_CLOUD_URL),
            "Start the local server first: `cargo run -p pay-cloud` (from rust/), then retry."
        );
    }

    #[test]
    fn cloud_url_prefers_explicit_then_local_then_production() {
        assert_eq!(cloud_url_from(None, None), DEFAULT_CLOUD_URL);
        assert_eq!(cloud_url_from(None, Some("1")), LOCAL_CLOUD_URL);
        assert_eq!(cloud_url_from(None, Some("true")), LOCAL_CLOUD_URL);
        assert_eq!(cloud_url_from(None, Some("0")), DEFAULT_CLOUD_URL);
        assert_eq!(cloud_url_from(None, Some("false")), DEFAULT_CLOUD_URL);
        assert_eq!(
            cloud_url_from(Some("http://127.0.0.1:9000/"), Some("1")),
            "http://127.0.0.1:9000"
        );
        assert_eq!(cloud_url_from(Some("  "), None), DEFAULT_CLOUD_URL);
    }
    use axum::Json;
    use axum::routing::post;

    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn pkce_matches_rfc_7636_vector() {
        assert_eq!(pkce_challenge(RFC_VERIFIER), RFC_CHALLENGE);
    }

    #[test]
    fn generated_material_is_base64url_and_consistent() {
        let pkce = Pkce::generate();
        for token in [&pkce.state, &pkce.code_verifier, &pkce.code_challenge] {
            assert_eq!(token.len(), 43);
            assert!(
                token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{token}"
            );
        }
        assert_ne!(pkce.state, pkce.code_verifier);
        assert_eq!(pkce.code_challenge, pkce_challenge(&pkce.code_verifier));
    }

    #[test]
    fn browser_url_is_percent_encoded() {
        let url = build_browser_url(&BrowserUrlParams {
            cloud_url: "http://127.0.0.1:8402/",
            callback: "http://127.0.0.1:53211/callback",
            state: "st_ate-1234567890",
            code_challenge: RFC_CHALLENGE,
            account: "my account&x",
            host: "ludo's mbp",
            cli: "0.29.0",
        });
        assert_eq!(
            url,
            format!(
                "http://127.0.0.1:8402/onboard?callback=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback\
&state=st_ate-1234567890&code_challenge={RFC_CHALLENGE}&account=my%20account%26x&host=ludo%27s%20mbp&cli=0.29.0"
            )
        );
    }

    #[test]
    fn accept_callback_checks_state_and_code() {
        let q = |code: Option<&str>, state: Option<&str>| CallbackQuery {
            code: code.map(str::to_string),
            state: state.map(str::to_string),
            ..CallbackQuery::default()
        };
        assert_eq!(
            accept_callback(Expect::Code, &q(Some("abc"), Some("s")), "s"),
            Ok(Accepted::Code("abc".to_string()))
        );
        assert_eq!(
            accept_callback(Expect::Code, &q(Some("abc"), Some("other")), "s"),
            Err("state mismatch")
        );
        assert_eq!(
            accept_callback(Expect::Code, &q(Some("abc"), None), "s"),
            Err("missing state")
        );
        assert_eq!(
            accept_callback(Expect::Code, &q(None, Some("s")), "s"),
            Err("missing code")
        );
        assert_eq!(
            accept_callback(Expect::Code, &q(Some(""), Some("s")), "s"),
            Err("missing code")
        );
    }

    #[test]
    fn accept_callback_reads_a_payment_when_funding() {
        let q = |payment_id: Option<&str>, signature: Option<&str>| CallbackQuery {
            state: Some("s".to_string()),
            payment_id: payment_id.map(str::to_string),
            signature: signature.map(str::to_string),
            ..CallbackQuery::default()
        };
        assert_eq!(
            accept_callback(Expect::Payment, &q(Some("pay_1"), Some("5ig")), "s"),
            Ok(Accepted::Payment {
                payment_id: "pay_1".to_string(),
                signature: Some("5ig".to_string()),
            })
        );
        assert_eq!(
            accept_callback(Expect::Payment, &q(Some("pay_1"), Some("")), "s"),
            Ok(Accepted::Payment {
                payment_id: "pay_1".to_string(),
                signature: None,
            })
        );
        assert_eq!(
            accept_callback(Expect::Payment, &q(None, None), "s"),
            Err("missing payment_id")
        );
        // A code is not a payment, and vice versa.
        let with_code = CallbackQuery {
            code: Some("abc".to_string()),
            state: Some("s".to_string()),
            ..CallbackQuery::default()
        };
        assert_eq!(
            accept_callback(Expect::Payment, &with_code, "s"),
            Err("missing payment_id")
        );
    }

    #[test]
    fn fund_url_carries_the_address_and_the_return_path() {
        let url = build_fund_url(&FundUrlParams {
            cloud_url: "http://127.0.0.1:8402/",
            address: "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z",
            callback: Some("http://127.0.0.1:53211/callback"),
            state: Some("st4te"),
            account: "ledger-test",
            cli: "0.29.0",
        });
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(
            parsed.origin().ascii_serialization(),
            "http://127.0.0.1:8402"
        );
        assert_eq!(parsed.path(), "/fund");
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["address"], "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z");
        assert_eq!(q["callback"], "http://127.0.0.1:53211/callback");
        assert_eq!(q["state"], "st4te");
        assert_eq!(q["account"], "ledger-test");
        assert_eq!(q["cli"], "0.29.0");

        let bare = build_fund_url(&FundUrlParams {
            cloud_url: "https://cloud.pay.sh",
            address: "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z",
            callback: None,
            state: None,
            account: "default",
            cli: "0.29.0",
        });
        assert!(!bare.contains("callback="));
        assert!(!bare.contains("state="));
    }

    #[test]
    fn callback_listener_end_to_end() {
        let rt = runtime();
        rt.block_on(async {
            let mut listener = bind_callback_listener("expected-state", Expect::Code)
                .await
                .unwrap();
            assert_eq!(listener.addr.ip(), LOOPBACK_IP);
            let base = listener.callback_url();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();

            // Wrong state: 400 page, oneshot untouched.
            let res = client
                .get(format!("{base}?code=evil&state=wrong"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
            assert!(res.text().await.unwrap().contains("state mismatch"));
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut listener.result_rx)
                    .await
                    .is_err(),
                "wrong state must not resolve the code"
            );

            // Right state: success page, code delivered.
            let res = client
                .get(format!("{base}?code=the-code&state=expected-state"))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::OK);
            assert!(res.text().await.unwrap().contains("You're all set"));

            let code = listener
                .wait_for_code(Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(code, "the-code");
        });
    }

    #[test]
    fn callback_listener_times_out() {
        let rt = runtime();
        rt.block_on(async {
            let listener = bind_callback_listener("s", Expect::Code).await.unwrap();
            let err = listener
                .wait_for_code(Duration::from_millis(50))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("Timed out"), "{err}");
        });
    }

    /// Serve `app` on an ephemeral loopback port from a background thread
    /// with its own runtime so the test thread can use the blocking client.
    fn spawn_stub(app: Router) -> String {
        let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind((LOOPBACK_IP, 0))
                    .await
                    .unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.ok();
            });
        });
        format!("http://{}", rx.recv().unwrap())
    }

    #[test]
    fn exchange_parses_success_response() {
        let app = Router::new().route(
            "/v1/onboard/exchange",
            post(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["code"], "c0de");
                assert_eq!(body["code_verifier"], RFC_VERIFIER);
                Json(serde_json::json!({
                    "provider": "pay-cloud",
                    "status": "pending",
                    "email": "a@b.co",
                    "network": "mainnet",
                    "message": "Wallet provisioning is not available yet.",
                    "future_field": { "ignored": true },
                }))
            }),
        );
        let base = spawn_stub(app);

        let result = exchange_code(&format!("{base}/"), "c0de", RFC_VERIFIER).unwrap();
        assert_eq!(result.provider, "pay-cloud");
        assert_eq!(result.status, "pending");
        assert_eq!(result.email, "a@b.co");
        assert_eq!(result.network, "mainnet");
        assert_eq!(
            result.message.as_deref(),
            Some("Wallet provisioning is not available yet.")
        );
    }

    #[test]
    fn exchange_surfaces_server_error_message() {
        let app = Router::new().route(
            "/v1/onboard/exchange",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_grant",
                        "message": "The authorization code is unknown.",
                    })),
                )
            }),
        );
        let base = spawn_stub(app);

        let err = exchange_code(&base, "c0de", RFC_VERIFIER).unwrap_err();
        assert!(
            matches!(&err, pay_core::Error::Config(m) if m == "The authorization code is unknown."),
            "{err}"
        );
    }

    #[test]
    fn exchange_falls_back_to_http_status_without_message() {
        let app = Router::new().route(
            "/v1/onboard/exchange",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
        let base = spawn_stub(app);

        let err = exchange_code(&base, "c0de", RFC_VERIFIER).unwrap_err();
        assert!(err.to_string().contains("HTTP 500"), "{err}");
    }
}
