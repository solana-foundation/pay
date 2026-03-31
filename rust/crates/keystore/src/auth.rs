//! Authentication gates — biometric or password prompts before secret access.

use crate::Result;

/// How the user proves identity before accessing secrets.
pub trait AuthGate: Send + Sync {
    /// Prompt the user to authenticate. Returns `Ok(())` on success.
    fn authenticate(&self, reason: &str) -> Result<()>;

    /// Check if this auth mechanism is available on the current device.
    fn is_available(&self) -> bool;
}

/// No authentication — always succeeds. Used for testing and backends
/// where auth is handled externally (e.g. 1Password's `op` CLI).
pub struct NoAuth;

impl AuthGate for NoAuth {
    fn authenticate(&self, _reason: &str) -> Result<()> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}
