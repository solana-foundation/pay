//! Windows: Windows Hello authentication + Credential Manager storage.

use crate::{AuthGate, AuthIntent, Error, Result, SecretStore, Zeroizing};
use std::cell::Cell;
use std::slice;
use windows::{
    Foundation::IAsyncOperation,
    Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    },
    Win32::{
        Foundation::HWND,
        Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
            CredReadW, CredWriteW,
        },
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx},
            Console::GetConsoleWindow,
            WinRT::IUserConsentVerifierInterop,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible},
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

// ── COM initialization ──────────────────────────────────────────────────────

thread_local! {
    static COM_INIT: Cell<bool> = const { Cell::new(false) };
}

fn ensure_com_init() -> Result<()> {
    // Audit #64: the previous version dropped the CoInitializeEx
    // result on the floor, so a genuine COM-init failure surfaced
    // later as a confusing WinRT call error. Propagating the
    // HRESULT here keeps the failure attached to its source.
    //
    // Returning Ok if the cell is already set means the per-thread
    // initialization stays one-shot, but the result of the first
    // call is the one that matters for fault diagnosis.
    COM_INIT.with(|cell| {
        if cell.get() {
            return Ok(());
        }
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| Error::Backend(format!("COM init failed: {e}")))?;
        cell.set(true);
        Ok(())
    })
}

// ── Windows Hello auth gate ─────────────────────────────────────────────────

pub struct WindowsHelloAuth;

impl WindowsHelloAuth {
    pub fn is_available() -> bool {
        // Audit #64: an init failure on the availability probe just
        // surfaces as "not available", which is the safer answer.
        if ensure_com_init().is_err() {
            return false;
        }
        UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| op.get())
            .map(|r| r == UserConsentVerifierAvailability::Available)
            .unwrap_or(false)
    }
}

impl AuthGate for WindowsHelloAuth {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        ensure_com_init()?;

        let message = windows_hello_reason_wrapper(&intent.prompt_message());
        let result = request_verification(&message)
            .map_err(|e| Error::Backend(format!("Windows Hello request failed: {e}")))?;

        // Audit #15: distinguish user-attempt outcomes (Verified /
        // Canceled / RetriesExhausted — `AuthDenied`, retryable by
        // the user) from device-state outcomes (DeviceBusy /
        // DisabledByPolicy / NotConfiguredForUser — `Backend`, not
        // retryable without operator / admin action). The previous
        // collapse to `AuthDenied` for everything led the higher-up
        // UX to nudge the user to "try again" even when the device
        // wasn't configured for biometrics at all.
        match result {
            UserConsentVerificationResult::Verified => Ok(()),
            UserConsentVerificationResult::Canceled => {
                Err(Error::AuthDenied("Windows Hello: cancelled".into()))
            }
            UserConsentVerificationResult::RetriesExhausted => {
                Err(Error::AuthDenied("Windows Hello: too many attempts".into()))
            }
            UserConsentVerificationResult::DeviceBusy => {
                Err(Error::Backend("Windows Hello: device busy".into()))
            }
            UserConsentVerificationResult::DisabledByPolicy => {
                Err(Error::Backend("Windows Hello: disabled by policy".into()))
            }
            UserConsentVerificationResult::NotConfiguredForUser => Err(Error::Backend(
                "Windows Hello: not configured — set up in Settings first".into(),
            )),
            _ => Err(Error::Backend(
                "Windows Hello: unexpected verification result".into(),
            )),
        }
    }

    fn is_available(&self) -> bool {
        Self::is_available()
    }
}

// Audit #65: desktop apps need the HWND-based interop API for the
// Windows Hello prompt — the plain
// `UserConsentVerifier::RequestVerificationAsync` is the UWP path
// and ignores parent-window context.
//
// Earlier code hand-rolled the `IUserConsentVerifierInterop` COM
// interface (vtable + IID + transmute_copy on HSTRING). The `windows`
// crate now ships the interop under `Win32::System::WinRT` when the
// `Win32_System_WinRT` feature is enabled, with a typed
// `RequestVerificationForWindowAsync` method. Using the upstream
// definition removes the manual transmutes and the per-call IID
// argument.
fn request_verification(message: &str) -> windows::core::Result<UserConsentVerificationResult> {
    let message = HSTRING::from(message);

    if let Some(hwnd) = prompt_parent_window()
        && let Ok(op) = request_verification_for_window_async(hwnd, &message)
    {
        return op.get();
    }

    UserConsentVerifier::RequestVerificationAsync(&message)?.get()
}

