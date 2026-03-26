//! Account registry — `~/.config/pay/accounts.yml`.
//!
//! Tracks which accounts exist and where their keys are stored.
//!
//! ```yaml
//! default_account: default
//!
//! accounts:
//!   default:
//!     keystore: apple-keychain
//!     pubkey: 7xKX...abc
//!   work:
//!     keystore: 1password
//!     vault: Work
//!     pubkey: 9yLM...def
//!   legacy:
//!     keystore: file
//!     path: ~/.config/solana/id.json
//!     pubkey: 3zNP...ghi
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const ACCOUNTS_FILE: &str = "~/.config/pay/accounts.yml";

/// Which keystore backend holds the secret key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Keystore {
    AppleKeychain,
    OnePassword,
    File,
}

impl std::fmt::Display for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Keystore::AppleKeychain => write!(f, "apple-keychain"),
            Keystore::OnePassword => write!(f, "1password"),
            Keystore::File => write!(f, "file"),
        }
    }
}

/// A single account entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Which keystore backend stores the secret key.
    pub keystore: Keystore,

    /// Base-58 public key (cached for display without auth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubkey: Option<String>,

    /// 1Password vault name (only for `keystore: 1password`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,

    /// File path (only for `keystore: file`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Account {
    /// Build the signer source string used by `pay_core::signer::load_signer`.
    pub fn signer_source(&self, name: &str) -> String {
        match self.keystore {
            Keystore::AppleKeychain => format!("keychain:{name}"),
            Keystore::OnePassword => format!("1password:{name}"),
            Keystore::File => self
                .path
                .clone()
                .unwrap_or_else(|| format!("~/.config/pay/{name}.json")),
        }
    }
}

/// The top-level accounts file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountsFile {
    /// Name of the default account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_account: Option<String>,

    /// All registered accounts.
    #[serde(default)]
    pub accounts: BTreeMap<String, Account>,
}

impl AccountsFile {
    /// Load from `~/.config/pay/accounts.yml`, or return empty if it doesn't exist.
    pub fn load() -> crate::Result<Self> {
        let path = accounts_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| crate::Error::Config(format!("Failed to read {}: {e}", path.display())))?;
        serde_yml::from_str(&contents)
            .map_err(|e| crate::Error::Config(format!("Invalid accounts.yml: {e}")))
    }

    /// Save to `~/.config/pay/accounts.yml` with restricted permissions.
    pub fn save(&self) -> crate::Result<()> {
        let path = accounts_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::Error::Config(format!("Failed to create dir: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
            }
        }
        let yaml = serde_yml::to_string(self)
            .map_err(|e| crate::Error::Config(format!("YAML error: {e}")))?;

        write_private(&path, yaml.as_bytes())
            .map_err(|e| crate::Error::Config(format!("Failed to write {}: {e}", path.display())))
    }

    /// Get the default account, if one is set and exists.
    pub fn default_account(&self) -> Option<(&str, &Account)> {
        let name = self.default_account.as_deref().or(Some("default"))?;
        self.accounts.get(name).map(|a| (name, a))
    }

    /// Add or update an account. Sets it as default if it's the first one.
    pub fn upsert(&mut self, name: &str, account: Account) {
        if self.accounts.is_empty() || self.default_account.is_none() {
            self.default_account = Some(name.to_string());
        }
        self.accounts.insert(name.to_string(), account);
    }

    /// Remove an account. Clears default if it was the removed one.
    pub fn remove(&mut self, name: &str) -> Option<Account> {
        let removed = self.accounts.remove(name);
        if self.default_account.as_deref() == Some(name) {
            self.default_account = self.accounts.keys().next().cloned();
        }
        removed
    }
}

fn accounts_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde(ACCOUNTS_FILE).into_owned())
}

/// Write data to a file with `0600` permissions (owner-only).
fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data)
    }

    #[cfg(not(unix))]
    std::fs::write(path, data)
}
