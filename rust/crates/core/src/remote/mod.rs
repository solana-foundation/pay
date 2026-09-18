//! Remote signing backends — wallets whose private key never reaches this
//! machine.
//!
//! A remote backend holds the key in a provider's custody (TEE, HSM, MPC)
//! and signs over HTTPS. pay stores only that provider's API credentials,
//! as a credential blob in the platform secret store, gated by the same
//! auth path as local keypairs (Touch ID / Windows Hello / polkit). The
//! credentials are revocable and never sufficient to extract the key.
//!
//! Everything in this module is provider-agnostic: credential storage,
//! the setup flow's prompting and wallet discovery, and accounts.yml
//! resolution. A provider supplies only what is genuinely specific to it
//! by implementing [`RemoteProvider`] — see `openfort.rs` for the
//! reference implementation, and [`provider`] for the registry a new
//! backend registers itself in.
//!
//! ## Adding a provider
//!
//! 1. Implement [`RemoteProvider`] in `remote/<name>.rs`, declaring the
//!    credentials it needs ([`CredentialField`]), how to list the wallets
//!    those credentials can sign with, and how to connect one.
//! 2. Add it to [`PROVIDERS`].
//!
//! Nothing else in pay changes: `accounts.yml` stores the provider as a
//! free-text `provider` field beside `keystore: remote`, the CLI prompts
//! from the declared fields, and `{PROVIDER}_{FIELD}` environment
//! variables work automatically. `solana-keychain` already ships signers
//! for a dozen custody providers, so an implementation is usually a thin
//! wrapper over one of those.

#[cfg(feature = "ledger")]
pub mod ledger;
pub mod openfort;
pub mod privy;

use std::collections::BTreeMap;

use pay_kit::solana_keychain::TransactionSigner;

use crate::accounts::Account;
use crate::backend::{Gate, SigningBackend, StoreParams};
use crate::keystore::AuthIntent;
use crate::signer::{AuthOverride, ResolvedSigner};
use crate::{Error, Result};

/// The registered remote backends, by [`RemoteProvider::id`].
static PROVIDERS: &[&dyn RemoteProvider] = &[
    &openfort::Openfort,
    &privy::Privy,
    #[cfg(feature = "ledger")]
    &ledger::Ledger,
];

/// Look up a backend by id (`openfort`), or `None` if unregistered.
pub fn provider(id: &str) -> Option<&'static dyn RemoteProvider> {
    PROVIDERS.iter().copied().find(|p| p.id() == id)
}

/// Every registered backend id, for CLI help and error messages.
pub fn provider_ids() -> Vec<&'static str> {
    providers().map(|p| p.id()).collect()
}

/// Every registered backend, for menus built from the registry.
pub fn providers() -> impl Iterator<Item = &'static dyn RemoteProvider> {
    PROVIDERS.iter().copied()
}

/// Where an account's remote credentials come from.
///
/// The CLI reads them from the platform secret store behind Touch ID;
/// pay-cloud holds them per tenant. An [`AccountsStore`](crate::accounts::AccountsStore)
/// names its source, so the same signing paths serve both without knowing
/// which one they are on. The source applies `gate` before handing the
/// credentials out: that is where a spending policy or a prompt runs.
pub trait CredentialSource: Send + Sync {
    fn load(
        &self,
        account: &str,
        provider_id: &str,
        gate: Gate,
        intent: &AuthIntent,
    ) -> Result<Credentials>;
}

/// The platform secret store, the CLI's source.
pub struct PlatformCredentials;

impl CredentialSource for PlatformCredentials {
    fn load(
        &self,
        account: &str,
        provider_id: &str,
        gate: Gate,
        intent: &AuthIntent,
    ) -> Result<Credentials> {
        let (ks, backend) = platform_keystore(gate)?;
        if !ks.credential_exists(account) {
            return Err(Error::Config(format!(
                "No credentials stored for account `{account}`.\n\
                 Run `pay account new {account} --backend {provider_id}` to connect it."
            )));
        }
        let blob = ks
            .load_credential_with_intent(account, intent)
            .map_err(|e| crate::signer::map_keystore_backend_error(backend, e))?;
        serde_json::from_slice(&blob).map_err(|e| {
            Error::Config(format!(
                "Stored credentials for `{account}` are corrupted ({e}). \
                 Re-connect the wallet: `pay account destroy {account}` then \
                 `pay account new {account} --backend {provider_id}`."
            ))
        })
    }
}

