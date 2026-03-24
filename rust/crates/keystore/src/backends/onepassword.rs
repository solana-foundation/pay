//! 1Password backend — stores keypairs as Secure Notes via the `op` CLI.
//!
//! Items are stored as:
//!   Title:  `pay/<account>`  (e.g. `pay/default`)
//!   Vault:  configurable, defaults to the user's default vault
//!   Tags:   `pay`
//!   Fields:
//!     - `keypair` (concealed): hex-encoded 64-byte keypair
//!     - `pubkey`  (text):      hex-encoded 32-byte public key
//!
//! Auth is handled by 1Password itself — the `op` CLI triggers biometric
//! or password authentication as configured by the user.

use std::process::Command;

use crate::{Error, KeystoreBackend, Result, SyncMode};

const TAG: &str = "pay";

/// 1Password backend via the `op` CLI.
pub struct OnePassword {
    /// Vault name or ID. If `None`, uses 1Password's default vault.
    vault: Option<String>,
}

impl OnePassword {
    /// Create a new 1Password backend using the default vault.
    pub fn new() -> Self {
        Self { vault: None }
    }

    /// Create a new 1Password backend targeting a specific vault.
    pub fn with_vault(vault: impl Into<String>) -> Self {
        Self {
            vault: Some(vault.into()),
        }
    }

    /// Check if the `op` CLI is installed and the user is signed in.
    pub fn is_available() -> bool {
        Command::new("op")
            .args(["whoami", "--format=json"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn item_title(account: &str) -> String {
        format!("pay/{account}")
    }
}

impl Default for OnePassword {
    fn default() -> Self {
        Self::new()
    }
}

impl KeystoreBackend for OnePassword {
    fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        if keypair_bytes.len() != 64 {
            return Err(Error::InvalidKeypair(format!(
                "expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }

        let title = Self::item_title(account);
        let keypair_hex: String = keypair_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let pubkey_hex: String = keypair_bytes[32..64]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        // Delete existing item if present (op doesn't have upsert)
        if self.exists(account) {
            self.delete(account)?;
        }

        let mut cmd = Command::new("op");
        cmd.args([
            "item",
            "create",
            "--category=Secure Note",
            &format!("--title={title}"),
            &format!("--tags={TAG}"),
            &format!("keypair[concealed]={keypair_hex}"),
            &format!("pubkey[text]={pubkey_hex}"),
        ]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }

        let output = cmd.output().map_err(|e| {
            Error::Backend(format!(
                "Failed to run `op` CLI: {e}. Is 1Password CLI installed?"
            ))
        })?;

        if !output.status.success() {
            return Err(Error::Backend(format!(
                "op item create failed: {}",
                stderr_str(&output.stderr)
            )));
        }

        Ok(())
    }

    fn exists(&self, account: &str) -> bool {
        let title = Self::item_title(account);
        let mut cmd = Command::new("op");
        cmd.args(["item", "get", &title, "--format=json"]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }
        cmd.output().is_ok_and(|o| o.status.success())
    }

    fn delete(&self, account: &str) -> Result<()> {
        let title = Self::item_title(account);
        let mut cmd = Command::new("op");
        cmd.args(["item", "delete", &title]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }
        let output = cmd
            .output()
            .map_err(|e| Error::Backend(format!("op: {e}")))?;
        if !output.status.success() {
            let err = stderr_str(&output.stderr);
            if err.contains("not found") {
                return Ok(());
            }
            return Err(Error::Backend(format!("op item delete failed: {err}")));
        }
        Ok(())
    }

    fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        let title = Self::item_title(account);
        let mut cmd = Command::new("op");
        cmd.args(["item", "get", &title, "--fields=pubkey", "--reveal"]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }
        let output = cmd
            .output()
            .map_err(|e| Error::Backend(format!("op: {e}")))?;
        if !output.status.success() {
            return Err(Error::Backend(format!(
                "op item get failed: {}",
                stderr_str(&output.stderr)
            )));
        }
        let hex = String::from_utf8_lossy(&output.stdout).trim().to_string();
        hex_to_bytes(&hex)
    }

    fn load_keypair(&self, account: &str, _reason: &str) -> Result<Vec<u8>> {
        // 1Password handles its own auth (biometrics/password) via the `op` CLI.
        // The `reason` parameter is not used — 1Password shows its own prompt.
        let title = Self::item_title(account);
        let mut cmd = Command::new("op");
        cmd.args(["item", "get", &title, "--fields=keypair", "--reveal"]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }
        let output = cmd
            .output()
            .map_err(|e| Error::Backend(format!("op: {e}")))?;
        if !output.status.success() {
            let err = stderr_str(&output.stderr);
            if err.contains("authorization") || err.contains("denied") || err.contains("cancel") {
                return Err(Error::AuthDenied(err));
            }
            return Err(Error::Backend(format!("op item get failed: {err}")));
        }
        let hex = String::from_utf8_lossy(&output.stdout).trim().to_string();
        hex_to_bytes(&hex)
    }
}

fn stderr_str(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return Err(Error::InvalidKeypair("odd hex length".to_string()));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| Error::InvalidKeypair(format!("hex: {e}")))
        })
        .collect()
}
