//! Resolve a signer for a pay account.
//!
//! Every account is described by a [`SigningBackend`] (see
//! [`crate::backend`]); this module turns an account plus an approval
//! policy into a [`ResolvedSigner`]. Local keystore backends hand back a
//! keypair that becomes an in-memory signer; remote providers hand back a
//! signer that talks to their API. Callers never match on the account kind
//! here: what a backend can do (export, raw message signing) is asked of the
//! descriptor.

use pay_kit::mpp::solana_keychain::MemorySigner;
use pay_kit::solana_keychain::{
    SignTransactionResult, SignerError, SolanaSigner, TransactionSigner,
};
use solana_transaction::versioned::VersionedTransaction;

use crate::accounts::{
    Account, AccountChoice, AccountsFile, AccountsStore, BackendKind, MAINNET_NETWORK,
    ResolvedEphemeral, load_or_create_ephemeral_for_network,
    load_or_create_ephemeral_for_network_as, resolve_account_for_network,
};
use crate::backend::{Custody, Gate, SigningBackend};
use crate::keystore::{AuthGate, AuthIntent};
use crate::{Error, Result};

/// Signer resolved from a pay account, together with the backend it came
/// from. Implements [`SolanaSigner`] and [`TransactionSigner`] by
/// delegation, so both MPP and x402 payment paths accept it wherever a
/// `&dyn TransactionSigner` is expected, and exposes the backend's
/// capabilities so those paths can pick a compatible payment scheme.
pub struct ResolvedSigner {
    backend: &'static dyn SigningBackend,
    inner: SignerImpl,
}

enum SignerImpl {
    /// An in-memory keypair loaded from a local keystore.
    Memory(Box<MemorySigner>),
    /// Any provider registered in [`crate::remote`]; pay never needs to
    /// know which one.
    Remote(Box<dyn TransactionSigner>),
}

impl ResolvedSigner {
    /// A signer whose keypair was loaded from `backend` into memory.
    pub fn local(backend: &'static dyn SigningBackend, signer: MemorySigner) -> Self {
        Self {
            backend,
            inner: SignerImpl::Memory(Box::new(signer)),
        }
    }

    /// A signer that signs through a remote or hardware `backend`.
    pub fn remote(
        backend: &'static dyn SigningBackend,
        signer: Box<dyn TransactionSigner>,
    ) -> Self {
        Self {
            backend,
            inner: SignerImpl::Remote(signer),
        }
    }

    /// The backend this signer came from.
    pub fn backend(&self) -> &'static dyn SigningBackend {
        self.backend
    }

    /// Where the key is held.
    pub fn custody(&self) -> Custody {
        self.backend.custody()
    }

    /// Whether the raw keypair could be read out of this account's backend.
    pub fn is_exportable(&self) -> bool {
        self.backend.is_exportable()
    }

    /// Whether `sign_message` returns a raw ed25519 signature over the
    /// bytes given. See [`SigningBackend::signs_raw_messages`].
    pub fn signs_raw_messages(&self) -> bool {
        self.backend.signs_raw_messages()
    }

    /// Highest transaction version this signer can produce, to pass as the
    /// builders' `max_tx_version`. See [`SigningBackend::max_tx_version`].
    pub fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        self.backend.max_tx_version()
    }

    /// Refuse, with an explanation, when a payment path needs a raw
    /// `sign_message` signature this backend cannot produce. `what` names
    /// the thing being signed ("an x402 sign-in challenge").
    pub fn require_raw_message_signing(&self, what: &str) -> Result<()> {
        self.backend.require_raw_message_signing(what)
    }

    fn as_dyn(&self) -> &dyn TransactionSigner {
        match &self.inner {
            SignerImpl::Memory(signer) => signer.as_ref(),
            SignerImpl::Remote(signer) => signer.as_ref(),
        }
    }
}

#[async_trait::async_trait]
impl SolanaSigner for ResolvedSigner {
    fn pubkey(&self) -> solana_pubkey::Pubkey {
        self.as_dyn().pubkey()
    }

    async fn sign_message(
        &self,
        message: &[u8],
    ) -> std::result::Result<solana_signature::Signature, SignerError> {
        self.as_dyn().sign_message(message).await
    }

    async fn is_available(&self) -> bool {
        self.as_dyn().is_available().await
    }
}

#[async_trait::async_trait]
impl TransactionSigner for ResolvedSigner {
    async fn sign_transaction(
        &self,
        tx: &mut VersionedTransaction,
    ) -> std::result::Result<SignTransactionResult, SignerError> {
        self.as_dyn().sign_transaction(tx).await
    }
}

/// Optional auth-gate override threaded through the signing path.
///
/// When present, the override replaces the platform's biometric gate
/// (Touch ID / Windows Hello / polkit) for accounts that require auth.
/// Used by `pay-mcp` to redirect approval prompts through MCP
/// elicitation when the connected client supports it. `None` keeps the
/// platform default, which is what every non-MCP caller passes.
///
/// The override is consumed (`Box`) because it is built per-request: a
/// fresh `ElicitationAuth` is constructed at the top of each MCP tool
/// call and threaded down through one signing operation.
pub type AuthOverride = Option<Box<dyn AuthGate>>;

/// Load a `MemorySigner` from the given source.
///
/// - `keychain:<account>` — load from macOS Keychain (triggers Touch ID)
/// - `gnome-keyring:<account>` — load from GNOME Keyring (triggers polkit)
/// - `windows-hello:<account>` — load from Windows Credential Manager (triggers Windows Hello)
/// - `1password:<account>` — load from 1Password (triggers `op` CLI auth)
/// - anything else — treat as a file path
pub fn load_signer(source: &str) -> Result<MemorySigner> {
    load_signer_with_intent(source, &AuthIntent::default_payment())
}

