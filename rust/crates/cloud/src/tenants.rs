//! Tenants: who a connector session acts for, and with what.
//!
//! A tenant is an OAuth subject bound to one remote wallet: the account
//! entry, the provider credentials that sign for it, and a spending policy.
//! [`CloudContext`] turns each MCP call into a [`CallScope`] for its tenant,
//! so pay-mcp's tools run unchanged against a per-tenant accounts store
//! whose credentials live here rather than in a keystore.
//!
//! This is the in-memory registry; the Postgres-backed store replaces it
//! without changing the context.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pay_core::accounts::{Account, AccountsFile, AccountsStore, BackendKind, MAINNET_NETWORK};
use pay_core::remote::{CredentialSource, Credentials, MemoryCredentials};
use pay_mcp::context::{CallScope, PayContext};
use pay_mcp::policy::{MemoryLedger, PolicyApproval, SpendLedger, SpendPolicy};
use rmcp::service::{RequestContext, RoleServer};

use crate::mcp::Tenant;

/// What a fresh connector wallet may spend until its owner changes it:
/// a dollar a call, ten a day. Enough for a demo, small enough to lose.
pub const DEFAULT_POLICY: SpendPolicy = SpendPolicy {
    per_call_ceiling: Some(10_000),
    daily_cap: Some(100_000),
};

/// Account name a connector wallet carries in prompts and receipts.
pub const CONNECTOR_ACCOUNT: &str = "connector";

/// How long a wallet-link ticket handed to a guest stays valid.
pub const LINK_TTL: Duration = Duration::from_secs(30 * 60);
/// Most link tickets held at once; expired ones are swept when full.
const MAX_LINKS: usize = 4096;

/// Subjects minted for guests: connected, no wallet yet.
pub const GUEST_PREFIX: &str = "guest_";

pub fn is_guest(subject: &str) -> bool {
    subject.starts_with(GUEST_PREFIX)
}

/// The subject for a provider account: stable across sign-ins and
/// browsers, opaque, and not reversible to the provider's id. This is the
/// identity a tenant hangs on; the browser cookie is only a shortcut to it.
pub fn subject_for(provider_id: &str, account_identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{provider_id}:{account_identity}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sub_{}", &hex[..32])
}

/// One tenant's wallet and rules.
#[derive(Clone)]
pub struct TenantRecord {
    /// OAuth subject (or static-token fingerprint) this wallet belongs to.
    pub subject: String,
    /// Account name shown in prompts and receipts.
    pub account_name: String,
    /// Remote provider id (`openfort`).
    pub provider: String,
    /// Provider-side wallet id.
    pub wallet_id: String,
    /// The wallet's address.
    pub pubkey: String,
    /// Provider credentials that sign for this wallet.
    pub credentials: Credentials,
    pub policy: SpendPolicy,
}

impl TenantRecord {
    /// A tenant for a wallet the onboarding driver just provisioned.
    pub fn from_wallet(subject: &str, wallet: &crate::drivers::ProvisionedWallet) -> Self {
        Self {
            subject: subject.to_string(),
            account_name: CONNECTOR_ACCOUNT.to_string(),
            provider: wallet.provider.to_string(),
            wallet_id: wallet.wallet_id.clone(),
            pubkey: wallet.address.clone(),
            credentials: wallet.credentials.clone(),
            policy: DEFAULT_POLICY,
        }
    }

    /// The `accounts.yml` entry this tenant would have on a laptop: a remote
    /// account gated by policy (`auth_required` on, so the override applies).
    fn account(&self) -> Account {
        Account {
            backend: BackendKind::Remote,
            provider: Some(self.provider.clone()),
            active: true,
            auth_required: Some(true),
            pubkey: Some(self.pubkey.clone()),
            vault: None,
            account: Some(self.wallet_id.clone()),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: Default::default(),
        }
    }
}

