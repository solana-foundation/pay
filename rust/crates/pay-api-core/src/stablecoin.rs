//! Stablecoin balance fetching.
//!
//! The registry is config-driven: callers pass a slice of [`Stablecoin`]s built
//! from YAML (via [`StablecoinSpec::resolve`]). One `getMultipleAccounts` call
//! retrieves every ATA at once; the SPL Token / Token-2022 amount field at
//! offset 64..72 (u64 LE) gives the raw balance — Token-2022 only adds
//! *trailing* extension TLV data after the base 165 bytes, so the layout is
//! identical for our purposes.

use std::collections::BTreeMap;
use std::str::FromStr;

use pay_api_types::{Network, StablecoinBalance, StablecoinBalances};
use pay_kit::core::payment_channels::PAYMENT_CHANNELS_PROGRAM_ID;
use pay_kit::generated::payment_channels::accounts::Channel;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::ata::{SPL_TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, associated_token_address};
use crate::error::{Error, Result};
use crate::rpc::RpcClient;

/// Which token program a mint lives under. Maps to the canonical program ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenProgram {
    /// Original SPL Token program (`TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`).
    #[serde(rename = "spl_token")]
    SplToken,
    /// Token-2022 / Token Extensions (`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`).
    #[serde(rename = "token_2022", alias = "token2022")]
    Token2022,
}

impl TokenProgram {
    pub fn program_id(self) -> Pubkey {
        match self {
            Self::SplToken => SPL_TOKEN_PROGRAM_ID,
            Self::Token2022 => TOKEN_2022_PROGRAM_ID,
        }
    }
}

/// Config-shaped entry — what YAML deserialises into. `mint` is a base58 string
/// here so unparsable mints fail loudly at startup, not per-request.
#[derive(Debug, Clone, Deserialize)]
pub struct StablecoinSpec {
    pub symbol: String,
    pub mint: String,
    pub token_program: TokenProgram,
    pub decimals: u8,
}

impl StablecoinSpec {
    pub fn resolve(&self) -> Result<Stablecoin> {
        let mint = Pubkey::from_str(&self.mint).map_err(|_| Error::InvalidMint {
            symbol: self.symbol.clone(),
            mint: self.mint.clone(),
        })?;
        Ok(Stablecoin {
            symbol: self.symbol.clone(),
            mint,
            token_program: self.token_program.program_id(),
            decimals: self.decimals,
        })
    }
}

/// Runtime registry entry — pubkeys are pre-parsed for cheap per-request use.
#[derive(Debug, Clone)]
pub struct Stablecoin {
    pub symbol: String,
    pub mint: Pubkey,
    pub token_program: Pubkey,
    pub decimals: u8,
}

/// Fetch every configured stablecoin balance for `owner` against `rpc_url`.
///
/// One round trip: derives the ATAs locally, then calls
/// `getMultipleAccounts` once with all of them.
pub async fn fetch_stablecoin_balances(
    client: &RpcClient,
    rpc_url: &str,
    owner: &Pubkey,
    network: Network,
    coins: &[Stablecoin],
) -> Result<StablecoinBalances> {
    let ata_strs: Vec<String> = coins
        .iter()
        .map(|c| associated_token_address(owner, &c.mint, &c.token_program).to_string())
        .collect();

    let accounts = client.get_multiple_accounts(rpc_url, &ata_strs).await?;

    let mut balances = Vec::with_capacity(coins.len());
    for (coin, account) in coins.iter().zip(accounts.iter()) {
        let raw = match account {
            Some(data) => parse_token_amount(data)?,
            None => 0,
        };
        balances.push(StablecoinBalance {
            symbol: coin.symbol.clone(),
            mint: coin.mint.to_string(),
            decimals: coin.decimals,
            raw_amount: raw.to_string(),
            ui_amount: ui_amount(raw, coin.decimals),
        });
    }

    Ok(StablecoinBalances {
        address: owner.to_string(),
        network,
        balances,
        committable_channel_balances: Vec::new(),
        channel_balances_unavailable: false,
        credits: BTreeMap::new(),
        credits_unavailable: false,
    })
}

const CHANNEL_ACCOUNT_SIZE: usize = 256;
const CHANNEL_PAYER_OFFSET: usize = 88;
const CHANNEL_STATUS_OPEN: u8 = 0;