/// Load a signer for a payment, prefixing rejection errors with the amount
/// (e.g. "$0.10 payment authorization was rejected by user at Apple Keychain").
pub fn load_signer_for_payment(source: &str, amount: &str, desc: &str) -> Result<MemorySigner> {
    let intent = AuthIntent::authorize_payment(amount, desc);
    load_signer_with_intent(source, &intent).map_err(|e| match e {
        Error::PaymentRejected(where_) => {
            Error::PaymentRejected(format!("{amount} payment authorization was {where_}"))
        }
        other => other,
    })
}

// ── Network-aware loaders ───────────────────────────────────────────────────

/// Resolve the wallet for a Solana network slug and return a signer.
///
/// Lookup order:
///
/// 1. **`accounts.yml` mapping** — if `networks.<network>` points at an
///    account, use that account. Keystore-backed accounts go through the
///    normal `load_signer_with_reason` path; ephemeral accounts have
///    their inline secret bytes loaded directly (no Touch ID, no prompt).
///
/// 2. **Network-agnostic fallback** — if an explicitly named account has
///    no mapping on this network but exists on `mainnet` with a remote
///    signing backend (`keystore: remote`), use the mainnet entry. A
///    remote signer signs raw bytes and carries no chain state, so it is
///    valid on every network; without this fallback an explicit
///    `--account` would silently shadow the remote signer with a lazy
///    ephemeral of the same name. The mainnet entry's `auth_required`
///    travels with it; add an explicit `accounts.<network>` entry to
///    override per network.
///
/// 3. **Lazy ephemeral creation** — if no mapping exists AND the network
///    is one we consider "throwaway" (`localnet` / `devnet`), generate a
///    fresh ephemeral, persist it as `accounts.<network> + networks.<network>`,
///    and return it. The returned `Option<ResolvedEphemeral>` is `Some` only
///    in this case so the caller knows to print a notice.
///
/// 4. **Mainnet without a wallet** — error. We never auto-create a wallet
///    for `mainnet`; the user must run `pay setup` to bind their real
///    wallet first. This is intentional — silently generating a mainnet
///    wallet would be a footgun.
pub fn load_signer_for_network(
    network: &str,
    store: &dyn AccountsStore,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_with_intent(network, store, None, &AuthIntent::default_payment())
}

/// Variant of [`load_signer_for_network`] that takes an explicit reason
/// string for the keystore auth prompt (e.g.
/// "authorize payment of $0.10 for accessing API api.example.com").
pub fn load_signer_for_network_with_reason(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    reason: &str,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_with_intent(
        network,
        store,
        account_override,
        &AuthIntent::from_reason(reason),
    )
}

/// Variant of [`load_signer_for_network`] that takes a typed keystore auth
/// intent.
pub fn load_signer_for_network_with_intent(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    intent: &AuthIntent,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_with_intent_and_override(network, store, account_override, intent, None)
}

/// Variant of [`load_signer_for_network_with_intent`] that accepts an
/// optional auth-gate override.
pub fn load_signer_for_network_with_intent_and_override(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    let file = store.load()?;
    match select_account(&file, network, account_override)? {
        AccountSelection::Configured { name, account } => {
            let signer = load_signer_from_account_with_source(
                store.credential_source(),
                &account,
                &name,
                network,
                intent,
                auth_override,
            )?;
            Ok((signer, None))
        }
        AccountSelection::LazyEphemeral { name: Some(name) } => {
            let resolved = load_or_create_ephemeral_for_network_as(network, &name, store)?;
            let signer = signer_for_ephemeral_account(&resolved.account)?;
            Ok((signer, Some(resolved)))
        }
        AccountSelection::LazyEphemeral { name: None } => {
            let resolved = load_or_create_ephemeral_for_network(network, store)?;
            let signer = signer_for_ephemeral_account(&resolved.account)?;
            Ok((signer, Some(resolved)))
        }
    }
}

/// Which account a payment on `network` will use, before anything is
/// loaded or prompted for.
enum AccountSelection {
    /// An entry in `accounts.yml`.
    Configured { name: String, account: Box<Account> },
    /// Nothing configured on an ephemeral network: a throwaway wallet is
    /// created at payment time, under `name` when one was requested.
    LazyEphemeral { name: Option<String> },
}

/// Resolve the account for `network` the way every loader does: the named
/// account if `account_override` is given (falling back to a same-named
/// remote account on `mainnet`), else the network's default, else a lazy
/// ephemeral wallet on networks that allow one.
fn select_account(
    file: &AccountsFile,
    network: &str,
    account_override: Option<&str>,
) -> Result<AccountSelection> {
    if let Some(name) = account_override {
        if let Some(account) = file
            .named_account_for_network(network, name)
            .or_else(|| network_agnostic_fallback(file, network, name))
        {
            return Ok(AccountSelection::Configured {
                name: name.to_string(),
                account: Box::new(account.clone()),
            });
        }
        if is_lazy_ephemeral_network(network) {
            return Ok(AccountSelection::LazyEphemeral {
                name: Some(name.to_string()),
            });
        }
        return Err(Error::Config(format!(
            "No account named `{name}` configured for network `{network}`."
        )));
    }
    match resolve_account_for_network(network, file) {
        AccountChoice::Resolved { name, account } => {
            Ok(AccountSelection::Configured { name, account })
        }
        AccountChoice::Missing if is_lazy_ephemeral_network(network) => {
            Ok(AccountSelection::LazyEphemeral { name: None })
        }
        AccountChoice::Missing => Err(Error::Config(format!(
            "No account configured for network `{network}`.\n\n\
             Run `pay setup` to create an account."
        ))),
    }
}

