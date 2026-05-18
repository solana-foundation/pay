//! Device-owner authentication via LocalAuthentication.framework.
//!
//! Calls `LAContext.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, …)`
//! over an `objc2` binding. Each call creates a fresh `LAContext` so we
//! never reuse a cached evaluation result.

use crate::{Error, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAPolicy};
use std::sync::mpsc::sync_channel;

pub fn evaluate(reason: &str) -> Result<()> {
    let ctx: Retained<LAContext> = unsafe { LAContext::new() };
    let reason = NSString::from_str(reason);

    // LA invokes the reply block exactly once from its internal dispatch
    // queue. The one-slot channel blocks the caller until then.
    let (tx, rx) = sync_channel::<core::result::Result<(), Retained<NSError>>>(1);
    let block = RcBlock::new(move |success: Bool, error: *mut NSError| {
        let outcome = if success.as_bool() {
            Ok(())
        } else {
            // SAFETY: when `success` is `false`, LA guarantees the error
            // pointer is a non-null, valid, autoreleased `NSError`.
            // `Retained::retain_autoreleased` bumps the refcount so the
            // value outlives LA's autoreleasepool.
            let retained = unsafe { Retained::retain(error) }
                .unwrap_or_else(|| panic!("LAContext reply: NSError unexpectedly null"));
            Err(retained)
        };
        // Receiver may have hung up if the calling thread was cancelled
        // — best effort, never panic from inside a reply block.
        let _ = tx.send(outcome);
    });

    // SAFETY: `ctx` is a valid LAContext; `reason` is a valid NSString;
    // `block` is heap-allocated and retained by LA for the lifetime of
    // the pending evaluation.
    unsafe {
        ctx.evaluatePolicy_localizedReason_reply(
            LAPolicy::DeviceOwnerAuthenticationWithBiometrics,
            &reason,
            &block,
        );
    }

    let outcome = rx
        .recv()
        .map_err(|_| Error::Backend("LAContext reply channel closed".to_string()))?;

    match outcome {
        Ok(()) => Ok(()),
        Err(error) => Err(classify(&error)),
    }
}

pub fn can_evaluate() -> bool {
    let ctx: Retained<LAContext> = unsafe { LAContext::new() };
    // The binding hides the BOOL + `NSError**` out-param shape and
    // returns `Result<(), Retained<NSError>>`. We don't care which
    // `LAError` code blocked us — only whether biometric is usable
    // right now — so just check Ok.
    // SAFETY: `ctx` is a valid LAContext.
    unsafe { ctx.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthenticationWithBiometrics) }.is_ok()
}

fn classify(error: &NSError) -> Error {
    let code = error.code();
    let msg = error.localizedDescription().to_string();
    match classify_code(code) {
        Disposition::Denied => Error::AuthDenied(msg),
        Disposition::Backend => Error::Backend(msg),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    Denied,
    Backend,
}

/// Bucket an `LAError` code into the keystore's two-class error model.
///
/// Replaces the previous "search the localised description for the
/// substring `cancel`" heuristic (audit #13): localised strings change
/// per-OS-version and per-locale, so we ground every classification in
/// the documented `LAError` codes instead.
///
/// `LAError` (LocalAuthentication.framework):
/// ```text
///   -1  authenticationFailed   user attempted but failed (exhausted retries)
///   -2  userCancel             user pressed Cancel
///   -3  userFallback           user tapped "Use Password…"
///   -4  systemCancel           system canceled (app to background, etc.)
///   -5  passcodeNotSet         device has no passcode set
///   -6  biometryNotAvailable   no Touch ID / Face ID hardware
///   -7  biometryNotEnrolled    biometric not configured for this user
///   -8  biometryLockout        too many failures; needs passcode unlock
///   -9  appCancel              programmatic `invalidate()` cancel
///  -10  invalidContext         context was invalidated
/// ```
///
/// Anything in the user-attempted-or-cancelled range maps to
/// `AuthDenied`; device-state and programmer-error codes map to
/// `Backend` so the calling layer can show device-setup guidance.
fn classify_code(code: isize) -> Disposition {
    match code {
        -1 | -2 | -3 | -4 | -9 => Disposition::Denied,
        _ => Disposition::Backend,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_attempted_or_cancelled_codes_map_to_denied() {
        for code in [-1, -2, -3, -4, -9] {
            assert_eq!(
                classify_code(code),
                Disposition::Denied,
                "LAError code {code} must classify as Denied",
            );
        }
    }

    #[test]
    fn device_state_codes_map_to_backend() {
        for code in [-5, -6, -7, -8, -10] {
            assert_eq!(
                classify_code(code),
                Disposition::Backend,
                "LAError code {code} must classify as Backend",
            );
        }
    }

    #[test]
    fn unknown_codes_map_to_backend() {
        for code in [0, -100, isize::MIN, isize::MAX] {
            assert_eq!(classify_code(code), Disposition::Backend);
        }
    }
}
