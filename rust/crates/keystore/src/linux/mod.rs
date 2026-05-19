//! Linux: Polkit authentication + GNOME Secret Service storage.

use crate::{AuthGate, AuthIntent, Error, Result, SecretStore, Zeroizing};
use secret_service::{EncryptionType, SecretService};
use std::collections::HashMap;

// ── Polkit auth gate ────────────────────────────────────────────────────────

// The generic `sh.pay.authorize-payment` action still exists in the
// installed policy file as a catch-all, but `polkit_action_for_intent`
// no longer falls back to it (audit #4): unparseable amounts route to
// the most restrictive bucket so policy choice tracks the real risk.
const POLKIT_ACTION_CREATE: &str = "sh.pay.create-keypair";
const POLKIT_ACTION_IMPORT: &str = "sh.pay.import-keypair";
const POLKIT_ACTION_EXPORT: &str = "sh.pay.export-keypair";
const POLKIT_ACTION_DELETE: &str = "sh.pay.delete-keypair";
const POLKIT_ACTION_SESSION: &str = "sh.pay.open-session";
const POLKIT_ACTION_GATEWAY_FEE_PAYER: &str = "sh.pay.use-gateway-fee-payer";
const POLKIT_ACTION_USE: &str = "sh.pay.use-keypair";
const LEGACY_POLKIT_ACTION: &str = "sh.pay.unlock-keypair";

pub(crate) struct Polkit;

impl AuthGate for Polkit {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        // Audit #35: the previous implementation fell back to the
        // generic `sh.pay.unlock-keypair` action whenever the typed
        // action (per-amount payment bucket, delete/import/session/
        // gateway-fee-payer) was missing. That silently changes which
        // authorization the user and the policy engine see — an admin
        // who tightened `authorize-payment-above-usd-50` got bypassed
        // through the catch-all. Fail closed instead: if the typed
        // action isn't installed, surface that as a structured error
        // so the operator can fix the policy file or notice that a
        // previously-typed action was demoted to the legacy bucket.
        let action = polkit_action_for_intent(intent);
        run(async move { polkit_authenticate(action, true).await })
    }

    fn is_available(&self) -> bool {
        // Audit #44: probing the system bus alone reported "available"
        // even when the polkit action wasn't installed — the failure
        // then showed up at the next interactive call. Drive a
        // non-interactive `CheckAuthorization` against the legacy
        // action and treat anything that isn't a missing-action error
        // (Ok / AuthDenied / other backend errors) as "the action is
        // reachable; we just may not be authorized yet."
        run(async {
            if zbus::Connection::system().await.is_err() {
                return false;
            }
            match polkit_authenticate(LEGACY_POLKIT_ACTION, false).await {
                Ok(()) => true,
                Err(e) => !is_missing_action(&e),
            }
        })
    }
}

async fn polkit_authenticate(action: &str, interactive: bool) -> Result<()> {
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
    // PolicyKit `CheckAuthorizationFlags`: bit 0 = AllowUserInteraction.
    // Non-interactive probes (`is_available`) clear it so the call
    // returns immediately with an authorized/challenge bool pair
    // instead of blocking on a user prompt (audit #44).
    let flags: u32 = u32::from(interactive);

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

    let (authorized, challenge, _): (bool, bool, HashMap<String, String>) = reply
        .body()
        .map_err(|e| Error::Backend(format!("polkit response: {e}")))?;

    // Audit #47: PolicyKit's reply tells us *why* an unauthorized
    // result came back. `challenge=true` means the user was prompted
    // and dismissed/cancelled. `challenge=false` means policy refused
    // to even challenge the user — e.g. the action's
    // `allow_active`/`allow_inactive` is set to `no`, or an admin
    // rule denies the action. Retrying after a policy denial is
    // pointless; surfacing that distinction lets callers display the
    // right next step instead of nudging the user to "try again."
    if authorized {
        Ok(())
    } else if challenge {
        Err(Error::AuthDenied("authentication cancelled".to_string()))
    } else {
        Err(Error::AuthDenied(
            "not authorized by polkit policy".to_string(),
        ))
    }
}

fn polkit_action_for_intent(intent: &AuthIntent) -> &'static str {
    match intent {
        // Audit #4: an `AuthorizePayment` without a structured `limit`
        // means the caller could not validate the amount (parse failure,
        // prose-derived intent, locale formatting). Falling back to the
        // generic payment action would be *less* restrictive than the
        // bucketed actions. Fail closed to the most restrictive bucket
        // so unparseable amounts request the strictest policy.
        AuthIntent::AuthorizePayment { limit, .. } => limit
            .map(polkit_payment_limit_action)
            .unwrap_or_else(|| polkit_payment_limit_action(crate::PaymentLimit::AboveUsd50)),
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
    // Audit #48: the previous implementation parsed /proc/self/stat by
    // hand and indexed field 19. The `19` came from the offset of
    // `starttime` in the post-`comm` field list, which is not obvious
    // without the proc(5) man page in hand. `procfs` reads the same
    // file and exposes `starttime` as a named field, removing the
    // magic-number maintenance hazard.
    use procfs::process::Process;
    Process::myself()
        .and_then(|p| p.stat())
        .map(|s| s.starttime)
        .map_err(|e| Error::Backend(format!("/proc/self/stat: {e}")))
}

// ── Secret Service store ────────────────────────────────────────────────────

const SERVICE_ATTR: &str = "pay.sh";
const COLLECTION_LABEL: &str = "pay";

pub(crate) struct SecretServiceStore;

impl SecretServiceStore {
    /// Probe the Secret Service backend without storing anything.
    ///
    /// Takes `&self` so callers go through method-call dispatch rather
    /// than the inherent static (audit #39): keeps the call shape
    /// consistent with the per-backend `AuthGate::is_available` impls,
    /// which all take `&self`.
    pub fn is_available(&self) -> bool {
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
        // Audit #4: a payment intent with no structured amount must NOT
        // fall back to the generic action — that would be less
        // restrictive than the per-bucket actions. Map to AboveUsd50
        // instead so unparseable amounts request the strictest policy.
        assert_eq!(
            polkit_action_for_intent(&AuthIntent::default_payment()),
            "sh.pay.authorize-payment-above-usd-50",
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

    /// Audit #4: `AuthorizePayment { limit: None }` (parse failure /
    /// missing structured amount) must NOT fall back to the generic
    /// payment action, which is *less* restrictive than the bucketed
    /// actions. Falling closed to the most restrictive policy
    /// (`AboveUsd50`) means an unparseable amount fails up, not down.
    #[test]
    fn audit_4_unparseable_amount_maps_to_most_restrictive_polkit_action() {
        let intent = AuthIntent::AuthorizePayment {
            message: "authorize payment".to_string(),
            limit: None,
        };
        assert_eq!(
            polkit_action_for_intent(&intent),
            "sh.pay.authorize-payment-above-usd-50",
        );
    }

    /// Audit #4: typed payment intents whose amount string fails to
    /// parse (commas, locale formatting, malformed input) must also
    /// route to the most restrictive bucket — never the generic action.
    #[test]
    fn audit_4_typed_payment_with_comma_amount_uses_restrictive_bucket() {
        let intent = AuthIntent::authorize_payment("$50,000", "accessing API");
        assert_eq!(
            polkit_action_for_intent(&intent),
            "sh.pay.authorize-payment-above-usd-50",
        );
    }
}
