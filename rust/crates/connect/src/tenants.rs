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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use pay_core::accounts::{Account, AccountsFile, AccountsStore, BackendKind, MAINNET_NETWORK};
use pay_core::keystore::{AuthIntent, USD_MINOR_UNITS_PER_DOLLAR};
use pay_core::remote::{CredentialSource, Credentials, MemoryCredentials};
use pay_mcp::context::{CallScope, PayContext};
use pay_mcp::policy::{
    MemoryLedger, PolicyApproval, PolicyGate, Reserved, SpendLedger, SpendPolicy,
};
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

/// The upstream login session a browser or credential acts in: the
/// provider's session id (Privy's `sid`) and when the token proving it was
/// issued, in unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSession {
    pub id: String,
    pub issued_at: u64,
}

/// What a browser's signed subject cookie says: who it is and the login
/// session it was set in. A cookie never stands on its own; every approval
/// made with it is bound to, and revoked with, that session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLogin {
    pub subject: String,
    pub session: LoginSession,
}

/// One tenant's wallet and rules.
#[derive(Clone)]
pub struct TenantRecord {
    /// OAuth subject (or static-token fingerprint) this wallet belongs to.
    pub subject: String,
    /// Account name shown in prompts and receipts.
    pub account_name: String,
    /// Remote provider id (`privy`).
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
    /// The spend-ledger key: the provider wallet, not the subject, so every
    /// connection linked to one wallet shares one allowance.
    pub fn ledger_key(&self) -> String {
        format!("wallet:{}:{}", self.provider, self.wallet_id)
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
    claimed: bool,
}

/// What a wallet has already paid out, from a record that outlives this
/// process, so a restart cannot hand out a second daily allowance.
#[async_trait::async_trait]
pub trait SpendHistory: Send + Sync {
    /// USD minor units `wallet` paid out in stablecoins during the current
    /// UTC day. An error means unknown, and binding fails closed.
    async fn spent_today(&self, wallet: &str) -> Result<u64, String>;
}

/// A history with a fixed answer, for tests.
pub struct FixedHistory(pub u64);

#[async_trait::async_trait]
impl SpendHistory for FixedHistory {
    async fn spent_today(&self, _wallet: &str) -> Result<u64, String> {
        Ok(self.0)
    }
}

/// Every bound tenant, by subject, plus the link tickets outstanding.
pub struct TenantRegistry {
    tenants: Mutex<HashMap<String, Arc<TenantRecord>>>,
    links: Mutex<HashMap<String, LinkTicket>>,
    provisioning: Mutex<HashSet<String>>,
    /// Ledger keys whose spend before this process started is recorded.
    seeded: Mutex<HashSet<String>>,
    ledger: Arc<dyn SpendLedger>,
    cookie_key: [u8; 32],
}

/// An allowance reservation that is automatically released unless a wallet
/// provider actually returns a signature.
pub struct SpendReservation {
    subject: String,
    reserved: Option<Reserved>,
    ledger: Arc<dyn SpendLedger>,
    committed: bool,
}

impl SpendReservation {
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for SpendReservation {
    fn drop(&mut self) {
        if !self.committed
            && let Some(reserved) = self.reserved
        {
            self.ledger
                .release(&self.subject, reserved.day, reserved.amount);
        }
    }
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
        use rand::RngCore;
        let mut cookie_key = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut cookie_key);
        Self {
            tenants: Mutex::default(),
            links: Mutex::default(),
            provisioning: Mutex::default(),
            seeded: Mutex::default(),
            ledger,
            cookie_key,
        }
    }

    /// Apply a tenant's hosted spending policy to a transaction whose amount
    /// has already been derived from its instructions, not supplied by the
    /// caller. Successful authorization reserves the amount in the ledger.
    pub fn reserve_cli_transaction(
        &self,
        tenant: &TenantRecord,
        amount_minor: u64,
    ) -> Result<SpendReservation, String> {
        let whole = amount_minor / USD_MINOR_UNITS_PER_DOLLAR;
        let fraction = amount_minor % USD_MINOR_UNITS_PER_DOLLAR;
        let amount = format!("${whole}.{fraction:04}");
        let key = tenant.ledger_key();
        let reserved = PolicyGate::new(&key, tenant.policy, self.ledger.clone())
            .reserve(&AuthIntent::authorize_payment_details(
                &amount,
                "hosted CLI transaction",
                "connect.pay.sh",
            ))
            .map_err(|error| error.to_string())?;
        Ok(SpendReservation {
            subject: key,
            reserved,
            ledger: self.ledger.clone(),
            committed: false,
        })
    }

