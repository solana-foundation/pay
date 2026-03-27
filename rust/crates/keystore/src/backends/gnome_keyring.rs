//! GNOME Keyring backend — stores keypairs via the Secret Service D-Bus API
//! (org.freedesktop.secrets), available on GNOME and KDE (Plasma 6+) desktops.
//!
//! Storage layout:
//!   Collection: "pay"  (separate keyring, locked at rest)
//!     label:      "pay/<account>"
//!     attributes: service = "pay.sh", account = "<account>"
//!     secret:     64-byte raw keypair
//!
//!   Default collection (login keyring, no auth):
//!     label:      "pay/<account>.pubkey"
//!     attributes: service = "pay.sh", account = "<account>.pubkey"
//!     secret:     32-byte raw public key
//!
//! Auth gate: polkit action `sh.pay.unlock-keypair` is checked before every
//! `load_keypair` call via `org.freedesktop.PolicyKit1`. Polkit uses PAM
//! internally, so fingerprint (pam_fprintd) and password both work.
//! This is equivalent to Touch ID on macOS — polkit never caches between calls.
//!
//! Requires the polkit action file to be installed:
//!   sudo cp linux/polkit/sh.pay.unlock-keypair.policy \
//!            /usr/share/polkit-1/actions/
//! For snap installs this is handled automatically.

use std::collections::HashMap;

use secret_service::{EncryptionType, SecretService};

use crate::{Error, KeystoreBackend, Result, SyncMode, Zeroizing};

const SERVICE_ATTR: &str = "pay.sh";
const COLLECTION_LABEL: &str = "pay";
const POLKIT_ACTION: &str = "sh.pay.unlock-keypair";

pub struct GnomeKeyring;

impl GnomeKeyring {
    /// Check if the Secret Service D-Bus interface is reachable.
    /// Returns false on headless/server systems where GNOME Keyring is not running.
    pub fn is_available() -> bool {
        run(async { SecretService::connect(EncryptionType::Plain).await.is_ok() })
    }
}

impl KeystoreBackend for GnomeKeyring {
    fn import(&self, account: &str, keypair_bytes: &[u8], _sync: SyncMode) -> Result<()> {
        if keypair_bytes.len() != 64 {
            return Err(Error::InvalidKeypair(format!(
                "expected 64 bytes, got {}",
                keypair_bytes.len()
            )));
        }
        let account = account.to_owned();
        let keypair_bytes = keypair_bytes.to_owned();
        run(async move {
            // Polkit authentication — prompts before writing the keypair.
            polkit_authenticate("store keypair").await?;

            let ss = connect().await?;

            // Public key goes into the default (login) collection — readable without auth.
            let default = ss.get_default_collection().await.map_err(ss_err)?;
            store_item(&default, &pubkey_account(&account), &keypair_bytes[32..64]).await?;

            // Full keypair goes into the locked "pay" collection.
            // If the collection is new, GNOME Keyring shows a "set keyring password" dialog.
            let col = get_or_create_collection(&ss).await?;
            ensure_unlocked(&col).await?;
            store_item(&col, &account, &keypair_bytes).await?;
            col.lock().await.map_err(ss_err)?;

            Ok(())
        })
    }

    fn exists(&self, account: &str) -> bool {
        let account = account.to_owned();
        run(async move {
            let Ok(ss) = connect().await else {
                return false;
            };
            // Both must be present: public key in the default collection AND
            // the pay collection itself. Checking only the public key gives a
            // false positive when a previous failed setup wrote the public key
            // but never created the pay collection.
            let Ok(default) = ss.get_default_collection().await else {
                return false;
            };
            let pubkey_exists = default
                .search_items(attrs(&pubkey_account(&account)))
                .await
                .map(|items| !items.is_empty())
                .unwrap_or(false);

            pubkey_exists && get_collection(&ss).await.is_some()
        })
    }

