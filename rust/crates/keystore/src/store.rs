//! Secret storage backends — where encrypted bytes are persisted.

use crate::{Error, Result, Zeroizing};

/// Where secrets are stored. Each impl handles raw byte storage only — no auth.
pub trait SecretStore: Send + Sync {
    fn store(&self, key: &str, data: &[u8]) -> Result<()>;
    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>>;
    fn exists(&self, key: &str) -> bool;
    fn delete(&self, key: &str) -> Result<()>;
}

// ── In-memory store (testing) ───────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Mutex;

/// In-memory secret store for testing. Not persistent.
pub struct InMemoryStore {
    data: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for InMemoryStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        self.data
            .lock()
            .unwrap()
            .insert(key.to_string(), data.to_vec());
        Ok(())
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        self.data
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .map(Zeroizing::new)
            .ok_or_else(|| Error::Backend(format!("key not found: {key}")))
    }

    fn exists(&self, key: &str) -> bool {
        self.data.lock().unwrap().contains_key(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.data.lock().unwrap().remove(key);
        Ok(())
    }
}

// ── Shared hex helpers ──────────────────────────────────────────────────────

pub fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(hex: &str) -> Result<Vec<u8>> {
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

// ── 1Password via `op` CLI (cross-platform) ─────────────────────────────────

use std::process::Command;

const OP_TAG: &str = "pay";

/// 1Password storage via the `op` CLI. Auth is handled by `op` internally.
pub struct OnePasswordStore {
    vault: Option<String>,
}

impl OnePasswordStore {
    pub fn new() -> Self {
        Self { vault: None }
    }

    pub fn with_vault(vault: impl Into<String>) -> Self {
        Self {
            vault: Some(vault.into()),
        }
    }

    pub fn is_available() -> bool {
        Command::new("op")
            .args(["whoami", "--format=json"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn item_title(key: &str) -> String {
        format!("pay/{key}")
    }
}

impl Default for OnePasswordStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for OnePasswordStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let title = Self::item_title(key);
        let hex = hex_encode(data);

        if self.exists(key) {
            self.delete(key)?;
        }

        let mut cmd = Command::new("op");
        cmd.args([
            "item",
            "create",
            "--category=Secure Note",
            &format!("--title={title}"),
            &format!("--tags={OP_TAG}"),
            &format!("data[concealed]={hex}"),
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

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        let title = Self::item_title(key);
        let mut cmd = Command::new("op");
        cmd.args(["item", "get", &title, "--fields=data", "--reveal"]);
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
        hex_decode(&hex).map(Zeroizing::new)
    }

    fn exists(&self, key: &str) -> bool {
        let title = Self::item_title(key);
        let mut cmd = Command::new("op");
        cmd.args(["item", "get", &title, "--format=json"]);
        if let Some(vault) = &self.vault {
            cmd.arg(format!("--vault={vault}"));
        }
        cmd.output().is_ok_and(|o| o.status.success())
    }

    fn delete(&self, key: &str) -> Result<()> {
        let title = Self::item_title(key);
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
}

fn stderr_str(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}