/// A one-time ticket letting a browser attach a wallet to a subject that
/// has none: minted when a guest's tool call needs to pay, redeemed by the
/// pages app after a sign-in. Keyed by the ticket's hash.
struct LinkTicket {
    subject: String,
    created_at: Instant,
}

/// Every bound tenant, by subject, plus the link tickets outstanding.
pub struct TenantRegistry {
    tenants: Mutex<HashMap<String, Arc<TenantRecord>>>,
    links: Mutex<HashMap<String, LinkTicket>>,
    ledger: Arc<dyn SpendLedger>,
}

impl Default for TenantRegistry {
    fn default() -> Self {
        Self::with_ledger(Arc::new(MemoryLedger::new()))
    }
}

impl TenantRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ledger(ledger: Arc<dyn SpendLedger>) -> Self {
        Self {
            tenants: Mutex::default(),
            links: Mutex::default(),
            ledger,
        }
    }

    /// A fresh ticket for `subject`; `None` when the table is full of live
    /// tickets. The plaintext goes into the tool error, only its hash stays.
    pub fn mint_link(&self, subject: &str) -> Option<String> {
        let ticket = crate::onboard::random_token();
        let now = Instant::now();
        let mut links = self.links.lock().unwrap();
        if links.len() >= MAX_LINKS {
            links.retain(|_, t| now.saturating_duration_since(t.created_at) <= LINK_TTL);
            if links.len() >= MAX_LINKS {
                return None;
            }
        }
        links.insert(
            crate::onboard::sha256_hex(&ticket),
            LinkTicket {
                subject: subject.to_string(),
                created_at: now,
            },
        );
        Some(ticket)
    }

    /// The subject a live ticket is for, without consuming it.
    pub fn peek_link(&self, ticket: &str) -> Option<String> {
        let links = self.links.lock().unwrap();
        let t = links.get(&crate::onboard::sha256_hex(ticket))?;
        (Instant::now().saturating_duration_since(t.created_at) <= LINK_TTL)
            .then(|| t.subject.clone())
    }

    /// Consume a live ticket.
    pub fn take_link(&self, ticket: &str) -> Option<String> {
        let t = self
            .links
            .lock()
            .unwrap()
            .remove(&crate::onboard::sha256_hex(ticket))?;
        (Instant::now().saturating_duration_since(t.created_at) <= LINK_TTL).then_some(t.subject)
    }

    /// Bind (or rebind) a subject to a wallet.
    pub fn bind(&self, record: TenantRecord) {
        self.tenants
            .lock()
            .unwrap()
            .insert(record.subject.clone(), Arc::new(record));
    }

    pub fn get(&self, subject: &str) -> Option<Arc<TenantRecord>> {
        self.tenants.lock().unwrap().get(subject).cloned()
    }

    pub fn remove(&self, subject: &str) -> Option<Arc<TenantRecord>> {
        self.tenants.lock().unwrap().remove(subject)
    }

    /// Replace a tenant's credentials in place (a returning user's rotated
    /// key). `None` when the subject is unknown.
    pub fn update_credentials(
        &self,
        subject: &str,
        update: impl FnOnce(&mut Credentials),
    ) -> Option<Arc<TenantRecord>> {
        let mut tenants = self.tenants.lock().unwrap();
        let current = tenants.get(subject)?;
        let mut record = (**current).clone();
        update(&mut record.credentials);
        let record = Arc::new(record);
        tenants.insert(subject.to_string(), record.clone());
        Some(record)
    }

    pub fn len(&self) -> usize {
        self.tenants.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A read-only accounts store holding one tenant's single account, with
/// its credentials in memory.
pub struct TenantAccounts {
    file: AccountsFile,
    credentials: MemoryCredentials,
}

impl TenantAccounts {
    pub fn new(record: &TenantRecord) -> Self {
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, &record.account_name, record.account());
        Self {
            file,
            credentials: MemoryCredentials::new(record.credentials.clone()),
        }
    }
}