    /// Record what `tenant`'s wallet already spent today before this process
    /// saw it, once per wallet. The in-process ledger covers the rest.
    pub async fn seed_spend(
        &self,
        tenant: &TenantRecord,
        history: &dyn SpendHistory,
    ) -> Result<(), String> {
        let key = tenant.ledger_key();
        if self.seeded.lock().unwrap().contains(&key) {
            return Ok(());
        }
        let spent = history.spent_today(&tenant.pubkey).await?;
        if self.seeded.lock().unwrap().insert(key.clone()) {
            let _ = self.ledger.check_and_record(&key, spent, None);
        }
        Ok(())
    }

    fn mac_hex(&self, purpose: &[u8], value: &str) -> String {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&self.cookie_key)
            .expect("HMAC accepts a key of any size");
        mac.update(purpose);
        mac.update(&[0]);
        mac.update(value.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn sign_cookie_value(&self, purpose: &[u8], value: &str) -> String {
        format!("{value}.{}", self.mac_hex(purpose, value))
    }

    /// Derive the opaque tenant subject from a provider-authenticated
    /// identity. The server secret makes this safe even when the identity is
    /// itself a credential, and the in-memory registry resets with the key.
    pub fn provider_subject(&self, provider_id: &str, identity: &str) -> String {
        let digest = self.mac_hex(b"provider_identity", &format!("{provider_id}:{identity}"));
        format!("sub_{}", &digest[..32])
    }

    fn verify_cookie_value(&self, purpose: &[u8], signed: &str) -> Option<String> {
        let (value, signature) = signed.rsplit_once('.')?;
        let signature = hex_decode(signature)?;
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&self.cookie_key).ok()?;
        mac.update(purpose);
        mac.update(&[0]);
        mac.update(value.as_bytes());
        mac.verify_slice(&signature).ok()?;
        Some(value.to_string())
    }

    /// Read a server-authenticated browser subject. Unsigned and tampered
    /// cookies are deliberately ignored.
    pub fn subject_from_cookie(&self, headers: &axum::http::HeaderMap) -> Option<String> {
        self.login_from_cookie(headers).map(|login| login.subject)
    }

    /// Who this browser signed in as, and in which login session. Whether
    /// that session is still live is the caller's question to the provider.
    pub fn login_from_cookie(&self, headers: &axum::http::HeaderMap) -> Option<BrowserLogin> {
        use base64::Engine;
        let signed = cookie::value(headers, cookie::SUBJECT_NAME)?;
        let value = self.verify_cookie_value(b"subject", &signed)?;
        let mut parts = value.split('~');
        let (Some(subject), Some(id), Some(issued_at), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(id)
            .ok()
            .and_then(|id| String::from_utf8(id).ok())
            .filter(|id| !id.is_empty())?;
        if subject.is_empty() || subject.len() > 128 {
            return None;
        }
        Some(BrowserLogin {
            subject: subject.to_string(),
            session: LoginSession {
                id,
                issued_at: issued_at.parse().ok()?,
            },
        })
    }

    /// Remember a browser as `subject`, signed in during `session`: the
    /// cookie can approve again only while that session is live.
    pub fn subject_cookie(
        &self,
        subject: &str,
        session: &LoginSession,
        secure: bool,
    ) -> axum::http::HeaderValue {
        use base64::Engine;
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&session.id);
        let value = format!("{subject}~{id}~{}", session.issued_at);
        cookie::set(
            cookie::SUBJECT_NAME,
            &self.sign_cookie_value(b"subject", &value),
            secure,
        )
    }

    /// Bind link completion to the browser that deliberately opened the
    /// confirmation view. Only the ticket hash is placed in the cookie.
    pub fn link_cookie(&self, ticket: &str, secure: bool) -> axum::http::HeaderValue {
        let ticket_hash = crate::protocol::sha256_hex(ticket);
        cookie::set(
            cookie::LINK_NAME,
            &self.sign_cookie_value(b"link", &ticket_hash),
            secure,
        )
    }

    pub fn link_cookie_matches(&self, headers: &axum::http::HeaderMap, ticket: &str) -> bool {
        let Some(signed) = cookie::value(headers, cookie::LINK_NAME) else {
            return false;
        };
        self.verify_cookie_value(b"link", &signed).as_deref()
            == Some(crate::protocol::sha256_hex(ticket).as_str())
    }

