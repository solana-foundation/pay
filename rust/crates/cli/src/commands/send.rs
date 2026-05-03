//! `pay send` — send stablecoins to a recipient.

use dialoguer::{Select, theme::ColorfulTheme};
use owo_colors::OwoColorize;
use pay_core::accounts::{
    AccountChoice, AccountsFile, FileAccountsStore, MAINNET_NETWORK,
    load_or_create_ephemeral_for_network, load_or_create_ephemeral_for_network_as,
    resolve_account_for_network,
};
use pay_core::balance::{AccountBalances, TokenBalance};
use pay_core::send::{STABLECOIN_DECIMALS, format_token_amount, parse_token_amount};

use crate::no_dna;

const DEFAULT_STABLECOIN: &str = "USDC";

/// Send stablecoins to a recipient address.
///
/// Examples:
///   pay send 1 <address>                         Choose an eligible stablecoin
///   pay send 1 <address> --currency USDC         Send 1 USDC
///   pay send 5 <address> --currency USDT         Send 5 USDT
///   pay send max <address>                       Send an entire stablecoin balance
///   pay send 1 <address> --memo invoice-123      Attach memo metadata
#[derive(clap::Args)]
pub struct SendCommand {
    /// Amount of stablecoin to send (e.g. "1.25"), or "max" to send the
    /// entire selected stablecoin balance.
    pub amount: String,

    /// Recipient public key (base-58).
    pub recipient: String,

    /// Hidden catcher used only to turn shell-expanded `*` into a safe error.
    #[arg(hide = true, num_args = 0..)]
    pub extra_args: Vec<String>,

    /// Stablecoin symbol or mint address. When omitted, pay selects an
    /// eligible balance or asks you to choose if more than one can pay.
    #[arg(long, value_name = "TOKEN")]
    pub currency: Option<String>,

    /// Optional memo metadata for the recipient split.
    #[arg(long, value_name = "MEMO")]
    pub memo: Option<String>,

    /// Take the fee-payer refund out of AMOUNT instead of adding it on top.
    /// This is implied when AMOUNT is "max".
    #[arg(long)]
    pub fee_within: bool,
}

impl SendCommand {
    pub fn run(
        self,
        _active_account_name: Option<&str>,
        network_override: Option<&str>,
        account_override: Option<&str>,
        verbose: bool,
    ) -> pay_core::Result<()> {
        let (amount, recipient) =
            normalize_send_positionals(self.amount, self.recipient, self.extra_args)?;
        let config = pay_core::Config::load().unwrap_or_default();
        let network = network_override.unwrap_or(pay_core::accounts::MAINNET_NETWORK);
        let rpc_url = std::env::var("PAY_RPC_URL")
            .ok()
            .or_else(|| config.rpc_url.clone().filter(|url| !url.trim().is_empty()));
        let fee_within = effective_fee_within(&amount, self.fee_within);

        let currency = resolve_send_currency(
            &amount,
            self.currency.as_deref(),
            network,
            account_override,
            rpc_url.as_deref(),
        )?;

        let amount_display = if sends_entire_balance(&amount) {
            format!("max {currency}")
        } else {
            format!("{amount} {currency}")
        };

        if verbose {
            eprintln!(
                "{}",
                format!("Sending {amount_display} to {recipient}...").dimmed()
            );
        }

        let result = pay_core::client::send::send_stablecoin(
            &amount,
            &recipient,
            &currency,
            network,
            account_override,
            self.memo.as_deref(),
            fee_within,
            rpc_url.as_deref(),
        )?;

        let title = send_success_title(&result);
        if no_dna::is_agent() {
            eprintln!("{}", title.dimmed());
            println!("{}", result.signature);
        } else {
            eprintln!(
                "{}",
                crate::components::notice(
                    crate::components::NoticeLevel::Success,
                    &title,
                    &send_success_body(&result),
                )
            );
        }

        Ok(())
    }
}

