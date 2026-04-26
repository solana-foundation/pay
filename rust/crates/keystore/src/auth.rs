//! Authentication gates — biometric or password prompts before secret access.

use crate::Result;

pub(crate) const DEFAULT_AUTH_REASON: &str = "Authorize pay to use your payment account.";

/// Why the keystore is asking the user to authenticate.
///
/// Platforms render this differently: Windows Hello and Touch ID display the
/// full message, while Linux Polkit maps the variant to a static action
/// message installed in the policy file. Payment limits are only used by the
/// Linux Polkit mapper; other platforms keep showing the exact amount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthIntent {
    AuthorizePayment {
        message: String,
        limit: Option<PaymentLimit>,
    },
    CreateAccount(String),
    ImportAccount(String),
    ExportAccount(String),
    DeleteAccount(String),
    OpenSession(String),
    UseGatewayFeePayer(String),
    UseAccount(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentLimit {
    Usd00001,
    Usd0001,
    Usd0005,
    Usd001,
    Usd005,
    Usd01,
    Usd05,
    Usd1,
    Usd2,
    Usd5,
    Usd10,
    Usd15,
    Usd20,
    Usd25,
    Usd50,
    AboveUsd50,
}

impl PaymentLimit {
    const BUCKETS: &[(u64, Self)] = &[
        (1, Self::Usd00001),
        (10, Self::Usd0001),
        (50, Self::Usd0005),
        (100, Self::Usd001),
        (500, Self::Usd005),
        (1_000, Self::Usd01),
        (5_000, Self::Usd05),
        (10_000, Self::Usd1),
        (20_000, Self::Usd2),
        (50_000, Self::Usd5),
        (100_000, Self::Usd10),
        (150_000, Self::Usd15),
        (200_000, Self::Usd20),
        (250_000, Self::Usd25),
        (500_000, Self::Usd50),
    ];

    pub fn from_amount(amount: &str) -> Option<Self> {
        parse_usd_minor_units(amount).map(Self::from_minor_units)
    }

    fn from_minor_units(units: u64) -> Self {
        Self::BUCKETS
            .iter()
            .find_map(|(ceiling, limit)| (units <= *ceiling).then_some(*limit))
            .unwrap_or(Self::AboveUsd50)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Usd00001 => "$0.0001",
            Self::Usd0001 => "$0.001",
            Self::Usd0005 => "$0.005",
            Self::Usd001 => "$0.01",
            Self::Usd005 => "$0.05",
            Self::Usd01 => "$0.10",
            Self::Usd05 => "$0.50",
            Self::Usd1 => "$1",
            Self::Usd2 => "$2",
            Self::Usd5 => "$5",
            Self::Usd10 => "$10",
            Self::Usd15 => "$15",
            Self::Usd20 => "$20",
            Self::Usd25 => "$25",
            Self::Usd50 => "$50",
            Self::AboveUsd50 => "more than $50",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Usd00001 => "00001",
            Self::Usd0001 => "0001",
            Self::Usd0005 => "0005",
            Self::Usd001 => "001",
            Self::Usd005 => "005",
            Self::Usd01 => "01",
            Self::Usd05 => "05",
            Self::Usd1 => "1",
            Self::Usd2 => "2",
            Self::Usd5 => "5",
            Self::Usd10 => "10",
            Self::Usd15 => "15",
            Self::Usd20 => "20",
            Self::Usd25 => "25",
            Self::Usd50 => "50",
            Self::AboveUsd50 => "above-50",
        }
    }
}

impl AuthIntent {
    pub fn authorize_payment(amount: &str, description: &str) -> Self {
        Self::AuthorizePayment {
            message: format!("Authorize payment of {amount} for {description}."),
            limit: PaymentLimit::from_amount(amount),
        }
    }

    pub fn default_payment() -> Self {
        Self::AuthorizePayment {
            message: "Authorize a payment with pay.".to_string(),
            limit: None,
        }
    }

    pub fn send_sol(recipient: &str) -> Self {
        Self::AuthorizePayment {
            message: format!("Authorize sending SOL to {recipient}."),
            limit: None,
        }
    }