fn request_verification_for_window_async(
    hwnd: HWND,
    message: &HSTRING,
) -> windows::core::Result<IAsyncOperation<UserConsentVerificationResult>> {
    let interop: IUserConsentVerifierInterop =
        windows::core::factory::<UserConsentVerifier, IUserConsentVerifierInterop>()?;
    unsafe { interop.RequestVerificationForWindowAsync(hwnd, message) }
}

fn prompt_parent_window() -> Option<HWND> {
    let console = unsafe { GetConsoleWindow() };
    if !console.is_invalid() && unsafe { IsWindowVisible(console).0 != 0 } {
        return Some(console);
    }

    // Audit #66: `GetForegroundWindow()` returns whatever window has
    // keyboard focus at this instant — could be the user's browser,
    // another terminal, or any random app. Using it as a Pay parent
    // could attach our Touch-ID prompt to an unrelated window, which
    // is confusing at best and a UI-redress risk at worst. Only use
    // the foreground window if it belongs to our own process; falling
    // through to `None` triggers the UWP path
    // (`RequestVerificationAsync`), which renders the prompt without
    // a parent.
    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.is_invalid() {
        let mut foreground_pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
        if foreground_pid == std::process::id() {
            return Some(foreground);
        }
    }

    if !console.is_invalid() {
        return Some(console);
    }

    None
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
    // Audit #68: CREDENTIALW takes `*mut` pointers, but the caller
    // hands us `&[u16]` / `&[u8]` slices. Casting `as_ptr().cast_mut()`
    // tells Windows "you may write here" while the underlying storage
    // may be read-only (e.g. if a caller ever feeds in include_bytes!
    // or a static slice). Even if Windows doesn't actually write, the
    // compiler may miscompile around the &[u8] immutability assumption.
    // Copy into owned, mutable buffers up front so the pointers we
    // hand to Windows truly point at writable memory.
    //
    // Audit #42: the local blob copy holds key material until the
    // function returns. Wrap it in Zeroizing so the bytes are wiped
    // on drop (normal return *and* any unwind path), instead of
    // sitting in the freelist for whoever asks for that memory next.
    let mut target = target.to_vec();
    let mut blob = Zeroizing::new(blob.to_vec());
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob
            .len()
            .try_into()
            .map_err(|_| Error::Backend("credential blob too large".into()))?,
        CredentialBlob: blob.as_mut_ptr(),
        // CRED_PERSIST_LOCAL_MACHINE: credential is per-user, persists across
        // reboots, and is protected by DPAPI (user-scoped encryption).
        // It does NOT grant access to other users on the machine despite
        // the misleading name.
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| Error::Backend(format!("CredWriteW failed: {e}")))
}

/// RAII guard for a `*mut CREDENTIALW` returned by `CredReadW`.
///
/// Audit #69: the previous `cred_read` ran `to_vec()` between
/// `CredReadW` and `CredFree`. If `to_vec()` panicked (OOM, etc.)
/// the credential allocation leaked. The guard ensures `CredFree`
/// runs on every unwind path.
struct CredentialGuard(*mut CREDENTIALW);

impl CredentialGuard {
    /// Safety: `ptr` must be a Windows-allocated `CREDENTIALW` from
    /// `CredReadW` (or null, in which case the guard is a no-op).
    fn new(ptr: *mut CREDENTIALW) -> Self {
        Self(ptr)
    }

    fn as_ref(&self) -> Option<&CREDENTIALW> {
        if self.0.is_null() {
            None
        } else {
            // Safety: ptr was returned by Windows and lives until
            // CredFree runs (which only happens in Drop).
            Some(unsafe { &*self.0 })
        }
    }
}

impl Drop for CredentialGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CredFree(self.0.cast()) };
        }
    }
}

