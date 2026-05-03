use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub mod metering;
pub mod registry;
pub mod splits;

/// Well-known mint addresses for supported Solana stablecoins.
pub mod stablecoin_mints {
    pub const USDC_MAINNET: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const USDC_DEVNET: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
    pub const USDC_TESTNET: &str = USDC_DEVNET;
    pub const USDT_MAINNET: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
    pub const PYUSD_MAINNET: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";
    pub const PYUSD_DEVNET: &str = "CXk2AMBfi3TwaEL2468s6zP8xq9NxTXjp9gjMgzeUynM";
    pub const PYUSD_TESTNET: &str = PYUSD_DEVNET;
    pub const CASH_MAINNET: &str = "CASHx9KJUStyftLFWGvEVf59SGeG9sh5FfcnZMVPCASH";
    pub const USDG_MAINNET: &str = "2u1tszSeqZ3qBWF3uNGPFc8TzMk2tdiwknnRMWGWjGWH";
}

/// Stablecoins supported by `pay send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stablecoin {
    Usdc,
    Usdt,
    Pyusd,
    Cash,
    Usdg,
}

impl Stablecoin {
    pub const ALL: [Self; 5] = [Self::Usdc, Self::Usdt, Self::Pyusd, Self::Cash, Self::Usdg];
    pub const SYMBOL_LIST: &'static str = "USDC, USDT, PYUSD, CASH, or USDG";

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Usdc => "USDC",
            Self::Usdt => "USDT",
            Self::Pyusd => "PYUSD",
            Self::Cash => "CASH",
            Self::Usdg => "USDG",
        }
    }

    pub fn mint(self, network: Option<&str>) -> &'static str {
        match self {
            Self::Usdc => match network {
                Some("devnet") => stablecoin_mints::USDC_DEVNET,
                Some("testnet") => stablecoin_mints::USDC_TESTNET,
                _ => stablecoin_mints::USDC_MAINNET,
            },
            Self::Usdt => stablecoin_mints::USDT_MAINNET,
            Self::Pyusd => match network {
                Some("devnet") => stablecoin_mints::PYUSD_DEVNET,
                Some("testnet") => stablecoin_mints::PYUSD_TESTNET,
                _ => stablecoin_mints::PYUSD_MAINNET,
            },
            Self::Cash => stablecoin_mints::CASH_MAINNET,
            Self::Usdg => stablecoin_mints::USDG_MAINNET,
        }
    }

    pub fn symbol_for_mint(mint: &str) -> Option<&'static str> {
        Self::from_mint(mint).map(Self::symbol)
    }

    pub fn from_mint(mint: &str) -> Option<Self> {
        match mint {
            stablecoin_mints::USDC_MAINNET | stablecoin_mints::USDC_DEVNET => Some(Self::Usdc),
            stablecoin_mints::USDT_MAINNET => Some(Self::Usdt),
            stablecoin_mints::PYUSD_MAINNET | stablecoin_mints::PYUSD_DEVNET => Some(Self::Pyusd),
            stablecoin_mints::CASH_MAINNET => Some(Self::Cash),
            stablecoin_mints::USDG_MAINNET => Some(Self::Usdg),
            _ => None,
        }
    }

    pub fn parse_symbol(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "USDC" => Some(Self::Usdc),
            "USDT" => Some(Self::Usdt),
            "PYUSD" => Some(Self::Pyusd),
            "CASH" => Some(Self::Cash),
            "USDG" => Some(Self::Usdg),
            _ => None,
        }
    }
}

impl std::fmt::Display for Stablecoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.symbol())
    }
}

impl FromStr for Stablecoin {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse_symbol(value).ok_or_else(|| {
            format!(
                "`pay send` sends stablecoins only; choose {}",
                Self::SYMBOL_LIST
            )
        })
    }
}

/// Represents an HTTP 402 payment challenge returned by a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentChallenge {
    /// The URL that requires payment.
    pub resource_url: String,
    /// The payment endpoint to submit payment to.
    pub payment_url: String,
    /// Amount required in the smallest unit (e.g., satoshis, lamports).
    pub amount: u64,
    /// Currency or token identifier (e.g., "USD", "SOL", "BTC").
    pub currency: String,
    /// Human-readable description of what is being purchased.
    #[serde(default)]
    pub description: Option<String>,
}

/// The result of a successful payment, containing a receipt token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentReceipt {
    /// Opaque token proving payment was made.
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_challenge_serde_roundtrip() {
        let challenge = PaymentChallenge {
            resource_url: "https://api.example.com/data".to_string(),
            payment_url: "https://pay.example.com".to_string(),
            amount: 1000,
            currency: "USDC".to_string(),
            description: Some("API access".to_string()),
        };
        let json = serde_json::to_string(&challenge).unwrap();
        let back: PaymentChallenge = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resource_url, challenge.resource_url);
        assert_eq!(back.payment_url, challenge.payment_url);
        assert_eq!(back.amount, challenge.amount);
        assert_eq!(back.currency, challenge.currency);
        assert_eq!(back.description, challenge.description);
    }

    #[test]
    fn payment_challenge_without_description() {
        let json = r#"{"resource_url":"https://a.com","payment_url":"https://b.com","amount":500,"currency":"SOL"}"#;
        let challenge: PaymentChallenge = serde_json::from_str(json).unwrap();
        assert_eq!(challenge.amount, 500);
        assert!(challenge.description.is_none());
    }

    #[test]
    fn payment_receipt_serde_roundtrip() {
        let receipt = PaymentReceipt {
            token: "receipt_token_123".to_string(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let back: PaymentReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token, receipt.token);
    }

    #[test]
    fn stablecoin_parses_supported_symbols() {
        assert_eq!("usdc".parse::<Stablecoin>().unwrap(), Stablecoin::Usdc);
        assert_eq!("USDG".parse::<Stablecoin>().unwrap(), Stablecoin::Usdg);
    }

    #[test]
    fn stablecoin_resolves_known_mints() {
        assert_eq!(
            Stablecoin::Usdg.mint(Some("mainnet")),
            stablecoin_mints::USDG_MAINNET
        );
        assert_eq!(
            Stablecoin::from_mint(stablecoin_mints::USDG_MAINNET),
            Some(Stablecoin::Usdg)
        );
    }
}
