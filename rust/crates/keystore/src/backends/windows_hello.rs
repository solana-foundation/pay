//! Windows Hello keystore backend.
//!
//! Stores keypairs in the Windows Credential Manager and requires a Windows Hello
//! authentication prompt (fingerprint, face recognition, or PIN) before returning
//! any private key material — mirroring the Touch ID (macOS) and Polkit (Linux)
//! behaviour of the other backends.
//!
//! # Storage layout
//!
//! | Credential target          | Contents                         | Auth required? |
//! |----------------------------|----------------------------------|----------------|
//! | `pay.sh/{account}.pubkey`  | 32-byte raw public key           | No             |
//! | `pay.sh/{account}`         | 64-byte raw keypair              | Yes            |
//!
//! Both credentials use `CRED_TYPE_GENERIC` and `CRED_PERSIST_LOCAL_MACHINE`
//! (device-only; no roaming / cloud sync).

use std::{cell::Cell, slice};

use windows::{
    Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    },
    Win32::{
        Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
            CredReadW, CredWriteW,
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx},
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

use crate::{Error, KeystoreBackend, Result, SyncMode, Zeroizing};

// ── COM/WinRT initialisation ────────────────────────────────────────────────

thread_local! {
    /// Track whether *this* thread initialised COM so we only do it once.
    static COM_INIT: Cell<bool> = const { Cell::new(false) };
}

/// Ensure the current thread has a COM/WinRT apartment.
///
/// `CoInitializeEx` with `COINIT_MULTITHREADED` is safe to call repeatedly:
/// - `S_OK`  — we initialised the MTA on this thread.
/// - `S_FALSE` — thread already in MTA, nothing to do.
/// - `RPC_E_CHANGED_MODE` — thread is in an STA; WinRT calls will still work
///   through the MTA proxy, so we proceed anyway.
fn ensure_com_init() {
    COM_INIT.with(|cell| {
        if !cell.get() {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            cell.set(true);
        }
    });
}

// ── Windows Hello authentication ─────────────────────────────────────────────

/// Prompt for Windows Hello (fingerprint / face / PIN).
///
/// Returns `Ok(())` only when the user is successfully verified.  Every other
/// outcome — cancellation, policy restriction, retries exhausted — is mapped to
/// an appropriate `Error`.
fn verify_consent(reason: &str) -> Result<()> {
    ensure_com_init();

    let message = HSTRING::from(reason);
    let result = UserConsentVerifier::RequestVerificationAsync(&message)
        .map_err(|e| Error::Backend(format!("Windows Hello unavailable: {e}")))?
        .get()
        .map_err(|e| Error::Backend(format!("Windows Hello request failed: {e}")))?;

    match result {
        UserConsentVerificationResult::Verified => Ok(()),
        UserConsentVerificationResult::Canceled => Err(Error::AuthDenied(
            "Windows Hello: authentication cancelled".into(),
        )),
        UserConsentVerificationResult::DeviceBusy => Err(Error::AuthDenied(
            "Windows Hello: biometric device busy — try again".into(),
        )),
        UserConsentVerificationResult::RetriesExhausted => Err(Error::AuthDenied(
            "Windows Hello: too many failed attempts".into(),
        )),
        UserConsentVerificationResult::DisabledByPolicy => Err(Error::AuthDenied(
            "Windows Hello: disabled by group policy".into(),
        )),
        UserConsentVerificationResult::NotConfiguredForUser => Err(Error::AuthDenied(
            "Windows Hello: not configured — set up Windows Hello in Settings first".into(),
        )),
        _ => Err(Error::AuthDenied(
            "Windows Hello: authentication failed".into(),
        )),
    }
}

// ── Windows Credential Manager helpers ───────────────────────────────────────

/// Build a null-terminated UTF-16 string for use with Win32 wide-string APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn keypair_target(account: &str) -> Vec<u16> {
    to_wide(&format!("pay.sh/{account}"))
}

fn pubkey_target(account: &str) -> Vec<u16> {
    to_wide(&format!("pay.sh/{account}.pubkey"))
}

/// Store `blob` bytes under `target` in the Windows Credential Manager.
///
/// # Safety
/// The raw pointers passed to `CredWriteW` point into the provided slices.
/// `CredWriteW` treats them as read-only input even though the C API uses
/// non-const pointers (a historical Win32 convention).
fn cred_write(target: &[u16], blob: &[u8]) -> Result<()> {
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        // SAFETY: CredWriteW reads but does not modify TargetName or CredentialBlob.
        TargetName: PWSTR(target.as_ptr().cast_mut()),
        CredentialBlobSize: blob
            .len()
            .try_into()
            .map_err(|_| Error::Backend("credential blob too large".into()))?,
        CredentialBlob: blob.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    // SAFETY: `cred` is fully initialised; all pointer fields remain valid for
    // the duration of this call.
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| Error::Backend(format!("CredWriteW failed: {e}")))
}

