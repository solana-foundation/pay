//! Dev mode: generate a funded keypair on localnet via surfpool cheatcodes.

use std::io::Write;

use tempfile::NamedTempFile;
use tracing::info;

use crate::{Error, Result};

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// A dev keypair with its temp file (kept alive for the process lifetime).
pub struct DevKeypair {
    /// Path to the JSON keypair file.
    pub path: String,
    /// Base58 public key.
    pub pubkey: String,
    /// Keep the temp file alive.
    _file: NamedTempFile,
}

/// Generate a fresh keypair, fund it on localnet, and return the path.
///
/// Uses surfpool cheatcodes:
/// - `surfnet_setAccount` for 100 SOL
/// - `surfnet_setTokenAccount` for 1000 USDC
pub fn setup_dev_keypair(rpc_url: &str) -> Result<DevKeypair> {
    // Generate 64 random bytes (32 secret + 32 public via ed25519)
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let mut keypair_bytes = Vec::with_capacity(64);
    keypair_bytes.extend_from_slice(&signing_key.to_bytes());
    keypair_bytes.extend_from_slice(&verifying_key.to_bytes());

    // Write as JSON array to a temp file (standard solana-keygen format)
    let mut file = NamedTempFile::new()?;
    let json: Vec<u8> = keypair_bytes.clone();
    write!(file, "{}", serde_json::to_string(&json)?)?;

    let path = file.path().to_string_lossy().to_string();

    // Derive pubkey (base58)
    let pubkey = bs58::encode(&verifying_key.to_bytes()).into_string();

    info!(pubkey = %pubkey, "Generated dev keypair");

    // Check surfpool is reachable before attempting to fund
    check_surfpool(rpc_url)?;

    // Fund via surfpool cheatcodes
    fund_sol(rpc_url, &pubkey)?;
    fund_usdc(rpc_url, &pubkey)?;

    info!(pubkey = %pubkey, "Dev keypair funded (100 SOL + 1000 USDC)");

    Ok(DevKeypair {
        path,
        pubkey,
        _file: file,
    })
}

fn check_surfpool(rpc_url: &str) -> Result<()> {
    let resp = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getHealth",
        }))
        .send();

    if resp.is_err() {
        return Err(Error::Config(format!(
            "Could not connect to Surfpool at {rpc_url}\n\n\
             Install and start Surfpool:\n\n  \
             # Install Surfpool CLI\n  \
             curl -sL https://run.surfpool.run/ | bash\n\n  \
             # Start local Solana network\n  \
             surfpool start\n"
        )));
    }

    Ok(())
}

fn rpc_call(rpc_url: &str, method: &str, params: serde_json::Value) -> Result<()> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let resp = reqwest::blocking::Client::new()
        .post(rpc_url)
        .json(&body)
        .send()
        .map_err(|e| Error::Config(format!("RPC call to {rpc_url} failed: {e}")))?;

    let result: serde_json::Value = resp
        .json()
        .map_err(|e| Error::Config(format!("Invalid RPC response: {e}")))?;

    if let Some(err) = result.get("error") {
        return Err(Error::Config(format!("RPC error: {err}")));
    }

    Ok(())
}

fn fund_sol(rpc_url: &str, pubkey: &str) -> Result<()> {
    rpc_call(
        rpc_url,
        "surfnet_setAccount",
        serde_json::json!([pubkey, {
            "lamports": 100_000_000_000_u64,
            "data": "",
            "executable": false,
            "owner": "11111111111111111111111111111111",
        }]),
    )
}

fn fund_usdc(rpc_url: &str, pubkey: &str) -> Result<()> {
    rpc_call(
        rpc_url,
        "surfnet_setTokenAccount",
        serde_json::json!([pubkey, USDC_MINT, {
            "amount": 1_000_000_000_u64,
        }]),
    )
}
