pub(crate) struct PaymentPromptContext {
    pub reason: String,
    pub operator: String,
}

pub(crate) fn payment_prompt_context(
    reason: Option<&str>,
    resource_urls: &[Option<&str>],
) -> PaymentPromptContext {
    PaymentPromptContext {
        reason: reason
            .and_then(normalized_nonempty)
            .unwrap_or_else(|| "API access".to_string()),
        operator: resource_urls
            .iter()
            .filter_map(|url| url.and_then(operator_label))
            .next()
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

/// Token decimals a challenge asks a payment to be valued at, bounded so a
/// `u64` base-unit amount always has a meaningful decimal rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decimals(u8);

impl Decimals {
    /// `u64::MAX` has 20 digits.
    pub(crate) const MAX: u8 = 19;

    pub(crate) fn new(decimals: u64) -> crate::Result<Self> {
        u8::try_from(decimals)
            .ok()
            .filter(|decimals| *decimals <= Self::MAX)
            .map(Self)
            .ok_or_else(|| {
                crate::Error::InvalidChallenge(format!(
                    "asset decimals {decimals} exceed the supported maximum of {}",
                    Self::MAX
                ))
            })
    }

    pub(crate) fn get(self) -> u8 {
        self.0
    }
}

/// Exact decimal rendering of `base` units at `decimals` places, without
/// trailing zeros (`1500000000`, 9 -> `1.5`).
pub(crate) fn format_base_units(base: u64, decimals: u32) -> String {
    let decimals = decimals as usize;
    let digits = format!("{base:0>width$}", width = decimals + 1);
    let (whole, fraction) = digits.split_at(digits.len() - decimals);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// Native SOL is not a USD amount. It is named in SOL so no USD spending
/// policy reads it as dollars; such a policy then refuses it as unpriced.
pub(crate) fn native_sol_display(base_units: &str, currency: &str) -> Option<String> {
    currency.eq_ignore_ascii_case("SOL").then(|| {
        let base = base_units.parse::<u64>().unwrap_or(0);
        format!("{} SOL", format_base_units(base, 9))
    })
}

/// The amount an authorization prompt shows, and that a USD spending policy
/// reads. Only known stablecoins are dollars, valued at the decimals the
/// payment is signed at; native SOL and other assets name their own unit and
/// carry no USD value, so such a policy refuses them as unpriced.
pub(crate) fn payment_display_amount(
    base_units: &str,
    currency: &str,
    decimals: Option<Decimals>,
) -> crate::Result<String> {
    if let Some(native) = native_sol_display(base_units, currency) {
        return Ok(native);
    }
    let base = base_units.parse::<u64>().unwrap_or(0);
    let stablecoin = pay_types::Stablecoin::from_mint(currency)
        .or_else(|| pay_types::Stablecoin::parse_symbol(currency));
    match (stablecoin, decimals.map(Decimals::get)) {
        (Some(coin), Some(decimals)) if decimals != coin.decimals() => {
            Err(crate::Error::PaymentRejected(format!(
                "{} has {} decimals, but the payment declares {decimals}",
                coin.symbol(),
                coin.decimals()
            )))
        }
        (Some(coin), _) => {
            // This text also feeds the spending policy. Render the signed
            // integer amount exactly: rounding even a sub-cent payment can
            // both mislead the user and understate the authorization amount.
            let mut amount = format_base_units(base, u32::from(coin.decimals()));
            if base != 0 {
                match amount.split_once('.').map(|(_, fraction)| fraction.len()) {
                    None => amount.push_str(".00"),
                    Some(1) => amount.push('0'),
                    _ => {}
                }
            }
            Ok(format!("${amount}"))
        }
        (None, Some(decimals)) => Ok(format!(
            "{} {}",
            format_base_units(base, u32::from(decimals)),
            asset_label(currency)
        )),
        (None, None) => Ok(format!("{base} base units of {}", asset_label(currency))),
    }
}

fn asset_label(currency: &str) -> String {
    match currency.char_indices().nth(8) {
        Some((end, _)) => format!("{}…", &currency[..end]),
        None => currency.to_string(),
    }
}

pub(crate) fn payment_description(
    challenge_description: Option<&str>,
    resource_urls: &[Option<&str>],
) -> String {
    if let Some(description) = meaningful_description(challenge_description) {
        return description.to_string();
    }

    resource_urls
        .iter()
        .filter_map(|url| url.and_then(api_label))
        .next()
        .map(|label| format!("accessing API {label}"))
        .unwrap_or_else(|| "accessing API".to_string())
}

fn meaningful_description(description: Option<&str>) -> Option<&str> {
    let description = description?.trim();
    if description.is_empty() || is_generic_api_access(description) {
        return None;
    }
    Some(description)
}

fn normalized_nonempty(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value.trim();
    (!value.is_empty()).then(|| display_reason(value))
}

fn display_reason(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

fn is_generic_api_access(description: &str) -> bool {
    description.eq_ignore_ascii_case("api access")
}

fn api_label(resource_url: &str) -> Option<String> {
    let resource_url = resource_url.trim();
    if resource_url.is_empty() {
        return None;
    }

    let domain = domain_from_url(resource_url)?;
    crate::skills::service_fqn_for_resource_url(resource_url).or(Some(domain))
}

fn operator_label(resource_url: &str) -> Option<String> {
    let domain = domain_from_url(resource_url)?;
    Some(operator_from_domain(&domain))
}

fn operator_from_domain(domain: &str) -> String {
    domain
        .strip_prefix("api.")
        .or_else(|| domain.strip_prefix("www."))
        .unwrap_or(domain)
        .to_string()
}

fn domain_from_url(resource_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(resource_url).ok()?;
    url.host_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_description_preserves_specific_challenge_description() {
        assert_eq!(
            payment_description(
                Some("Run a SQL query"),
                &[Some("https://api.example.com/v1/query")]
            ),
            "Run a SQL query"
        );
    }

    #[test]
    fn payment_description_replaces_generic_api_access_with_domain() {
        assert_eq!(
            payment_description(
                Some("API access"),
                &[Some("https://api.example.com/v1/query")]
            ),
            "accessing API api.example.com"
        );
    }

    #[test]
    fn payment_description_uses_domain_when_description_is_missing() {
        assert_eq!(
            payment_description(None, &[Some("https://api.example.com/v1/query")]),
            "accessing API api.example.com"
        );
    }

    #[test]
    fn payment_description_ignores_empty_resource_candidates() {
        assert_eq!(
            payment_description(Some("API access"), &[Some(""), None]),
            "accessing API"
        );
    }

    #[test]
    fn payment_prompt_context_uses_challenge_description_and_operator() {
        let context = payment_prompt_context(
            Some(" Run   a SQL query "),
            &[Some("https://api.gateway-402.com/v1/query")],
        );

        assert_eq!(context.reason, "run a SQL query");
        assert_eq!(context.operator, "gateway-402.com");
    }

    #[test]
    fn payment_prompt_context_lowercases_reason_first_letter() {
        let context = payment_prompt_context(
            Some(" Run   request "),
            &[Some("https://api.gateway-402.com/v1/query")],
        );

        assert_eq!(context.reason, "run request");
        assert_eq!(context.operator, "gateway-402.com");
    }

    #[test]
    fn payment_prompt_context_omits_empty_description() {
        let context = payment_prompt_context(Some("  "), &[Some("https://example.com/v1")]);

        assert_eq!(context.reason, "API access");
        assert_eq!(context.operator, "example.com");
    }

    #[test]
    fn format_base_units_is_exact() {
        assert_eq!(format_base_units(0, 9), "0");
        assert_eq!(format_base_units(1_000_000_000, 9), "1");
        assert_eq!(format_base_units(1, 9), "0.000000001");
        assert_eq!(format_base_units(1_500_000_000, 9), "1.5");
        assert_eq!(format_base_units(u64::MAX, 0), u64::MAX.to_string());
        assert_eq!(format_base_units(u64::MAX, 19), "1.8446744073709551615");
        assert_eq!(format_base_units(50_000_000, 2), "500000");
        // Decimals past any integer power of ten still render exactly.
        assert_eq!(
            format_base_units(15, 128),
            format!("0.{}15", "0".repeat(126))
        );
        assert_eq!(format_base_units(0, u32::from(u8::MAX)), "0");
    }

    #[test]
    fn absurd_decimals_are_an_invalid_challenge() {
        for decimals in [20, 128, 255, 256, u64::MAX] {
            assert!(matches!(
                Decimals::new(decimals),
                Err(crate::Error::InvalidChallenge(_))
            ));
        }
        // Positive control: the largest supported scale still renders.
        let max = Decimals::new(u64::from(Decimals::MAX)).unwrap();
        let mint = "Mint111111111111111111111111111111111111111";
        assert_eq!(
            payment_display_amount(&u64::MAX.to_string(), mint, Some(max)).unwrap(),
            "1.8446744073709551615 Mint1111…"
        );
    }

    #[test]
    fn native_sol_display_only_for_sol() {
        assert_eq!(
            native_sol_display("1000000000", "SOL").as_deref(),
            Some("1 SOL")
        );
        assert_eq!(
            native_sol_display("1", "sol").as_deref(),
            Some("0.000000001 SOL")
        );
        assert_eq!(native_sol_display("1000000", "USDC"), None);
    }

    #[test]
    fn stablecoins_are_dollars_at_their_own_decimals() {
        const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        assert_eq!(
            payment_display_amount("1000000", USDC, Some(Decimals::new(6).unwrap())).unwrap(),
            "$1.00"
        );
        assert_eq!(
            payment_display_amount("1000000", "USDC", None).unwrap(),
            "$1.00"
        );
        assert_eq!(
            payment_display_amount("1", USDC, None).unwrap(),
            "$0.000001"
        );
        // A stablecoin claimed at other decimals is refused outright.
        assert!(payment_display_amount("50000000", USDC, Some(Decimals::new(2).unwrap())).is_err());
    }

    #[test]
    fn stablecoin_authorization_preserves_every_base_unit() {
        for coin in pay_types::Stablecoin::ALL {
            for (base, expected) in [
                (0, "$0"),
                (1, "$0.000001"),
                (1_234, "$0.001234"),
                (50, "$0.00005"),
                (101, "$0.000101"),
                (500, "$0.0005"),
                (999, "$0.000999"),
                (1_000, "$0.001"),
                (1_001, "$0.001001"),
                (5_000, "$0.005"),
                (9_999, "$0.009999"),
                (10_000, "$0.01"),
                (10_001, "$0.010001"),
                (100_000, "$0.10"),
                (1_000_000, "$1.00"),
                (1_000_001, "$1.000001"),
                (1_001_500, "$1.0015"),
                (1_500_000, "$1.50"),
                (9_007_199_254_740_993, "$9007199254.740993"),
                (u64::MAX, "$18446744073709.551615"),
            ] {
                for currency in [coin.symbol(), coin.mint(Some("mainnet"))] {
                    let shown = payment_display_amount(
                        &base.to_string(),
                        currency,
                        Some(Decimals::new(u64::from(coin.decimals())).unwrap()),
                    )
                    .unwrap();
                    assert_eq!(shown, expected, "{currency}: {base} base units");
                }
            }
        }
    }

    #[test]
    fn stablecoin_authorization_does_not_understate_the_spending_policy_amount() {
        let shown = payment_display_amount("1234", "USDC", None).unwrap();
        let intent = crate::keystore::AuthIntent::authorize_payment_details(&shown, "api", "op");
        // USD policy amounts round up to 1/10,000 dollar units, so this
        // payment must not fit a $0.001 (10-unit) cap.
        assert_eq!(intent.amount_minor_units(), Some(13));
    }

    #[test]
    fn other_assets_name_their_unit_and_carry_no_usd_value() {
        let mint = "Mint111111111111111111111111111111111111111";
        let shown =
            payment_display_amount("50000000", mint, Some(Decimals::new(2).unwrap())).unwrap();
        assert_eq!(shown, "500000 Mint1111…");
        let intent = crate::keystore::AuthIntent::authorize_payment_details(&shown, "api", "op");
        assert_eq!(intent.amount_minor_units(), None);

        let shown = payment_display_amount("7", mint, None).unwrap();
        assert_eq!(shown, "7 base units of Mint1111…");
        assert_eq!(
            crate::keystore::AuthIntent::authorize_payment_details(&shown, "api", "op")
                .amount_minor_units(),
            None
        );
        assert_eq!(
            payment_display_amount("1000000000", "SOL", Some(Decimals::new(9).unwrap())).unwrap(),
            "1 SOL"
        );
    }
}