    fn delete(&self, account: &str) -> Result<()> {
        let account = account.to_owned();
        run(async move {
            polkit_authenticate("delete keypair").await?;

            let ss = connect().await?;

            // Delete keypair from the pay collection (requires unlock).
            if let Some(col) = get_collection(&ss).await {
                ensure_unlocked(&col).await?;
                for item in col.search_items(attrs(&account)).await.map_err(ss_err)? {
                    item.delete().await.map_err(ss_err)?;
                }
                col.lock().await.map_err(ss_err)?;
            }

            // Delete public key from the default collection (no unlock needed).
            let default = ss.get_default_collection().await.map_err(ss_err)?;
            for item in default
                .search_items(attrs(&pubkey_account(&account)))
                .await
                .map_err(ss_err)?
            {
                item.delete().await.map_err(ss_err)?;
            }

            Ok(())
        })
    }

    fn pubkey(&self, account: &str) -> Result<Vec<u8>> {
        let account = account.to_owned();
        run(async move {
            let ss = connect().await?;
            let default = ss.get_default_collection().await.map_err(ss_err)?;
            let items = default
                .search_items(attrs(&pubkey_account(&account)))
                .await
                .map_err(ss_err)?;
            let item = items
                .first()
                .ok_or_else(|| Error::Backend("public key not found".to_string()))?;
            item.get_secret().await.map_err(ss_err)
        })
    }

    fn load_keypair(&self, account: &str, reason: &str) -> Result<Zeroizing<Vec<u8>>> {
        let account = account.to_owned();
        let reason = reason.to_owned();
        run(async move {
            // Polkit authentication — always prompts (password or fingerprint via PAM).
            // This is the auth gate; GNOME Keyring is only used for encrypted storage.
            polkit_authenticate(&reason).await?;

            let ss = connect().await?;
            let col = get_collection(&ss).await.ok_or_else(|| {
                Error::Backend("pay keyring not found — run `pay setup` first".to_string())
            })?;

            ensure_unlocked(&col).await?;

            let items = col.search_items(attrs(&account)).await.map_err(ss_err)?;
            let item = items
                .first()
                .ok_or_else(|| Error::Backend("keypair not found".to_string()))?;
            let secret = Zeroizing::new(item.get_secret().await.map_err(ss_err)?);

            // Lock so the keypair is encrypted at rest between calls.
            col.lock().await.map_err(ss_err)?;

            Ok(secret)
        })
    }
}

// ── Polkit auth ───────────────────────────────────────────────────────────────

/// Authenticate via polkit before reading the keypair.
///
/// Polkit uses PAM internally, so this supports both password and fingerprint
/// (if pam_fprintd is enabled via `pam-auth-update --enable fprintd`).
///
/// Requires the action file to be installed:
///   sudo cp linux/polkit/sh.pay.unlock-keypair.policy /usr/share/polkit-1/actions/
async fn polkit_authenticate(_reason: &str) -> Result<()> {
    use zbus::zvariant::{OwnedValue, Value};

    let conn = zbus::Connection::system()
        .await
        .map_err(|e| Error::Backend(format!("D-Bus system bus: {e}")))?;

    let pid = std::process::id();
    let start_time = process_start_time()?;

    // Subject: the current process ("unix-process" with pid + start-time).
    // start-time prevents PID reuse attacks.
    let subject_details: HashMap<String, OwnedValue> = [
        (
            "pid".to_owned(),
            OwnedValue::try_from(Value::new(pid))
                .map_err(|e| Error::Backend(format!("polkit pid: {e}")))?,
        ),
        (
            "start-time".to_owned(),
            OwnedValue::try_from(Value::new(start_time))
                .map_err(|e| Error::Backend(format!("polkit start-time: {e}")))?,
        ),
    ]
    .into();

    // details a{ss}: must be empty for unprivileged callers — only uid 0 or
    // the action owner may pass custom details to CheckAuthorization.
    let details: HashMap<String, String> = HashMap::new();

    // flags: 0x1 = AllowUserInteraction (shows the auth dialog).
    let flags: u32 = 0x1;

    let reply = conn
        .call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(
                ("unix-process", subject_details),
                POLKIT_ACTION,
                details,
                flags,
                "", // cancellation_id
            ),
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("No such action") || msg.contains("not registered") {
                Error::Backend(format!(
                    "polkit action '{POLKIT_ACTION}' is not installed.\n\
                     Install it with:\n\
                     \x20 sudo cp linux/polkit/sh.pay.unlock-keypair.policy \\\n\
                     \x20      /usr/share/polkit-1/actions/"
                ))
            } else {
                Error::Backend(format!("polkit: {msg}"))
            }
        })?;

    let (authorized, _, _): (bool, bool, HashMap<String, String>) = reply
        .body()
        .map_err(|e| Error::Backend(format!("polkit response: {e}")))?;

    if authorized {
        Ok(())
    } else {
        Err(Error::AuthDenied("authentication cancelled".to_string()))
    }
}

