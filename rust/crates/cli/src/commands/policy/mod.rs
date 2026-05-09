//! `pay policy` — manage spending policies enforced before paid HTTP calls.

pub mod create;
pub mod delete;
pub mod list;
pub mod pause;
pub mod resume;
pub mod status;
pub mod update;
pub mod use_cmd;

use clap::Subcommand;
use owo_colors::OwoColorize;

#[derive(Subcommand)]
pub enum PolicyCommand {
    /// Create a new spending policy.
    Create(create::CreateCommand),
    /// List all configured policies.
    #[command(alias = "ls")]
    List(list::ListCommand),
    /// Show policy rules + remaining daily budget.
    Status(status::StatusCommand),
    /// Pause a policy (every paid request rejects).
    Pause(pause::PauseCommand),
    /// Resume a paused policy.
    Resume(resume::ResumeCommand),
    /// Update an existing policy in place.
    Update(update::UpdateCommand),
    /// Delete a policy and clear its tracked spend.
    #[command(alias = "rm")]
    Delete(delete::DeleteCommand),
    /// Set the default policy used when no `--policy` flag is passed.
    Use(use_cmd::UseCommand),
}

impl PolicyCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match self {
            Self::Create(cmd) => cmd.run(),
            Self::List(cmd) => cmd.run(),
            Self::Status(cmd) => cmd.run(),
            Self::Pause(cmd) => cmd.run(),
            Self::Resume(cmd) => cmd.run(),
            Self::Update(cmd) => cmd.run(),
            Self::Delete(cmd) => cmd.run(),
            Self::Use(cmd) => cmd.run(),
        }
    }
}

/// Default behaviour when `pay policy` is run without a subcommand: list
/// policies and print the available subcommands so the user discovers them.
pub fn run_default() -> pay_core::Result<()> {
    list::ListCommand { json: false }.run()?;

    eprintln!("{}", "Subcommands:".dimmed());
    for (name, summary) in SUBCOMMAND_HELP {
        eprintln!(
            "{}",
            format!("  pay policy {name:<10}  {summary}").dimmed()
        );
    }
    Ok(())
}

const SUBCOMMAND_HELP: &[(&str, &str)] = &[
    ("create", "Create a new policy"),
    ("status", "Show rules + remaining daily budget"),
    ("update", "Edit an existing policy"),
    ("pause", "Pause a policy (kill switch)"),
    ("resume", "Resume a paused policy"),
    ("use", "Set the default policy"),
    ("rm", "Delete a policy (alias: delete)"),
];

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Parse a USD amount string ("0.10", "1", "1.5") to micro-USDC.
/// Mirrors `parse_decimal_micro_units` in `main.rs` so the CLI parses USD
/// inputs identically across `--yolo-upto` and policy flags.
pub(crate) fn parse_usd_to_micro(input: &str) -> pay_core::Result<u64> {
    let trimmed = input.trim();
    let without_symbol = trimmed.strip_prefix('$').unwrap_or(trimmed).trim();
    let without_suffix = without_symbol
        .strip_suffix("USDC")
        .or_else(|| without_symbol.strip_suffix("usdc"))
        .unwrap_or(without_symbol)
        .trim();
    parse_decimal_micro_units(without_suffix)
        .map_err(|e| pay_core::Error::Config(format!("invalid amount `{input}`: {e}")))
}

fn parse_decimal_micro_units(input: &str) -> Result<u64, String> {
    if input.is_empty() {
        return Err("amount must not be empty".to_string());
    }
    if input.starts_with('-') {
        return Err("amount must be positive".to_string());
    }
    let mut parts = input.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 6
    {
        return Err("use a decimal amount with at most 6 decimal places".to_string());
    }
    let whole_units = whole
        .parse::<u64>()
        .map_err(|_| "amount is too large".to_string())?
        .checked_mul(1_000_000)
        .ok_or_else(|| "amount is too large".to_string())?;
    let mut fraction_units = 0u64;
    for (index, byte) in fraction.bytes().enumerate() {
        let digit = (byte - b'0') as u64;
        let place = 10_u64.pow(5 - index as u32);
        fraction_units = fraction_units
            .checked_add(digit * place)
            .ok_or_else(|| "amount is too large".to_string())?;
    }
    whole_units
        .checked_add(fraction_units)
        .ok_or_else(|| "amount is too large".to_string())
}