impl AccountsStore for TenantAccounts {
    fn load(&self) -> pay_core::Result<AccountsFile> {
        Ok(self.file.clone())
    }

    /// Tenant accounts change through the tenant store, never through a
    /// tool call.
    fn save(&self, _file: &AccountsFile) -> pay_core::Result<()> {
        Err(pay_core::Error::Config(
            "tenant accounts are read-only".to_string(),
        ))
    }

    fn credential_source(&self) -> &dyn CredentialSource {
        &self.credentials
    }
}

/// The browser's memory of who it is: a subject cookie set when a wallet
/// is provisioned, read when the same browser connects another client, so
/// one person keeps one wallet across hosts.
pub mod cookie {
    use axum::http::{HeaderMap, HeaderValue, header};

    pub const NAME: &str = "pay_subject";
    const ONE_YEAR: u64 = 365 * 24 * 60 * 60;

    /// The subject the request's cookie names, if any.
    pub fn subject(headers: &HeaderMap) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|line| line.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(k, _)| *k == NAME)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty() && v.len() <= 128)
    }

    /// `Set-Cookie` for `subject`. `Secure` when the site is served over
    /// https; a local http server would otherwise never see it back.
    pub fn set(subject: &str, secure: bool) -> HeaderValue {
        let mut cookie =
            format!("{NAME}={subject}; Path=/; Max-Age={ONE_YEAR}; HttpOnly; SameSite=Lax");
        if secure {
            cookie.push_str("; Secure");
        }
        HeaderValue::from_str(&cookie).expect("cookie is ascii")
    }

    /// `Set-Cookie` that deletes the subject cookie.
    pub fn clear(secure: bool) -> HeaderValue {
        let mut cookie = format!("{NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax");
        if secure {
            cookie.push_str("; Secure");
        }
        HeaderValue::from_str(&cookie).expect("cookie is ascii")
    }
}

/// pay-mcp context for the hosted connector.
pub struct CloudContext {
    registry: Arc<TenantRegistry>,
    /// The page where a wallet-less subject attaches a wallet; the ticket
    /// goes in `?link=`. `None` leaves the error without a link.
    link_page: Option<String>,
}

impl CloudContext {
    pub fn new(registry: Arc<TenantRegistry>) -> Self {
        Self {
            registry,
            link_page: None,
        }
    }

    /// Point wallet-less callers at `page` (`https://pay.sh/connect`).
    pub fn with_link_page(mut self, page: impl Into<String>) -> Self {
        self.link_page = Some(page.into().trim_end_matches('/').to_string());
        self
    }

    /// What a paying call tells a subject with no wallet. Guests get a
    /// one-time link to attach one; the wording is for the host's user.
    fn no_wallet_message(&self, subject: &str) -> String {
        let link = self
            .link_page
            .as_deref()
            .and_then(|page| Some(format!("{page}?link={}", self.registry.mint_link(subject)?)));
        match link {
            Some(url) => format!(
                "This connection has no pay wallet yet, so it cannot pay for this call. \
                 Set one up in a minute at {url} (sign in with your email, add USDC with a \
                 card), then ask again. Browsing the catalog works without a wallet."
            ),
            None => "This connection has no wallet yet. Finish setting up your pay account at \
                     cloud.pay.sh, then try again."
                .to_string(),
        }
    }
}

/// The tenant the bearer middleware attached to this request.
fn tenant_of(call: &RequestContext<RoleServer>) -> Option<Tenant> {
    call.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Tenant>())
        .cloned()
}