    /// A fresh ticket for `subject`; `None` when the table is full of live
    /// tickets. The plaintext goes into the tool error, only its hash stays.
    pub fn mint_link(&self, subject: &str) -> Option<String> {
        let ticket = crate::protocol::random_token();
        let now = Instant::now();
        let mut links = self.links.lock().unwrap();
        links.retain(|_, t| now.saturating_duration_since(t.created_at) <= LINK_TTL);
        if links.values().any(|ticket| ticket.subject == subject) {
            return None;
        }
        if links.len() >= MAX_LINKS {
            return None;
        }
        links.insert(
            crate::protocol::sha256_hex(&ticket),
            LinkTicket {
                subject: subject.to_string(),
                created_at: now,
                claimed: false,
            },
        );
        Some(ticket)
    }

    /// The subject a live ticket is for, without consuming it.
    pub fn peek_link(&self, ticket: &str) -> Option<String> {
        let links = self.links.lock().unwrap();
        let t = links.get(&crate::protocol::sha256_hex(ticket))?;
        (!t.claimed && Instant::now().saturating_duration_since(t.created_at) <= LINK_TTL)
            .then(|| t.subject.clone())
    }

    /// Reserve a live ticket before external provisioning. Dropping the
    /// claim releases it for a retry; committing consumes it.
    pub(crate) fn claim_link<'a>(&'a self, ticket: &str) -> Option<LinkClaim<'a>> {
        let key = crate::protocol::sha256_hex(ticket);
        let mut links = self.links.lock().unwrap();
        let entry = links.get_mut(&key)?;
        if entry.claimed || Instant::now().saturating_duration_since(entry.created_at) > LINK_TTL {
            return None;
        }
        entry.claimed = true;
        Some(LinkClaim {
            registry: self,
            key,
            subject: entry.subject.clone(),
            committed: false,
        })
    }

    /// Reserve first-time provisioning for one stable provider subject.
    pub(crate) fn claim_provisioning<'a>(&'a self, subject: &str) -> Option<ProvisioningClaim<'a>> {
        let mut provisioning = self.provisioning.lock().unwrap();
        if !provisioning.insert(subject.to_string()) {
            return None;
        }
        Some(ProvisioningClaim {
            registry: self,
            subject: subject.to_string(),
        })
    }

    /// Bind (or rebind) a subject to a wallet.
    pub fn bind(&self, record: TenantRecord) {
        self.tenants
            .lock()
            .unwrap()
            .insert(record.subject.clone(), Arc::new(record));
    }

    /// Bind the provider wallet's owner, in one step unbinding every other
    /// subject still on that wallet: a former owner, or connections that
    /// former owner linked. Returns those subjects, whose credentials the
    /// caller must revoke.
    pub fn bind_owner(&self, record: TenantRecord) -> Vec<String> {
        let mut tenants = self.tenants.lock().unwrap();
        let stale: Vec<String> = tenants
            .values()
            .filter(|t| {
                t.provider == record.provider
                    && t.wallet_id == record.wallet_id
                    && t.subject != record.subject
            })
            .map(|t| t.subject.clone())
            .collect();
        for subject in &stale {
            tenants.remove(subject);
        }
        tenants.insert(record.subject.clone(), Arc::new(record));
        stale
    }

    pub fn get(&self, subject: &str) -> Option<Arc<TenantRecord>> {
        self.tenants.lock().unwrap().get(subject).cloned()
    }

    /// Unbind every subject on one provider wallet; returns those subjects.
    pub fn remove_wallet(&self, provider: &str, wallet_id: &str) -> Vec<String> {
        let mut tenants = self.tenants.lock().unwrap();
        let subjects: Vec<String> = tenants
            .values()
            .filter(|t| t.provider == provider && t.wallet_id == wallet_id)
            .map(|t| t.subject.clone())
            .collect();
        for subject in &subjects {
            tenants.remove(subject);
        }
        subjects
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

pub(crate) struct LinkClaim<'a> {
    registry: &'a TenantRegistry,
    key: String,
    subject: String,
    committed: bool,
}

impl LinkClaim<'_> {
    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn commit(mut self) {
        self.registry.links.lock().unwrap().remove(&self.key);
        self.committed = true;
    }
}

impl Drop for LinkClaim<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Some(ticket) = self.registry.links.lock().unwrap().get_mut(&self.key)
        {
            ticket.claimed = false;
        }
    }
}