/// The backend that will sign a payment on `network`, read from
/// `accounts.yml` alone: no secret is touched and nothing prompts. `None`
/// when no account is configured yet and a throwaway wallet will be
/// created at payment time (those are software keys that sign anything).
///
/// Lets a caller pick a payment offer the account can actually sign before
/// committing to it; see
/// [`crate::runner::RunOutcome::configured_signer_support`].
pub fn backend_for_network(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
) -> Result<Option<&'static dyn SigningBackend>> {
    let file = store.load()?;
    match select_account(&file, network, account_override)? {
        AccountSelection::Configured { account, .. } => account.descriptor().map(Some),
        AccountSelection::LazyEphemeral { .. } => Ok(None),
    }
}

/// Whether the configured signer can produce raw message signatures.
///
/// Returning only a boolean keeps account-derived configuration data out of
/// callers that may later write an unrelated HTTP response body to stdout.
pub fn raw_message_support_for_network(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
) -> Result<Option<bool>> {
    Ok(backend_for_network(network, store, account_override)?
        .map(crate::backend::SigningBackend::signs_raw_messages))
}

/// Fall back to a same-named `mainnet` account when the current network
/// has no mapping and the account signs remotely (`keystore: remote`).
///
/// Keypair-backed accounts never fall through: silently reusing a mainnet
/// key on another network is a footgun. A remote signer signs raw bytes
/// and carries no chain state, so the same account is valid on every
/// network.
fn network_agnostic_fallback<'a>(
    file: &'a AccountsFile,
    network: &str,
    name: &str,
) -> Option<&'a Account> {
    if network == MAINNET_NETWORK {
        return None;
    }
    file.named_account_for_network(MAINNET_NETWORK, name)
        .filter(|account| account.backend == BackendKind::Remote)
}

/// Network-aware loader for a payment, with the same amount-prefixed
/// rejection-error rewrap as [`load_signer_for_payment`].
pub fn load_signer_for_network_payment(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    amount: &str,
    desc: &str,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    let intent = AuthIntent::authorize_payment(amount, desc);
    load_signer_for_network_payment_with_intent(network, store, account_override, amount, &intent)
}

pub fn load_signer_for_network_payment_with_intent(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    amount: &str,
    intent: &AuthIntent,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_payment_with_intent_and_override(
        network,
        store,
        account_override,
        amount,
        intent,
        None,
    )
}