impl PayContext for CloudContext {
    fn scope(&self, call: &RequestContext<RoleServer>) -> Result<CallScope, rmcp::ErrorData> {
        let tenant = tenant_of(call).ok_or_else(|| {
            rmcp::ErrorData::invalid_request(
                "This call carries no authenticated tenant.".to_string(),
                None,
            )
        })?;
        let record = self.registry.get(&tenant.id).ok_or_else(|| {
            rmcp::ErrorData::invalid_request(self.no_wallet_message(&tenant.id), None)
        })?;
        Ok(CallScope {
            accounts: Arc::new(TenantAccounts::new(&record)),
            // Hosted wallets live on mainnet and there is exactly one.
            network_override: Some(MAINNET_NETWORK.to_string()),
            account_override: Some(record.account_name.clone()),
            rpc_url_override: None,
            approval: Arc::new(PolicyApproval {
                subject: record.subject.clone(),
                policy: record.policy,
                ledger: self.registry.ledger.clone(),
            }),
            // The server has no access to the caller's files.
            body_files: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_core::backend::Gate;
    use pay_core::keystore::AuthIntent;

    pub(crate) fn record(subject: &str) -> TenantRecord {
        let mut credentials = Credentials::new();
        credentials.insert("secret_key".to_string(), "sk_test_1".to_string());
        credentials.insert("wallet_secret".to_string(), "ws".to_string());
        TenantRecord {
            subject: subject.to_string(),
            account_name: "grok".to_string(),
            provider: "openfort".to_string(),
            wallet_id: "acc_1".to_string(),
            pubkey: "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z".to_string(),
            credentials,
            policy: SpendPolicy::dollars(1.0, 5.0),
        }
    }

    #[test]
    fn tenant_accounts_expose_one_gated_remote_account_and_its_credentials() {
        let accounts = TenantAccounts::new(&record("sub_1"));
        let file = accounts.load().unwrap();
        let (name, account) = file.account_for_network(MAINNET_NETWORK).unwrap();
        assert_eq!(name, "grok");
        assert_eq!(account.backend, BackendKind::Remote);
        assert_eq!(account.provider.as_deref(), Some("openfort"));
        assert_eq!(account.account.as_deref(), Some("acc_1"));
        assert!(
            account.auth_required_for_network(MAINNET_NETWORK),
            "policy must apply"
        );
        assert!(accounts.save(&file).is_err());

        let creds = accounts
            .credential_source()
            .load(
                "grok",
                "openfort",
                Gate::Disabled,
                &AuthIntent::default_payment(),
            )
            .unwrap();
        assert_eq!(creds["secret_key"], "sk_test_1");
    }

    #[test]
    fn subject_cookie_round_trips() {
        use axum::http::{HeaderMap, header};
        let value = cookie::set("sub_abc", true);
        let text = value.to_str().unwrap();
        assert!(
            text.starts_with(
                "pay_subject=sub_abc; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax; Secure"
            ),
            "{text}"
        );
        assert!(!cookie::set("s", false).to_str().unwrap().contains("Secure"));
        assert_eq!(
            cookie::clear(true).to_str().unwrap(),
            "pay_subject=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "theme=dark; pay_subject=sub_abc; other=1".parse().unwrap(),
        );
        assert_eq!(cookie::subject(&headers).as_deref(), Some("sub_abc"));
        let mut none = HeaderMap::new();
        none.insert(header::COOKIE, "theme=dark".parse().unwrap());
        assert_eq!(cookie::subject(&none), None);
        assert_eq!(cookie::subject(&HeaderMap::new()), None);
    }

    #[test]
    fn subjects_are_stable_opaque_and_provider_scoped() {
        let a = subject_for("openfort", "pro_123");
        assert_eq!(a, subject_for("openfort", "pro_123"));
        assert_ne!(a, subject_for("openfort", "pro_124"));
        assert_ne!(a, subject_for("circle", "pro_123"));
        assert!(a.starts_with("sub_") && a.len() == 36, "{a}");
        assert!(!a.contains("pro_123"));
    }

    #[test]
    fn credentials_can_be_refreshed_in_place() {
        let registry = TenantRegistry::new();
        registry.bind(record("sub_1"));
        let updated = registry
            .update_credentials("sub_1", |c| {
                c.insert("secret_key".to_string(), "sk_rotated".to_string());
            })
            .unwrap();
        assert_eq!(updated.credentials["secret_key"], "sk_rotated");
        assert_eq!(
            updated.credentials["wallet_secret"], "ws",
            "untouched fields stay"
        );
        assert_eq!(
            registry.get("sub_1").unwrap().credentials["secret_key"],
            "sk_rotated"
        );
        assert!(registry.update_credentials("sub_x", |_| {}).is_none());
    }

    #[test]
    fn link_tickets_are_single_use_and_name_their_subject() {
        let registry = TenantRegistry::new();
        let ticket = registry.mint_link("guest_1").unwrap();
        assert!(ticket.len() >= 32);
        assert_eq!(registry.peek_link(&ticket).as_deref(), Some("guest_1"));
        assert_eq!(registry.take_link(&ticket).as_deref(), Some("guest_1"));
        assert!(registry.peek_link(&ticket).is_none(), "consumed");
        assert!(registry.take_link("nope").is_none());
        assert!(is_guest("guest_abc") && !is_guest("sub_abc"));
    }

    #[test]
    fn registry_binds_and_rebinds_by_subject() {
        let registry = TenantRegistry::new();
        assert!(registry.is_empty());
        registry.bind(record("sub_1"));
        let mut again = record("sub_1");
        again.wallet_id = "acc_2".to_string();
        registry.bind(again);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("sub_1").unwrap().wallet_id, "acc_2");
        assert!(registry.get("sub_2").is_none());
        assert!(registry.remove("sub_1").is_some());
        assert!(registry.is_empty());
    }

    // ── Through /mcp ───────────────────────────────────────────────────

    use crate::mcp::tests::{INIT, INITIALIZED, app_with_tenants, mcp_post, sse_json};
    use axum::http::StatusCode;

    const TOPUP: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"topup","arguments":{"method":"mobile_wallet","amount_usdc":5}}}"#;
    const CURL_FILE: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"curl","arguments":{"url":"https://example.test/x","method":"POST","body_file":"/tmp/x.json"}}}"#;

    async fn session(app: &axum::Router, bearer: &str) -> String {
        let (status, headers, body) = mcp_post(app, Some(bearer), None, INIT).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = headers["mcp-session-id"].to_str().unwrap().to_string();
        let (status, _, _) = mcp_post(app, Some(bearer), Some(&id), INITIALIZED).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        id
    }

    #[tokio::test]
    async fn tools_act_for_the_bound_tenant() {
        let registry = Arc::new(TenantRegistry::new());
        registry.bind(record(&crate::mcp::token_fingerprint("tok-alpha")));
        let app = app_with_tenants(registry);
        let sid = session(&app, "tok-alpha").await;

        let (status, _, body) = mcp_post(&app, Some("tok-alpha"), Some(&sid), TOPUP).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result = &sse_json(&body)[0]["result"];
        assert_ne!(result["isError"], true, "{body}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z"),
            "{text}"
        );
        assert!(text.contains("grok"), "{text}");
    }

    #[tokio::test]
    async fn a_subject_without_a_wallet_is_told_to_finish_setup() {
        let app = app_with_tenants(Arc::default());
        let sid = session(&app, "tok-alpha").await;
        let (status, _, body) = mcp_post(&app, Some("tok-alpha"), Some(&sid), TOPUP).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let message = sse_json(&body)[0]["error"]["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains("no wallet yet"), "{body}");
    }

    #[tokio::test]
    async fn hosted_calls_cannot_read_the_servers_files() {
        let registry = Arc::new(TenantRegistry::new());
        registry.bind(record(&crate::mcp::token_fingerprint("tok-alpha")));
        let app = app_with_tenants(registry);
        let sid = session(&app, "tok-alpha").await;
        let (_, _, body) = mcp_post(&app, Some("tok-alpha"), Some(&sid), CURL_FILE).await;
        let result = &sse_json(&body)[0]["result"];
        assert_eq!(result["isError"], true, "{body}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not available on this server"), "{text}");
    }
}