pub(crate) struct ProvisioningClaim<'a> {
    registry: &'a TenantRegistry,
    subject: String,
}

impl Drop for ProvisioningClaim<'_> {
    fn drop(&mut self) {
        self.registry
            .provisioning
            .lock()
            .unwrap()
            .remove(&self.subject);
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

    pub const SUBJECT_NAME: &str = "pay_subject";
    pub const LINK_NAME: &str = "pay_link";
    const ONE_YEAR: u64 = 365 * 24 * 60 * 60;

    /// The subject the request's cookie names, if any.
    pub(super) fn value(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|line| line.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty() && v.len() <= 256)
    }

    /// `Set-Cookie` for `subject`. `Secure` when the site is served over
    /// https; a local http server would otherwise never see it back.
    pub(super) fn set(name: &str, value: &str, secure: bool) -> HeaderValue {
        let mut cookie =
            format!("{name}={value}; Path=/; Max-Age={ONE_YEAR}; HttpOnly; SameSite=Lax");
        if secure {
            cookie.push_str("; Secure");
        }
        HeaderValue::from_str(&cookie).expect("cookie is ascii")
    }

    /// `Set-Cookie` that deletes the subject cookie.
    pub fn clear(secure: bool) -> HeaderValue {
        let mut cookie = format!("{SUBJECT_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax");
        if secure {
            cookie.push_str("; Secure");
        }
        HeaderValue::from_str(&cookie).expect("cookie is ascii")
    }
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
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
                     connect.pay.sh, then try again."
                .to_string(),
        }
    }
}

impl CloudContext {
    fn approval(&self, record: &TenantRecord) -> PolicyApproval {
        PolicyApproval {
            subject: record.ledger_key(),
            policy: record.policy,
            ledger: self.registry.ledger.clone(),
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
            approval: Arc::new(self.approval(&record)),
            // Fail closed on challenge-selected terms: mainnet stablecoins
            // only, before any payment is built or signed.
            payment_permissions: Some(Arc::new(pay_mcp::McpPermissions::hosted())),
            // The server has no access to the caller's files.
            body_files: false,
            // Arbitrary public APIs remain available, but tenant-controlled
            // requests cannot reach Cloud Run metadata, loopback, or private
            // services from this privileged process.
            outbound_policy: pay_core::client::fetch::OutboundPolicy::PublicInternetOnly,
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
            provider: "test-provider".to_string(),
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
        assert_eq!(account.provider.as_deref(), Some("test-provider"));
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
                "test-provider",
                Gate::Disabled,
                &AuthIntent::default_payment(),
            )
            .unwrap();
        assert_eq!(creds["secret_key"], "sk_test_1");
    }

