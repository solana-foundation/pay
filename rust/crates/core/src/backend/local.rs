//! Local keystore backends: the OS credential store, 1Password, an
//! owner-only keypair file, and the inline ephemeral wallet.
//!
//! Each platform store is declared on every operating system so callers
//! never need `cfg`; on a foreign OS it reports unavailable and refuses to
//! build a keystore.

use super::{Approval, Custody, Gate, LocalKeystoreBackend, SigningBackend, StoreParams};
use crate::accounts::BackendKind;
use crate::keystore::auth::NoAuth;
use crate::keystore::{AuthGate, Keystore, SecretStore};
use crate::{Error, Result};

/// Place `gate` in front of `store`. Platform stores prompt on writes too,
/// so creating or deleting an account is approved like using it.
fn compose(
    store: impl SecretStore + 'static,
    platform_gate: impl AuthGate + 'static,
    gate: Gate,
    auth_on_write: bool,
) -> Keystore {
    match gate {
        Gate::Platform => Keystore::new(platform_gate, store, auth_on_write),
        Gate::Disabled => Keystore::new(NoAuth, store, false),
        Gate::Override(override_gate) => {
            Keystore::from_boxed_auth(override_gate, Box::new(store), auth_on_write)
        }
    }
}

// ── Apple Keychain ──────────────────────────────────────────────────────────

/// macOS Keychain, gated by Touch ID.
pub struct AppleKeychain;

impl SigningBackend for AppleKeychain {
    fn id(&self) -> &'static str {
        "apple-keychain"
    }
    fn flag(&self) -> &'static str {
        "keychain"
    }
    fn display_name(&self) -> &'static str {
        "Apple Keychain"
    }
    fn description(&self) -> &'static str {
        "requires Touch ID"
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
        Approval::PlatformPrompt
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        cfg!(target_os = "macos")
    }
}

impl LocalKeystoreBackend for AppleKeychain {
    fn kind(&self) -> BackendKind {
        BackendKind::AppleKeychain
    }

    fn keystore(&self, params: &StoreParams<'_>, gate: Gate) -> Result<Keystore> {
        let _ = params;
        #[cfg(target_os = "macos")]
        {
            use crate::keystore::macos::{AppleKeychainStore, TouchId};
            Ok(compose(AppleKeychainStore, TouchId, gate, true))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = gate;
            Err(super::unavailable_on_platform(self))
        }
    }

    fn platform_gate_available(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            crate::keystore::macos::TouchId.is_available()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

// ── GNOME Keyring ───────────────────────────────────────────────────────────

/// GNOME Keyring over Secret Service, gated by polkit.
pub struct GnomeKeyring;

impl SigningBackend for GnomeKeyring {
    fn id(&self) -> &'static str {
        "gnome-keyring"
    }
    fn display_name(&self) -> &'static str {
        "GNOME Keyring"
    }
    fn description(&self) -> &'static str {
        "password prompt"
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
        Approval::PlatformPrompt
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            crate::keystore::linux::SecretServiceStore::is_available()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

impl LocalKeystoreBackend for GnomeKeyring {
    fn kind(&self) -> BackendKind {
        BackendKind::GnomeKeyring
    }

    fn keystore(&self, params: &StoreParams<'_>, gate: Gate) -> Result<Keystore> {
        let _ = params;
        #[cfg(target_os = "linux")]
        {
            use crate::keystore::linux::{Polkit, SecretServiceStore};
            Ok(compose(SecretServiceStore, Polkit, gate, true))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = gate;
            Err(super::unavailable_on_platform(self))
        }
    }

    fn platform_gate_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            crate::keystore::linux::Polkit.is_available()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

// ── Windows Hello ───────────────────────────────────────────────────────────

/// Windows Credential Manager, gated by Windows Hello.
pub struct WindowsHello;

impl SigningBackend for WindowsHello {
    fn id(&self) -> &'static str {
        "windows-hello"
    }
    fn display_name(&self) -> &'static str {
        "Windows Hello"
    }
    fn description(&self) -> &'static str {
        "fingerprint / face / PIN"
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
        Approval::PlatformPrompt
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        // The credential store always exists on Windows; setup requires the
        // Hello prompt as well, so availability follows the prompt.
        self.platform_gate_available()
    }
}

