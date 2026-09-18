//! Signing backends — where an account's key lives and what it can do.
//!
//! Every pay account is backed by exactly one [`SigningBackend`]. A backend
//! is a static descriptor: it says how the key is held ([`Custody`]), how a
//! signature is approved ([`Approval`]), whether the raw keypair can ever be
//! read out ([`SigningBackend::is_exportable`]), and whether `sign_message`
//! produces a raw ed25519 signature ([`SigningBackend::signs_raw_messages`]).
//! Callers ask the backend instead of pattern-matching on the account kind,
//! so adding a backend never means auditing every `match` in the tree.
//!
//! Three families implement the trait:
//!
//! - **Local keystores** ([`local`]): the OS credential store, 1Password, an
//!   owner-only file, and the inline ephemeral wallets used on sandbox
//!   networks. These also implement [`LocalKeystoreBackend`], which composes
//!   the `pay-keystore` handle with the right approval [`Gate`] in front of
//!   the secret store. The platform-specific `cfg` lives here, once.
//! - **Remote providers** ([`crate::remote`]): the key is in a custody
//!   provider's TEE or HSM and signing happens over HTTPS. Only the
//!   provider's credentials are stored locally.
//! - **Hardware wallets**: a future family; nothing here assumes a key is
//!   either local or remote.
//!
//! The registry ([`backends`], [`by_flag`], [`local_by_kind`]) is the single
//! place a backend is listed. `--backend` flags, the setup picker, legacy
//! `<flag>:<name>` signer sources, and `accounts.yml` resolution all go
//! through it.

pub mod local;

use crate::accounts::BackendKind;
use crate::keystore::{AuthGate, Keystore};
use crate::{Error, Result};

pub use local::{AppleKeychain, Ephemeral, File, GnomeKeyring, OnePassword, WindowsHello};

/// How a backend holds the private key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Custody {
    /// The keypair is stored on this machine: OS credential store,
    /// 1Password, an owner-only file, or inline in `accounts.yml`.
    Local,
    /// The key lives with a custody provider and signing is a network call.
    /// Only the provider's API credentials exist locally.
    Remote,
    /// The key lives in a hardware device attached to this machine and never
    /// leaves it.
    Hardware,
}

impl Custody {
    /// Short phrase for error messages: "on this machine", "in the
    /// provider's custody", "on the hardware device".
    pub fn location_phrase(self) -> &'static str {
        match self {
            Custody::Local => "on this machine",
            Custody::Remote => "in the provider's custody",
            Custody::Hardware => "on the hardware device",
        }
    }
}

/// How the user approves a signature at use time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// An OS prompt (Touch ID, Windows Hello, polkit) releases the key or
    /// credential. Can be replaced by an [`AuthOverride`](crate::signer::AuthOverride)
    /// such as MCP elicitation.
    PlatformPrompt,
    /// An external tool owns the prompt (1Password's `op` CLI). Overrides do
    /// not apply.
    ExternalTool,
    /// The provider enforces policy server-side; pay gates the stored API
    /// credential with the platform prompt.
    ProviderPolicy,
    /// Physical confirmation on the device.
    DeviceConfirmation,
    /// No approval step: ephemeral test wallets, or a plain file when
    /// `auth_required` is off.
    None,
}

/// A signing backend, described statically.
///
/// Implementations are unit structs registered in [`backends`]. Every
/// attribute is a required method on purpose: a new backend has to state
/// what it can do rather than inherit a default that may be wrong for it.
pub trait SigningBackend: Send + Sync {
    /// Canonical id. For local kinds this is the `accounts.yml` `keystore`
    /// value (`apple-keychain`); for remote providers it is the `provider`
    /// value (`openfort`).
    fn id(&self) -> &'static str;