    pub fn create_account(account: &str) -> Self {
        Self::CreateAccount(format!("Set up the \"{account}\" payment account."))
    }

    pub fn import_account(account: &str) -> Self {
        Self::ImportAccount(format!("Import the \"{account}\" payment account."))
    }

    pub fn export_account(account: &str) -> Self {
        Self::ExportAccount(format!("Export the \"{account}\" payment account."))
    }

    pub fn delete_account(account: &str) -> Self {
        Self::DeleteAccount(format!("Delete the \"{account}\" payment account."))
    }

    pub fn open_session() -> Self {
        Self::OpenSession("Authorize opening a pay session.".to_string())
    }

    pub fn use_gateway_fee_payer() -> Self {
        Self::UseGatewayFeePayer("Use your pay account as the gateway fee payer.".to_string())
    }

    pub fn use_account(message: impl Into<String>) -> Self {
        Self::UseAccount(message.into())
    }

    pub fn from_reason(reason: &str) -> Self {
        let message = normalize_message(reason);
        let lower = message.to_ascii_lowercase();

        if lower.starts_with("authorize payment")
            || lower.starts_with("authorize a payment")
            || lower.starts_with("authorize sending")
        {
            let limit = payment_limit_from_message(&message);
            Self::AuthorizePayment { message, limit }
        } else if lower.starts_with("set up") || lower.starts_with("store keypair") {
            Self::CreateAccount(message)
        } else if lower.starts_with("import") {
            Self::ImportAccount(message)
        } else if lower.starts_with("export") {
            Self::ExportAccount(message)
        } else if lower.starts_with("delete") {
            Self::DeleteAccount(message)
        } else if lower.starts_with("authorize opening a pay session") {
            Self::OpenSession(message)
        } else if lower.contains("gateway fee payer") {
            Self::UseGatewayFeePayer(message)
        } else {
            Self::UseAccount(message)
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::AuthorizePayment { message, .. }
            | Self::CreateAccount(message)
            | Self::ImportAccount(message)
            | Self::ExportAccount(message)
            | Self::DeleteAccount(message)
            | Self::OpenSession(message)
            | Self::UseGatewayFeePayer(message)
            | Self::UseAccount(message) => message,
        }
    }

    pub fn payment_limit(&self) -> Option<PaymentLimit> {
        match self {
            Self::AuthorizePayment { limit, .. } => *limit,
            _ => None,
        }
    }

    #[cfg(any(test, target_os = "macos", target_os = "windows"))]
    pub(crate) fn prompt_message(&self) -> String {
        truncate_for_prompt(self.message(), 220)
    }
}

fn payment_limit_from_message(message: &str) -> Option<PaymentLimit> {
    let start = message.find('$')?;
    let amount = message[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '$' || *ch == '.')
        .collect::<String>();
    PaymentLimit::from_amount(&amount)
}

fn parse_usd_minor_units(amount: &str) -> Option<u64> {
    let amount = amount.trim().strip_prefix('$').unwrap_or(amount.trim());
    if amount.is_empty() {
        return None;
    }

    let mut parts = amount.split('.');
    let whole = parts.next()?;
    let frac = parts.next().unwrap_or("");
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }

    let whole_units = whole.parse::<u64>().ok()?.checked_mul(10_000)?;
    let frac_units = fractional_units(frac)?;
    whole_units.checked_add(frac_units)
}

fn fractional_units(frac: &str) -> Option<u64> {
    let mut units = 0u64;
    let mut multiplier = 1_000u64;
    for b in frac.bytes().take(4) {
        units = units.checked_add((b - b'0') as u64 * multiplier)?;
        multiplier /= 10;
    }
    if frac.bytes().skip(4).any(|b| b != b'0') {
        units = units.checked_add(1)?;
    }
    Some(units)
}

/// How the user proves identity before accessing secrets.
pub trait AuthGate: Send + Sync {
    /// Prompt the user to authenticate. Backends should present `intent`
    /// when the platform auth API allows it. Returns `Ok(())` on success.
    fn authenticate(&self, intent: &AuthIntent) -> Result<()>;

    /// Check if this auth mechanism is available on the current device.
    fn is_available(&self) -> bool;
}