impl LocalKeystoreBackend for WindowsHello {
    fn kind(&self) -> BackendKind {
        BackendKind::WindowsHello
    }

    fn keystore(&self, params: &StoreParams<'_>, gate: Gate) -> Result<Keystore> {
        let _ = params;
        #[cfg(target_os = "windows")]
        {
            use crate::keystore::windows::{WindowsCredentialStore, WindowsHelloAuth};
            Ok(compose(
                WindowsCredentialStore,
                WindowsHelloAuth,
                gate,
                true,
            ))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = gate;
            Err(super::unavailable_on_platform(self))
        }
    }

    fn platform_gate_available(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            crate::keystore::windows::WindowsHelloAuth::is_available()
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }
}

// ── 1Password ───────────────────────────────────────────────────────────────

/// 1Password through the `op` CLI. The CLI owns the approval prompt, so the
/// [`Gate`] is not applied.
///
/// Deprecated: kept so accounts created with it keep working; no new
/// account may be stored here.
pub struct OnePassword;

/// Shown wherever a new 1Password account is refused.
pub const ONE_PASSWORD_DEPRECATION: &str = "The 1Password backend is deprecated and cannot store new accounts. Use the platform \
     keystore or a remote wallet; existing 1Password accounts keep working.";

impl SigningBackend for OnePassword {
    fn id(&self) -> &'static str {
        "1password"
    }
    fn display_name(&self) -> &'static str {
        "1Password"
    }
    fn description(&self) -> &'static str {
        "via the op CLI"
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
        Approval::ExternalTool
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        Keystore::onepassword_available()
    }
    fn deprecated(&self) -> Option<&'static str> {
        Some(ONE_PASSWORD_DEPRECATION)
    }
}

impl LocalKeystoreBackend for OnePassword {
    fn kind(&self) -> BackendKind {
        BackendKind::OnePassword
    }

    fn keystore(&self, params: &StoreParams<'_>, _gate: Gate) -> Result<Keystore> {
        let op_account = params.op_account.map(str::to_string);
        Ok(match params.vault {
            Some(vault) => Keystore::onepassword_with_vault(vault, op_account),
            None => Keystore::onepassword(op_account),
        })
    }

    fn platform_gate_available(&self) -> bool {
        true
    }
}

// ── Keypair file ────────────────────────────────────────────────────────────

/// An owner-only Solana JSON keypair file. Not encrypted; the platform
/// prompt gates reads when the account requires auth.
pub struct File;

impl SigningBackend for File {
    fn id(&self) -> &'static str {
        "file"
    }
    fn display_name(&self) -> &'static str {
        "Keypair file"
    }
    fn description(&self) -> &'static str {
        "owner-only file, not encrypted"
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
        Approval::PlatformPrompt
    }
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        true
    }
}

impl LocalKeystoreBackend for File {
    fn kind(&self) -> BackendKind {
        BackendKind::File
    }

    fn keystore(&self, params: &StoreParams<'_>, gate: Gate) -> Result<Keystore> {
        let path = params
            .file_path
            .ok_or_else(|| Error::Config("The file backend needs a keypair path.".to_string()))?;
        // Account files historically accepted `~` paths.  Expand here rather
        // than in FileStore so callers that intentionally use a literal path
        // keep doing so.
        let path = shellexpand::tilde(path).into_owned();
        let store = crate::keystore::store::FileStore::new(path);
        // Writes never prompt: creating the file is the explicit action.
        Ok(match gate {
            Gate::Platform => {
                Keystore::from_boxed_auth(super::platform_gate()?, Box::new(store), false)
            }
            Gate::Disabled => Keystore::new(NoAuth, store, false),
            Gate::Override(override_gate) => {
                Keystore::from_boxed_auth(override_gate, Box::new(store), false)
            }
        })
    }

    fn platform_gate_available(&self) -> bool {
        super::platform_gate_available()
    }
}

// ── Ephemeral ───────────────────────────────────────────────────────────────