/// Sum the escrow that can still be committed in all open channels owned by
/// `payer`. A payer memcmp keeps the scan scoped to one wallet; decoding then
/// filters by status and configured stablecoin mint.
pub async fn fetch_committable_channel_balances(
    client: &RpcClient,
    rpc_url: &str,
    payer: &Pubkey,
    coins: &[Stablecoin],
) -> Result<Vec<StablecoinBalance>> {
    let accounts = client
        .get_program_accounts_filtered(
            rpc_url,
            PAYMENT_CHANNELS_PROGRAM_ID,
            CHANNEL_ACCOUNT_SIZE,
            CHANNEL_PAYER_OFFSET,
            payer.as_ref(),
        )
        .await?;
    let mut totals = BTreeMap::<Pubkey, u64>::new();
    for account in accounts {
        let channel = Channel::from_bytes(&account.data).map_err(|_| Error::RpcMalformed)?;
        if channel.status != CHANNEL_STATUS_OPEN {
            continue;
        }
        let mint = Pubkey::from(channel.mint.to_bytes());
        let available = channel.deposit.saturating_sub(channel.settlement.settled);
        let total = totals.entry(mint).or_default();
        *total = total.saturating_add(available);
    }

    Ok(coins
        .iter()
        .map(|coin| {
            let raw = totals.get(&coin.mint).copied().unwrap_or_default();
            StablecoinBalance {
                symbol: coin.symbol.clone(),
                mint: coin.mint.to_string(),
                decimals: coin.decimals,
                raw_amount: raw.to_string(),
                ui_amount: ui_amount(raw, coin.decimals),
            }
        })
        .collect())
}

/// SPL Token / Token-2022 base account: amount is a u64 LE at offset 64.
fn parse_token_amount(data: &[u8]) -> Result<u64> {
    if data.len() < 72 {
        return Err(Error::TokenAccountDecode);
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[64..72]);
    Ok(u64::from_le_bytes(buf))
}

fn ui_amount(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_kit::generated::payment_channels::types::SettlementWatermarks;

    #[test]
    fn parse_amount_zero() {
        let mut data = vec![0u8; 165];
        assert_eq!(parse_token_amount(&data).unwrap(), 0);
        // 1_000_000 raw = 1.0 USDC
        data[64..72].copy_from_slice(&1_000_000u64.to_le_bytes());
        assert_eq!(parse_token_amount(&data).unwrap(), 1_000_000);
    }

    #[test]
    fn parse_amount_too_short() {
        assert!(parse_token_amount(&[0u8; 32]).is_err());
    }

    #[test]
    fn ui_amount_six_decimals() {
        assert!((ui_amount(1_500_000, 6) - 1.5).abs() < f64::EPSILON);
        assert_eq!(ui_amount(0, 6), 0.0);
    }

    #[test]
    fn token_program_program_ids() {
        assert_eq!(TokenProgram::SplToken.program_id(), SPL_TOKEN_PROGRAM_ID);
        assert_eq!(TokenProgram::Token2022.program_id(), TOKEN_2022_PROGRAM_ID);
    }

    #[test]
    fn spec_resolves_valid_mint() {
        let spec = StablecoinSpec {
            symbol: "USDC".into(),
            mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            token_program: TokenProgram::SplToken,
            decimals: 6,
        };
        let coin = spec.resolve().unwrap();
        assert_eq!(coin.symbol, "USDC");
        assert_eq!(coin.token_program, SPL_TOKEN_PROGRAM_ID);
        assert_eq!(coin.decimals, 6);
    }

    #[test]
    fn spec_rejects_invalid_mint() {
        let spec = StablecoinSpec {
            symbol: "BAD".into(),
            mint: "not-base58!".into(),
            token_program: TokenProgram::SplToken,
            decimals: 6,
        };
        assert!(spec.resolve().is_err());
    }

    #[test]
    fn token_program_serde() {
        // YAML / JSON shape is snake_case.
        let p: TokenProgram = serde_json::from_str("\"spl_token\"").unwrap();
        assert_eq!(p, TokenProgram::SplToken);
        let p: TokenProgram = serde_json::from_str("\"token_2022\"").unwrap();
        assert_eq!(p, TokenProgram::Token2022);
    }

    #[test]
    fn channel_scan_constants_match_generated_account_layout() {
        let payer = Pubkey::new_unique();
        let address = |value: Pubkey| solana_address::Address::from(value.to_bytes());
        let channel = Channel {
            discriminator: 1,
            version: 1,
            bump: 255,
            status: CHANNEL_STATUS_OPEN,
            salt: 7,
            deposit: 1_000_000,
            settlement: SettlementWatermarks {
                settled: 250_000,
                payout_watermark: 100_000,
            },
            closure_started_at: 0,
            payer_withdrawn_at: 0,
            grace_period: 60,
            distribution_hash: [0; 32],
            payer: address(payer),
            payee: address(Pubkey::new_unique()),
            authorized_signer: address(Pubkey::new_unique()),
            mint: address(Pubkey::new_unique()),
            rent_payer: address(Pubkey::new_unique()),
            open_slot: 42,
        };
        let encoded = borsh::to_vec(&channel).unwrap();

        assert_eq!(encoded.len(), CHANNEL_ACCOUNT_SIZE);
        assert_eq!(
            &encoded[CHANNEL_PAYER_OFFSET..CHANNEL_PAYER_OFFSET + 32],
            payer.as_ref()
        );
        let decoded = Channel::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.deposit - decoded.settlement.settled, 750_000);
    }
}
