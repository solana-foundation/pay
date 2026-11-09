use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use pay_core::accounts::{Account, AccountsFile, AccountsStore, BackendKind, FileAccountsStore};

// Inline test keys only. No OS key store, real wallet, or balance lookup:
// leaving the cached pubkey unset prevents account-list RPC requests.
fn account(seed: u8, active: bool) -> Account {
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let mut bytes = key.to_bytes().to_vec();
    bytes.extend_from_slice(key.verifying_key().as_bytes());
    Account {
        backend: BackendKind::Ephemeral,
        provider: None,
        active,
        auth_required: Some(false),
        pubkey: None,
        vault: None,
        account: None,
        path: None,
        secret_key_b58: Some(bs58::encode(bytes).into_string()),
        created_at: None,
        subscriptions: BTreeMap::new(),
    }
}

fn store(home: &Path) -> FileAccountsStore {
    FileAccountsStore::at(home.join(".config/pay/accounts.yml"))
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pay"))
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .current_dir(home)
        .args(args)
        .output()
        .unwrap()
}

fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> AccountsFile {
    let mut accounts = AccountsFile::default();
    accounts.upsert("mainnet", "alice", account(1, true));
    accounts.upsert("mainnet", "bob", account(2, false));
    accounts.upsert("localnet", "alice", account(3, true));
    accounts.upsert("localnet", "bob", account(4, false));
    accounts.upsert("devnet", "alice", account(5, true));
    accounts
}

#[test]
fn sandbox_remove_preserves_other_networks_and_their_default() {
    // Both flag locations are user-facing entry points to sandbox mode.
    for args in [
        vec!["account", "remove", "alice", "--sandbox", "--yes"],
        vec!["--sandbox", "account", "remove", "alice", "--yes"],
    ] {
        let home = tempfile::tempdir().unwrap();
        let before = fixture();
        store(home.path()).save(&before).unwrap();
        let output = run(home.path(), &args);
        succeeds(&output);
        let after = store(home.path()).load().unwrap();
        let mut expected = before;
        expected.remove("localnet", "alice");
        assert_eq!(after, expected, "args: {args:?}");
        assert_eq!(after.account_for_network("mainnet").unwrap().0, "alice");
        assert_eq!(after.account_for_network("localnet").unwrap().0, "bob");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("No accounts remaining"));
    }
}

#[test]
fn sandbox_remove_last_account_still_displays_remaining_mainnet_accounts() {
    let home = tempfile::tempdir().unwrap();
    let mut before = fixture();
    before.remove("localnet", "bob");
    store(home.path()).save(&before).unwrap();
    let output = run(
        home.path(),
        &["account", "remove", "alice", "--sandbox", "--yes"],
    );
    succeeds(&output);
    let mut expected = before;
    expected.remove("localnet", "alice");
    assert_eq!(store(home.path()).load().unwrap(), expected);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("No accounts remaining"));
}

#[test]
fn sandbox_remove_does_not_claim_all_accounts_are_gone_when_localnet_remains() {
    let home = tempfile::tempdir().unwrap();
    let mut before = AccountsFile::default();
    before.upsert("localnet", "alice", account(3, true));
    before.upsert("localnet", "bob", account(4, false));
    store(home.path()).save(&before).unwrap();
    let output = run(
        home.path(),
        &["account", "remove", "alice", "--sandbox", "--yes"],
    );
    succeeds(&output);
    let mut expected = before;
    expected.remove("localnet", "alice");
    assert_eq!(store(home.path()).load().unwrap(), expected);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("No accounts remaining"));
}

#[test]
fn mainnet_remove_preserves_same_named_sandbox_account() {
    let home = tempfile::tempdir().unwrap();
    let before = fixture();
    store(home.path()).save(&before).unwrap();
    succeeds(&run(home.path(), &["account", "remove", "alice", "--yes"]));
    let mut expected = before;
    expected.remove("mainnet", "alice");
    assert_eq!(store(home.path()).load().unwrap(), expected);
}

#[test]
fn sandbox_remove_never_falls_back_to_same_named_mainnet_account() {
    let home = tempfile::tempdir().unwrap();
    let mut before = fixture();
    before.remove("localnet", "alice");
    store(home.path()).save(&before).unwrap();
    let output = run(
        home.path(),
        &["account", "remove", "alice", "--sandbox", "--yes"],
    );
    assert!(!output.status.success());
    assert_eq!(store(home.path()).load().unwrap(), before);
}

#[test]
fn export_uses_the_selected_network_key_without_changing_accounts() {
    for (prefix, suffix, network) in [
        (vec![], vec!["--sandbox"], "localnet"),
        (vec!["--sandbox"], vec![], "localnet"),
        (vec![], vec![], "mainnet"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let before = fixture();
        store(home.path()).save(&before).unwrap();
        let mut args = prefix;
        args.extend(["account", "export", "alice", "backup.json"]);
        args.extend(suffix);
        succeeds(&run(home.path(), &args));
        let bytes: Vec<u8> =
            serde_json::from_slice(&std::fs::read(home.path().join("backup.json")).unwrap())
                .unwrap();
        let expected = before
            .named_account_for_network(network, "alice")
            .unwrap()
            .ephemeral_keypair_bytes()
            .unwrap();
        assert_eq!(bytes, expected, "args: {args:?}");
        assert_eq!(store(home.path()).load().unwrap(), before);
    }
}

#[test]
fn conflicting_network_flags_do_not_remove_or_export_anything() {
    for command in ["remove", "export"] {
        let home = tempfile::tempdir().unwrap();
        let before = fixture();
        store(home.path()).save(&before).unwrap();
        let mut args = vec!["--mainnet", "account", command, "alice", "--sandbox"];
        if command == "remove" {
            args.push("--yes");
        } else {
            args.push("backup.json");
        }
        let output = run(home.path(), &args);
        assert!(!output.status.success());
        assert_eq!(store(home.path()).load().unwrap(), before);
        assert!(!home.path().join("backup.json").exists());
    }
}