    /// What `--backend` accepts and what legacy `<flag>:<name>` signer
    /// sources use. Defaults to [`id`](Self::id); Apple Keychain keeps its
    /// historical `keychain` flag.
    fn flag(&self) -> &'static str {
        self.id()
    }

    /// Human-readable name: "Apple Keychain", "Openfort backend wallet".
    fn display_name(&self) -> &'static str;

    /// One line for the setup picker: "requires Touch ID".
    fn description(&self) -> &'static str;

    /// Where the key is held.
    fn custody(&self) -> Custody;

    /// Whether the raw 64-byte keypair can be read out of this backend
    /// (`pay account export`, migration to another store). Local custody
    /// does not imply yes and remote custody does not imply no: a provider
    /// may offer key export, and a local store may refuse it.
    fn is_exportable(&self) -> bool;

    /// Whether `sign_message` returns a raw ed25519 signature over exactly
    /// the bytes given. Hardware wallets wrap the payload in an envelope, so
    /// the signature verifies over the envelope instead; proof-style
    /// credentials (SIWMPP, session and subscription proofs, batch vouchers)
    /// need this to be true.
    fn signs_raw_messages(&self) -> bool;

    /// How a signature is approved at use time.
    fn approval(&self) -> Approval;

    /// Whether the backend can be used on this machine right now. False on
    /// the wrong operating system, or when a required service or tool is
    /// missing.
    fn is_available(&self) -> bool;

    /// Highest transaction version the backend can sign, when that is below
    /// what servers may advertise. `None` means any version. The Ledger
    /// Solana app signs version 0 but not yet version 1; software and
    /// remote signers sign whatever bytes they are given.
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion>;

    /// Why this backend must not be offered for new accounts, when it is
    /// deprecated. Existing accounts keep loading so nobody is locked out;
    /// setup, import, and the picker refuse it. `None` for a live backend.
    fn deprecated(&self) -> Option<&'static str> {
        None
    }

    /// Refuse, with an explanation, when a payment path needs a raw
    /// `sign_message` signature this backend cannot produce. `what` names
    /// the thing being signed ("an x402 sign-in challenge").
    fn require_raw_message_signing(&self, what: &str) -> Result<()> {
        if self.signs_raw_messages() {
            return Ok(());
        }
        Err(Error::Config(format!(
            "{name} cannot sign {what}: it needs a raw message signature, and a {name} only \
             signs transactions. Pay with a charge or a client-signed session instead, or use \
             a software or remote wallet for this service.",
            name = self.display_name(),
        )))
    }
}

/// Which approval gate to place in front of a local secret store.
pub enum Gate {
    /// The platform's own prompt (Touch ID, Windows Hello, polkit).
    Platform,
    /// No prompt. Used when an account has `auth_required: false`, and at
    /// setup time on hosts with no enrolled biometric.
    Disabled,
    /// A caller-supplied gate, such as MCP elicitation. See
    /// [`AuthOverride`](crate::signer::AuthOverride).
    Override(Box<dyn AuthGate>),
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Gate::Platform => f.write_str("Platform"),
            Gate::Disabled => f.write_str("Disabled"),
            Gate::Override(_) => f.write_str("Override(..)"),
        }
    }
}

impl Gate {
    /// The gate an account's policy asks for: the override when present,
    /// the platform prompt when auth is required, nothing otherwise.
    pub fn for_policy(auth_required: bool, auth_override: Option<Box<dyn AuthGate>>) -> Self {
        if !auth_required {
            return Gate::Disabled;
        }
        match auth_override {
            Some(gate) => Gate::Override(gate),
            None => Gate::Platform,
        }
    }
}

/// Per-account parameters a local store needs beyond the account name.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreParams<'a> {
    /// 1Password vault.
    pub vault: Option<&'a str>,
    /// 1Password account UUID or shorthand.
    pub op_account: Option<&'a str>,
    /// Keypair file path for the file backend.
    pub file_path: Option<&'a str>,
}

/// A backend whose secrets live in a `pay-keystore` [`Keystore`] on this
/// machine.
pub trait LocalKeystoreBackend: SigningBackend {
    /// The `accounts.yml` kind this backend persists as.
    fn kind(&self) -> BackendKind;

    /// Compose the keystore handle for an account, placing `gate` in front
    /// of the secret store. Errors on the wrong platform.
    fn keystore(&self, params: &StoreParams<'_>, gate: Gate) -> Result<Keystore>;

    /// Whether the platform's own approval prompt is usable right now. A
    /// store can exist without its prompt: a Mac with no enrolled Touch ID,
    /// a headless Linux session with an unlocked keyring and no polkit
    /// agent.
    fn platform_gate_available(&self) -> bool;
}

/// Every local keystore backend, on every platform. Ones that do not apply
/// to this operating system report `is_available() == false`.
pub fn local_backends() -> &'static [&'static dyn LocalKeystoreBackend] {
    &[
        &AppleKeychain,
        &GnomeKeyring,
        &WindowsHello,
        &OnePassword,
        &File,
    ]
}