/// Format a micro-USDC amount as a human-readable USD string.
pub(crate) fn format_usd(micro: u64) -> String {
    let whole = micro / 1_000_000;
    let frac = micro % 1_000_000;
    if frac == 0 {
        format!("${whole}")
    } else {
        let mut frac_s = format!("{frac:06}");
        while frac_s.ends_with('0') {
            frac_s.pop();
        }
        format!("${whole}.{frac_s}")
    }
}

/// Heuristically classify an `--allow <X>` value: looks like a base58 Solana
/// pubkey ⇒ recipient; otherwise treat as a request-origin host.
pub(crate) fn classify_allow_value(value: &str) -> AllowKind {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return AllowKind::Origin(String::new());
    }
    // Base58 Solana pubkeys decode to 32 bytes — exactly. Use length
    // post-decode to avoid false positives like "abc".
    if let Ok(bytes) = bs58::decode(trimmed).into_vec()
        && bytes.len() == 32
    {
        return AllowKind::Recipient(trimmed.to_string());
    }
    AllowKind::Origin(trimmed.to_lowercase())
}

pub(crate) enum AllowKind {
    Recipient(String),
    Origin(String),
}

/// Open the file store, look up the named policy, error helpfully if missing.
pub(crate) fn load_policy_or_error(
    store: &pay_core::policy::FilePolicyStore,
    name: &str,
) -> pay_core::Result<(pay_core::policy::PoliciesFile, pay_core::policy::Policy)> {
    let file = pay_core::policy::PolicyStore::load_policies(store)?;
    let policy = file
        .get(name)
        .cloned()
        .ok_or_else(|| pay_core::Error::Config(format!("policy `{name}` not found")))?;
    Ok((file, policy))
}

/// Resolve the policy name a `[name]` arg refers to: explicit name → that;
/// none → the configured default; both missing → error.
pub(crate) fn resolve_target_name(
    explicit: Option<&str>,
    file: &pay_core::policy::PoliciesFile,
) -> pay_core::Result<String> {
    if let Some(name) = explicit {
        return Ok(name.to_string());
    }
    file.default.clone().ok_or_else(|| {
        pay_core::Error::Config(
            "no default policy set — pass <name> or run `pay policy use <name>`".to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usd_basic() {
        assert_eq!(parse_usd_to_micro("1").unwrap(), 1_000_000);
        assert_eq!(parse_usd_to_micro("0.10").unwrap(), 100_000);
        assert_eq!(parse_usd_to_micro("$1.25").unwrap(), 1_250_000);
        assert_eq!(parse_usd_to_micro("0.000001 USDC").unwrap(), 1);
    }

    #[test]
    fn parse_usd_rejects_invalid() {
        assert!(parse_usd_to_micro("").is_err());
        assert!(parse_usd_to_micro("-1").is_err());
        assert!(parse_usd_to_micro("abc").is_err());
        assert!(parse_usd_to_micro("1.0000001").is_err());
    }

    #[test]
    fn format_usd_strips_trailing_zeros() {
        assert_eq!(format_usd(1_000_000), "$1");
        assert_eq!(format_usd(1_250_000), "$1.25");
        assert_eq!(format_usd(1), "$0.000001");
    }

    #[test]
    fn classify_recognizes_pubkey_and_host() {
        // Real base58 32-byte pubkey.
        let pk = "11111111111111111111111111111111";
        assert!(matches!(classify_allow_value(pk), AllowKind::Recipient(_)));
        assert!(matches!(
            classify_allow_value("api.example.com"),
            AllowKind::Origin(_)
        ));
        // A short string that decodes as base58 but is too short.
        assert!(matches!(
            classify_allow_value("abc"),
            AllowKind::Origin(_)
        ));
    }
}