/// Variant of [`load_signer_for_network_payment_with_intent`] that accepts
/// an optional auth-gate override.
pub fn load_signer_for_network_payment_with_intent_and_override(
    network: &str,
    store: &dyn AccountsStore,
    account_override: Option<&str>,
    amount: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<(ResolvedSigner, Option<ResolvedEphemeral>)> {
    load_signer_for_network_with_intent_and_override(
        network,
        store,
        account_override,
        intent,
        auth_override,
    )
    .map_err(|e| match e {
        Error::PaymentRejected(where_) => {
            Error::PaymentRejected(format!("{amount} payment authorization was {where_}"))
        }
        other => other,
    })
}

/// Networks where missing-entry → auto-generate-an-ephemeral is a safe
/// default. Real money networks are NOT in this list — we refuse to
/// silently create a mainnet wallet.
fn is_lazy_ephemeral_network(network: &str) -> bool {
    matches!(network, "localnet" | "devnet")
}

// ── Account → keypair bytes ─────────────────────────────────────────────────

pub fn load_keypair_bytes_from_account_with_reason(
    account: &Account,
    name: &str,
    network: &str,
    reason: &str,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    load_keypair_bytes_from_account_with_intent(
        account,
        name,
        network,
        &AuthIntent::from_reason(reason),
    )
}

pub fn load_keypair_bytes_from_account_with_intent(
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    load_keypair_bytes_from_account_with_intent_and_override(account, name, network, intent, None)
}

/// Read an account's raw 64-byte keypair, applying its approval policy.
///
/// Refuses when the account's backend is not exportable (see
/// [`SigningBackend::is_exportable`]); that is the single place the rule
/// lives, so `pay account export` and every other raw-key path agree.
/// Accepts an optional auth-gate override; see [`AuthOverride`].
pub fn load_keypair_bytes_from_account_with_intent_and_override(
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    let backend = account.descriptor()?;
    if !backend.is_exportable() {
        return Err(not_exportable(backend, name));
    }

    match account.backend {
        BackendKind::Ephemeral => account
            .ephemeral_keypair_bytes()
            .map(crate::keystore::Zeroizing::new)
            .ok_or_else(|| {
                Error::Config(
                    "Ephemeral account is missing its inline `secret_key_b58` field".to_string(),
                )
            }),
        BackendKind::Remote => Err(Error::Config(format!(
            "Account `{name}` signs through the `{}` backend, which can export its key, \
             but pay does not implement key export for remote providers yet.",
            backend.id()
        ))),
        _ => {
            let local = crate::backend::local_by_kind(&account.backend)
                .expect("descriptor resolved, so the kind is a registered local backend");
            let gate = Gate::for_policy(account.auth_required_for_network(network), auth_override);
            let file_path = account.file_path(name);
            let params = account.store_params(Some(&file_path));
            let ks = local.keystore(&params, gate)?;
            ks.load_keypair_with_intent(name, &intent.with_account_context(name))
                .map_err(|e| map_keystore_backend_error(local.flag(), e))
        }
    }
}

fn not_exportable(backend: &dyn SigningBackend, name: &str) -> Error {
    Error::Config(format!(
        "Account `{name}` signs through the `{}` backend: its private key stays {} \
         and cannot be loaded or exported.",
        backend.id(),
        backend.custody().location_phrase()
    ))
}

// ── Account → signer ────────────────────────────────────────────────────────

pub fn load_signer_from_account_with_reason(
    account: &Account,
    name: &str,
    network: &str,
    reason: &str,
) -> Result<ResolvedSigner> {
    load_signer_from_account_with_intent(account, name, network, &AuthIntent::from_reason(reason))
}

pub fn load_signer_from_account_with_intent(
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
) -> Result<ResolvedSigner> {
    load_signer_from_account_with_intent_and_override(account, name, network, intent, None)
}

/// Resolve an account into a signer, applying its approval policy.
/// Accepts an optional auth-gate override; see [`AuthOverride`].
pub fn load_signer_from_account_with_intent_and_override(
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<ResolvedSigner> {
    load_signer_from_account_with_source(
        &crate::remote::PlatformCredentials,
        account,
        name,
        network,
        intent,
        auth_override,
    )
}

/// [`load_signer_from_account_with_intent_and_override`] with remote
/// credentials read from `source` instead of the platform secret store.
pub fn load_signer_from_account_with_source(
    source: &dyn crate::remote::CredentialSource,
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<ResolvedSigner> {
    if account.backend == BackendKind::Remote {
        return crate::remote::load_remote_signer_from(
            source,
            account,
            name,
            network,
            intent,
            auth_override,
        );
    }

    let backend = account.descriptor()?;
    let bytes = load_keypair_bytes_from_account_with_intent_and_override(
        account,
        name,
        network,
        intent,
        auth_override,
    )?;
    let memory = MemorySigner::from_bytes(&bytes).map_err(|e| {
        Error::Config(format!(
            "Storage corrupted: keypair for account `{name}` on `{network}` failed to decode ({e}). \
             Re-import the account: `pay account destroy --name {name}` then `pay account new --name {name}`."
        ))
    })?;
    Ok(ResolvedSigner::local(backend, memory))
}

/// Build a signer for an ephemeral account from its inline keypair. No
/// prompt: ephemeral wallets are ungated by design.
pub fn signer_for_ephemeral_account(account: &Account) -> Result<ResolvedSigner> {
    let bytes = account.ephemeral_keypair_bytes().ok_or_else(|| {
        Error::Config("Ephemeral account is missing its inline `secret_key_b58` field".to_string())
    })?;
    MemorySigner::from_bytes(&bytes)
        .map(|s| ResolvedSigner::local(&crate::backend::Ephemeral, s))
        .map_err(|e| Error::Config(format!("Invalid ephemeral keypair bytes: {e}")))
}

// ── Legacy `<flag>:<name>` sources ─────────────────────────────────────────

/// Load a `MemorySigner` with a custom reason string.
pub fn load_signer_with_reason(source: &str, reason: &str) -> Result<MemorySigner> {
    load_signer_with_intent(source, &AuthIntent::from_reason(reason))
}

/// Load a `MemorySigner` with a typed auth intent.
pub fn load_signer_with_intent(source: &str, intent: &AuthIntent) -> Result<MemorySigner> {
    let bytes = load_signer_keypair_bytes_with_intent(source, intent)?;
    MemorySigner::from_bytes(&bytes).map_err(|e| {
        Error::Config(format!(
            "Storage corrupted: keypair from `{source}` failed to decode ({e}). \
             Re-import this keypair via `pay account new` or `pay account import`."
        ))
    })
}

/// Like [`load_signer_with_intent`], but returns a [`ResolvedSigner`] tagged
/// with the backend the source names: a registered `<flag>:` prefix, or the
/// file backend for paths and inline keys.
pub fn load_resolved_signer_with_intent(
    source: &str,
    intent: &AuthIntent,
) -> Result<ResolvedSigner> {
    let signer = load_signer_with_intent(source, intent)?;
    Ok(ResolvedSigner::local(source_backend(source), signer))
}

/// The backend a legacy source string refers to.
fn source_backend(source: &str) -> &'static dyn SigningBackend {
    source
        .split_once(':')
        .and_then(|(flag, _)| crate::backend::local_by_flag(flag))
        .filter(|local| local.kind() != BackendKind::File)
        .map(|local| local as &'static dyn SigningBackend)
        .unwrap_or(&crate::backend::File)
}

pub fn load_signer_keypair_bytes_with_reason(
    source: &str,
    reason: &str,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    load_signer_keypair_bytes_with_intent(source, &AuthIntent::from_reason(reason))
}

/// Read keypair bytes from a legacy source string: `<flag>:<account>` for
/// a registered local keystore backend, else a file path or inline key.
pub fn load_signer_keypair_bytes_with_intent(
    source: &str,
    intent: &AuthIntent,
) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    if let Some((flag, account)) = source.split_once(':')
        && let Some(local) = crate::backend::local_by_flag(flag)
        && local.kind() != BackendKind::File
    {
        let ks = local.keystore(&crate::backend::StoreParams::default(), Gate::Platform)?;
        return ks
            .load_keypair_with_intent(account, &intent.with_account_context(account))
            .map_err(|e| map_keystore_backend_error(local.flag(), e));
    }
    load_from_file(source)
}

/// Human-readable name of the auth UI for a given keystore backend, used in
/// "Payment rejected" messages when the user cancels at the OS prompt.
fn rejection_source(backend_flag: &str) -> String {
    match crate::backend::local_by_flag(backend_flag) {
        Some(local) if local.kind() != BackendKind::File => {
            format!("rejected by user at {}", local.display_name())
        }
        _ => "rejected by user at authentication prompt".to_string(),
    }
}

pub(crate) fn map_keystore_backend_error(backend_flag: &str, e: crate::keystore::Error) -> Error {
    if matches!(e, crate::keystore::Error::AuthDenied(_)) {
        Error::PaymentRejected(rejection_source(backend_flag))
    } else {
        Error::Config(format!("{backend_flag}: {e}"))
    }
}

fn load_from_file(path: &str) -> Result<crate::keystore::Zeroizing<Vec<u8>>> {
    let expanded = shellexpand::tilde(path);
    // Newer solana-keychain split file vs inline-string parsing into two
    // separate constructors. Prefer the file path when the argument exists
    // on disk; otherwise fall back to treating the source as an inline
    // private key (base58 or u8-array literal).
    if std::path::Path::new(expanded.as_ref()).exists() {
        let data = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))?;
        parse_private_key_string(&data)
            .map(crate::keystore::Zeroizing::new)
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
    } else {
        parse_private_key_string(expanded.as_ref())
            .map(crate::keystore::Zeroizing::new)
            .map_err(|e| Error::Config(format!("Failed to load keypair from {path}: {e}")))
    }
}