/// Read bytes previously stored under `target`.
fn cred_read(target: &[u16]) -> Result<Zeroizing<Vec<u8>>> {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: We pass a valid null-terminated wide string and a valid out-pointer.
    unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr) }
        .map_err(|e| Error::Backend(format!("CredReadW failed: {e}")))?;

    // SAFETY: On success `ptr` is non-null and points to a CREDENTIALW allocated
    // by the system.  We copy the blob out before freeing.
    let blob = unsafe {
        let c = &*ptr;
        slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
    };
    // SAFETY: `ptr` was returned by CredReadW; CredFree is the correct release fn.
    unsafe { CredFree(ptr.cast()) };

    Ok(Zeroizing::new(blob))
}

/// Return `true` if a credential with the given `target` name exists.
fn cred_exists(target: &[u16]) -> bool {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: same as cred_read.
    let found =
        unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr).is_ok() };
    if found && !ptr.is_null() {
        unsafe { CredFree(ptr.cast()) };
    }
    found
}

/// Delete the credential stored under `target`.
fn cred_delete(target: &[u16]) -> Result<()> {
    // SAFETY: valid null-terminated wide string.
    unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) }
        .map_err(|e| Error::Backend(format!("CredDeleteW failed: {e}")))
}

// ── Public backend struct ─────────────────────────────────────────────────────

/// Keystore backend backed by the Windows Credential Manager with Windows Hello
/// authentication.
///
/// Create via [`WindowsHello::new`].  Before using, call
/// [`WindowsHello::is_available`] to verify that Windows Hello is set up on
/// this device.
pub struct WindowsHello;

impl Default for WindowsHello {
    fn default() -> Self {
        Self
    }
}

impl WindowsHello {
    pub fn new() -> Self {
        Self
    }

    /// Returns `true` if Windows Hello is configured and ready on this device.
    ///
    /// A return value of `false` means the user has not set up Windows Hello
    /// (fingerprint / face / PIN) in Windows Settings, or the hardware is
    /// unavailable.
    pub fn is_available() -> bool {
        ensure_com_init();
        UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| op.get())
            .map(|r| r == UserConsentVerifierAvailability::Available)
            .unwrap_or(false)
    }
}

impl KeystoreBackend for WindowsHello {
    /// Import a keypair, storing it in the Windows Credential Manager.
    ///
    /// The user will be prompted for Windows Hello authentication before the
    /// keypair is written.  `sync` is accepted but ignored — credentials are
    /// always stored with `CRED_PERSIST_LOCAL_MACHINE` (device-only).
    fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        if keypair_bytes.len() != 64 {
            return Err(Error::InvalidKeypair(format!(
                "expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }
        verify_consent(&format!(
            "Authorize storing pay keypair for account \"{account}\""
        ))?;

        // Store the public key (no auth needed to read) and the full keypair.
        cred_write(&pubkey_target(account), &keypair_bytes[32..])?;
        cred_write(&keypair_target(account), keypair_bytes)?;
        Ok(())
    }

    /// Returns `true` only when *both* the keypair and public-key credentials
    /// exist (guarding against partial setups).
    fn exists(&self, account: &str) -> bool {
        cred_exists(&keypair_target(account)) && cred_exists(&pubkey_target(account))
    }

    /// Delete the keypair from the Credential Manager after a Windows Hello prompt.
    fn delete(&self, account: &str) -> Result<()> {
        verify_consent(&format!(
            "Authorize deleting pay keypair for account \"{account}\""
        ))?;
        cred_delete(&keypair_target(account))?;
        // Best-effort: ignore "not found" errors for the public-key entry.
        let _ = cred_delete(&pubkey_target(account));
        Ok(())
    }

    /// Return the 32-byte public key without requiring authentication.
    fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        let bytes = cred_read(&pubkey_target(account))?;
        if bytes.len() != 32 {
            return Err(Error::InvalidKeypair(format!(
                "expected 32-byte public key, got {}",
                bytes.len()
            )));
        }
        Ok(bytes.to_vec())
    }

    /// Return the full 64-byte keypair after a Windows Hello authentication prompt.
    ///
    /// `reason` is displayed in the Windows Hello dialog (e.g.
    /// `"pay 0.001 SOL for weather API"`).  Authentication is required on
    /// **every** call — there is no credential caching.
    fn load_keypair(&self, account: &str, reason: &str) -> Result<Zeroizing<Vec<u8>>> {
        verify_consent(reason)?;
        let bytes = cred_read(&keypair_target(account))?;
        if bytes.len() != 64 {
            return Err(Error::InvalidKeypair(format!(
                "expected 64-byte keypair, got {}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }
}