/// Every backend: local keystores, the ephemeral inline wallet, and every
/// remote provider registered in [`crate::remote`].
pub fn backends() -> impl Iterator<Item = &'static dyn SigningBackend> {
    local_backends()
        .iter()
        .map(|b| *b as &'static dyn SigningBackend)
        .chain(std::iter::once(&Ephemeral as &'static dyn SigningBackend))
        .chain(crate::remote::providers().map(|p| p as &'static dyn SigningBackend))
}

/// Resolve a `--backend` flag or legacy source prefix.
pub fn by_flag(flag: &str) -> Option<&'static dyn SigningBackend> {
    backends().find(|b| b.flag() == flag || b.id() == flag)
}

/// Resolve a `--backend` flag to a local keystore backend.
pub fn local_by_flag(flag: &str) -> Option<&'static dyn LocalKeystoreBackend> {
    local_backends()
        .iter()
        .copied()
        .find(|b| b.flag() == flag || b.id() == flag)
}

/// Resolve an `accounts.yml` kind to its local keystore backend. `None` for
/// [`BackendKind::Ephemeral`] and [`BackendKind::Remote`], which have no
/// keystore.
pub fn local_by_kind(kind: &BackendKind) -> Option<&'static dyn LocalKeystoreBackend> {
    local_backends().iter().copied().find(|b| b.kind() == *kind)
}

/// The operating system's native credential store, when this OS has one.
pub fn platform() -> Option<&'static dyn LocalKeystoreBackend> {
    #[cfg(target_os = "macos")]
    {
        Some(&AppleKeychain)
    }
    #[cfg(target_os = "linux")]
    {
        Some(&GnomeKeyring)
    }
    #[cfg(target_os = "windows")]
    {
        Some(&WindowsHello)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// The platform's own approval prompt as a standalone gate, for stores that
/// have none of their own (the file backend).
pub fn platform_gate() -> Result<Box<dyn AuthGate>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::keystore::macos::TouchId))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::keystore::linux::Polkit))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::keystore::windows::WindowsHelloAuth))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(Error::Config(
            "No platform approval prompt is available on this operating system.".to_string(),
        ))
    }
}

/// True when the platform prompt (Touch ID, Windows Hello, polkit) is usable
/// on this machine. Callers that can fall back to another approval channel
/// (MCP elicitation) check this first.
pub fn platform_gate_available() -> bool {
    platform().is_some_and(|p| p.platform_gate_available())
}

/// Error for a backend that is not usable on this operating system.
pub(crate) fn unavailable_on_platform(backend: &dyn SigningBackend) -> Error {
    Error::Config(format!(
        "{} is not available on this platform",
        backend.display_name()
    ))
}