/// Credentials held in memory, gated on every load. pay-cloud's per-tenant
/// source, and a test double.
pub struct MemoryCredentials {
    credentials: Credentials,
}

impl MemoryCredentials {
    pub fn new(credentials: Credentials) -> Self {
        Self { credentials }
    }
}

impl CredentialSource for MemoryCredentials {
    fn load(
        &self,
        _account: &str,
        _provider_id: &str,
        gate: Gate,
        intent: &AuthIntent,
    ) -> Result<Credentials> {
        match gate {
            Gate::Disabled => {}
            Gate::Override(auth) => auth
                .authenticate(intent)
                .map_err(|e| Error::Config(format!("Authorization refused: {e}")))?,
            Gate::Platform => {
                return Err(Error::Config(
                    "In-memory credentials have no platform prompt to gate them; \
                     supply an auth override or disable the gate."
                        .to_string(),
                ));
            }
        }
        Ok(self.credentials.clone())
    }
}

/// One credential a provider needs in order to sign — an API key, a
/// secret, a project id.
pub struct CredentialField {
    /// Storage key, also the environment-variable suffix: `secret_key`
    /// is read from `{PROVIDER}_SECRET_KEY`.
    pub key: &'static str,
    /// Prompt text shown during setup.
    pub label: &'static str,
    /// Whether input is masked and never echoed.
    pub secret: bool,
}

/// A wallet a provider's credentials can sign with.
pub struct RemoteWallet {
    /// Provider-side wallet id, stored in `accounts.yml` as `account`.
    pub id: String,
    /// The wallet's Solana address, base58.
    pub address: String,
}

/// A provider's credentials, keyed by [`CredentialField::key`].
pub type Credentials = BTreeMap<String, String>;

/// A remote signing backend.
///
/// Implementations are stateless descriptors: they declare what pay must
/// collect, then turn those credentials into wallet listings and signers.
/// Everything else — prompting, storage, the auth gate, accounts.yml — is
/// handled generically by this module.
///
/// The provider's identity and capabilities ([`SigningBackend::id`],
/// [`SigningBackend::is_exportable`], …) come from the supertrait; this
/// trait adds only what connecting to a remote wallet needs.
pub trait RemoteProvider: SigningBackend {
    /// The credentials to collect, in prompt order. Empty for backends where
    /// the device is the credential (hardware wallets).
    fn credential_fields(&self) -> &'static [CredentialField];

    /// Whether anything is stored in the platform secret store for an
    /// account on this backend. False when [`credential_fields`](Self::credential_fields)
    /// is empty: setup prompts for nothing, resolution loads nothing, and
    /// destroy deletes nothing.
    fn requires_credentials(&self) -> bool {
        !self.credential_fields().is_empty()
    }

    /// One line telling the user where to obtain the credentials.
    fn credentials_hint(&self) -> &'static str;