/// A throwaway wallet stored inline in `accounts.yml`, for sandbox
/// networks. No store, no prompt.
pub struct Ephemeral;

impl SigningBackend for Ephemeral {
    fn id(&self) -> &'static str {
        "ephemeral"
    }
    fn display_name(&self) -> &'static str {
        "Ephemeral wallet"
    }
    fn description(&self) -> &'static str {
        "inline throwaway wallet for sandbox networks"
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
    fn max_tx_version(&self) -> Option<pay_kit::core::tx::TxVersion> {
        None
    }
    fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keystore::AuthIntent;

    struct Deny;
    impl AuthGate for Deny {
        fn authenticate(&self, _: &AuthIntent) -> std::result::Result<(), crate::keystore::Error> {
            Err(crate::keystore::Error::AuthDenied("test".into()))
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    fn keypair_file() -> (tempfile::TempDir, String, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kp.json");
        let bytes: Vec<u8> = (0..64).collect();
        std::fs::write(&path, serde_json::to_string(&bytes).unwrap()).unwrap();
        (dir, path.to_string_lossy().into_owned(), bytes)
    }

    #[test]
    fn file_backend_requires_a_path() {
        let Err(err) = File.keystore(&StoreParams::default(), Gate::Disabled) else {
            panic!("no path must fail");
        };
        assert!(err.to_string().contains("needs a keypair path"));
    }

    #[test]
    fn file_backend_reads_without_a_gate_when_disabled() {
        let (_dir, path, expected) = keypair_file();
        let ks = File
            .keystore(
                &StoreParams {
                    file_path: Some(&path),
                    ..StoreParams::default()
                },
                Gate::Disabled,
            )
            .unwrap();
        let bytes = ks
            .load_keypair_with_intent("x", &AuthIntent::default_payment())
            .unwrap();
        assert_eq!(&*bytes, &expected);
    }

    #[test]
    fn file_backend_applies_the_override_gate_before_reading() {
        let (_dir, path, _) = keypair_file();
        let ks = File
            .keystore(
                &StoreParams {
                    file_path: Some(&path),
                    ..StoreParams::default()
                },
                Gate::Override(Box::new(Deny)),
            )
            .unwrap();
        let err = ks
            .load_keypair_with_intent("x", &AuthIntent::default_payment())
            .expect_err("gate denies");
        assert!(matches!(err, crate::keystore::Error::AuthDenied(_)));
    }

    #[test]
    fn file_backend_never_prompts_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        let path = path.to_string_lossy().into_owned();
        let ks = File
            .keystore(
                &StoreParams {
                    file_path: Some(&path),
                    ..StoreParams::default()
                },
                Gate::Override(Box::new(Deny)),
            )
            .unwrap();
        let bytes: Vec<u8> = (0..64).collect();
        ks.import_with_intent(
            "x",
            &bytes,
            crate::keystore::SyncMode::ThisDeviceOnly,
            &AuthIntent::create_account("x"),
        )
        .expect("writes bypass the gate");
        assert!(std::path::Path::new(&path).exists());
    }

    #[test]
    fn one_password_builds_without_the_gate() {
        // The op CLI owns the prompt, so any gate is accepted and ignored;
        // building must not touch it or the CLI.
        OnePassword
            .keystore(
                &StoreParams {
                    vault: Some("Work"),
                    op_account: Some("my.1password.com"),
                    ..StoreParams::default()
                },
                Gate::Override(Box::new(Deny)),
            )
            .expect("1Password keystore composes without probing op");
    }

    #[test]
    fn apple_keychain_keeps_its_legacy_flag() {
        assert_eq!(AppleKeychain.flag(), "keychain");
        assert_eq!(AppleKeychain.id(), "apple-keychain");
        for b in [
            &GnomeKeyring as &dyn SigningBackend,
            &WindowsHello,
            &OnePassword,
            &File,
            &Ephemeral,
        ] {
            assert_eq!(b.flag(), b.id(), "{}", b.id());
        }
    }

    #[test]
    fn ephemeral_needs_no_approval() {
        assert_eq!(Ephemeral.approval(), Approval::None);
        assert_eq!(Ephemeral.custody(), Custody::Local);
    }
}
