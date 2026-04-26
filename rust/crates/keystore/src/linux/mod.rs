//! Linux: Polkit authentication + GNOME Secret Service storage.

use crate::{AuthGate, AuthIntent, Error, Result, SecretStore, Zeroizing};
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

// ── Polkit auth gate ────────────────────────────────────────────────────────

const POLKIT_ACTION_PAYMENT: &str = "sh.pay.authorize-payment";
const POLKIT_ACTION_CREATE: &str = "sh.pay.create-keypair";
const POLKIT_ACTION_IMPORT: &str = "sh.pay.import-keypair";
const POLKIT_ACTION_EXPORT: &str = "sh.pay.export-keypair";
const POLKIT_ACTION_DELETE: &str = "sh.pay.delete-keypair";
const POLKIT_ACTION_SESSION: &str = "sh.pay.open-session";
const POLKIT_ACTION_GATEWAY_FEE_PAYER: &str = "sh.pay.use-gateway-fee-payer";
const POLKIT_ACTION_USE: &str = "sh.pay.use-keypair";
const LEGACY_POLKIT_ACTION: &str = "sh.pay.unlock-keypair";

pub struct Polkit;

impl AuthGate for Polkit {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        let action = polkit_action_for_intent(intent);
        run(async move {
            match polkit_authenticate(action).await {
                Err(e) if action != LEGACY_POLKIT_ACTION && is_missing_action(&e) => {
                    polkit_authenticate(LEGACY_POLKIT_ACTION).await
                }
                result => result,
            }
        })
    }

    fn is_available(&self) -> bool {
        run(async { zbus::Connection::system().await.is_ok() })
    }
}

async fn polkit_authenticate(action: &str) -> Result<()> {
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
                action,
                details,
                flags,
                "",
            ),
        )
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("No such action") || msg.contains("not registered") {
                missing_action_error(action)
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

fn polkit_action_for_intent(intent: &AuthIntent) -> &'static str {
    match intent {
        AuthIntent::AuthorizePayment { limit, .. } => limit
            .map(polkit_payment_limit_action)
            .unwrap_or(POLKIT_ACTION_PAYMENT),
        AuthIntent::CreateAccount(_) => POLKIT_ACTION_CREATE,
        AuthIntent::ImportAccount(_) => POLKIT_ACTION_IMPORT,
        AuthIntent::ExportAccount(_) => POLKIT_ACTION_EXPORT,
        AuthIntent::DeleteAccount(_) => POLKIT_ACTION_DELETE,
        AuthIntent::OpenSession(_) => POLKIT_ACTION_SESSION,
        AuthIntent::UseGatewayFeePayer(_) => POLKIT_ACTION_GATEWAY_FEE_PAYER,
        AuthIntent::UseAccount(_) => POLKIT_ACTION_USE,
    }
}

fn polkit_payment_limit_action(limit: crate::PaymentLimit) -> &'static str {
    match limit {
        crate::PaymentLimit::Usd00001 => "sh.pay.authorize-payment-up-to-usd-00001",
        crate::PaymentLimit::Usd0001 => "sh.pay.authorize-payment-up-to-usd-0001",
        crate::PaymentLimit::Usd0005 => "sh.pay.authorize-payment-up-to-usd-0005",
        crate::PaymentLimit::Usd001 => "sh.pay.authorize-payment-up-to-usd-001",
        crate::PaymentLimit::Usd005 => "sh.pay.authorize-payment-up-to-usd-005",
        crate::PaymentLimit::Usd01 => "sh.pay.authorize-payment-up-to-usd-01",
        crate::PaymentLimit::Usd05 => "sh.pay.authorize-payment-up-to-usd-05",
        crate::PaymentLimit::Usd1 => "sh.pay.authorize-payment-up-to-usd-1",
        crate::PaymentLimit::Usd2 => "sh.pay.authorize-payment-up-to-usd-2",
        crate::PaymentLimit::Usd5 => "sh.pay.authorize-payment-up-to-usd-5",
        crate::PaymentLimit::Usd10 => "sh.pay.authorize-payment-up-to-usd-10",
        crate::PaymentLimit::Usd15 => "sh.pay.authorize-payment-up-to-usd-15",
        crate::PaymentLimit::Usd20 => "sh.pay.authorize-payment-up-to-usd-20",
        crate::PaymentLimit::Usd25 => "sh.pay.authorize-payment-up-to-usd-25",
        crate::PaymentLimit::Usd50 => "sh.pay.authorize-payment-up-to-usd-50",
        crate::PaymentLimit::AboveUsd50 => "sh.pay.authorize-payment-above-usd-50",
    }
}

fn missing_action_error(action: &str) -> Error {
    Error::Backend(format!(
        "polkit action '{action}' is not installed.\n\
         Run `pay setup` to install the embedded policy, or install it manually with:\n\
         \x20 sudo cp rust/config/polkit/sh.pay.unlock-keypair.policy \\\n\
         \x20      /usr/share/polkit-1/actions/"
    ))
}

fn is_missing_action(error: &Error) -> bool {
    matches!(error, Error::Backend(msg) if msg.contains("is not installed"))
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
        let data = Zeroizing::new(data.to_owned());
        run(async move {
            let ss = connect().await?;
            let col = get_or_create_collection(&ss).await?;
            ensure_unlocked(&col).await?;
            let result = store_item(&col, &key, &data).await;
            col.lock().await.map_err(ss_err)?;
            result
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

            let result = async {
                let items = col.search_items(attrs(&key)).await.map_err(ss_err)?;
                let item = items
                    .first()
                    .ok_or_else(|| Error::Backend(format!("key not found: {key}")))?;
                Ok(Zeroizing::new(item.get_secret().await.map_err(ss_err)?))
            }
            .await;
            col.lock().await.map_err(ss_err)?;
            result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_intents_use_payment_action() {
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::authorize_payment(
                "$0.05",
                "accessing API api.example.com"
            )),
            "sh.pay.authorize-payment-up-to-usd-005"
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::default_payment()),
            POLKIT_ACTION_PAYMENT
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::send_sol("11111111111111111111111111111111")),
            POLKIT_ACTION_PAYMENT
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::authorize_payment("$0.0501", "accessing API")),
            "sh.pay.authorize-payment-up-to-usd-01"
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::authorize_payment("$50.01", "accessing API")),
            "sh.pay.authorize-payment-above-usd-50"
        );
    }

    #[test]
    fn account_lifecycle_intents_use_specific_actions() {
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::create_account("default")),
            POLKIT_ACTION_CREATE
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::import_account("default")),
            POLKIT_ACTION_IMPORT
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::export_account("default")),
            POLKIT_ACTION_EXPORT
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::delete_account("default")),
            POLKIT_ACTION_DELETE
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::open_session()),
            POLKIT_ACTION_SESSION
        );
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::use_gateway_fee_payer()),
            POLKIT_ACTION_GATEWAY_FEE_PAYER
        );
    }

    #[test]
    fn use_account_intent_uses_generic_action() {
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::use_account(
                "Use your pay account with the Solana CLI."
            )),
            POLKIT_ACTION_USE
        );
    }
}