/// No authentication — always succeeds. Used for testing and backends
/// where auth is handled externally (e.g. 1Password's `op` CLI).
pub struct NoAuth;

impl AuthGate for NoAuth {
    fn authenticate(&self, _intent: &AuthIntent) -> Result<()> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn normalize_message(reason: &str) -> String {
    let normalized = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalized.trim();
    if normalized.is_empty() {
        DEFAULT_AUTH_REASON
    } else {
        normalized
    }
    .to_string()
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_message_preserves_user_facing_reason() {
        assert_eq!(
            AuthIntent::from_reason("Authorize a payment with pay.").prompt_message(),
            "Authorize a payment with pay."
        );
    }

    #[test]
    fn prompt_message_preserves_specific_payment_reason() {
        assert_eq!(
            AuthIntent::authorize_payment("$0.05", "accessing API api.example.com")
                .prompt_message(),
            "Authorize payment of $0.05 for accessing API api.example.com."
        );
    }

    #[test]
    fn prompt_message_trims_whitespace_and_punctuation() {
        assert_eq!(
            AuthIntent::from_reason("  delete default account.  ").prompt_message(),
            "delete default account."
        );
    }

    #[test]
    fn prompt_message_falls_back_for_empty_reason() {
        assert_eq!(
            AuthIntent::from_reason("   ").prompt_message(),
            DEFAULT_AUTH_REASON
        );
    }

    #[test]
    fn prompt_message_bounds_long_reasons() {
        let long = "a".repeat(221);
        let message = AuthIntent::from_reason(&long).prompt_message();

        assert!(message.ends_with("..."));
        assert!(message.len() < 230);
    }

    #[test]
    fn from_reason_maps_known_reason_shapes_to_variants() {
        assert!(matches!(
            AuthIntent::from_reason("Authorize sending SOL to recipient."),
            AuthIntent::AuthorizePayment { .. }
        ));
        assert!(matches!(
            AuthIntent::from_reason("Set up the \"default\" payment account."),
            AuthIntent::CreateAccount(_)
        ));
        assert!(matches!(
            AuthIntent::from_reason("Import the \"default\" payment account."),
            AuthIntent::ImportAccount(_)
        ));
        assert!(matches!(
            AuthIntent::from_reason("Export the \"default\" payment account."),
            AuthIntent::ExportAccount(_)
        ));
        assert!(matches!(
            AuthIntent::from_reason("Delete the \"default\" payment account."),
            AuthIntent::DeleteAccount(_)
        ));
        assert!(matches!(
            AuthIntent::from_reason("Authorize opening a pay session."),
            AuthIntent::OpenSession(_)
        ));
        assert!(matches!(
            AuthIntent::from_reason("Use your pay account as the gateway fee payer."),
            AuthIntent::UseGatewayFeePayer(_)
        ));
    }

    #[test]
    fn payment_limits_round_up_to_static_buckets() {
        assert_eq!(
            PaymentLimit::from_amount("$0"),
            Some(PaymentLimit::Usd00001)
        );
        assert_eq!(
            PaymentLimit::from_amount("$0.0001"),
            Some(PaymentLimit::Usd00001)
        );
        assert_eq!(
            PaymentLimit::from_amount("$0.00011"),
            Some(PaymentLimit::Usd0001)
        );
        assert_eq!(
            PaymentLimit::from_amount("$0.049"),
            Some(PaymentLimit::Usd005)
        );
        assert_eq!(
            PaymentLimit::from_amount("$0.0501"),
            Some(PaymentLimit::Usd01)
        );
        assert_eq!(PaymentLimit::from_amount("$50"), Some(PaymentLimit::Usd50));
        assert_eq!(
            PaymentLimit::from_amount("$50.01"),
            Some(PaymentLimit::AboveUsd50)
        );
    }

    #[test]
    fn authorize_payment_captures_limit() {
        assert_eq!(
            AuthIntent::authorize_payment("$0.05", "accessing API api.example.com").payment_limit(),
            Some(PaymentLimit::Usd005)
        );
        assert_eq!(
            AuthIntent::from_reason("Authorize payment of $0.0501 for accessing API.")
                .payment_limit(),
            Some(PaymentLimit::Usd01)
        );
    }
}