fn parse_private_key_string(input: &str) -> std::result::Result<Vec<u8>, String> {
    let trimmed = input.trim();

    if trimmed.starts_with('[') {
        let bytes: Vec<u8> =
            serde_json::from_str(trimmed).map_err(|e| format!("Invalid keypair JSON: {e}"))?;
        if bytes.len() != 64 {
            return Err(format!("Expected 64 bytes, got {}", bytes.len()));
        }
        return Ok(bytes);
    }

    let bytes = crate::b58::decode_64(trimmed)
        .map_err(|e| format!("Invalid base58 private key (expected 64 bytes): {e}"))?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: We do NOT test keychain:/gnome-keyring:/1password: prefixes here
    // because they trigger interactive auth prompts (Touch ID, op CLI, etc.)
    // that hang in CI/test environments.

    #[test]
    fn load_signer_file_not_found() {
        let result = load_signer("/nonexistent/path/to/keypair.json");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to load keypair"));
    }

    #[test]
    fn keystore_auth_denial_maps_to_payment_rejected() {
        let err = map_keystore_backend_error(
            "keychain",
            crate::keystore::Error::AuthDenied("cancel".into()),
        );

        match err {
            Error::PaymentRejected(reason) => {
                assert_eq!(reason, "rejected by user at Apple Keychain");
            }
            other => panic!("expected PaymentRejected, got {other:?}"),
        }
    }

    #[test]
    fn keystore_backend_error_stays_config_error() {
        let err = map_keystore_backend_error(
            "keychain",
            crate::keystore::Error::Backend("missing helper".into()),
        );

        match err {
            Error::Config(message) => {
                assert_eq!(message, "keychain: Keystore error: missing helper");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn load_signer_with_valid_keypair_file() {
        use pay_kit::mpp::solana_keychain::SolanaSigner;

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut keypair_bytes = Vec::with_capacity(64);
        keypair_bytes.extend_from_slice(&signing_key.to_bytes());
        keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join("test-keypair.json");
        let json: Vec<u8> = keypair_bytes;
        std::fs::write(&key_path, serde_json::to_string(&json).unwrap()).unwrap();

        let signer = load_signer(key_path.to_str().unwrap()).unwrap();
        let expected_pubkey = bs58::encode(verifying_key.to_bytes()).into_string();
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
    }

    #[test]
    fn load_signer_invalid_file_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join("bad-keypair.json");
        std::fs::write(&key_path, "not valid keypair data").unwrap();

        let result = load_signer(key_path.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn load_signer_accepts_inline_private_key_string() {
        use pay_kit::mpp::solana_keychain::SolanaSigner;

        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut keypair_bytes = Vec::with_capacity(64);
        keypair_bytes.extend_from_slice(&signing_key.to_bytes());
        keypair_bytes.extend_from_slice(&verifying_key.to_bytes());
        let inline = bs58::encode(keypair_bytes).into_string();

        let signer = load_signer(&inline).unwrap();
        let expected_pubkey = bs58::encode(verifying_key.to_bytes()).into_string();
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
    }

    #[test]
    fn load_signer_windows_hello_unavailable() {
        #[cfg(not(target_os = "windows"))]
        {
            let result = load_signer("windows-hello:default");
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("not available on this platform")
            );
        }
    }

    #[test]
    fn source_backend_follows_the_prefix() {
        assert_eq!(source_backend("keychain:default").id(), "apple-keychain");
        assert_eq!(source_backend("1password:work").id(), "1password");
        assert_eq!(source_backend("/tmp/key.json").id(), "file");
        assert_eq!(source_backend("C:\\keys\\x.json").id(), "file");
        assert_eq!(source_backend("file:whatever").id(), "file");
    }

    #[test]
    fn legacy_source_with_colon_in_a_path_is_a_file() {
        // `C:\keys\x.json`-style or `dir:name`-style paths are not backend
        // prefixes and must fall through to the file loader.
        let err = load_signer("no-such-backend:default").unwrap_err();
        assert!(err.to_string().contains("Failed to load keypair"), "{err}");
    }

    // ── load_signer_for_network ────────────────────────────────────────────

    use crate::accounts::{Account, AccountsFile, MAINNET_NETWORK, MemoryAccountsStore};

    struct DenyAuth;

    impl AuthGate for DenyAuth {
        fn authenticate(
            &self,
            _intent: &AuthIntent,
        ) -> std::result::Result<(), crate::keystore::Error> {
            Err(crate::keystore::Error::AuthDenied(
                "denied by test".to_string(),
            ))
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    fn fresh_file_account(auth_required: Option<bool>) -> (tempfile::TempDir, Account, Vec<u8>) {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut keypair_bytes = Vec::with_capacity(64);
        keypair_bytes.extend_from_slice(&signing_key.to_bytes());
        keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

        let temp_dir = tempfile::tempdir().unwrap();
        let key_path = temp_dir.path().join("test-keypair.json");
        std::fs::write(&key_path, serde_json::to_string(&keypair_bytes).unwrap()).unwrap();

        let account = Account {
            backend: BackendKind::File,
            provider: None,
            active: false,
            auth_required,
            pubkey: Some(bs58::encode(verifying_key.to_bytes()).into_string()),
            vault: None,
            account: None,
            path: Some(key_path.to_string_lossy().into_owned()),
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        };

        (temp_dir, account, keypair_bytes)
    }

    fn remote_account(account_id: Option<&str>) -> Account {
        Account {
            backend: BackendKind::Remote,
            provider: Some("openfort".to_string()),
            active: false,
            auth_required: Some(true),
            pubkey: Some("C78fUoBw1YDJDmzNx7viRZFnuhku3t3eiy9eiV2hafff".to_string()),
            vault: None,
            account: account_id.map(str::to_string),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn explicit_remote_account_falls_back_from_mainnet_on_other_networks() {
        // Registered on mainnet only (what `pay account new` writes), but
        // addressed explicitly on localnet: resolution must reach the
        // remote loader instead of shadowing the name with a lazy
        // ephemeral. The missing-account-id error proves which path ran
        // without needing keystore credentials.
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "openfort", remote_account(None));
        let store = MemoryAccountsStore::with_file(file);

        let err = match load_signer_for_network_with_intent(
            "localnet",
            &store,
            Some("openfort"),
            &AuthIntent::default_payment(),
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected the remote loader to reject a missing wallet id"),
        };

        let Error::Config(msg) = err else {
            panic!("expected Config error");
        };
        assert!(msg.contains("missing its `account` field"), "{msg}");
        assert_eq!(store.save_count(), 0, "no ephemeral must be created");
    }

    #[test]
    fn backend_for_network_reads_the_descriptor_without_loading_the_signer() {
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "openfort", remote_account(None));
        let store = MemoryAccountsStore::with_file(file);

        // The account has no wallet id, so loading it would fail; reading
        // the backend must not care.
        let backend = backend_for_network(MAINNET_NETWORK, &store, Some("openfort"))
            .unwrap()
            .expect("a configured account has a backend");
        assert_eq!(backend.id(), "openfort");

        // Explicitly named remote accounts follow the same mainnet fallback
        // as the loader on other networks.
        let backend = backend_for_network("localnet", &store, Some("openfort"))
            .unwrap()
            .expect("remote accounts serve every network");
        assert_eq!(backend.id(), "openfort");
        assert_eq!(store.save_count(), 0);
    }

    #[test]
    fn backend_for_network_is_none_when_a_throwaway_wallet_would_be_created() {
        let store = MemoryAccountsStore::new();
        assert!(
            backend_for_network("localnet", &store, None)
                .unwrap()
                .is_none()
        );
        assert!(
            backend_for_network("localnet", &store, Some("scratch"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.save_count(),
            0,
            "resolution must not create the wallet"
        );
    }

    #[test]
    fn backend_for_network_reports_missing_accounts_like_the_loader() {
        let store = MemoryAccountsStore::new();
        let Err(Error::Config(msg)) = backend_for_network(MAINNET_NETWORK, &store, None) else {
            panic!("mainnet never creates a wallet on its own");
        };
        assert!(
            msg.contains("No account configured for network `mainnet`"),
            "{msg}"
        );
        let Err(Error::Config(msg)) = backend_for_network(MAINNET_NETWORK, &store, Some("nope"))
        else {
            panic!("an unknown name is an error");
        };
        assert!(msg.contains("No account named `nope`"), "{msg}");
    }

    #[test]
    fn keypair_backed_mainnet_entry_does_not_fall_back() {
        let (_temp_dir, account, _) = fresh_file_account(Some(false));
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "worker", account);
        let store = MemoryAccountsStore::with_file(file);

        let (signer, resolved) = load_signer_for_network_with_intent(
            "localnet",
            &store,
            Some("worker"),
            &AuthIntent::default_payment(),
        )
        .unwrap();

        assert_eq!(signer.custody(), Custody::Local);
        assert_eq!(signer.backend().id(), "ephemeral");
        assert!(
            resolved.is_some_and(|r| r.created),
            "keypair-backed accounts must keep the lazy-ephemeral behavior"
        );
    }

    #[test]
    fn file_account_honors_explicit_auth_gate_override() {
        let (_temp_dir, account, _) = fresh_file_account(Some(true));

        let err = load_keypair_bytes_from_account_with_intent_and_override(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
            Some(Box::new(DenyAuth)),
        )
        .unwrap_err();

        assert!(matches!(err, Error::PaymentRejected(_)));
    }

    #[test]
    fn file_account_authenticates_before_reading_keypair() {
        let (temp_dir, mut account, _) = fresh_file_account(Some(true));
        account.path = Some(
            temp_dir
                .path()
                .join("missing-keypair.json")
                .to_string_lossy()
                .into_owned(),
        );

        let err = load_keypair_bytes_from_account_with_intent_and_override(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
            Some(Box::new(DenyAuth)),
        )
        .unwrap_err();

        assert!(matches!(err, Error::PaymentRejected(_)));
    }

    #[test]
    fn file_account_honors_mainnet_default_auth_gate_override() {
        let (_temp_dir, account, _) = fresh_file_account(None);

        let err = load_keypair_bytes_from_account_with_intent_and_override(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
            Some(Box::new(DenyAuth)),
        )
        .unwrap_err();

        assert!(matches!(err, Error::PaymentRejected(_)));
    }

    #[test]
    fn file_account_skips_auth_gate_when_disabled() {
        let (_temp_dir, account, expected) = fresh_file_account(Some(false));

        let bytes = load_keypair_bytes_from_account_with_intent_and_override(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
            Some(Box::new(DenyAuth)),
        )
        .unwrap();

        assert_eq!(&*bytes, &expected);
    }

    #[test]
    fn file_account_resolves_to_a_local_exportable_signer() {
        let (_temp_dir, account, _) = fresh_file_account(Some(false));
        let signer = load_signer_from_account_with_intent(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
        )
        .unwrap();
        assert_eq!(signer.backend().id(), "file");
        assert_eq!(signer.custody(), Custody::Local);
        assert!(signer.is_exportable());
        assert!(signer.signs_raw_messages());
        assert_eq!(signer.pubkey().to_string(), account.pubkey.unwrap());
    }

    fn fresh_ephemeral_account() -> Account {
        // Build an ephemeral account directly so the test doesn't depend
        // on the lazy-create internals.
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying_key = signing_key.verifying_key();
        let mut full = Vec::with_capacity(64);
        full.extend_from_slice(&signing_key.to_bytes());
        full.extend_from_slice(&verifying_key.to_bytes());
        Account {
            backend: BackendKind::Ephemeral,
            provider: None,
            active: false,
            auth_required: Some(false),
            pubkey: Some(bs58::encode(verifying_key.to_bytes()).into_string()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: Some(bs58::encode(&full).into_string()),
            created_at: Some("2026-04-10T00:00:00Z".to_string()),
            subscriptions: std::collections::BTreeMap::new(),
        }
    }

    fn fresh_remote_account(account_id: Option<&str>) -> Account {
        Account {
            backend: BackendKind::Remote,
            provider: Some("openfort".to_string()),
            active: false,
            auth_required: Some(false),
            pubkey: None,
            vault: None,
            account: account_id.map(str::to_string),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn remote_account_refuses_raw_keypair_access() {
        let account = fresh_remote_account(Some("acc_test"));

        let err = load_keypair_bytes_from_account_with_intent(
            &account,
            "default",
            MAINNET_NETWORK,
            &AuthIntent::default_payment(),
        )
        .unwrap_err();

        let msg = err.to_string();
        // The message names the account's own backend, whichever it is.
        assert!(msg.contains("`openfort` backend"), "wrong error: {msg}");
        assert!(
            msg.contains("in the provider's custody"),
            "wrong error: {msg}"
        );
        assert!(
            msg.contains("cannot be loaded or exported"),
            "wrong error: {msg}"
        );
    }

    #[test]
    fn remote_account_without_provider_is_a_config_error() {
        let mut account = fresh_remote_account(Some("acc_test"));
        account.provider = None;
        let Err(err) = account.descriptor() else {
            panic!("no provider must fail");
        };
        assert!(
            err.to_string().contains("missing its `provider` field"),
            "{err}"
        );

        account.provider = Some("nope".to_string());
        let Err(err) = account.descriptor() else {
            panic!("unknown provider must fail");
        };
        assert!(
            err.to_string().contains("unknown remote backend `nope`"),
            "{err}"
        );
    }

    #[test]
    fn load_signer_for_network_routes_remote_accounts() {
        // A remote account missing its wallet id fails inside the remote
        // resolution path — proving the network loader routes
        // `keystore: remote` entries to the remote signer instead of
        // the local keypair path.
        let mut file = AccountsFile::default();
        file.upsert(MAINNET_NETWORK, "default", fresh_remote_account(None));
        let store = MemoryAccountsStore::with_file(file);

        let err = load_signer_for_network(MAINNET_NETWORK, &store)
            .map(|_| ())
            .unwrap_err();

        assert!(
            err.to_string().contains("missing its `account` field"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn ephemeral_account_never_uses_auth_gate() {
        for auth_required in [Some(true), None] {
            let mut account = fresh_ephemeral_account();
            account.auth_required = auth_required;
            let expected = account.ephemeral_keypair_bytes().unwrap();

            let bytes = load_keypair_bytes_from_account_with_intent_and_override(
                &account,
                "default",
                MAINNET_NETWORK,
                &AuthIntent::default_payment(),
                Some(Box::new(DenyAuth)),
            )
            .unwrap();

            assert_eq!(&*bytes, &expected);
        }
    }

    #[test]
    fn load_signer_for_network_resolves_existing_ephemeral() {
        let mut file = AccountsFile::default();
        let acct = fresh_ephemeral_account();
        let expected_pubkey = acct.pubkey.clone().unwrap();
        file.upsert("localnet", "default", acct);
        let store = MemoryAccountsStore::with_file(file);

        let (signer, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        use pay_kit::mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
        assert_eq!(signer.backend().id(), "ephemeral");
        assert!(
            ephemeral.is_none(),
            "must NOT report a creation when the entry already existed"
        );
        assert_eq!(store.save_count(), 0, "no writes on cache hit");
    }

    #[test]
    fn load_signer_for_network_lazy_creates_localnet() {
        // No mapping → auto-create + persist + return Some(ResolvedEphemeral).
        let store = MemoryAccountsStore::new();
        let (signer, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        use pay_kit::mpp::solana_keychain::SolanaSigner;

        let resolved = ephemeral.expect("ephemeral creation must be reported");
        assert!(resolved.created);
        assert_eq!(resolved.network, "localnet");
        assert_eq!(
            resolved.account.pubkey.as_deref(),
            Some(signer.pubkey().to_string().as_str())
        );
        assert_eq!(
            store.save_count(),
            1,
            "lazy create must persist exactly once"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_devnet() {
        let store = MemoryAccountsStore::new();
        let (_, ephemeral) = load_signer_for_network("devnet", &store).unwrap();
        let resolved = ephemeral.expect("devnet must lazy-create");
        assert_eq!(resolved.network, "devnet");
        assert!(resolved.created);
    }

    #[test]
    fn load_signer_for_network_refuses_to_create_mainnet() {
        // Real money: never silently create. User must run `pay setup`.
        let store = MemoryAccountsStore::new();
        let err = load_signer_for_network(MAINNET_NETWORK, &store)
            .map(|_| ())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No account configured"),
            "missing setup hint: {msg}"
        );
        assert!(msg.contains("pay setup"), "missing setup command: {msg}");
        assert_eq!(
            store.save_count(),
            0,
            "must not write to store on mainnet miss"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_when_missing() {
        // No account for localnet → auto-create an ephemeral.
        let store = MemoryAccountsStore::new();
        let (_, ephemeral) = load_signer_for_network("localnet", &store).unwrap();
        let resolved = ephemeral.expect("ephemeral creation must be reported");
        assert!(resolved.created);
        assert_eq!(resolved.network, "localnet");
        assert_eq!(store.save_count(), 1);
    }

    #[test]
    fn load_signer_for_network_caches_lazy_created_keypair() {
        // First call creates, second call must hit the cache (same pubkey,
        // no new write).
        let store = MemoryAccountsStore::new();
        let (signer1, e1) = load_signer_for_network("localnet", &store).unwrap();
        let (signer2, e2) = load_signer_for_network("localnet", &store).unwrap();

        use pay_kit::mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer1.pubkey().to_string(), signer2.pubkey().to_string());
        assert!(e1.is_some(), "first call should report creation");
        assert!(e2.is_none(), "second call must be a cache hit");
        assert_eq!(store.save_count(), 1, "exactly one write across both calls");
    }

    #[test]
    fn load_signer_for_network_resolves_named_existing_ephemeral() {
        let mut file = AccountsFile::default();
        let acct = fresh_ephemeral_account();
        let expected_pubkey = acct.pubkey.clone().unwrap();
        file.upsert("localnet", "alice", acct);
        let store = MemoryAccountsStore::with_file(file);

        let (signer, ephemeral) =
            load_signer_for_network_with_reason("localnet", &store, Some("alice"), "test").unwrap();

        use pay_kit::mpp::solana_keychain::SolanaSigner;
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
        assert!(
            ephemeral.is_none(),
            "existing named account must not report creation"
        );
        assert_eq!(
            store.save_count(),
            0,
            "existing named account must not write"
        );
    }

    #[test]
    fn load_signer_for_network_lazy_creates_named_localnet_account() {
        let store = MemoryAccountsStore::new();
        let (signer, ephemeral) =
            load_signer_for_network_with_reason("localnet", &store, Some("alice"), "test").unwrap();

        use pay_kit::mpp::solana_keychain::SolanaSigner;
        let resolved = ephemeral.expect("named localnet miss must create");
        assert!(resolved.created);
        assert_eq!(resolved.account_name, "alice");
        assert_eq!(resolved.network, "localnet");
        assert_eq!(
            resolved.account.pubkey.as_deref(),
            Some(signer.pubkey().to_string().as_str())
        );

        let snapshot = store.snapshot();
        assert!(
            snapshot
                .named_account_for_network("localnet", "alice")
                .is_some()
        );
        assert_eq!(store.save_count(), 1);
    }

    #[test]
    fn load_signer_for_network_rejects_missing_named_mainnet_account() {
        let store = MemoryAccountsStore::new();
        let err =
            load_signer_for_network_with_reason(MAINNET_NETWORK, &store, Some("alice"), "test")
                .map(|_| ())
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("No account named `alice` configured for network `mainnet`.")
        );
        assert_eq!(store.save_count(), 0);
    }

    #[test]
    fn signer_for_ephemeral_account_rejects_missing_inline_secret() {
        let account = Account {
            backend: BackendKind::Ephemeral,
            provider: None,
            active: false,
            auth_required: Some(false),
            pubkey: None,
            vault: None,
            path: None,
            account: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: std::collections::BTreeMap::new(),
        };

        let err = signer_for_ephemeral_account(&account)
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Ephemeral account is missing its inline `secret_key_b58` field")
        );
    }

    #[test]
    fn rejection_source_maps_known_backends() {
        assert_eq!(
            rejection_source("keychain"),
            "rejected by user at Apple Keychain"
        );
        assert_eq!(
            rejection_source("windows-hello"),
            "rejected by user at Windows Hello"
        );
        assert_eq!(
            rejection_source("gnome-keyring"),
            "rejected by user at GNOME Keyring"
        );
        assert_eq!(
            rejection_source("1password"),
            "rejected by user at 1Password"
        );
        assert_eq!(
            rejection_source("file"),
            "rejected by user at authentication prompt"
        );
        assert_eq!(
            rejection_source("unknown"),
            "rejected by user at authentication prompt"
        );
    }
}
