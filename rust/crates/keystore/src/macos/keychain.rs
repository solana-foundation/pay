//! Apple Keychain generic-password operations via `SecItem*` FFI.
//!
//! Items are stored under `kSecClassGenericPassword` with
//! `kSecAttrService = "pay.sh"`, account = the caller-supplied key, and
//! `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` — never synced to
//! iCloud, never readable while the Mac is locked.

use crate::{Error, Result, Zeroizing};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;
use security_framework_sys::base::{errSecDuplicateItem, errSecItemNotFound, errSecSuccess};
use security_framework_sys::item::{
    kSecAttrAccount, kSecAttrService, kSecClass, kSecClassGenericPassword, kSecReturnData,
    kSecValueData,
};
use security_framework_sys::keychain_item::{
    SecItemAdd, SecItemCopyMatching, SecItemDelete, SecItemUpdate,
};

// `security-framework-sys` v2 exports the `kSecAttrAccessible` *values*
// in `access_control` but omits the dictionary *key* itself. Declaring
// the extern by hand keeps the rest of the call sites tidy and avoids
// pulling in the higher-level `security-framework` wrapper just for one
// constant. Security.framework is linked transitively via
// security-framework-sys, so no `#[link]` attribute is required here.
unsafe extern "C" {
    static kSecAttrAccessible: CFStringRef;
}

const SERVICE: &str = "pay.sh";

pub fn store(account: &str, data: &[u8]) -> Result<()> {
    let mut pairs = base_query(account);
    pairs.push(pair(
        unsafe { kSecValueData },
        CFData::from_buffer(data).as_CFType(),
    ));
    pairs.push(pair(
        unsafe { kSecAttrAccessible },
        k_str(unsafe { kSecAttrAccessibleWhenUnlockedThisDeviceOnly }),
    ));
    let dict = CFDictionary::from_CFType_pairs(&pairs);

    // SAFETY: `dict` is a valid CFDictionary owned by this stack frame
    // and lives for the duration of the call. The null result pointer
    // signals that we don't want any return value.
    let status = unsafe { SecItemAdd(dict.as_concrete_TypeRef(), std::ptr::null_mut()) };

    if status == errSecDuplicateItem {
        // Item already exists for this (service, account). Replace the
        // value in-place rather than delete-then-add — keeps the
        // account record durably present even if the second call fails.
        return update(account, data);
    }

    osstatus_to_result("SecItemAdd", status)
}

fn update(account: &str, data: &[u8]) -> Result<()> {
    let query = CFDictionary::from_CFType_pairs(&base_query(account));
    let updates = CFDictionary::from_CFType_pairs(&[pair(
        unsafe { kSecValueData },
        CFData::from_buffer(data).as_CFType(),
    )]);

    // SAFETY: both dicts are valid CFDictionaries with disjoint
    // ownership; SecItemUpdate copies whatever it needs.
    let status =
        unsafe { SecItemUpdate(query.as_concrete_TypeRef(), updates.as_concrete_TypeRef()) };
    osstatus_to_result("SecItemUpdate", status)
}

pub fn load(account: &str) -> Result<Zeroizing<Vec<u8>>> {
    let mut pairs = base_query(account);
    // SecItemCopyMatching defaults to returning a single match; we
    // don't set `kSecMatchLimit` because security-framework-sys does
    // not export `kSecMatchLimitOne` (and the default already covers
    // the single-item case).
    pairs.push(pair(
        unsafe { kSecReturnData },
        CFBoolean::true_value().as_CFType(),
    ));
    let dict = CFDictionary::from_CFType_pairs(&pairs);

    let mut result: CFTypeRef = std::ptr::null();
    // SAFETY: `dict` is valid for the duration of the call; `result` is
    // a valid out-param that SecItemCopyMatching writes a `+1` CFData
    // reference into on success.
    let status = unsafe { SecItemCopyMatching(dict.as_concrete_TypeRef(), &mut result) };

    if status != errSecSuccess {
        return Err(Error::Backend(format!(
            "SecItemCopyMatching failed: {}",
            describe_status(status)
        )));
    }
    if result.is_null() {
        return Err(Error::Backend(
            "SecItemCopyMatching returned success without data".to_string(),
        ));
    }

    // SAFETY: `result` is a `+1` CFData ref returned by a successful
    // SecItemCopyMatching with `kSecReturnData = true`. `wrap_under_create_rule`
    // takes ownership of the reference and releases it on drop.
    let data = unsafe { CFData::wrap_under_create_rule(result.cast()) };
    Ok(Zeroizing::new(data.bytes().to_vec()))
}