fn send_success_title(result: &pay_core::client::send::SendResult) -> String {
    let amount_sent = format_token_amount(result.amount_raw, result.decimals);
    let title = format!("Sent {amount_sent} {} to {}", result.currency, result.to);
    if result.total_amount_raw != result.amount_raw {
        let total = format_token_amount(result.total_amount_raw, result.decimals);
        let fee = if result.fee_refund_raw > 0 {
            result.fee_refund_raw
        } else {
            result.total_amount_raw.saturating_sub(result.amount_raw)
        };
        let fee = format_token_amount(fee, result.decimals);
        return format!(
            "{title} (total paid: {total} {}, fee: {fee} {})",
            result.currency, result.currency
        );
    }
    title
}

fn send_success_body(result: &pay_core::client::send::SendResult) -> String {
    format!(
        "{} {}",
        crate::components::solana_transaction_link(&result.signature, "mainnet", ""),
        result.signature
    )
}

fn effective_fee_within(amount: &str, fee_within: bool) -> bool {
    fee_within || sends_entire_balance(amount)
}

fn sends_entire_balance(amount: &str) -> bool {
    amount == "*" || amount.eq_ignore_ascii_case("max")
}

fn normalize_send_positionals(
    amount: String,
    recipient: String,
    extra_args: Vec<String>,
) -> pay_core::Result<(String, String)> {
    if extra_args.is_empty() {
        return Ok((amount, recipient));
    }

    Err(pay_core::Error::Config(
        "Unexpected extra arguments for `pay send`. If you used `*`, your shell expanded it before pay could read it. Use `pay send max <recipient>` instead."
            .to_string(),
    ))
}

fn resolve_send_currency(
    amount: &str,
    requested_currency: Option<&str>,
    network: &str,
    account_override: Option<&str>,
    rpc_url_override: Option<&str>,
) -> pay_core::Result<String> {
    if let Some(currency) = requested_currency {
        let currency = currency.trim();
        if currency.is_empty() {
            return Err(pay_core::Error::Config(
                "Currency must not be empty".to_string(),
            ));
        }
        return Ok(currency.to_string());
    }

    let Some(sender) = sender_pubkey_for_network(network, account_override)? else {
        if sends_entire_balance(amount) {
            return Err(pay_core::Error::Config(format!(
                "Cannot choose a stablecoin for `pay send max` without a configured {network} account"
            )));
        }
        return Ok(DEFAULT_STABLECOIN.to_string());
    };

    let rpc_url = balance_rpc_url(network, rpc_url_override);
    let balances = balances_for_sender(network, &rpc_url, &sender)?;
    if balances.tokens_unavailable {
        if sends_entire_balance(amount) {
            return Err(pay_core::Error::Config(
                "Stablecoin balances are unavailable; pass --currency TOKEN once balances are reachable"
                    .to_string(),
            ));
        }
        return Ok(DEFAULT_STABLECOIN.to_string());
    }

    let eligible = eligible_stablecoins(&balances, amount)?;
    match eligible.as_slice() {
        [] => Err(pay_core::Error::Config(no_eligible_stablecoin_message(
            amount, &balances,
        ))),
        [only] => Ok(only.currency.clone()),
        many => {
            if !can_prompt() {
                return Err(pay_core::Error::Config(format!(
                    "Multiple stablecoin balances can cover {amount}; pass --currency TOKEN. Eligible balances: {}",
                    eligible_summary(many)
                )));
            }
            prompt_for_stablecoin(many)
        }
    }
}

fn sender_pubkey_for_network(
    network: &str,
    account_override: Option<&str>,
) -> pay_core::Result<Option<String>> {
    let file = AccountsFile::load()?;
    if let Some(name) = account_override {
        if let Some(pubkey) = file
            .named_account_for_network(network, name)
            .and_then(|account| account.pubkey.clone())
        {
            return Ok(Some(pubkey));
        }

        if network != MAINNET_NETWORK {
            let store = FileAccountsStore::default_path();
            let resolved = load_or_create_ephemeral_for_network_as(network, name, &store)?;
            return Ok(resolved.account.pubkey);
        }

        return Ok(None);
    }

    match resolve_account_for_network(network, &file) {
        AccountChoice::Resolved { account, .. } => Ok(account.pubkey),
        AccountChoice::Missing => {
            if network != MAINNET_NETWORK {
                let store = FileAccountsStore::default_path();
                let resolved = load_or_create_ephemeral_for_network(network, &store)?;
                return Ok(resolved.account.pubkey);
            }
            Ok(None)
        }
    }
}

