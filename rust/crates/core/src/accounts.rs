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
    GnomeKeyring,
    WindowsHello,
    OnePassword,
    File,
}

impl std::fmt::Display for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Keystore::AppleKeychain => write!(f, "apple-keychain"),
            Keystore::GnomeKeyring => write!(f, "gnome-keyring"),
            Keystore::WindowsHello => write!(f, "windows-hello"),
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
            Keystore::GnomeKeyring => format!("gnome-keyring:{name}"),
            Keystore::WindowsHello => format!("windows-hello:{name}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystore_display() {
        assert_eq!(Keystore::AppleKeychain.to_string(), "apple-keychain");
        assert_eq!(Keystore::GnomeKeyring.to_string(), "gnome-keyring");
        assert_eq!(Keystore::WindowsHello.to_string(), "windows-hello");
        assert_eq!(Keystore::OnePassword.to_string(), "1password");
        assert_eq!(Keystore::File.to_string(), "file");
    }

    #[test]
    fn keystore_serde_roundtrip() {
        for ks in [
            Keystore::AppleKeychain,
            Keystore::GnomeKeyring,
            Keystore::WindowsHello,
            Keystore::OnePassword,
            Keystore::File,
        ] {
            let json = serde_json::to_string(&ks).unwrap();
            let back: Keystore = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ks);
        }
    }

    #[test]
    fn signer_source_keychain() {
        let account = Account {
            keystore: Keystore::AppleKeychain,
            pubkey: None,
            vault: None,
            path: None,
        };
        assert_eq!(account.signer_source("default"), "keychain:default");
    }

    #[test]
    fn signer_source_gnome_keyring() {
        let account = Account {
            keystore: Keystore::GnomeKeyring,
            pubkey: None,
            vault: None,
            path: None,
        };
        assert_eq!(account.signer_source("mykey"), "gnome-keyring:mykey");
    }

    #[test]
    fn signer_source_windows_hello() {
        let account = Account {
            keystore: Keystore::WindowsHello,
            pubkey: None,
            vault: None,
            path: None,
        };
        assert_eq!(account.signer_source("work"), "windows-hello:work");
    }

    #[test]
    fn signer_source_onepassword() {
        let account = Account {
            keystore: Keystore::OnePassword,
            pubkey: None,
            vault: Some("Work".to_string()),
            path: None,
        };
        assert_eq!(account.signer_source("work"), "1password:work");
    }

    #[test]
    fn signer_source_file_with_path() {
        let account = Account {
            keystore: Keystore::File,
            pubkey: None,
            vault: None,
            path: Some("/home/user/.config/solana/id.json".to_string()),
        };
        assert_eq!(
            account.signer_source("legacy"),
            "/home/user/.config/solana/id.json"
        );
    }

    #[test]
    fn signer_source_file_default_path() {
        let account = Account {
            keystore: Keystore::File,
            pubkey: None,
            vault: None,
            path: None,
        };
        assert_eq!(
            account.signer_source("myaccount"),
            "~/.config/pay/myaccount.json"
        );
    }

    #[test]
    fn accounts_file_default_is_empty() {
        let af = AccountsFile::default();
        assert!(af.accounts.is_empty());
        assert!(af.default_account.is_none());
    }

    #[test]
    fn default_account_returns_none_when_empty() {
        let af = AccountsFile::default();
        // "default" key doesn't exist, so returns None
        assert!(af.default_account().is_none());
    }

    #[test]
    fn default_account_returns_explicit_default() {
        let mut af = AccountsFile {
            default_account: Some("work".to_string()),
            ..Default::default()
        };
        af.accounts.insert(
            "work".to_string(),
            Account {
                keystore: Keystore::OnePassword,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        let (name, _acct) = af.default_account().unwrap();
        assert_eq!(name, "work");
    }

    #[test]
    fn default_account_falls_back_to_default_name() {
        let mut af = AccountsFile::default();
        // No explicit default_account set, falls back to "default"
        af.accounts.insert(
            "default".to_string(),
            Account {
                keystore: Keystore::AppleKeychain,
                pubkey: Some("abc123".to_string()),
                vault: None,
                path: None,
            },
        );
        let (name, acct) = af.default_account().unwrap();
        assert_eq!(name, "default");
        assert_eq!(acct.pubkey.as_deref(), Some("abc123"));
    }

    #[test]
    fn upsert_sets_default_on_first_insert() {
        let mut af = AccountsFile::default();
        af.upsert(
            "first",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        assert_eq!(af.default_account.as_deref(), Some("first"));
    }

    #[test]
    fn upsert_does_not_change_existing_default() {
        let mut af = AccountsFile::default();
        af.upsert(
            "first",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        af.upsert(
            "second",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        assert_eq!(af.default_account.as_deref(), Some("first"));
        assert_eq!(af.accounts.len(), 2);
    }

    #[test]
    fn remove_clears_default_and_picks_next() {
        let mut af = AccountsFile::default();
        af.upsert(
            "alpha",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        af.upsert(
            "beta",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        assert_eq!(af.default_account.as_deref(), Some("alpha"));

        let removed = af.remove("alpha");
        assert!(removed.is_some());
        // Should pick next available
        assert_eq!(af.default_account.as_deref(), Some("beta"));
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut af = AccountsFile::default();
        assert!(af.remove("nonexistent").is_none());
    }

    #[test]
    fn remove_last_account_clears_default() {
        let mut af = AccountsFile::default();
        af.upsert(
            "only",
            Account {
                keystore: Keystore::File,
                pubkey: None,
                vault: None,
                path: None,
            },
        );
        af.remove("only");
        assert!(af.default_account.is_none());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("accounts.yml");

        let mut af = AccountsFile::default();
        af.upsert(
            "test",
            Account {
                keystore: Keystore::File,
                pubkey: Some("pubkey123".to_string()),
                vault: None,
                path: Some("/tmp/key.json".to_string()),
            },
        );

        let yaml = serde_yml::to_string(&af).unwrap();
        std::fs::write(&path, &yaml).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let loaded: AccountsFile = serde_yml::from_str(&contents).unwrap();

        assert_eq!(loaded.default_account.as_deref(), Some("test"));
        assert_eq!(loaded.accounts.len(), 1);
        let acct = loaded.accounts.get("test").unwrap();
        assert_eq!(acct.keystore, Keystore::File);
        assert_eq!(acct.pubkey.as_deref(), Some("pubkey123"));
    }

    #[test]
    fn yaml_skip_serializing_none_fields() {
        let account = Account {
            keystore: Keystore::AppleKeychain,
            pubkey: None,
            vault: None,
            path: None,
        };
        let yaml = serde_yml::to_string(&account).unwrap();
        // None fields should be omitted
        assert!(!yaml.contains("pubkey"));
        assert!(!yaml.contains("vault"));
        assert!(!yaml.contains("path"));
    }

    #[test]
    fn yaml_includes_present_fields() {
        let account = Account {
            keystore: Keystore::OnePassword,
            pubkey: Some("abc123".to_string()),
            vault: Some("Work".to_string()),
            path: None,
        };
        let yaml = serde_yml::to_string(&account).unwrap();
        assert!(yaml.contains("pubkey"));
        assert!(yaml.contains("vault"));
        assert!(!yaml.contains("path"));
    }

    #[test]
    fn accounts_file_multi_account_yaml() {
        let mut af = AccountsFile::default();
        af.upsert(
            "default",
            Account {
                keystore: Keystore::AppleKeychain,
                pubkey: Some("pk1".to_string()),
                vault: None,
                path: None,
            },
        );
        af.upsert(
            "work",
            Account {
                keystore: Keystore::OnePassword,
                pubkey: Some("pk2".to_string()),
                vault: Some("Work".to_string()),
                path: None,
            },
        );

        let yaml = serde_yml::to_string(&af).unwrap();
        let loaded: AccountsFile = serde_yml::from_str(&yaml).unwrap();

        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.default_account.as_deref(), Some("default"));
        assert_eq!(
            loaded.accounts["work"].vault.as_deref(),
            Some("Work")
        );
    }

    #[test]
    fn upsert_overwrites_existing() {
        let mut af = AccountsFile::default();
        af.upsert(
            "test",
            Account {
                keystore: Keystore::File,
                pubkey: Some("old".to_string()),
                vault: None,
                path: None,
            },
        );
        af.upsert(
            "test",
            Account {
                keystore: Keystore::AppleKeychain,
                pubkey: Some("new".to_string()),
                vault: None,
                path: None,
            },
        );
        assert_eq!(af.accounts.len(), 1);
        assert_eq!(af.accounts["test"].pubkey.as_deref(), Some("new"));
        assert_eq!(af.accounts["test"].keystore, Keystore::AppleKeychain);
    }
}
