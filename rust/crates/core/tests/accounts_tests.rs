//! Tests for accounts registry (in-memory, no filesystem side effects).

use pay_core::accounts::{Account, AccountsFile, Keystore};

#[test]
fn empty_accounts_file() {
    let f = AccountsFile::default();
    assert!(f.accounts.is_empty());
    assert!(f.default_account.is_none());
    assert!(f.default_account().is_none());
}

#[test]
fn upsert_first_account_becomes_default() {
    let mut f = AccountsFile::default();
    f.upsert(
        "alice",
        Account {
            keystore: Keystore::AppleKeychain,
            pubkey: Some("abc".into()),
            vault: None,
            path: None,
        },
    );
    assert_eq!(f.default_account, Some("alice".to_string()));
    assert_eq!(f.accounts.len(), 1);
}

#[test]
fn upsert_second_account_preserves_default() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account { keystore: Keystore::AppleKeychain, pubkey: None, vault: None, path: None });
    f.upsert("bob", Account { keystore: Keystore::File, pubkey: None, vault: None, path: Some("/tmp/bob.json".into()) });
    assert_eq!(f.default_account, Some("alice".to_string()));
    assert_eq!(f.accounts.len(), 2);
}

#[test]
fn upsert_overwrites_existing() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account { keystore: Keystore::AppleKeychain, pubkey: Some("old".into()), vault: None, path: None });
    f.upsert("alice", Account { keystore: Keystore::File, pubkey: Some("new".into()), vault: None, path: None });
    assert_eq!(f.accounts.len(), 1);
    assert_eq!(f.accounts["alice"].pubkey.as_deref(), Some("new"));
    assert_eq!(f.accounts["alice"].keystore, Keystore::File);
}

#[test]
fn remove_account() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account { keystore: Keystore::AppleKeychain, pubkey: None, vault: None, path: None });
    f.upsert("bob", Account { keystore: Keystore::File, pubkey: None, vault: None, path: None });
    let removed = f.remove("alice");
    assert!(removed.is_some());
    assert_eq!(f.accounts.len(), 1);
}

#[test]
fn remove_default_reassigns() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account { keystore: Keystore::AppleKeychain, pubkey: None, vault: None, path: None });
    f.upsert("bob", Account { keystore: Keystore::File, pubkey: None, vault: None, path: None });
    assert_eq!(f.default_account, Some("alice".to_string()));
    f.remove("alice");
    // Should reassign to next available
    assert!(f.default_account.is_some());
    assert_ne!(f.default_account.as_deref(), Some("alice"));
}

#[test]
fn remove_last_account_clears_default() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account { keystore: Keystore::AppleKeychain, pubkey: None, vault: None, path: None });
    f.remove("alice");
    assert!(f.default_account.is_none());
    assert!(f.accounts.is_empty());
}

#[test]
fn remove_nonexistent_returns_none() {
    let mut f = AccountsFile::default();
    assert!(f.remove("ghost").is_none());
}

#[test]
fn default_account_returns_correct_entry() {
    let mut f = AccountsFile::default();
    f.upsert("main", Account { keystore: Keystore::OnePassword, pubkey: Some("pk1".into()), vault: Some("Work".into()), path: None });
    let (name, acct) = f.default_account().unwrap();
    assert_eq!(name, "main");
    assert_eq!(acct.keystore, Keystore::OnePassword);
    assert_eq!(acct.vault.as_deref(), Some("Work"));
}

#[test]
fn default_account_falls_back_to_default_name() {
    let mut f = AccountsFile { default_account: None, accounts: Default::default() };
    f.accounts.insert("default".to_string(), Account { keystore: Keystore::File, pubkey: None, vault: None, path: None });
    let (name, _) = f.default_account().unwrap();
    assert_eq!(name, "default");
}

// ── Signer source ──

#[test]
fn signer_source_keychain() {
    let acct = Account { keystore: Keystore::AppleKeychain, pubkey: None, vault: None, path: None };
    assert_eq!(acct.signer_source("mykey"), "keychain:mykey");
}

#[test]
fn signer_source_gnome_keyring() {
    let acct = Account { keystore: Keystore::GnomeKeyring, pubkey: None, vault: None, path: None };
    assert_eq!(acct.signer_source("mykey"), "gnome-keyring:mykey");
}

#[test]
fn signer_source_onepassword() {
    let acct = Account { keystore: Keystore::OnePassword, pubkey: None, vault: Some("Work".into()), path: None };
    assert_eq!(acct.signer_source("mykey"), "1password:mykey");
}

#[test]
fn signer_source_file_with_path() {
    let acct = Account { keystore: Keystore::File, pubkey: None, vault: None, path: Some("/tmp/key.json".into()) };
    assert_eq!(acct.signer_source("mykey"), "/tmp/key.json");
}

#[test]
fn signer_source_file_without_path() {
    let acct = Account { keystore: Keystore::File, pubkey: None, vault: None, path: None };
    assert_eq!(acct.signer_source("mykey"), "~/.config/pay/mykey.json");
}

// ── Keystore Display ──

#[test]
fn keystore_display() {
    assert_eq!(Keystore::AppleKeychain.to_string(), "apple-keychain");
    assert_eq!(Keystore::GnomeKeyring.to_string(), "gnome-keyring");
    assert_eq!(Keystore::OnePassword.to_string(), "1password");
    assert_eq!(Keystore::File.to_string(), "file");
}

// ── Serialization round-trip ──

#[test]
fn yaml_round_trip() {
    let mut f = AccountsFile::default();
    f.upsert("alice", Account {
        keystore: Keystore::AppleKeychain,
        pubkey: Some("7xKXabc".into()),
        vault: None,
        path: None,
    });
    f.upsert("bob", Account {
        keystore: Keystore::File,
        pubkey: Some("9yLMdef".into()),
        vault: None,
        path: Some("/keys/bob.json".into()),
    });

    let yaml = serde_yml::to_string(&f).unwrap();
    let parsed: AccountsFile = serde_yml::from_str(&yaml).unwrap();
    assert_eq!(parsed.accounts.len(), 2);
    assert_eq!(parsed.accounts["alice"].keystore, Keystore::AppleKeychain);
    assert_eq!(parsed.accounts["bob"].path.as_deref(), Some("/keys/bob.json"));
}