fn balance_rpc_url(network: &str, rpc_url_override: Option<&str>) -> String {
    rpc_url_override
        .map(str::to_string)
        .or_else(|| std::env::var("PAY_RPC_URL").ok())
        .unwrap_or_else(|| {
            if network == MAINNET_NETWORK {
                pay_core::balance::mainnet_rpc_url()
            } else {
                pay_core::config::SANDBOX_RPC_URL.to_string()
            }
        })
}

fn balances_for_sender(
    network: &str,
    rpc_url: &str,
    sender: &str,
) -> pay_core::Result<AccountBalances> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| pay_core::Error::Config(format!("Failed to create runtime: {e}")))?;

    if network != MAINNET_NETWORK {
        let _ = rt.block_on(pay_core::sandbox::fund_via_surfpool(rpc_url, sender));
    }

    rt.block_on(pay_core::balance::get_balances(rpc_url, sender))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EligibleStablecoin {
    currency: String,
    balance: String,
}

fn eligible_stablecoins(
    balances: &AccountBalances,
    amount: &str,
) -> pay_core::Result<Vec<EligibleStablecoin>> {
    let minimum_raw = if sends_entire_balance(amount) {
        1
    } else {
        let raw = parse_token_amount(amount, STABLECOIN_DECIMALS)?;
        if raw == 0 {
            return Err(pay_core::Error::Config(
                "Amount must be greater than 0".to_string(),
            ));
        }
        raw
    };

    let mut eligible = balances
        .tokens
        .iter()
        .filter(|token| token.raw_amount >= minimum_raw)
        .map(|token| EligibleStablecoin {
            currency: token_currency(token),
            balance: format_token_amount(token.raw_amount, STABLECOIN_DECIMALS),
        })
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| left.currency.cmp(&right.currency));
    Ok(eligible)
}

fn token_currency(token: &TokenBalance) -> String {
    token
        .symbol
        .map(str::to_string)
        .unwrap_or_else(|| token.mint.clone())
}

fn can_prompt() -> bool {
    !no_dna::is_agent() && std::io::IsTerminal::is_terminal(&std::io::stderr())
}

fn prompt_for_stablecoin(eligible: &[EligibleStablecoin]) -> pay_core::Result<String> {
    let labels = eligible
        .iter()
        .map(|token| format!("{}  {} available", token.currency, token.balance))
        .collect::<Vec<_>>();
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose stablecoin")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|e| pay_core::Error::Config(format!("Prompt error: {e}")))?;
    Ok(eligible[selection].currency.clone())
}

fn eligible_summary(eligible: &[EligibleStablecoin]) -> String {
    eligible
        .iter()
        .map(|token| format!("{} {}", token.currency, token.balance))
        .collect::<Vec<_>>()
        .join(", ")
}

fn no_eligible_stablecoin_message(amount: &str, balances: &AccountBalances) -> String {
    let balances = stablecoin_balance_summary(balances);
    if sends_entire_balance(amount) {
        return if balances.is_empty() {
            "No stablecoin balances available to send".to_string()
        } else {
            format!("No non-zero stablecoin balance available to send. Balances: {balances}")
        };
    }

    if balances.is_empty() {
        format!("No stablecoin balance can cover {amount}")
    } else {
        format!("No stablecoin balance can cover {amount}. Balances: {balances}")
    }
}