    /// Reject a malformed credential as soon as it is entered, so a typo
    /// costs one prompt rather than a failed round trip.
    fn validate_credential(&self, _key: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    /// Reject a malformed wallet id supplied by flag or environment.
    fn validate_wallet_id(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    /// List the Solana wallets these credentials can sign with.
    ///
    /// Called before the full credential set is collected when possible,
    /// so it doubles as an early check that the credentials are valid.
    /// Return only wallets the provider can actually sign for.
    fn discover(&self, credentials: &Credentials) -> Result<Vec<RemoteWallet>>;

    /// What to tell the user when [`discover`](Self::discover) finds no
    /// usable wallet — typically how to create one.
    fn no_wallets_hint(&self) -> &'static str;

    /// Connect to a wallet, resolving and pinning its address.
    fn connect(
        &self,
        credentials: &Credentials,
        wallet_id: &str,
    ) -> Result<Box<dyn TransactionSigner>>;
}

// ── Credential storage ──────────────────────────────────────────────────────

/// The platform secret store for remote credential blobs, behind `gate`.
///
/// Returns the keystore and the backend flag used in error messages.
fn platform_keystore(gate: Gate) -> Result<(crate::keystore::Keystore, &'static str)> {
    let platform = crate::backend::platform().ok_or_else(|| {
        Error::Config(
            "Remote backend accounts require a platform secret store (Keychain, GNOME \
             Keyring, or Windows Credential Manager), which is unavailable on this platform."
                .to_string(),
        )
    })?;
    Ok((
        platform.keystore(&StoreParams::default(), gate)?,
        platform.flag(),
    ))
}

/// Store credentials through an already-built platform keystore. The CLI
/// builds the keystore itself so setup-time gating fallbacks (e.g. no
/// enrolled Touch ID) stay in one place.
pub fn store_credentials(
    ks: &crate::keystore::Keystore,
    name: &str,
    credentials: &Credentials,
    intent: &AuthIntent,
) -> Result<()> {
    let blob = serde_json::to_vec(credentials)
        .map_err(|e| Error::Config(format!("Failed to serialize credentials: {e}")))?;
    ks.import_credential_with_intent(name, &blob, intent)
        .map_err(|e| Error::Config(format!("Failed to store credentials: {e}")))
}

/// Check whether credentials exist for this account name in the platform
/// secret store. Never prompts.
pub fn credentials_exist(name: &str) -> bool {
    platform_keystore(Gate::Disabled).is_ok_and(|(ks, _)| ks.credential_exists(name))
}

/// Delete the credential blob for this account name from the platform
/// secret store, passing through the platform auth prompt.
pub fn delete_credentials(name: &str, intent: &AuthIntent) -> Result<()> {
    let (ks, backend) = platform_keystore(Gate::Platform)?;
    ks.delete_credential_with_intent(name, intent)
        .map_err(|e| crate::signer::map_keystore_backend_error(backend, e))
}

// ── Account resolution ──────────────────────────────────────────────────────

/// Read an account's provider id, or explain that it is missing.
fn account_provider(account: &Account, name: &str) -> Result<&'static dyn RemoteProvider> {
    let id = account.provider.as_deref().ok_or_else(|| {
        Error::Config(format!(
            "Remote account `{name}` is missing its `provider` field in accounts.yml \
             (one of: {}).",
            provider_ids().join(", ")
        ))
    })?;
    provider(id).ok_or_else(|| {
        Error::Config(format!(
            "Account `{name}` names an unknown remote backend `{id}`. \
             This build of pay supports: {}.",
            provider_ids().join(", ")
        ))
    })
}

/// Connect a wallet and return its Solana address (base58). Used at setup
/// time to validate credentials and cache the address in `accounts.yml`.
pub fn fetch_wallet_address(
    provider: &dyn RemoteProvider,
    credentials: &Credentials,
    wallet_id: &str,
) -> Result<String> {
    Ok(provider
        .connect(credentials, wallet_id)?
        .pubkey()
        .to_string())
}

/// Resolve a remote account into a ready-to-sign [`ResolvedSigner`].
///
/// Loads the credential blob (through the platform auth gate when the
/// account requires auth on this network), connects the provider's signer
/// (which resolves and pins the wallet's address), and cross-checks that
/// address against the `pubkey` cached in `accounts.yml`.
///
/// Like the rest of the signer-resolution surface and the MPP/x402
/// payment builders, this is synchronous and blocks on network I/O.
/// Callers on async workers must isolate it with
/// `tokio::task::spawn_blocking` — the same contract the payer proxy and
/// MCP tools already follow for `build_credential` / `build_payment`.
pub fn load_remote_signer(
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<ResolvedSigner> {
    load_remote_signer_from(
        &PlatformCredentials,
        account,
        name,
        network,
        intent,
        auth_override,
    )
}

/// [`load_remote_signer`] with the credentials read from `source`.
pub fn load_remote_signer_from(
    source: &dyn CredentialSource,
    account: &Account,
    name: &str,
    network: &str,
    intent: &AuthIntent,
    auth_override: AuthOverride,
) -> Result<ResolvedSigner> {
    let provider = account_provider(account, name)?;

    let wallet_id = account.account.clone().ok_or_else(|| {
        Error::Config(format!(
            "Remote account `{name}` is missing its `account` field (the {} wallet id) \
             in accounts.yml.",
            provider.display_name()
        ))
    })?;

    let credentials = if provider.requires_credentials() {
        let gated = account.auth_required_for_network(network);
        let account_intent = intent.with_account_context(name);
        source.load(
            name,
            provider.id(),
            Gate::for_policy(gated, auth_override),
            &account_intent,
        )?
    } else {
        // Hardware wallets: nothing stored locally, the device approves.
        Credentials::new()
    };

    let signer = provider
        .connect(&credentials, &wallet_id)
        .map_err(|e| explain_connect_failure(provider, &credentials, &wallet_id, name, e))?;

    let address = signer.pubkey().to_string();
    if let Some(expected) = account.pubkey.as_deref()
        && expected != address
    {
        return Err(Error::Config(format!(
            "Account `{name}` resolves to address {address}, but accounts.yml caches \
             {expected}. The wallet behind `{wallet_id}` changed — re-connect it: \
             `pay account destroy {name}` then \
             `pay account new {name} --backend {}`.",
            provider.id()
        )));
    }

    Ok(ResolvedSigner::remote(provider, signer))
}

/// Explain a failed [`RemoteProvider::connect`] in terms of what is
/// actually wrong.
///
/// `connect` can only report what its HTTP call said — typically a bare
/// 401 — but the two everyday causes are distinguishable, and both are
/// invisible in that status: credentials that no longer authenticate at
/// all (rotated, revoked), and credentials that authenticate fine but
/// belong to a different project than the one holding this wallet.
/// Discovery separates them: it takes the same stored credentials and
/// lists the wallets they can sign for.
fn explain_connect_failure(
    provider: &dyn RemoteProvider,
    credentials: &Credentials,
    wallet_id: &str,
    name: &str,
    original: Error,
) -> Error {
    // The explanation below reasons about stored API credentials. A backend
    // with none (a hardware wallet) already says what is wrong: the device
    // is unplugged, locked, or busy.
    if !provider.requires_credentials() {
        return original;
    }

    let id = provider.id();
    let display_name = provider.display_name();
    let reconnect = format!("pay account new {name} --backend {id} --force");

    let Ok(wallets) = provider.discover(credentials) else {
        return Error::Config(format!(
            "The {display_name} credentials stored for account `{name}` neither signed nor \
             listed the project's wallets, so they were most likely rotated or revoked.\n\
             {original}\nRe-connect it:\n  {reconnect}"
        ));
    };

    if wallets.is_empty() {
        return Error::Config(format!(
            "The {display_name} credentials stored for account `{name}` can sign with no \
             Solana wallet.\n{}",
            provider.no_wallets_hint()
        ));
    }

    if wallets.iter().any(|w| w.id == wallet_id) {
        return original;
    }

    let listed = wallets
        .iter()
        .map(|w| format!("{} ({})", w.id, w.address))
        .collect::<Vec<_>>()
        .join("\n  ");

    Error::Config(format!(
        "Account `{name}` points at wallet `{wallet_id}`, which the {display_name} \
         credentials stored on this machine cannot see. They can sign with:\n  {listed}\n\
         The credentials were replaced, or they belong to a different project than the \
         wallet.\nRe-connect it:\n  {reconnect}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::BackendKind as KeystoreKind;
    use std::collections::BTreeMap;

    fn remote_account(provider: Option<&str>, wallet_id: Option<&str>) -> Account {
        Account {
            backend: KeystoreKind::Remote,
            provider: provider.map(str::to_string),
            active: false,
            auth_required: Some(false),
            pubkey: None,
            vault: None,
            account: wallet_id.map(str::to_string),
            path: None,
            secret_key_b58: None,
            created_at: None,
            subscriptions: BTreeMap::new(),
        }
    }

    #[test]
    fn registry_resolves_known_providers_only() {
        assert_eq!(provider("openfort").map(|p| p.id()), Some("openfort"));
        assert!(provider("not-a-backend").is_none());
        assert!(provider_ids().contains(&"openfort"));
        assert_eq!(provider("privy").map(|p| p.id()), Some("privy"));
    }

    /// Every registered provider must declare at least one credential and
    /// a unique id — the CLI drives its whole prompt flow from these.
    #[test]
    fn registered_providers_are_well_formed() {
        let ids = provider_ids();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "provider ids must be unique");

        for p in PROVIDERS {
            assert!(!p.display_name().is_empty(), "{}", p.id());
            assert_eq!(
                p.requires_credentials(),
                !p.credential_fields().is_empty(),
                "{}: requires_credentials must follow the declared fields",
                p.id()
            );
        }
    }

    #[test]
    fn missing_provider_field_is_reported() {
        let account = remote_account(None, Some("acc_x"));
        let Err(err) = account_provider(&account, "agent") else {
            panic!("expected a missing-provider error");
        };
        assert!(err.to_string().contains("missing its `provider` field"));
    }

    #[test]
    fn unknown_provider_lists_supported_backends() {
        let account = remote_account(Some("acme-custody"), Some("acc_x"));
        let Err(err) = account_provider(&account, "agent") else {
            panic!("expected an unknown-provider error");
        };
        let msg = err.to_string();
        assert!(msg.contains("unknown remote backend `acme-custody`"));
        assert!(msg.contains("openfort"));
    }

    /// An auth gate that records whether it ran and answers as told.
    struct Recording {
        allow: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl crate::keystore::AuthGate for Recording {
        fn authenticate(&self, _intent: &AuthIntent) -> pay_keystore::Result<()> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.allow {
                Ok(())
            } else {
                Err(pay_keystore::Error::AuthDenied(
                    "declined by policy".to_string(),
                ))
            }
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[test]
    fn memory_credentials_apply_the_gate_on_every_load() {
        let mut creds = Credentials::new();
        creds.insert("secret_key".to_string(), "sk_test".to_string());
        let source = MemoryCredentials::new(creds.clone());
        let intent = AuthIntent::default_payment();

        assert_eq!(
            source
                .load("t", "openfort", Gate::Disabled, &intent)
                .unwrap(),
            creds
        );

        let allow = Box::new(Recording {
            allow: true,
            calls: Default::default(),
        });
        assert!(
            source
                .load("t", "openfort", Gate::Override(allow), &intent)
                .is_ok()
        );

        let deny = Box::new(Recording {
            allow: false,
            calls: Default::default(),
        });
        let err = source
            .load("t", "openfort", Gate::Override(deny), &intent)
            .unwrap_err();
        assert!(err.to_string().contains("declined by policy"), "{err}");

        let err = source
            .load("t", "openfort", Gate::Platform, &intent)
            .unwrap_err();
        assert!(err.to_string().contains("no platform prompt"), "{err}");
    }

    #[test]
    fn load_remote_signer_requires_wallet_id() {
        let account = remote_account(Some("openfort"), None);
        let err = load_remote_signer(
            &account,
            "agent",
            "mainnet",
            &AuthIntent::default_payment(),
            None,
        )
        .map(|_| ())
        .unwrap_err();
        assert!(err.to_string().contains("missing its `account` field"));
    }

    /// A provider whose discovery result the test controls; `Err` stands
    /// for credentials the provider rejects outright.
    struct FakeProvider(std::result::Result<Vec<&'static str>, ()>);

    impl SigningBackend for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn display_name(&self) -> &'static str {
            "Fake custody"
        }
        fn description(&self) -> &'static str {
            "test double"
        }
        fn custody(&self) -> crate::backend::Custody {
            crate::backend::Custody::Remote
        }
        fn is_exportable(&self) -> bool {
            false
        }
        fn signs_raw_messages(&self) -> bool {
            true
        }
        fn approval(&self) -> crate::backend::Approval {
            crate::backend::Approval::ProviderPolicy
        }
        fn is_available(&self) -> bool {
            true
        }
        fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
            None
        }
    }

    impl RemoteProvider for FakeProvider {
        /// A credentialed provider, so connect failures get the stored
        /// credentials explanation.
        fn credential_fields(&self) -> &'static [CredentialField] {
            &[CredentialField {
                key: "api_key",
                label: "Fake API key",
                secret: true,
            }]
        }
        fn credentials_hint(&self) -> &'static str {
            "hint"
        }
        fn no_wallets_hint(&self) -> &'static str {
            "Create one first."
        }
        fn discover(&self, _credentials: &Credentials) -> Result<Vec<RemoteWallet>> {
            match &self.0 {
                Ok(ids) => Ok(ids
                    .iter()
                    .map(|id| RemoteWallet {
                        id: (*id).to_string(),
                        address: format!("addr-for-{id}"),
                    })
                    .collect()),
                Err(()) => Err(Error::Config("credentials rejected".to_string())),
            }
        }
        fn connect(&self, _c: &Credentials, _w: &str) -> Result<Box<dyn TransactionSigner>> {
            unimplemented!("tests only exercise the failure path")
        }
    }

    fn explain(discovery: std::result::Result<Vec<&'static str>, ()>, wallet: &str) -> String {
        explain_connect_failure(
            &FakeProvider(discovery),
            &Credentials::new(),
            wallet,
            "demo",
            Error::Config("Remote API error".to_string()),
        )
        .to_string()
    }

    /// The failure that costs the most time to diagnose: the stored
    /// credentials work, but against a project without this wallet.
    #[test]
    fn wallet_outside_the_credentials_project_is_named() {
        let msg = explain(Ok(vec!["acc_live"]), "acc_stale");
        assert!(msg.contains("acc_stale"), "{msg}");
        assert!(msg.contains("acc_live (addr-for-acc_live)"), "{msg}");
        assert!(
            msg.contains("pay account new demo --backend fake --force"),
            "{msg}"
        );
    }

    #[test]
    fn rejected_credentials_are_reported_as_such() {
        let msg = explain(Err(()), "acc_live");
        assert!(msg.contains("rotated or revoked"), "{msg}");
        assert!(msg.contains("Remote API error"), "{msg}");
        assert!(
            msg.contains("pay account new demo --backend fake --force"),
            "{msg}"
        );
    }

    #[test]
    fn empty_project_points_at_wallet_creation() {
        let msg = explain(Ok(vec![]), "acc_live");
        assert!(msg.contains("Create one first."), "{msg}");
    }

    /// A provider with nothing stored (a hardware wallet) has no
    /// credentials to reason about: its own error is the explanation.
    #[test]
    fn credential_less_provider_keeps_its_own_error() {
        struct DeviceOnly;
        impl SigningBackend for DeviceOnly {
            fn id(&self) -> &'static str {
                "device"
            }
            fn display_name(&self) -> &'static str {
                "Device"
            }
            fn description(&self) -> &'static str {
                "test double"
            }
            fn custody(&self) -> crate::backend::Custody {
                crate::backend::Custody::Hardware
            }
            fn is_exportable(&self) -> bool {
                false
            }
            fn signs_raw_messages(&self) -> bool {
                false
            }
            fn approval(&self) -> crate::backend::Approval {
                crate::backend::Approval::DeviceConfirmation
            }
            fn is_available(&self) -> bool {
                true
            }
            fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
                None
            }
        }
        impl RemoteProvider for DeviceOnly {
            fn credential_fields(&self) -> &'static [CredentialField] {
                &[]
            }
            fn credentials_hint(&self) -> &'static str {
                "plug it in"
            }
            fn no_wallets_hint(&self) -> &'static str {
                "no device"
            }
            fn discover(&self, _: &Credentials) -> Result<Vec<RemoteWallet>> {
                panic!("discovery must not run without credentials to check")
            }
            fn connect(&self, _: &Credentials, _: &str) -> Result<Box<dyn TransactionSigner>> {
                unreachable!()
            }
        }

        let msg = explain_connect_failure(
            &DeviceOnly,
            &Credentials::new(),
            "m/44'/501'/0'",
            "demo",
            Error::Config("Could not connect to the device".to_string()),
        )
        .to_string();
        assert_eq!(msg, "Configuration error: Could not connect to the device");
    }

    /// When the wallet *is* there, the provider's own error is the real
    /// story and must not be replaced by a guess.
    #[test]
    fn reachable_wallet_keeps_the_original_error() {
        let msg = explain(Ok(vec!["acc_live"]), "acc_live");
        assert_eq!(
            msg,
            Error::Config("Remote API error".to_string()).to_string()
        );
    }

    #[test]
    fn store_credentials_roundtrip_through_keystore() {
        let ks = crate::keystore::Keystore::in_memory();
        let intent = AuthIntent::from_reason("test");
        let creds = Credentials::from([
            ("secret_key".to_string(), "sk_test_abc".to_string()),
            ("wallet_secret".to_string(), "BASE64DER".to_string()),
        ]);

        store_credentials(&ks, "default", &creds, &intent).unwrap();
        assert!(ks.credential_exists("default"));

        let blob = ks.load_credential_with_intent("default", &intent).unwrap();
        let back: Credentials = serde_json::from_slice(&blob).unwrap();
        assert_eq!(back["secret_key"], "sk_test_abc");
        assert_eq!(back["wallet_secret"], "BASE64DER");
    }
}
