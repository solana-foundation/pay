//! Pins the `solana-remote-wallet` floor that Ledger support depends on.
//!
//! Solana app 1.16 on wallet-screen Ledgers (Stax, Flex, Gen5) answers
//! `GET_APP_CONFIGURATION` with 7 bytes. `solana-remote-wallet` < 4.4.0 only
//! accepts 5 and fails with "Version packet size mismatch", which
//! solana-keychain surfaces as an unparseable app configuration and pay cannot
//! enumerate the device at all. solana-keychain requires only `^4`, so the
//! floor is enforced by `Cargo.lock`; this test keeps a lockfile regeneration
//! from silently dropping below it.

use std::{fs, path::Path};

const MIN_SOLANA_REMOTE_WALLET: (u64, u64, u64) = (4, 4, 0);

fn locked_version(lock: &str, package: &str) -> Option<(u64, u64, u64)> {
    let needle = format!("name = \"{package}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() != needle {
            continue;
        }
        let version = lines
            .next()?
            .trim()
            .strip_prefix("version = \"")?
            .strip_suffix('"')?;
        let mut parts = version.split(['.', '-']).map(|p| p.parse::<u64>().ok());
        return Some((parts.next()??, parts.next()??, parts.next()??));
    }
    None
}

#[test]
fn locked_version_parses_lockfile_entries() {
    let lock =
        "[[package]]\nname = \"solana-remote-wallet\"\nversion = \"4.3.2\"\nsource = \"x\"\n";
    assert_eq!(
        locked_version(lock, "solana-remote-wallet"),
        Some((4, 3, 2))
    );
    assert!(locked_version(lock, "solana-remote-wallet").unwrap() < MIN_SOLANA_REMOTE_WALLET);
    assert_eq!(locked_version(lock, "missing"), None);
}

#[test]
fn solana_remote_wallet_parses_solana_app_1_16_configuration() {
    let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = fs::read_to_string(&lock_path).expect("workspace Cargo.lock");
    let version = locked_version(&lock, "solana-remote-wallet")
        .expect("solana-remote-wallet is locked in the workspace");
    assert!(
        version >= MIN_SOLANA_REMOTE_WALLET,
        "solana-remote-wallet {version:?} is locked but Ledger Solana app 1.16 needs >= \
         {MIN_SOLANA_REMOTE_WALLET:?}; run `cargo update -p solana-remote-wallet`"
    );
}
