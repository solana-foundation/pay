//! On-chain program-backed credit allowances.
//!
//! Coinflow's credit program stores one fixed-size allowance account per user:
//! an 8-byte discriminator, the 32-byte user pubkey, an 8-byte little-endian
//! amount, and an 8-byte trailing derivation value. Filtering program accounts
//! by the user bytes avoids transaction-history scans and does not confuse the
//! allowance with wallet-owned SPL token balances.

use std::collections::BTreeMap;
use std::str::FromStr;

use pay_api_types::{CreditBalance, Network};
use serde::Deserialize;
use solana_pubkey::Pubkey;

use crate::error::{Error, Result};
use crate::rpc::RpcClient;

const ACCOUNT_SIZE: usize = 56;
const USER_OFFSET: usize = 8;
const AMOUNT_OFFSET: usize = 40;

/// Config-shaped credit program entry.
#[derive(Debug, Clone, Deserialize)]
pub struct CreditProgramSpec {
    pub network: Network,
    pub program_id: String,
    pub currency: String,
    pub decimals: u8,
}

impl CreditProgramSpec {
    pub fn resolve(&self) -> Result<CreditProgram> {
        let program_id = Pubkey::from_str(&self.program_id)
            .map_err(|_| Error::InvalidCreditProgram(self.program_id.clone()))?;
        Ok(CreditProgram {
            network: self.network,
            program_id,
            currency: self.currency.clone(),
            decimals: self.decimals,
        })
    }
}

/// Validated runtime credit program entry.
#[derive(Debug, Clone)]
pub struct CreditProgram {
    pub network: Network,
    pub program_id: Pubkey,
    pub currency: String,
    pub decimals: u8,
}

/// Fetch the configured credit allowances for `owner` directly from RPC.
pub async fn fetch_credit_balances(
    client: &RpcClient,
    rpc_url: &str,
    owner: &Pubkey,
    network: Network,
    programs: &[CreditProgram],
) -> Result<BTreeMap<String, CreditBalance>> {
    let mut balances = BTreeMap::new();
    for program in programs.iter().filter(|program| program.network == network) {
        let accounts = client
            .get_program_accounts_filtered(
                rpc_url,
                &program.program_id.to_string(),
                ACCOUNT_SIZE,
                USER_OFFSET,
                owner.as_ref(),
            )
            .await?;
        if accounts.is_empty() {
            continue;
        }

        let mut raw = 0u64;
        let mut account_ids = Vec::with_capacity(accounts.len());
        for account in accounts {
            raw = raw
                .checked_add(parse_credit_amount(&account.data, owner)?)
                .ok_or(Error::CreditAccountDecode)?;
            account_ids.push(account.pubkey);
        }
        account_ids.sort();
        balances.insert(
            program.program_id.to_string(),
            CreditBalance {
                accounts: account_ids,
                currency: program.currency.clone(),
                decimals: program.decimals,
                raw_amount: raw.to_string(),
                ui_amount: raw as f64 / 10f64.powi(program.decimals as i32),
            },
        );
    }
    Ok(balances)
}

fn parse_credit_amount(data: &[u8], owner: &Pubkey) -> Result<u64> {
    if data.len() != ACCOUNT_SIZE || data[USER_OFFSET..AMOUNT_OFFSET] != owner.to_bytes() {
        return Err(Error::CreditAccountDecode);
    }
    let bytes: [u8; 8] = data[AMOUNT_OFFSET..AMOUNT_OFFSET + 8]
        .try_into()
        .map_err(|_| Error::CreditAccountDecode)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_observed_coinflow_credit_layout() {
        let owner = Pubkey::from_str("9yWVAaQv6Nr2TPaoFQM7wcsWdPsQwZXmzxqhDJWVq49D").unwrap();
        let mut data = vec![0u8; ACCOUNT_SIZE];
        data[USER_OFFSET..AMOUNT_OFFSET].copy_from_slice(owner.as_ref());
        data[AMOUNT_OFFSET..AMOUNT_OFFSET + 8].copy_from_slice(&5_000_000u64.to_le_bytes());
        data[48..56].copy_from_slice(&254u64.to_le_bytes());
        assert_eq!(parse_credit_amount(&data, &owner).unwrap(), 5_000_000);
    }

    #[test]
    fn rejects_wrong_owner_and_size() {
        let owner = Pubkey::new_unique();
        let mut data = vec![0u8; ACCOUNT_SIZE];
        data[USER_OFFSET..AMOUNT_OFFSET].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(parse_credit_amount(&data, &owner).is_err());
        assert!(parse_credit_amount(&data[..55], &owner).is_err());
    }
}