/// Read the process start time from /proc/self/stat (field 22).
/// Used to prevent PID-reuse attacks in the polkit subject.
fn process_start_time() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat")
        .map_err(|e| Error::Backend(format!("read /proc/self/stat: {e}")))?;
    // Format: pid (comm) state ppid ... starttime
    // The comm field can contain spaces and parentheses, so find the last ')'.
    let after_comm = stat
        .rfind(')')
        .ok_or_else(|| Error::Backend("parse /proc/self/stat".to_string()))?;
    let fields: Vec<&str> = stat[after_comm + 2..].split_ascii_whitespace().collect();
    // starttime is field 22 overall = index 19 after pid and comm
    fields
        .get(19)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Error::Backend("parse /proc/self/stat: starttime field missing".to_string()))
}

// ── Secret Service helpers ────────────────────────────────────────────────────

async fn connect() -> Result<SecretService<'static>> {
    SecretService::connect(EncryptionType::Plain)
        .await
        .map_err(|e| Error::Backend(format!("Secret Service unavailable: {e}")))
}

/// Find the "pay" collection by label, or `None` if it doesn't exist.
///
/// GNOME Keyring only supports the `"default"` alias; custom aliases return
/// `NotSupported`. We enumerate all collections and match by label instead.
async fn get_collection<'a>(ss: &'a SecretService<'a>) -> Option<secret_service::Collection<'a>> {
    let collections = ss.get_all_collections().await.ok()?;
    for col in collections {
        if col
            .get_label()
            .await
            .map(|l| l == COLLECTION_LABEL)
            .unwrap_or(false)
        {
            return Some(col);
        }
    }
    None
}

/// Get the "pay" collection, creating it if absent.
///
/// If new, GNOME Keyring shows a "set keyring password" dialog.
/// Empty alias is used since GNOME Keyring doesn't support custom alias names.
async fn get_or_create_collection<'a>(
    ss: &'a SecretService<'a>,
) -> Result<secret_service::Collection<'a>> {
    if let Some(col) = get_collection(ss).await {
        return Ok(col);
    }
    ss.create_collection(COLLECTION_LABEL, "")
        .await
        .map_err(ss_err)
}

/// Unlock `col` if locked. Maps cancellation/denial to `Error::AuthDenied`.
async fn ensure_unlocked(col: &secret_service::Collection<'_>) -> Result<()> {
    if col.is_locked().await.unwrap_or(true) {
        col.unlock().await.map_err(|e| {
            let msg = e.to_string().to_lowercase();
            if msg.contains("dismissed") || msg.contains("cancel") || msg.contains("denied") {
                Error::AuthDenied("keyring unlock cancelled".to_string())
            } else {
                Error::Backend(format!("unlock failed: {e}"))
            }
        })?;
    }
    Ok(())
}

async fn store_item(
    col: &secret_service::Collection<'_>,
    account: &str,
    secret: &[u8],
) -> Result<()> {
    col.create_item(
        &format!("pay/{account}"),
        attrs(account),
        secret,
        true, // replace existing item with same attributes
        "application/octet-stream",
    )
    .await
    .map_err(ss_err)
    .map(|_| ())
}

fn attrs(account: &str) -> HashMap<&str, &str> {
    HashMap::from([("service", SERVICE_ATTR), ("account", account)])
}

fn pubkey_account(account: &str) -> String {
    format!("{account}.pubkey")
}

fn ss_err(e: secret_service::Error) -> Error {
    Error::Backend(e.to_string())
}

fn run<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}