fn cred_read(target: &[u16]) -> Result<Zeroizing<Vec<u8>>> {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr) }
        .map_err(|e| Error::Backend(format!("CredReadW failed: {e}")))?;

    // Audit #69 hardening:
    //   - null-check the returned pointer (CredReadW can in theory
    //     return Ok with a null out-pointer; we refuse to dereference
    //     it);
    //   - guard the pointer in CredentialGuard so CredFree always
    //     runs, even if a downstream allocation panics;
    //   - validate CredentialBlobSize as usize without truncation
    //     (the field is DWORD = u32; cast is widening on 64-bit and
    //     identity on 32-bit, but we still reject the "blob pointer
    //     is null but size > 0" mismatch);
    //   - require non-null CredentialBlob before slicing.
    let guard = CredentialGuard::new(ptr);
    let cred = guard
        .as_ref()
        .ok_or_else(|| Error::Backend("CredReadW returned success with null pointer".into()))?;

    let size = cred.CredentialBlobSize as usize;
    // Audit #42: wrap the intermediate copy in Zeroizing the moment
    // it's allocated, not just at the function's return site. If the
    // function unwinds between `to_vec()` and the wrap, the bytes
    // would otherwise linger in the freelist.
    let blob: Zeroizing<Vec<u8>> = if size == 0 {
        Zeroizing::new(Vec::new())
    } else if cred.CredentialBlob.is_null() {
        return Err(Error::Backend(
            "CredReadW returned non-zero size with null blob pointer".into(),
        ));
    } else {
        // Safety: cred.CredentialBlob is non-null (just checked),
        // points to `size` initialized bytes owned by Windows until
        // guard is dropped, and is `*const u8`-aligned (u8 has
        // alignment 1, so no alignment check is needed).
        Zeroizing::new(unsafe { slice::from_raw_parts(cred.CredentialBlob, size) }.to_vec())
    };
    drop(guard);
    Ok(blob)
}

fn cred_exists(target: &[u16]) -> bool {
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    let found =
        unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr).is_ok() };
    // Audit #69: route through CredentialGuard so we don't have to
    // remember to call CredFree by hand on every branch.
    let _ = CredentialGuard::new(ptr);
    found
}

/// HRESULT for `ERROR_NOT_FOUND` returned by `CredDeleteW` when the
/// target credential is absent. `0x80070490` = `HRESULT_FROM_WIN32(1168)`.
const HRESULT_ERROR_NOT_FOUND: i32 = 0x8007_0490u32 as i32;

fn cred_delete(target: &[u16]) -> Result<()> {
    // Audit #18: treat ERROR_NOT_FOUND as success so delete is
    // idempotent across backends. macOS Keychain and Linux Secret
    // Service already return Ok on a missing item; without this,
    // Windows differs and breaks the shared
    // `Keystore::delete_with_intent` rollback semantics
    // (specifically audit #12, which now propagates the pubkey delete
    // result — a missing pubkey record would otherwise spuriously
    // fail account cleanup on Windows).
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
        Ok(()) => Ok(()),
        Err(e) if e.code().0 == HRESULT_ERROR_NOT_FOUND => Ok(()),
        Err(e) => Err(Error::Backend(format!("CredDeleteW failed: {e}"))),
    }
}

/// The canonical prompt message is a sentence fragment so macOS can render it
/// after "<app> is trying to ". Windows Hello shows the reason verbatim, so
/// wrap the fragment with the same "pay.sh is trying to" prefix and a trailing
/// period to keep the wording aligned across platforms.
fn windows_hello_reason_wrapper(message: &str) -> String {
    let trimmed = message.trim_end_matches('.').trim();
    if trimmed.is_empty() {
        return "pay.sh is trying to authenticate.".to_string();
    }
    format!("pay.sh is trying to {trimmed}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_prefixes_default_reason() {
        assert_eq!(
            windows_hello_reason_wrapper(&AuthIntent::default_payment().prompt_message()),
            "pay.sh is trying to authorize a payment with pay."
        );
    }

    #[test]
    fn wrapper_prefixes_specific_payment_reason() {
        assert_eq!(
            windows_hello_reason_wrapper(
                &AuthIntent::authorize_payment("$0.05", "accessing API api.example.com")
                    .prompt_message()
            ),
            "pay.sh is trying to authorize payment of $0.05 for accessing API api.example.com."
        );
    }

    #[test]
    fn wrapper_trims_whitespace_and_terminates() {
        assert_eq!(
            windows_hello_reason_wrapper(
                &AuthIntent::from_reason("  delete default account  ").prompt_message()
            ),
            "pay.sh is trying to delete default account."
        );
    }

    #[test]
    fn wrapper_falls_back_for_empty_reason() {
        assert_eq!(
            windows_hello_reason_wrapper(&AuthIntent::from_reason("   ").prompt_message()),
            "pay.sh is trying to authorize pay to use your payment account."
        );
    }

    #[test]
    fn prompt_message_bounds_long_reasons() {
        let long = "a".repeat(221);
        let message = AuthIntent::from_reason(&long).prompt_message();

        assert!(message.ends_with("..."));
        assert!(message.len() < 230);
    }
}
