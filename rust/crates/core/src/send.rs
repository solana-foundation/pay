//! Send SOL to a recipient address.

use solana_message::Message;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_rpc_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;
use tracing::info;

use crate::{Error, Result};

/// Result of a successful send.
pub struct SendResult {
    /// Transaction signature (base-58).
    pub signature: String,
    /// Amount sent in lamports.
    pub lamports: u64,
    /// Sender public key (base-58).
    pub from: String,
    /// Recipient public key (base-58).
    pub to: String,
}

/// Send SOL to `recipient`.
///
/// - `amount_str`: either a decimal SOL amount (e.g. `"0.1"`) or `"*"` to drain the account
///   (leaving enough for the transaction fee).
/// - `recipient`: base-58 public key of the recipient.
/// - `keypair_source`: keypair source string (file path or `keychain:default`).
/// - `rpc_url`: Solana RPC endpoint.
pub fn send_sol(
    amount_str: &str,
    recipient: &str,
    keypair_source: &str,
    rpc_url: &str,
) -> Result<SendResult> {
    let reason = format!("send SOL to {recipient}");
    let signer = crate::signer::load_signer_with_reason(keypair_source, &reason)?;
    let rpc = RpcClient::new(rpc_url.to_string());

    let sender_pubkey = signer.pubkey();

    let recipient_pubkey: Pubkey = recipient
        .parse()
        .map_err(|e| Error::Config(format!("Invalid recipient address: {e}")))?;

    let lamports = if amount_str == "*" {
        // Drain: get balance, subtract estimated fee (5000 lamports)
        let balance = rpc
            .get_balance(&sender_pubkey)
            .map_err(|e| Error::Config(format!("Failed to get balance: {e}")))?;
        let fee = 5000u64;
        if balance <= fee {
            return Err(Error::Config(format!(
                "Insufficient balance: {balance} lamports (need > {fee} for fee)"
            )));
        }
        balance - fee
    } else {
        parse_sol_amount(amount_str)?
    };

    if lamports == 0 {
        return Err(Error::Config("Amount must be greater than 0".to_string()));
    }

    info!(
        lamports,
        from = %sender_pubkey,
        to = %recipient,
        sol = format!("{:.9}", lamports as f64 / 1_000_000_000.0),
        "Sending SOL"
    );

    let ix = system_instruction::transfer(&sender_pubkey, &recipient_pubkey, lamports);

    let blockhash = rpc
        .get_latest_blockhash()
        .map_err(|e| Error::Config(format!("Failed to get blockhash: {e}")))?;

    let message = Message::new_with_blockhash(&[ix], Some(&sender_pubkey), &blockhash);
    let mut tx = Transaction::new_unsigned(message);

    // Sign
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Config(format!("Failed to create runtime: {e}")))?;

    let sig_bytes = rt
        .block_on(signer.sign_message(&tx.message_data()))
        .map_err(|e| Error::Config(format!("Signing failed: {e}")))?;

    let sig = Signature::from(<[u8; 64]>::from(sig_bytes));
    let signer_index = tx
        .message
        .account_keys
        .iter()
        .position(|k| k == &sender_pubkey)
        .ok_or_else(|| Error::Config("Signer not found in transaction".to_string()))?;
    tx.signatures[signer_index] = sig;

    // Send and confirm
    let signature = rpc
        .send_and_confirm_transaction(&tx)
        .map_err(|e| Error::Config(format!("Transaction failed: {e}")))?;

    Ok(SendResult {
        signature: signature.to_string(),
        lamports,
        from: sender_pubkey.to_string(),
        to: recipient.to_string(),
    })
}

/// Parse a human-friendly SOL amount into lamports.
fn parse_sol_amount(s: &str) -> Result<u64> {
    let sol: f64 = s
        .parse()
        .map_err(|_| Error::Config(format!("Invalid amount: {s}")))?;
    if sol < 0.0 {
        return Err(Error::Config("Amount must be positive".to_string()));
    }
    Ok((sol * 1_000_000_000.0) as u64)
}