    #[test]
    fn subject_cookie_round_trips() {
        use axum::http::{HeaderMap, header};
        let registry = TenantRegistry::new();
        let session = LoginSession {
            id: "sess_1".to_string(),
            issued_at: 1_700_000_000,
        };
        let value = registry.subject_cookie("sub_abc", &session, true);
        let text = value.to_str().unwrap();
        assert!(text.starts_with("pay_subject=sub_abc~"), "{text}");
        assert!(text.ends_with("; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax; Secure"));
        assert!(
            !registry
                .subject_cookie("s", &session, false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
        assert_eq!(
            cookie::clear(true).to_str().unwrap(),
            "pay_subject=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );

        let mut headers = HeaderMap::new();
        let request_cookie = text.split(';').next().unwrap();
        headers.insert(
            header::COOKIE,
            format!("theme=dark; {request_cookie}; other=1")
                .parse()
                .unwrap(),
        );
        assert_eq!(
            registry.subject_from_cookie(&headers).as_deref(),
            Some("sub_abc")
        );
        assert_eq!(
            registry.login_from_cookie(&headers),
            Some(BrowserLogin {
                subject: "sub_abc".to_string(),
                session: session.clone(),
            })
        );
        // A signed subject without a login session (the old format) is
        // not a login.
        let mut sessionless = HeaderMap::new();
        sessionless.insert(
            header::COOKIE,
            format!(
                "pay_subject={}",
                registry.sign_cookie_value(b"subject", "sub_abc")
            )
            .parse()
            .unwrap(),
        );
        assert_eq!(registry.login_from_cookie(&sessionless), None);

        let mut tampered = HeaderMap::new();
        tampered.insert(
            header::COOKIE,
            request_cookie
                .replace("sub_abc", "sub_evil")
                .parse()
                .unwrap(),
        );
        assert_eq!(registry.subject_from_cookie(&tampered), None);
        assert_eq!(TenantRegistry::new().subject_from_cookie(&headers), None);
        let mut none = HeaderMap::new();
        none.insert(header::COOKIE, "theme=dark".parse().unwrap());
        assert_eq!(registry.subject_from_cookie(&none), None);
        assert_eq!(registry.subject_from_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn subjects_are_stable_opaque_and_provider_scoped() {
        let a = subject_for("test-provider", "account_123");
        assert_eq!(a, subject_for("test-provider", "account_123"));
        assert_ne!(a, subject_for("test-provider", "account_124"));
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
        let claim = registry.claim_link(&ticket).unwrap();
        assert_eq!(claim.subject(), "guest_1");
        assert!(registry.peek_link(&ticket).is_none(), "claimed");
        drop(claim);
        assert!(registry.peek_link(&ticket).is_some(), "released for retry");
        registry.claim_link(&ticket).unwrap().commit();
        assert!(registry.peek_link(&ticket).is_none(), "consumed");
        assert!(registry.claim_link("nope").is_none());
        assert!(is_guest("guest_abc") && !is_guest("sub_abc"));
    }

    #[test]
    fn a_guest_can_hold_only_one_link_ticket() {
        let registry = TenantRegistry::new();
        assert!(registry.mint_link("guest_1").is_some());
        assert!(registry.mint_link("guest_1").is_none());
        assert!(registry.mint_link("guest_2").is_some());
    }

    #[test]
    fn first_time_provisioning_is_exclusive_per_subject() {
        let registry = TenantRegistry::new();
        let first = registry.claim_provisioning("sub_1").unwrap();
        assert!(registry.claim_provisioning("sub_1").is_none());
        assert!(registry.claim_provisioning("sub_2").is_some());
        drop(first);
        assert!(registry.claim_provisioning("sub_1").is_some());
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

    #[test]
    fn failed_signing_releases_reserved_allowance() {
        let ledger = Arc::new(MemoryLedger::new());
        let registry = TenantRegistry::with_ledger(ledger.clone());
        let tenant = record("sub_1");

        {
            let _failed = registry.reserve_cli_transaction(&tenant, 10_000).unwrap();
            assert_eq!(ledger.spent_today(&tenant.ledger_key()), 10_000);
        }
        assert_eq!(ledger.spent_today(&tenant.ledger_key()), 0);

        registry
            .reserve_cli_transaction(&tenant, 10_000)
            .unwrap()
            .commit();
        assert_eq!(ledger.spent_today(&tenant.ledger_key()), 10_000);
    }

    #[test]
    fn a_prior_day_failed_reservation_does_not_erase_new_day_spend() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let day = Arc::new(AtomicU64::new(100));
        let clock_day = day.clone();
        let ledger = Arc::new(MemoryLedger::with_clock(Arc::new(move || {
            clock_day.load(Ordering::SeqCst)
        })));
        let registry = TenantRegistry::with_ledger(ledger.clone());
        let mut tenant = record("sub_1");
        tenant.policy = SpendPolicy {
            per_call_ceiling: Some(100_000),
            daily_cap: Some(100_000),
        };

        let failed = registry.reserve_cli_transaction(&tenant, 60_000).unwrap();
        day.store(101, Ordering::SeqCst);
        registry
            .reserve_cli_transaction(&tenant, 60_000)
            .unwrap()
            .commit();
        drop(failed);
        assert_eq!(ledger.spent_today(&tenant.ledger_key()), 60_000);
        assert!(
            registry.reserve_cli_transaction(&tenant, 100_000).is_err(),
            "the committed day-101 spend still counts against the cap"
        );
        registry
            .reserve_cli_transaction(&tenant, 40_000)
            .unwrap()
            .commit();
    }

    #[tokio::test]
    async fn a_restarted_registry_counts_what_the_wallet_already_spent() {
        // Before the restart: the full $5 daily cap is spent.
        let before = TenantRegistry::new();
        let tenant = record("sub_1");
        for _ in 0..5 {
            before
                .reserve_cli_transaction(&tenant, 10_000)
                .unwrap()
                .commit();
        }
        assert!(before.reserve_cli_transaction(&tenant, 1).is_err());

        // After it: the chain says $5 left the wallet today.
        let after = TenantRegistry::new();
        after
            .seed_spend(&tenant, &FixedHistory(50_000))
            .await
            .unwrap();
        assert!(after.reserve_cli_transaction(&tenant, 1).is_err());
        // Seeding happens once; a later login does not double count.
        let partial = TenantRegistry::new();
        partial
            .seed_spend(&tenant, &FixedHistory(30_000))
            .await
            .unwrap();
        partial
            .seed_spend(&tenant, &FixedHistory(30_000))
            .await
            .unwrap();
        partial
            .reserve_cli_transaction(&tenant, 10_000)
            .unwrap()
            .commit();
        partial
            .reserve_cli_transaction(&tenant, 10_000)
            .unwrap()
            .commit();
        assert!(partial.reserve_cli_transaction(&tenant, 1).is_err());
    }

    #[tokio::test]
    async fn an_unknown_history_seeds_nothing_and_retries() {
        struct Down;
        #[async_trait::async_trait]
        impl SpendHistory for Down {
            async fn spent_today(&self, _wallet: &str) -> Result<u64, String> {
                Err("rpc down".into())
            }
        }
        let registry = TenantRegistry::new();
        let tenant = record("sub_1");
        assert!(registry.seed_spend(&tenant, &Down).await.is_err());
        registry
            .seed_spend(&tenant, &FixedHistory(50_000))
            .await
            .unwrap();
        assert!(registry.reserve_cli_transaction(&tenant, 1).is_err());
    }

    #[test]
    fn linked_subjects_share_one_wallet_allowance() {
        let registry = TenantRegistry::new();
        let stable = record("sub_owner");
        // What `link_complete` binds: the stable record under a guest subject.
        let mut guest_one = stable.clone();
        guest_one.subject = "guest_one".to_string();
        let mut guest_two = stable.clone();
        guest_two.subject = "guest_two".to_string();

        // $1 per call, $5 per day, across every alias of the wallet.
        for tenant in [&stable, &guest_one, &guest_two, &guest_one, &stable] {
            registry
                .reserve_cli_transaction(tenant, 10_000)
                .unwrap()
                .commit();
        }
        for tenant in [&stable, &guest_one, &guest_two] {
            assert!(registry.reserve_cli_transaction(tenant, 1).is_err());
        }

        // A different wallet has its own allowance.
        let mut other = record("guest_three");
        other.wallet_id = "acc_2".to_string();
        registry
            .reserve_cli_transaction(&other, 10_000)
            .unwrap()
            .commit();
    }

    #[test]
    fn hosted_mcp_approval_shares_the_cli_wallet_allowance() {
        let ledger = Arc::new(MemoryLedger::new());
        let registry = Arc::new(TenantRegistry::with_ledger(ledger.clone()));
        let context = CloudContext::new(registry.clone());
        let stable = record("sub_owner");
        let mut guest = stable.clone();
        guest.subject = "guest_one".to_string();
        let approval = context.approval(&guest);
        assert_eq!(approval.subject, stable.ledger_key());
        registry
            .reserve_cli_transaction(&stable, 10_000)
            .unwrap()
            .commit();
        assert_eq!(ledger.spent_today(&approval.subject), 10_000);
    }

    // ── Through /mcp ───────────────────────────────────────────────────

    use crate::mcp::tests::{INIT, INITIALIZED, app_with_tenants, mcp_post, sse_json};
    use axum::http::StatusCode;

    const TOPUP: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"topup","arguments":{"method":"mobile_wallet","amount_usdc":5}}}"#;
    const CURL_FILE: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"curl","arguments":{"url":"https://example.test/x","method":"POST","body_file":"/tmp/x.json"}}}"#;
    const CURL_PRIVATE: &str = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"curl","arguments":{"url":"http://127.0.0.1:1/private"}}}"#;

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

    #[tokio::test]
    async fn hosted_curl_rejects_non_public_destinations() {
        let registry = Arc::new(TenantRegistry::new());
        registry.bind(record(&crate::mcp::token_fingerprint("tok-alpha")));
        let app = app_with_tenants(registry);
        let sid = session(&app, "tok-alpha").await;
        let (_, _, body) = mcp_post(&app, Some("tok-alpha"), Some(&sid), CURL_PRIVATE).await;
        let result = &sse_json(&body)[0]["result"];
        assert_eq!(result["isError"], true, "{body}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("non-public"), "{text}");
    }
}
