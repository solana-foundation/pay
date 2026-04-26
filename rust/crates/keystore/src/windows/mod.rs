//! Windows: Windows Hello authentication + Credential Manager storage.

use crate::{AuthGate, AuthIntent, Error, Result, SecretStore, Zeroizing};
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::{MaybeUninit, transmute, transmute_copy};
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
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible},
    },
    core::{
        GUID, HRESULT, HSTRING, IInspectable, IInspectable_Vtbl, Interface, PCWSTR, PWSTR, Type,
    },
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
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        ensure_com_init();

        let message = windows_hello_reason_wrapper(&intent.prompt_message());
        let result = request_verification(&message)
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
            UserConsentVerificationResult::DisabledByPolicy => Err(Error::AuthDenied(
                "Windows Hello: disabled by policy".into(),
            )),
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

// Desktop apps need the HWND-based interop API for Windows Hello prompts. The
// plain UserConsentVerifier::RequestVerificationAsync API is the UWP path.
windows::core::imp::define_interface!(
    IUserConsentVerifierInterop,
    IUserConsentVerifierInteropVtbl,
    0x39e050c3_4e74_441a_8dc0_b81104df949c
);

impl core::ops::Deref for IUserConsentVerifierInterop {
    type Target = IInspectable;

    fn deref(&self) -> &Self::Target {
        unsafe { transmute(self) }
    }
}

windows::core::imp::interface_hierarchy!(
    IUserConsentVerifierInterop,
    windows::core::IUnknown,
    IInspectable
);

#[repr(C)]
pub struct IUserConsentVerifierInteropVtbl {
    pub base__: IInspectable_Vtbl,
    pub request_verification_for_window_async: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        MaybeUninit<HSTRING>,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
}

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
    let interop = windows::core::factory::<UserConsentVerifier, IUserConsentVerifierInterop>()?;

    unsafe {
        let mut result__ = core::mem::zeroed();
        (Interface::vtable(&interop).request_verification_for_window_async)(
            Interface::as_raw(&interop),
            hwnd,
            transmute_copy(message),
            &<IAsyncOperation<UserConsentVerificationResult> as Interface>::IID,
            &mut result__,
        )
        .and_then(|| Type::from_abi(result__))
    }
}

fn prompt_parent_window() -> Option<HWND> {
    let console = unsafe { GetConsoleWindow() };
    if !console.is_invalid() && unsafe { IsWindowVisible(console).0 != 0 } {
        return Some(console);
    }

    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.is_invalid() {
        return Some(foreground);
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
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_ptr().cast_mut()),
        CredentialBlobSize: blob
            .len()
            .try_into()
            .map_err(|_| Error::Backend("credential blob too large".into()))?,
        CredentialBlob: blob.as_ptr().cast_mut(),
        // CRED_PERSIST_LOCAL_MACHINE: credential is per-user, persists across
        // reboots, and is protected by DPAPI (user-scoped encryption).
        // It does NOT grant access to other users on the machine despite
        // the misleading name.
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| Error::Backend(format!("CredWriteW failed: {e}")))
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