/// Signing-backend doubles for tests across the crate.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A hardware-style backend: signs transactions only, never raw messages.
    pub(crate) struct TransactionsOnly;

    impl SigningBackend for TransactionsOnly {
        fn id(&self) -> &'static str {
            "transactions-only"
        }
        fn display_name(&self) -> &'static str {
            "Test hardware wallet"
        }
        fn description(&self) -> &'static str {
            "test double"
        }
        fn custody(&self) -> Custody {
            Custody::Hardware
        }
        fn is_exportable(&self) -> bool {
            false
        }
        fn signs_raw_messages(&self) -> bool {
            false
        }
        fn approval(&self) -> Approval {
            Approval::DeviceConfirmation
        }
        fn is_available(&self) -> bool {
            true
        }
        fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
            Some(pay_kit::core::tx::TxVersion::V0)
        }
    }

    /// A software-style backend that signs anything.
    pub(crate) struct SignsAnything;

    impl SigningBackend for SignsAnything {
        fn id(&self) -> &'static str {
            "signs-anything"
        }
        fn display_name(&self) -> &'static str {
            "Test software wallet"
        }
        fn description(&self) -> &'static str {
            "test double"
        }
        fn custody(&self) -> Custody {
            Custody::Local
        }
        fn is_exportable(&self) -> bool {
            true
        }
        fn signs_raw_messages(&self) -> bool {
            true
        }
        fn approval(&self) -> Approval {
            Approval::None
        }
        fn is_available(&self) -> bool {
            true
        }
        fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn raw_message_guard_names_the_backend_and_the_signature() {
        assert!(
            testing::SignsAnything
                .require_raw_message_signing("a proof")
                .is_ok()
        );
        let Err(Error::Config(msg)) =
            testing::TransactionsOnly.require_raw_message_signing("a subscription proof")
        else {
            panic!("a transactions-only backend must refuse raw messages");
        };
        assert!(
            msg.starts_with("Test hardware wallet cannot sign a subscription proof"),
            "{msg}"
        );
        assert!(msg.contains("only signs transactions"), "{msg}");
    }

    #[test]
    fn ids_and_flags_are_unique() {
        let ids: Vec<&str> = backends().map(|b| b.id()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate backend id: {ids:?}");

        let flags: Vec<&str> = backends().map(|b| b.flag()).collect();
        let unique: HashSet<&str> = flags.iter().copied().collect();
        assert_eq!(
            flags.len(),
            unique.len(),
            "duplicate backend flag: {flags:?}"
        );
    }

    #[test]
    fn every_backend_is_described() {
        for b in backends() {
            assert!(!b.id().is_empty());
            assert!(!b.display_name().is_empty(), "{}", b.id());
            assert!(!b.description().is_empty(), "{}", b.id());
        }
    }

    #[test]
    fn by_flag_accepts_flag_and_canonical_id() {
        assert_eq!(by_flag("keychain").map(|b| b.id()), Some("apple-keychain"));
        assert_eq!(
            by_flag("apple-keychain").map(|b| b.id()),
            Some("apple-keychain")
        );
        assert_eq!(by_flag("1password").map(|b| b.id()), Some("1password"));
        assert_eq!(
            by_flag("openfort").map(|b| b.custody()),
            Some(Custody::Remote)
        );
        assert!(by_flag("not-a-backend").is_none());
    }

    #[test]
    fn local_kinds_round_trip_through_the_registry() {
        for b in local_backends() {
            let kind = b.kind();
            let found = local_by_kind(&kind).expect("registered");
            assert_eq!(found.id(), b.id());
            assert_eq!(
                kind.to_string(),
                b.id(),
                "kind serialises as the backend id"
            );
        }
        assert!(local_by_kind(&BackendKind::Ephemeral).is_none());
        assert!(local_by_kind(&BackendKind::Remote).is_none());
    }

    #[test]
    fn exportability_is_declared_per_backend() {
        // Local keystores hand the keypair back; the inline wallet is the
        // keypair; a remote provider never does.
        for b in local_backends() {
            assert!(b.is_exportable(), "{}", b.id());
        }
        assert!(Ephemeral.is_exportable());
        for p in crate::remote::providers() {
            assert!(!p.is_exportable(), "{}", p.id());
        }
    }

    #[test]
    fn one_password_is_deprecated_and_the_rest_are_live() {
        for b in backends() {
            let expect_deprecated = b.id() == "1password";
            assert_eq!(b.deprecated().is_some(), expect_deprecated, "{}", b.id());
        }
        // Deprecated backends still resolve, so existing accounts load.
        assert!(local_by_kind(&BackendKind::OnePassword).is_some());
    }

    #[test]
    fn software_backends_sign_raw_messages_at_any_version() {
        for b in backends() {
            if b.custody() != Custody::Hardware {
                assert!(b.signs_raw_messages(), "{}", b.id());
                assert_eq!(b.max_tx_version(), None, "{}", b.id());
            }
        }
    }

    #[test]
    fn platform_matches_the_operating_system() {
        let platform = platform().map(|p| p.id());
        #[cfg(target_os = "macos")]
        assert_eq!(platform, Some("apple-keychain"));
        #[cfg(target_os = "linux")]
        assert_eq!(platform, Some("gnome-keyring"));
        #[cfg(target_os = "windows")]
        assert_eq!(platform, Some("windows-hello"));
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        assert_eq!(platform, None);
    }

    #[test]
    fn foreign_platform_stores_are_unavailable_and_refuse_to_build() {
        for b in local_backends() {
            let is_this_platform = platform().is_some_and(|p| p.id() == b.id());
            let is_portable = matches!(b.kind(), BackendKind::OnePassword | BackendKind::File);
            if is_this_platform || is_portable {
                continue;
            }
            assert!(!b.is_available(), "{} should be unavailable here", b.id());
            let Err(err) = b.keystore(&StoreParams::default(), Gate::Disabled) else {
                panic!("{} must not build here", b.id());
            };
            assert!(
                err.to_string().contains("not available on this platform"),
                "{}: {err}",
                b.id()
            );
        }
    }

    #[test]
    fn gate_for_policy_prefers_override_then_platform() {
        struct Always;
        impl AuthGate for Always {
            fn authenticate(
                &self,
                _: &crate::keystore::AuthIntent,
            ) -> std::result::Result<(), crate::keystore::Error> {
                Ok(())
            }
            fn is_available(&self) -> bool {
                true
            }
        }
        assert!(matches!(Gate::for_policy(false, None), Gate::Disabled));
        assert!(matches!(
            Gate::for_policy(false, Some(Box::new(Always))),
            Gate::Disabled
        ));
        assert!(matches!(Gate::for_policy(true, None), Gate::Platform));
        assert!(matches!(
            Gate::for_policy(true, Some(Box::new(Always))),
            Gate::Override(_)
        ));
    }
}