fn stablecoin_balance_summary(balances: &AccountBalances) -> String {
    balances
        .tokens
        .iter()
        .map(|token| {
            format!(
                "{} {}",
                token_currency(token),
                format_token_amount(token.raw_amount, STABLECOIN_DECIMALS)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balances(tokens: Vec<(&'static str, u64)>) -> AccountBalances {
        AccountBalances {
            sol_lamports: 0,
            tokens: tokens
                .into_iter()
                .map(|(symbol, raw_amount)| TokenBalance {
                    mint: format!("{symbol}_mint"),
                    raw_amount,
                    ui_amount: raw_amount as f64 / 1_000_000.0,
                    symbol: Some(symbol),
                })
                .collect(),
            tokens_unavailable: false,
        }
    }

    #[test]
    fn eligible_stablecoins_filters_by_amount() {
        let b = balances(vec![
            ("USDC", 900_000),
            ("USDT", 1_000_000),
            ("PYUSD", 2_500_000),
        ]);

        let eligible = eligible_stablecoins(&b, "1").unwrap();

        assert_eq!(
            eligible,
            vec![
                EligibleStablecoin {
                    currency: "PYUSD".to_string(),
                    balance: "2.5".to_string(),
                },
                EligibleStablecoin {
                    currency: "USDT".to_string(),
                    balance: "1".to_string(),
                },
            ]
        );
    }

    #[test]
    fn eligible_stablecoins_max_uses_non_zero_balances() {
        let b = balances(vec![("USDC", 0), ("USDT", 1)]);

        let eligible = eligible_stablecoins(&b, "max").unwrap();

        assert_eq!(
            eligible,
            vec![EligibleStablecoin {
                currency: "USDT".to_string(),
                balance: "0.000001".to_string(),
            }]
        );
    }

    #[test]
    fn no_eligible_message_lists_balances() {
        let b = balances(vec![("USDC", 500_000), ("USDT", 250_000)]);

        let message = no_eligible_stablecoin_message("1", &b);

        assert_eq!(
            message,
            "No stablecoin balance can cover 1. Balances: USDC 0.5, USDT 0.25"
        );
    }

    #[test]
    fn effective_fee_within_defaults_max_to_true() {
        assert!(effective_fee_within("max", false));
        assert!(effective_fee_within("MAX", false));
        assert!(effective_fee_within("*", false));
        assert!(effective_fee_within("1", true));
        assert!(!effective_fee_within("1", false));
    }

    #[test]
    fn normalize_send_positionals_keeps_normal_amount_and_recipient() {
        assert_eq!(
            normalize_send_positionals("1".to_string(), "recipient".to_string(), vec![]).unwrap(),
            ("1".to_string(), "recipient".to_string())
        );
    }

    #[test]
    fn normalize_send_positionals_rejects_extra_args_instead_of_recovering_star() {
        let err = normalize_send_positionals(
            "CONTRIBUTING.md".to_string(),
            "gateway".to_string(),
            vec![
                "Justfile".to_string(),
                "96WoyH3JmANSMsQLGC3MKyiGiXCymZyM9SLaWjcRrKuD".to_string(),
            ],
        )
        .unwrap_err();

        assert!(err.to_string().contains("Unexpected extra arguments"));
        assert!(err.to_string().contains("pay send max <recipient>"));
    }

    #[test]
    fn send_success_title_includes_total_paid_when_fee_is_added() {
        let result = pay_core::client::send::SendResult {
            signature: "sig123".to_string(),
            amount_raw: 1_000_000,
            total_amount_raw: 1_001_500,
            fee_refund_raw: 1_500,
            decimals: 6,
            currency: "USDC".to_string(),
            mint: "mint".to_string(),
            from: "from".to_string(),
            to: "to".to_string(),
        };

        assert_eq!(
            send_success_title(&result),
            "Sent 1 USDC to to (total paid: 1.0015 USDC, fee: 0.0015 USDC)"
        );
    }

    #[test]
    fn send_success_title_omits_total_when_no_fee_is_added() {
        let result = pay_core::client::send::SendResult {
            signature: "sig123".to_string(),
            amount_raw: 1_000_000,
            total_amount_raw: 1_000_000,
            fee_refund_raw: 0,
            decimals: 6,
            currency: "USDC".to_string(),
            mint: "mint".to_string(),
            from: "from".to_string(),
            to: "to".to_string(),
        };

        assert_eq!(send_success_title(&result), "Sent 1 USDC to to");
    }

    #[test]
    fn send_success_body_links_transaction() {
        let result = pay_core::client::send::SendResult {
            signature: "sig123".to_string(),
            amount_raw: 1_000_000,
            total_amount_raw: 1_000_000,
            fee_refund_raw: 0,
            decimals: 6,
            currency: "USDC".to_string(),
            mint: "mint".to_string(),
            from: "from".to_string(),
            to: "to".to_string(),
        };
        let body = send_success_body(&result);

        assert!(body.contains("Link to receipt"));
        assert!(body.contains("sig123"));
        assert!(
            body.contains(
                "https://explorer.solana.com/tx/sig123?cluster=mainnet-beta&view=receipt"
            )
        );
    }
}
