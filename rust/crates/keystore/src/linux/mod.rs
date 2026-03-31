//! Linux: Polkit authentication + GNOME Secret Service storage.

use crate::{AuthGate, Error, Result, SecretStore, Zeroizing};
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

// ── Polkit auth gate ────────────────────────────────────────────────────────

const POLKIT_ACTION: &str = "sh.pay.unlock-keypair";

pub struct Polkit;

impl AuthGate for Polkit {
    fn authenticate(&self, _reason: &str) -> Result<()> {
        run(async { polkit_authenticate().await })
    }

    fn is_available(&self) -> bool {
        run(async { zbus::Connection::system().await.is_ok() })
    }
}

async fn polkit_authenticate() -> Result<()> {
    use zbus::zvariant::{OwnedValue, Value};

    let conn = zbus::Connection::system()
        .await
        .map_err(|e| Error::Backend(format!("D-Bus system bus: {e}")))?;

    let pid = std::process::id();
    let start_time = process_start_time()?;

    let subject_details: HashMap<String, OwnedValue> = [
        ("pid".to_owned(), OwnedValue::from(Value::new(pid))),
        (
            "start-time".to_owned(),
            OwnedValue::from(Value::new(start_time)),
        ),
    ]
    .into();

    let details: HashMap<String, String> = HashMap::new();
    let flags: u32 = 0x1; // AllowUserInteraction

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
                "",
            ),
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("No such action") || msg.contains("not registered") {
                Error::Backend(format!(
                    "polkit action '{POLKIT_ACTION}' is not installed.\n\
                     Install it with:\n\
                     \x20 sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy \\\n\
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

fn process_start_time() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat")
        .map_err(|e| Error::Backend(format!("read /proc/self/stat: {e}")))?;
    let after_comm = stat
        .rfind(')')
        .ok_or_else(|| Error::Backend("parse /proc/self/stat".to_string()))?;
    let fields: Vec<&str> = stat[after_comm + 2..].split_ascii_whitespace().collect();
    fields
        .get(19)
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Error::Backend("parse /proc/self/stat: starttime field missing".to_string()))
}

// ── Secret Service store ────────────────────────────────────────────────────

const SERVICE_ATTR: &str = "pay.sh";
const COLLECTION_LABEL: &str = "pay";

pub struct SecretServiceStore;

impl SecretServiceStore {
    pub fn is_available() -> bool {
        run(async { SecretService::connect(EncryptionType::Dh).await.is_ok() })
    }
}

impl SecretStore for SecretServiceStore {
    fn store(&self, key: &str, data: &[u8]) -> Result<()> {
        let key = key.to_owned();
        let data = data.to_owned();
        run(async move {
            let ss = connect().await?;
            let col = get_or_create_collection(&ss).await?;
            ensure_unlocked(&col).await?;
            store_item(&col, &key, &data).await?;
            col.lock().await.map_err(ss_err)?;
            Ok(())
        })
    }

    fn load(&self, key: &str) -> Result<Zeroizing<Vec<u8>>> {
        let key = key.to_owned();
        run(async move {
            let ss = connect().await?;
            let col = get_collection(&ss).await.ok_or_else(|| {
                Error::Backend("pay keyring not found — run `pay setup` first".to_string())
            })?;
            ensure_unlocked(&col).await?;

            let items = col.search_items(attrs(&key)).await.map_err(ss_err)?;
            let item = items
                .first()
                .ok_or_else(|| Error::Backend(format!("key not found: {key}")))?;
            let secret = Zeroizing::new(item.get_secret().await.map_err(ss_err)?);
            col.lock().await.map_err(ss_err)?;
            Ok(secret)
        })
    }

    fn exists(&self, key: &str) -> bool {
        let key = key.to_owned();
        run(async move {
            let Ok(ss) = connect().await else {
                return false;
            };
            let Some(col) = get_collection(&ss).await else {
                let Ok(default) = ss.get_default_collection().await else {
                    return false;
                };
                return default
                    .search_items(attrs(&key))
                    .await
                    .map(|items| !items.is_empty())
                    .unwrap_or(false);
            };
            col.search_items(attrs(&key))
                .await
                .map(|items| !items.is_empty())
                .unwrap_or(false)
        })
    }

    fn delete(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        run(async move {
            let ss = connect().await?;
            if let Some(col) = get_collection(&ss).await {
                ensure_unlocked(&col).await?;
                for item in col.search_items(attrs(&key)).await.map_err(ss_err)? {
                    item.delete().await.map_err(ss_err)?;
                }
                col.lock().await.map_err(ss_err)?;
            }
            if let Ok(default) = ss.get_default_collection().await {
                for item in default.search_items(attrs(&key)).await.map_err(ss_err)? {
                    item.delete().await.map_err(ss_err)?;
                }
            }
            Ok(())
        })
    }
}

// ── Secret Service helpers ──────────────────────────────────────────────────

async fn connect() -> Result<SecretService<'static>> {
    SecretService::connect(EncryptionType::Dh)
        .await
        .map_err(|e| Error::Backend(format!("Secret Service unavailable: {e}")))
}

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

async fn store_item(col: &secret_service::Collection<'_>, key: &str, secret: &[u8]) -> Result<()> {
    col.create_item(
        &format!("pay/{key}"),
        attrs(key),
        secret,
        true,
        "application/octet-stream",
    )
    .await
    .map_err(ss_err)
    .map(|_| ())
}

fn attrs(key: &str) -> HashMap<&str, &str> {
    HashMap::from([("service", SERVICE_ATTR), ("account", key)])
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
