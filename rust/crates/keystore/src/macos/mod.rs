//! macOS: Touch ID authentication + Apple Keychain storage via native FFI.
//!
//! Talks directly to Security.framework and LocalAuthentication.framework
//! through `objc2` / `core-foundation` bindings. Previous builds shelled
//! out to a compiled Swift helper at `~/.cache/pay/pay.sh`; that helper
//! has been deleted along with its compile/codesign/cache machinery.

mod keychain;
mod touchid;

use crate::{AuthGate, AuthIntent, Result, SecretStore, Zeroizing};
use std::sync::OnceLock;

pub struct TouchId;

impl AuthGate for TouchId {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        cleanup_legacy_helper_once();
        touchid::evaluate(&intent.prompt_message())
            .map_err(|e| augment_unavailable(&e).unwrap_or(e))
    }

    fn is_available(&self) -> bool {
        touchid::can_evaluate()
    }
}

pub struct AppleKeychainStore;

impl SecretStore for AppleKeychainStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        cleanup_legacy_helper_once();
        keychain::store(key, data)
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        cleanup_legacy_helper_once();
        keychain::load(key)
    }

    fn exists(&self, key: &str) -> bool {
        keychain::exists(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        keychain::delete(key)
    }
}

/// On the first authentication or write of the process, remove the
/// legacy Swift helper artifacts under `~/.cache/pay/`. The cache
/// directory was the surface for several audit findings (#3, #28, #52,
/// #56–#63, #67, #70, #71); we no longer use it and would prefer not
/// to leave stale, ad-hoc-signed executables in the user's tree.
///
/// Only removes files this crate ever wrote there. Falls back silently
/// on any error — this is opportunistic cleanup, not a security control.
fn cleanup_legacy_helper_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let dir = std::path::PathBuf::from(home).join(".cache").join("pay");
        let _ = std::fs::remove_file(dir.join("pay.sh"));
        let _ = std::fs::remove_file(dir.join("pay.sh.entitlements"));
        // `remove_dir` (not `remove_dir_all`) only succeeds when the
        // directory is empty, so we never take out user files we don't
        // own.
        let _ = std::fs::remove_dir(&dir);
    });
}

/// Wrap an "auth unavailable" backend error with the setup guidance we
/// used to bundle with every Touch-ID failure. Returns `None` if the
/// error doesn't look like an unavailability problem.
fn augment_unavailable(err: &crate::Error) -> Option<crate::Error> {
    match err {
        crate::Error::Backend(msg) if mentions_unavailable(msg) => {
            Some(crate::Error::Backend(format!(
                "{msg}\n\nTouch ID is required to use macOS Keychain with pay. \
                 Make sure Touch ID is available and configured on this Mac, then try again."
            )))
        }
        _ => None,
    }
}

fn mentions_unavailable(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    // Match the substrings as they actually appear in the
    // `localizedDescription` returned by `LAError` (US English):
    //   -5 passcodeNotSet      → "Passcode is not set on the device."
    //   -6 biometryNotAvailable→ "Biometry is not available."
    //   -7 biometryNotEnrolled → "Biometry is not enrolled."
    //   -8 biometryLockout     → "Biometry is locked out."
    // The match is still substring-based so other locales tend to use
    // the same English-loanword keywords ("passcode", "biometry") even
    // in translated strings.
    m.contains("not available")
        || m.contains("not enrolled")
        || m.contains("passcode")
        || m.contains("locked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchid_is_available_does_not_panic() {
        // Read-only call into LocalAuthentication.framework. Whether the
        // host has Touch ID enrolled (a developer Mac) or not (CI runner
        // without biometric hardware), the call must return a bool
        // without panicking. Catches obvious FFI signature / linking
        // regressions before any keychain operation runs.
        let _ = TouchId.is_available();
    }

    #[test]
    fn keychain_exists_for_unknown_account_is_false() {
        // SecItemCopyMatching against an account name that has never
        // been written must return false, not error or panic. Uses a
        // process-and-time-unique account name so a parallel test run
        // can't collide.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let key = format!("pay-keystore-smoke-{}-{nanos}", std::process::id());
        assert!(!AppleKeychainStore.exists(&key));
    }

    #[test]
    fn augment_unavailable_recognises_device_state_messages() {
        for msg in [
            "Biometry is not available.",
            "Biometry is not enrolled.",
            "Passcode is not set on the device.",
            "Biometry is locked out.",
        ] {
            let err = crate::Error::Backend(msg.to_string());
            let augmented = augment_unavailable(&err)
                .unwrap_or_else(|| panic!("must add guidance for {msg:?}"));
            assert!(augmented.to_string().contains("Touch ID is required"));
        }
    }

    #[test]
    fn augment_unavailable_leaves_unrelated_errors_alone() {
        let err = crate::Error::Backend("SecItemAdd failed: OSStatus -34018".to_string());
        assert!(augment_unavailable(&err).is_none());
        let err = crate::Error::AuthDenied("User cancel".to_string());
        assert!(augment_unavailable(&err).is_none());
    }
}
