//! Windows: Windows Hello authentication + Credential Manager storage.

use crate::{AuthGate, Error, Result, SecretStore, Zeroizing};
use std::cell::Cell;
use std::slice;
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

// ── COM initialization ──────────────────────────────────────────────────────

thread_local! {
    static COM_INIT: Cell<bool> = const { Cell::new(false) };
}

fn ensure_com_init() {
    COM_INIT.with(|cell| {
        if !cell.get() {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            cell.set(true);
        }
    });
}

// ── Windows Hello auth gate ─────────────────────────────────────────────────

pub struct WindowsHelloAuth;

impl WindowsHelloAuth {
    pub fn is_available() -> bool {
        ensure_com_init();
        UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| op.get())
            .map(|r| r == UserConsentVerifierAvailability::Available)
            .unwrap_or(false)
    }
}

impl AuthGate for WindowsHelloAuth {
    fn authenticate(&self, reason: &str) -> Result<()> {
        ensure_com_init();

        let message = HSTRING::from(reason);
        let result = UserConsentVerifier::RequestVerificationAsync(&message)
            .map_err(|e| Error::Backend(format!("Windows Hello unavailable: {e}")))?
            .get()
            .map_err(|e| Error::Backend(format!("Windows Hello request failed: {e}")))?;

        match result {
            UserConsentVerificationResult::Verified => Ok(()),
            UserConsentVerificationResult::Canceled => {
                Err(Error::AuthDenied("Windows Hello: cancelled".into()))
            }
            UserConsentVerificationResult::DeviceBusy => {
                Err(Error::AuthDenied("Windows Hello: device busy".into()))
            }
            UserConsentVerificationResult::RetriesExhausted => {
                Err(Error::AuthDenied("Windows Hello: too many attempts".into()))
            }
            UserConsentVerificationResult::DisabledByPolicy => {
                Err(Error::AuthDenied("Windows Hello: disabled by policy".into()))
            }
            UserConsentVerificationResult::NotConfiguredForUser => Err(Error::AuthDenied(
                "Windows Hello: not configured — set up in Settings first".into(),
            )),
            _ => Err(Error::AuthDenied("Windows Hello: auth failed".into())),
        }
    }

    fn is_available(&self) -> bool {
        Self::is_available()
    }
}

// ── Windows Credential Manager store ────────────────────────────────────────

pub struct WindowsCredentialStore;

impl SecretStore for WindowsCredentialStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        cred_write(&to_wide(&format!("pay.sh/{key}")), data)
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        cred_read(&to_wide(&format!("pay.sh/{key}")))
    }

    fn exists(&self, key: &str) -> bool {
        cred_exists(&to_wide(&format!("pay.sh/{key}")))
    }

    fn delete(&self, key: &str) -> Result<()> {
        cred_delete(&to_wide(&format!("pay.sh/{key}")))
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn cred_write(target: &[u16], blob: &[u8]) -> Result<()> {
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_ptr().cast_mut()),
        CredentialBlobSize: blob
            .len()
            .try_into()
            .map_err(|_| Error::Backend("credential blob too large".into()))?,
        CredentialBlob: blob.as_ptr().cast_mut(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }
        .map_err(|e| Error::Backend(format!("CredWriteW failed: {e}")))
}

fn cred_read(target: &[u16]) -> Result<Zeroizing<Vec<u8>>> {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr) }
        .map_err(|e| Error::Backend(format!("CredReadW failed: {e}")))?;

    let blob = unsafe {
        let c = &*ptr;
        slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
    };
    unsafe { CredFree(ptr.cast()) };
    Ok(Zeroizing::new(blob))
}

fn cred_exists(target: &[u16]) -> bool {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    let found =
        unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr).is_ok() };
    if found && !ptr.is_null() {
        unsafe { CredFree(ptr.cast()) };
    }
    found
}

fn cred_delete(target: &[u16]) -> Result<()> {
    unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) }
        .map_err(|e| Error::Backend(format!("CredDeleteW failed: {e}")))
}