pub fn exists(account: &str) -> bool {
    let dict = CFDictionary::from_CFType_pairs(&base_query(account));
    // SAFETY: `dict` is valid. Passing a null result pointer means no
    // value is requested — SecItemCopyMatching only reports whether a
    // match exists. Our items use `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`,
    // which does not require interactive authentication, so this call
    // never triggers a Touch ID prompt.
    let status = unsafe { SecItemCopyMatching(dict.as_concrete_TypeRef(), std::ptr::null_mut()) };
    status == errSecSuccess
}

pub fn delete(account: &str) -> Result<()> {
    let dict = CFDictionary::from_CFType_pairs(&base_query(account));
    // SAFETY: `dict` is valid for the duration of the call.
    let status = unsafe { SecItemDelete(dict.as_concrete_TypeRef()) };
    if status == errSecSuccess || status == errSecItemNotFound {
        Ok(())
    } else {
        Err(Error::Backend(format!(
            "SecItemDelete failed: {}",
            describe_status(status)
        )))
    }
}

fn base_query(account: &str) -> Vec<(CFString, CFType)> {
    vec![
        pair(
            unsafe { kSecClass },
            k_str(unsafe { kSecClassGenericPassword }),
        ),
        pair(
            unsafe { kSecAttrService },
            CFString::new(SERVICE).as_CFType(),
        ),
        pair(
            unsafe { kSecAttrAccount },
            CFString::new(account).as_CFType(),
        ),
    ]
}

/// Wrap a static `kSec*` constant as a borrowed `CFString`, then erase
/// to `CFType` for use as a dictionary value.
///
/// SAFETY: the `kSec*` constants are static `CFStringRef` pointers
/// initialised by dyld when Security.framework loads. They live for the
/// process lifetime; reading them is safe under any execution model.
fn k_str(reference: CFStringRef) -> CFType {
    unsafe { CFString::wrap_under_get_rule(reference) }.as_CFType()
}

/// Build a `(CFString, CFType)` pair from a static framework constant
/// and an owned value. See [`k_str`] for the SAFETY rationale.
fn pair(key_ref: CFStringRef, value: CFType) -> (CFString, CFType) {
    let key = unsafe { CFString::wrap_under_get_rule(key_ref) };
    (key, value)
}

fn osstatus_to_result(operation: &str, status: i32) -> Result<()> {
    if status == errSecSuccess {
        Ok(())
    } else {
        Err(Error::Backend(format!(
            "{operation} failed: {}",
            describe_status(status)
        )))
    }
}

/// Map common Security-framework `OSStatus` codes to readable strings.
///
/// We deliberately avoid `SecCopyErrorMessageString` here — it returns a
/// `+1` CFString that would need its own release dance for a single log
/// line. The handful of codes below cover everything callers act on; any
/// other status falls through to its numeric form for debugging.
fn describe_status(status: i32) -> String {
    match status {
        -25291 => "Keychain not available (errSecNotAvailable)".to_string(),
        -25292 => "Keychain is read-only (errSecReadOnly)".to_string(),
        -25293 => "Keychain authentication failed (errSecAuthFailed)".to_string(),
        -25299 => "Keychain item already exists (errSecDuplicateItem)".to_string(),
        -25300 => "Keychain item not found (errSecItemNotFound)".to_string(),
        -25308 => "Keychain interaction not allowed (errSecInteractionNotAllowed)".to_string(),
        other => format!("OSStatus {other}"),
    }
}
