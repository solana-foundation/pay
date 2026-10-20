//! Channel decoding, distribution-preimage recovery, and instruction building
//! for the payment-channels program.
//!
//! Bound sessions use pay-kit's ownership-aware, versioned decoder. Existing
//! fleet jobs remain V1-only and reject V2 rather than decoding its V1 prefix.
//! ATA derivation and RPC access come from pay-api-core.

use std::str::FromStr;

use pay_api_core::ata::{SPL_TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, associated_token_address};
use pay_api_core::rpc::RpcClient;
use pay_kit::core::payment_channels::{default_program_id, find_event_authority_pda, to_address};
use pay_kit::core::payment_channels_v2::Deployment;
use pay_kit::generated::payment_channels::generated::accounts::Channel;
use pay_kit::generated::payment_channels::generated::instructions::{
    DistributeBuilder, ReclaimBuilder, RequestCloseBuilder, SealBuilder, SettleAndSealBuilder,
    TopUpBuilder, WithdrawPayerBuilder,
};
use pay_kit::generated::payment_channels::generated::types::{
    DistributeArgs, DistributionEntry, SettleAndSealArgs, TopUpArgs,
};
use pay_kit::mpp::server::session::SessionConfig;
use sha2::{Digest, Sha256};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use crate::error::JobError;

/// `Sysvar1nstructions1111111111111111111111111` — needed by
/// `settle_and_seal`.
pub const INSTRUCTIONS_SYSVAR: Pubkey =
    solana_pubkey::pubkey!("Sysvar1nstructions1111111111111111111111111");

/// Channel status byte values.
pub const STATUS_OPEN: u8 = 0;
pub const STATUS_SEALED: u8 = 1;
pub const STATUS_CLOSING: u8 = 2;
pub const STATUS_DISTRIBUTED: u8 = 3;

/// Fixed serialized size of the current epoch-addressed Channel account.
pub const CHANNEL_ACCOUNT_SIZE: usize = 256;

/// Accounts `distribute` names before its recipient-ATA tail.
const DISTRIBUTE_FIXED_ACCOUNTS: usize = 11;

/// Keys a payer-funded refund-account create adds beyond `distribute`'s own:
/// the associated-token and system programs.
const REFUND_ACCOUNT_CREATE_KEYS: usize = 2;

/// The most recipients whose distribution still fits one transaction on its
/// own: a version-1 message's static keys hold the fixed `distribute`
/// accounts, a payer-funded refund-account create, a distinct fee payer and
/// one ATA per recipient. A larger plan cannot be paid out by this worker, so recovery
/// reports it instead of retrying a transaction that can never be built.
pub const MAX_DISTRIBUTION_RECIPIENTS: usize = pay_kit::core::tx::TxVersion::V1
    .limits()
    .max_static_accounts
    - DISTRIBUTE_FIXED_ACCOUNTS
    - REFUND_ACCOUNT_CREATE_KEYS
    - 1;

/// Chain discovery cannot prove a channel was never deployment-bound. Only
/// an extant legacy row permits the fleet reclamation path.
pub fn is_recorded_legacy(
    state: Option<&pay_kit::core::store::ChannelState>,
) -> Result<bool, JobError> {
    match state {
        None => Ok(false),
        Some(state) => pay_kit::mpp::server::session::channel_binding(state)
            .map(|binding| binding.is_none())
            .map_err(|error| JobError::Config(format!("channel ownership: {error}"))),
    }
}

/// Bound payouts require positive mint-owner evidence; the legacy resolver's
/// SPL fallback is not an ownership proof.
pub async fn require_token_program(
    rpc: &RpcClient,
    rpc_url: &str,
    mint: &Pubkey,
    expected: &Pubkey,
) -> Result<(), JobError> {
    let accounts = rpc
        .get_multiple_accounts_with_owner(rpc_url, &[mint.to_string()])
        .await?;
    if !matches!(*expected, SPL_TOKEN_PROGRAM_ID | TOKEN_2022_PROGRAM_ID)
        || accounts
            .into_iter()
            .next()
            .flatten()
            .is_none_or(|account| account.owner != expected.to_string())
    {
        return Err(JobError::Config(
            "bound mint token-program ownership mismatch".into(),
        ));
    }
    Ok(())
}

/// Resolve the token program that owns `mint` (SPL Token vs Token-2022) by
/// reading the mint account's owner. Falls back to SPL Token if unknown.
pub async fn resolve_token_program(
    rpc: &RpcClient,
    rpc_url: &str,
    mint: &Pubkey,
) -> Result<Pubkey, JobError> {
    let accounts = rpc
        .get_multiple_accounts_with_owner(rpc_url, &[mint.to_string()])
        .await?;
    let owner = accounts
        .into_iter()
        .next()
        .flatten()
        .map(|a| a.owner)
        .unwrap_or_default();
    Ok(match owner.as_str() {
        s if s == TOKEN_2022_PROGRAM_ID.to_string() => TOKEN_2022_PROGRAM_ID,
        _ => SPL_TOKEN_PROGRAM_ID,
    })
}

/// A decoded live channel plus its account address.
#[derive(Debug, Clone)]
pub struct DecodedChannel {
    pub address: Pubkey,
    pub channel: Channel,
}

impl DecodedChannel {
    pub fn payer(&self) -> Pubkey {
        Pubkey::from(self.channel.payer.to_bytes())
    }
    pub fn payee(&self) -> Pubkey {
        Pubkey::from(self.channel.payee.to_bytes())
    }
    pub fn rent_payer(&self) -> Pubkey {
        Pubkey::from(self.channel.rent_payer.to_bytes())
    }
    pub fn mint(&self) -> Pubkey {
        Pubkey::from(self.channel.mint.to_bytes())
    }
    pub fn open_slot(&self) -> u64 {
        self.channel.open_slot
    }
    /// UNIX deadline after which a CLOSING channel can be permissionlessly
    /// sealed: `closure_started_at + grace_period`.
    pub fn close_deadline(&self) -> i64 {
        self.channel
            .closure_started_at
            .saturating_add(i64::from(self.channel.grace_period))
    }
}

/// Discover every `Distributed` channel directly from chain state. This is the
/// durable reclaim queue: a proxy restart cannot lose an in-process timer.
pub async fn discover_distributed_channels(
    rpc: &RpcClient,
    rpc_url: &str,
) -> Result<Vec<Pubkey>, JobError> {
    let program_id = default_program_id();
    let accounts = rpc
        .get_program_accounts_filtered(
            rpc_url,
            &program_id.to_string(),
            CHANNEL_ACCOUNT_SIZE,
            3,
            &[STATUS_DISTRIBUTED],
        )
        .await?;
    accounts
        .into_iter()
        .map(|account| {
            Pubkey::from_str(&account.pubkey).map_err(|_| JobError::InvalidAddress(account.pubkey))
        })
        .collect()
}

/// Fetch and borsh-decode a channel account. Returns `Ok(None)` when the
/// account doesn't exist, isn't owned by the payment-channels program, or
/// isn't a decodable `Channel` (e.g. a 1-byte tombstone / `ClosedChannel`).
pub async fn fetch_channel(
    rpc: &RpcClient,
    rpc_url: &str,
    address: &Pubkey,
) -> Result<Option<DecodedChannel>, JobError> {
    Ok(fetch_channels(rpc, rpc_url, &[*address])
        .await?
        .into_iter()
        .next()
        .flatten())
}

/// Fetch a V1 channel using an explicit program. V2 requires `fetch_session_channel`.
pub async fn fetch_channel_for_program(
    rpc: &RpcClient,
    rpc_url: &str,
    address: &Pubkey,
    program_id: &Pubkey,
) -> Result<Option<DecodedChannel>, JobError> {
    let accounts = rpc
        .get_multiple_accounts_with_owner(rpc_url, &[address.to_string()])
        .await?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok(None);
    };
    if account.owner != program_id.to_string() {
        return Err(JobError::Config("session channel program mismatch".into()));
    }
    decode_v1_channel(address, &account.data)
}

/// Account discriminator of the program's terminal `ClosedChannel` tombstone.
const CLOSED_CHANNEL_DISCRIMINATOR: u8 = 2;

/// Whether `address` is provably gone, not merely missing from one read.
///
/// A confirmed read can lag or return a stale null for a live channel, and
/// dropping its durable record would reopen settled vouchers. Absence counts
/// only at finalized commitment from a node past the slot after which the
/// channel can no longer be opened (`open_slot + OPEN_SLOT_WINDOW`), or when
/// the program has left its closed-channel tombstone.
pub async fn channel_absence_proven(
    rpc: &RpcClient,
    rpc_url: &str,
    address: &Pubkey,
    program_id: &Pubkey,
    open_slot: u64,
) -> Result<bool, JobError> {
    let (slot, accounts) = rpc
        .get_multiple_accounts_with_owner_at(
            rpc_url,
            &[address.to_string()],
            pay_api_core::rpc::Commitment::Finalized,
        )
        .await?;
    Ok(match accounts.into_iter().next().flatten() {
        None => slot > open_slot.saturating_add(pay_kit::core::payment_channels::OPEN_SLOT_WINDOW),
        Some(account) => {
            account.owner == program_id.to_string()
                && account.data.len() < CHANNEL_ACCOUNT_SIZE
                && account.data.first() == Some(&CLOSED_CHANNEL_DISCRIMINATOR)
        }
    })
}

/// Whether `address` reads as distributed at finalized commitment: a channel
/// account in `Distributed` status, or the program's closed tombstone. A
/// confirmed distribution can still be dropped by a fork, so durable records
/// are only finished on this proof.
pub async fn finalized_distributed(
    rpc: &RpcClient,
    rpc_url: &str,
    address: &Pubkey,
    program_id: &Pubkey,
) -> Result<bool, JobError> {
    let (_, accounts) = rpc
        .get_multiple_accounts_with_owner_at(
            rpc_url,
            &[address.to_string()],
            pay_api_core::rpc::Commitment::Finalized,
        )
        .await?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok(false);
    };
    if account.owner != program_id.to_string() {
        return Err(JobError::Config(format!(
            "channel {address} is owned by {}, not its program",
            account.owner
        )));
    }
    Ok(match account.data.first() {
        Some(1) => account.data.get(3) == Some(&STATUS_DISTRIBUTED),
        Some(&CLOSED_CHANNEL_DISCRIMINATOR) => account.data.len() < CHANNEL_ACCOUNT_SIZE,
        _ => false,
    })
}

/// Validate persisted ABI selection before any recovery or transaction building.
pub fn session_deployment(config: &SessionConfig) -> Result<Deployment, JobError> {
    config
        .channel_deployment()
        .map_err(|error| JobError::Config(format!("session deployment: {error}")))
}

/// Ownership and immutable hook configuration must both match the snapshot.
pub fn decode_session_channel(
    address: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
    config: &SessionConfig,
) -> Result<DecodedChannel, JobError> {
    let decoded = session_deployment(config)?
        .decode_account(owner, data)
        .map_err(|error| JobError::Config(format!("session channel: {error}")))?;
    if decoded.hooks != config.channel_hooks.clone().unwrap_or_default() {
        return Err(JobError::Config(
            "session channel hooks differ from snapshot".into(),
        ));
    }
    Ok(DecodedChannel {
        address: *address,
        channel: decoded.channel,
    })
}

pub async fn fetch_session_channel(
    rpc: &RpcClient,
    rpc_url: &str,
    address: &Pubkey,
    config: &SessionConfig,
) -> Result<Option<DecodedChannel>, JobError> {
    let deployment = session_deployment(config)?;
    if deployment.version == 1 {
        // Preserve V1 tombstone handling, but still reject a V2 prefix.
        return fetch_channel_for_program(rpc, rpc_url, address, &deployment.program_id).await;
    }
    let accounts = rpc
        .get_multiple_accounts_with_owner(rpc_url, &[address.to_string()])
        .await?;
    let Some(account) = accounts.into_iter().next().flatten() else {
        return Ok(None);
    };
    let owner =
        Pubkey::from_str(&account.owner).map_err(|_| JobError::InvalidAddress(account.owner))?;
    decode_session_channel(address, &owner, &account.data, config).map(Some)
}

fn decode_v1_channel(address: &Pubkey, data: &[u8]) -> Result<Option<DecodedChannel>, JobError> {
    // Codama's V1 decoder accepts a V2 prefix. Never infer V1 from that success.
    if data.get(1) == Some(&2)
        || data.len() == pay_kit::core::payment_channels_v2::CHANNEL_ACCOUNT_SIZE
    {
        return Err(JobError::Config(
            "V2 channel requires a persisted session deployment and hooks; V1 recovery is unsupported".into(),
        ));
    }
    Ok(Channel::from_bytes(data)
        .ok()
        .map(|channel| DecodedChannel {
            address: *address,
            channel,
        }))
}

fn decode_v1_account(
    address: &Pubkey,
    owner: &str,
    data: &[u8],
) -> Result<Option<DecodedChannel>, JobError> {
    if owner == pay_kit::core::payment_channels_v2::PAYMENT_CHANNELS_PROGRAM_ID {
        return Err(JobError::Config(
            "V2 channel is unsupported by the V1/batch worker path".into(),
        ));
    }
    // A live account owned by another program is not an absent channel: a
    // record for it belongs to a deployment this worker cannot judge.
    if owner != default_program_id().to_string() {
        return Err(JobError::Config(format!(
            "channel {address} is owned by {owner}, not the worker's payment-channels program"
        )));
    }
    decode_v1_channel(address, data)
}

/// Fetch and decode up to 100 channels with one `getMultipleAccounts` call.
///
/// Solana RPC accepts 100 addresses per request. Settlement workers should use
/// this instead of issuing one HTTP request per stored channel.
pub async fn fetch_channels(
    rpc: &RpcClient,
    rpc_url: &str,
    addresses: &[Pubkey],
) -> Result<Vec<Option<DecodedChannel>>, JobError> {
    fetch_channels_each(rpc, rpc_url, addresses)
        .await?
        .into_iter()
        .collect()
}

/// [`fetch_channels`] with one decode result per address, so one foreign or
/// unsupported account fails only its own channel.
pub async fn fetch_channels_each(
    rpc: &RpcClient,
    rpc_url: &str,
    addresses: &[Pubkey],
) -> Result<Vec<Result<Option<DecodedChannel>, JobError>>, JobError> {
    Ok(fetch_channels_each_at(rpc, rpc_url, addresses).await?.1)
}

/// [`fetch_channels_each`] with the slot the RPC node answered at, so a
/// recorded observation can be fenced against a lagging node.
pub async fn fetch_channels_each_at(
    rpc: &RpcClient,
    rpc_url: &str,
    addresses: &[Pubkey],
) -> Result<(u64, Vec<Result<Option<DecodedChannel>, JobError>>), JobError> {
    if addresses.len() > 100 {
        return Err(JobError::TxBuild(format!(
            "channel account batch has {} addresses; maximum is 100",
            addresses.len()
        )));
    }
    let (slot, accounts) = rpc
        .get_multiple_accounts_with_owner_at(
            rpc_url,
            &addresses
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            pay_api_core::rpc::Commitment::Confirmed,
        )
        .await?;
    if accounts.len() != addresses.len() {
        return Err(JobError::Rpc(pay_api_core::Error::RpcMalformed));
    }
    let channels = addresses
        .iter()
        .copied()
        .zip(accounts)
        .map(|(address, account)| {
            let Some(account) = account else {
                return Ok(None);
            };
            // Tombstoned / closed channels are a different (short) account
            // shape; a decode failure means this is not a live channel.
            decode_v1_account(&address, &account.owner, &account.data)
        })
        .collect();
    Ok((slot, channels))
}

/// The recovered distribution preimage: the raw `count || entries` bytes that
/// `distribute` expects, plus the decoded recipients (for building the
/// recipient-ATA remaining-accounts tail).
pub struct DistributionPreimage {
    /// `count(u32 LE) || entries(count × 34)` — borsh of `Vec<DistributionEntry>`.
    pub preimage_bytes: Vec<u8>,
    pub recipients: Vec<DistributionEntry>,
}

/// Reconstruct the original ordered payout without historical RPC availability.
/// A snapshot is usable only when its commitment matches the funded channel.
pub fn distribution_preimage_from_snapshot(
    recipients: &[pay_kit::core::payment_channels::Distribution],
    distribution_hash: &[u8; 32],
) -> Result<DistributionPreimage, JobError> {
    if recipients.len() > MAX_DISTRIBUTION_RECIPIENTS
        || pay_kit::core::payment_channels::distribution_hash(recipients) != *distribution_hash
    {
        return Err(JobError::DistributionHashMismatch);
    }
    let mut preimage_bytes = (recipients.len() as u32).to_le_bytes().to_vec();
    for recipient in recipients {
        preimage_bytes.extend_from_slice(recipient.recipient.as_ref());
        preimage_bytes.extend_from_slice(&recipient.bps.to_le_bytes());
    }
    Ok(DistributionPreimage {
        recipients: decode_recipients(&preimage_bytes)?,
        preimage_bytes,
    })
}

/// Recover the distribution preimage for `channel` from its `open` (creation)
/// transaction.
///
/// The on-chain `distribution_hash` is `sha256(count || entries)`; only the
/// hash is stored, so we find the channel's `open` in its address history and
/// re-encode its recipients. We verify `sha256(preimage) ==
/// channel.distribution_hash` before returning.
///
/// This works for every channel, including the empty-plan case (count = 0,
/// preimage = `00 00 00 00`).
pub async fn recover_distribution_preimage(
    rpc: &RpcClient,
    rpc_url: &str,
    channel: &DecodedChannel,
) -> Result<DistributionPreimage, JobError> {
    if channel.channel.version != 1 {
        return Err(JobError::Config(
            "native distribution recovery is V1-only; V2 requires its persisted snapshot".into(),
        ));
    }
    // Empty distribution plans are by far the common case and their preimage
    // is canonical. Avoid two historical RPC reads per channel when the hash
    // itself proves that this is the correct preimage.
    const EMPTY_PREIMAGE: [u8; 4] = [0; 4];
    if Sha256::digest(EMPTY_PREIMAGE).as_slice() == channel.channel.distribution_hash {
        return Ok(DistributionPreimage {
            preimage_bytes: EMPTY_PREIMAGE.to_vec(),
            recipients: Vec::new(),
        });
    }
    let preimage_bytes = find_open_preimage(rpc, rpc_url, channel).await?;

    // Verify against the on-chain hash before trusting the preimage.
    let computed = Sha256::digest(&preimage_bytes);
    if computed.as_slice() != channel.channel.distribution_hash {
        return Err(JobError::DistributionHashMismatch);
    }

    let recipients = decode_recipients(&preimage_bytes)?;
    Ok(DistributionPreimage {
        preimage_bytes,
        recipients,
    })
}

/// Find the channel's `open` and return its `count || entries` payload.
///
/// Anyone can mention the channel PDA in a transaction before it exists, so
/// the oldest mention is not necessarily the open. Walk the address history
/// (newest-first, paginated) and examine successful transactions that landed
/// in the slots an open for this channel can land in, oldest first, accepting
/// only one whose instruction is this channel's open. The open-slot window
/// bounds the scan.
async fn find_open_preimage(
    rpc: &RpcClient,
    rpc_url: &str,
    channel: &DecodedChannel,
) -> Result<Vec<u8>, JobError> {
    const PAGE: u32 = 1000;
    let program_id = default_program_id();
    let addr = channel.address.to_string();
    let first_slot = channel.open_slot();
    let last_slot = first_slot.saturating_add(pay_kit::core::payment_channels::OPEN_SLOT_WINDOW);
    let mut before: Option<String> = None;
    let mut candidates = Vec::new();
    loop {
        let page = rpc
            .get_signature_infos_for_address(rpc_url, &addr, before.as_deref(), PAGE)
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        before = Some(last.signature.clone());
        let short = (page.len() as u32) < PAGE;
        let past_window = last.slot < first_slot;
        candidates.extend(
            page.into_iter()
                .filter(|info| !info.failed && (first_slot..=last_slot).contains(&info.slot)),
        );
        if short || past_window {
            break;
        }
    }
    // Pages are newest-first; the open precedes every other use of the channel.
    for info in candidates.iter().rev() {
        let Some(tx_b64) = rpc.get_transaction_base64(rpc_url, &info.signature).await? else {
            continue;
        };
        if let Some(preimage) = open_preimage_in(&tx_b64, &channel.address, &program_id)? {
            return Ok(preimage);
        }
    }
    Err(JobError::OpenTxNotFound)
}

/// The `count || entries` payload of `channel`'s open in a base64 wire
/// transaction, or `None` when the transaction is not that open.
///
/// Decodes as a `VersionedTransaction`, so legacy, v0 and v1 messages are all
/// handled; the kit never uses lookup tables, so every account an instruction
/// names is a static key.
fn open_preimage_in(
    tx_b64: &str,
    channel: &Pubkey,
    program_id: &Pubkey,
) -> Result<Option<Vec<u8>>, JobError> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(tx_b64.trim())
        .map_err(|e| JobError::OpenIxDecode(format!("base64 decode: {e}")))?;
    let Ok(tx) = wincode::deserialize::<solana_transaction::versioned::VersionedTransaction>(&raw)
    else {
        return Ok(None);
    };
    let Some(args) =
        pay_kit::core::payment_channels::decode_open_for_channel(&tx.message, channel, program_id)
    else {
        return Ok(None);
    };
    if args.recipients.len() > MAX_DISTRIBUTION_RECIPIENTS {
        return Err(JobError::OpenIxDecode(format!(
            "distribution has {} recipients; maximum is {MAX_DISTRIBUTION_RECIPIENTS}",
            args.recipients.len()
        )));
    }
    let mut preimage = (args.recipients.len() as u32).to_le_bytes().to_vec();
    for entry in &args.recipients {
        preimage.extend_from_slice(entry.recipient.as_ref());
        preimage.extend_from_slice(&entry.bps.to_le_bytes());
    }
    Ok(Some(preimage))
}

/// Decode `count(u32 LE) || entries(count × 34)` into `DistributionEntry`s.
fn decode_recipients(preimage: &[u8]) -> Result<Vec<DistributionEntry>, JobError> {
    if preimage.len() < 4 {
        return Err(JobError::OpenIxDecode("preimage shorter than count".into()));
    }
    let count = u32::from_le_bytes([preimage[0], preimage[1], preimage[2], preimage[3]]) as usize;
    if count > MAX_DISTRIBUTION_RECIPIENTS {
        return Err(JobError::OpenIxDecode(format!(
            "distribution has {count} recipients; maximum is {MAX_DISTRIBUTION_RECIPIENTS}"
        )));
    }
    let payload_len = preimage.len() - 4;
    let expected_payload_len = count
        .checked_mul(34)
        .ok_or_else(|| JobError::OpenIxDecode("recipient payload length overflow".into()))?;
    if payload_len < expected_payload_len {
        return Err(JobError::OpenIxDecode(
            "preimage truncated mid-entry".into(),
        ));
    }
    if payload_len > expected_payload_len {
        return Err(JobError::OpenIxDecode(
            "preimage has trailing recipient data".into(),
        ));
    }
    let mut out = Vec::with_capacity(count);
    let mut off = 4;
    for _ in 0..count {
        let end = off + 34;
        let slice = preimage
            .get(off..end)
            .ok_or_else(|| JobError::OpenIxDecode("preimage truncated mid-entry".into()))?;
        let mut recipient = [0u8; 32];
        recipient.copy_from_slice(&slice[..32]);
        let bps = u16::from_le_bytes([slice[32], slice[33]]);
        out.push(DistributionEntry {
            recipient: solana_address::Address::from(recipient),
            bps,
        });
        off = end;
    }
    Ok(out)
}

// ── Instruction builders ────────────────────────────────────────────────────

/// `event_authority` PDA for the payment-channels program.
pub fn event_authority() -> Pubkey {
    find_event_authority_pda(&default_program_id()).0
}

/// `settle_and_seal` with `has_voucher = 0` (no voucher). Signer =
/// `merchant` (must be the channel's payee).
pub fn build_settle_and_seal_ix(channel: &Pubkey, payee: &Pubkey) -> Instruction {
    SettleAndSealBuilder::new()
        .payee(to_address(payee))
        .channel(to_address(channel))
        .instructions_sysvar(to_address(&INSTRUCTIONS_SYSVAR))
        .settle_and_seal_args(SettleAndSealArgs { has_voucher: 0 })
        .instruction()
}

/// `request_close` — starts the grace window. Signer = `payer`.
pub fn build_request_close_ix(channel: &Pubkey, payer: &Pubkey) -> Instruction {
    RequestCloseBuilder::new()
        .payer(to_address(payer))
        .channel(to_address(channel))
        .instruction()
}

/// `seal` — permissionless once the grace deadline has elapsed.
pub fn build_seal_ix(channel: &Pubkey) -> Instruction {
    SealBuilder::new()
        .channel(to_address(channel))
        .instruction()
}

/// Seal under the original channel program.
pub fn build_seal_ix_for_program(channel: &Pubkey, program_id: &Pubkey) -> Instruction {
    let mut instruction = build_seal_ix(channel);
    instruction.program_id = to_address(program_id);
    instruction
}

/// Permissionless rent reclaim for a fully distributed channel. The program
/// enforces `clock.slot > open_slot + OPEN_SLOT_WINDOW` and returns the PDA's
/// lamports to the channel-bound rent payer.
pub fn build_reclaim_ix(channel: &Pubkey, rent_payer: &Pubkey) -> Instruction {
    ReclaimBuilder::new()
        .channel(to_address(channel))
        .rent_payer(to_address(rent_payer))
        .instruction()
}

/// `withdraw_payer` — payer refund; no preimage requirement. Signer = `payer`.
#[allow(clippy::too_many_arguments)]
pub fn build_withdraw_payer_ix(
    channel: &Pubkey,
    payer: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let channel_ata = associated_token_address(channel, mint, token_program);
    let payer_ata = associated_token_address(payer, mint, token_program);
    WithdrawPayerBuilder::new()
        .payer(to_address(payer))
        .channel(to_address(channel))
        .channel_token_account(to_address(&channel_ata))
        .payer_token_account(to_address(&payer_ata))
        .mint(to_address(mint))
        .token_program(to_address(token_program))
        .instruction()
}

/// `top_up` — add `amount` (base units) to an open channel's deposit vault.
/// Signer = `payer` (the channel's payer), whose ATA funds the transfer.
pub fn build_top_up_ix(
    channel: &Pubkey,
    payer: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    amount: u64,
) -> Instruction {
    let channel_ata = associated_token_address(channel, mint, token_program);
    let payer_ata = associated_token_address(payer, mint, token_program);
    TopUpBuilder::new()
        .payer(to_address(payer))
        .channel(to_address(channel))
        .channel_token_account(to_address(&channel_ata))
        .payer_token_account(to_address(&payer_ata))
        .mint(to_address(mint))
        .token_program(to_address(token_program))
        .top_up_args(TopUpArgs { amount })
        .instruction()
}

/// Whether the payer's refund token account, which `distribute` writes, is
/// live at the confirmed commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundAccount {
    /// Owned by the channel's token program: `distribute` can land.
    Live,
    /// Absent (or not a token account): `distribute` would fail, and with it
    /// every channel co-batched in the same transaction.
    Closed,
}

/// Read the payer's refund account. The payer can close an emptied account at
/// any time, and the worker never recreates it at the operator's expense (the
/// rent would go straight back to the payer on the next close), so a closed
/// account defers distribution until the payer recreates it.
pub async fn payer_refund_account(
    rpc: &RpcClient,
    rpc_url: &str,
    channel: &DecodedChannel,
    token_program: &Pubkey,
) -> Result<RefundAccount, JobError> {
    let address = associated_token_address(&channel.payer(), &channel.mint(), token_program);
    let account = rpc
        .get_multiple_accounts_with_owner(rpc_url, &[address.to_string()])
        .await?
        .into_iter()
        .next()
        .flatten();
    Ok(
        if account.is_some_and(|account| account.owner == token_program.to_string()) {
            RefundAccount::Live
        } else {
            RefundAccount::Closed
        },
    )
}

/// Idempotently create the payer's refund token account, funded by the payer
/// itself. Only a payer-signed transaction can carry it: the funder is fixed
/// to the channel payer so no operator ever pays rent the payer can reclaim.
pub fn payer_funded_refund_account_ix(
    channel: &DecodedChannel,
    token_program: &Pubkey,
) -> Instruction {
    pay_kit::core::payment_channels::build_create_associated_token_account_instruction(
        &channel.payer(),
        &channel.payer(),
        &channel.mint(),
        token_program,
    )
}

/// When `instructions` (a close followed by `distribute`, optionally preceded
/// by the payer's own refund-account create) cannot be built as one `version`
/// transaction, drop the distribution so the close lands alone; the next pass
/// reads the sealed channel and distributes it by itself. Returns whether it
/// split.
pub fn defer_oversized_distribution(
    instructions: &mut Vec<Instruction>,
    fee_payer: &Pubkey,
    version: pay_kit::core::tx::TxVersion,
) -> bool {
    use pay_kit::generated::payment_channels::generated::instructions::DISTRIBUTE_DISCRIMINATOR;
    let ata_program = to_address(&pay_api_core::ata::ATA_PROGRAM_ID);
    let Some(mut split) = instructions.iter().rposition(|ix| {
        ix.program_id != ata_program
            && ix.data.first() == Some(&DISTRIBUTE_DISCRIMINATOR)
            && ix.accounts.len() >= DISTRIBUTE_FIXED_ACCOUNTS
    }) else {
        return false;
    };
    if split > 0
        && instructions[split - 1].program_id == ata_program
        && instructions[split - 1].data == [1]
    {
        split -= 1;
    }
    if split == 0
        || pay_kit::core::tx::build_unsigned(
            version,
            fee_payer,
            instructions,
            solana_hash::Hash::default(),
            None,
        )
        .is_ok()
    {
        return false;
    }
    instructions.truncate(split);
    true
}

/// Accounts derived for a `distribute` instruction, surfaced for logging.
pub struct DistributeAccounts {
    pub channel_ata: Pubkey,
    pub payer_ata: Pubkey,
    pub payee_ata: Pubkey,
    pub treasury_ata: Pubkey,
    pub recipient_atas: Vec<Pubkey>,
}

/// `distribute` — permissionless; refunds payer + pays recipients + tombstones.
///
/// Requires the recovered distribution preimage. The recipient-ATA tail is one
/// `ATA(recipient, mint, token_program)` per entry, in order, passed as
/// remaining accounts (writable, non-signer).
pub fn build_distribute_ix(
    channel: &DecodedChannel,
    treasury_owner: &Pubkey,
    token_program: &Pubkey,
    preimage: &DistributionPreimage,
) -> (Instruction, DistributeAccounts) {
    build_distribute_ix_for_program(
        channel,
        treasury_owner,
        token_program,
        preimage,
        &default_program_id(),
    )
}

/// Distribute using the channel's original program and its event authority.
pub fn build_distribute_ix_for_program(
    channel: &DecodedChannel,
    treasury_owner: &Pubkey,
    token_program: &Pubkey,
    preimage: &DistributionPreimage,
    program_id: &Pubkey,
) -> (Instruction, DistributeAccounts) {
    let channel_addr = channel.address;
    let payer = channel.payer();
    let rent_payer = channel.rent_payer();
    let payee = channel.payee();
    let mint = channel.mint();

    let channel_ata = associated_token_address(&channel_addr, &mint, token_program);
    let payer_ata = associated_token_address(&payer, &mint, token_program);
    let payee_ata = associated_token_address(&payee, &mint, token_program);
    let treasury_ata = associated_token_address(treasury_owner, &mint, token_program);

    let recipient_atas: Vec<Pubkey> = preimage
        .recipients
        .iter()
        .map(|e| {
            let recipient = Pubkey::from(e.recipient.to_bytes());
            associated_token_address(&recipient, &mint, token_program)
        })
        .collect();

    let remaining: Vec<solana_instruction::AccountMeta> = recipient_atas
        .iter()
        .map(|ata| solana_instruction::AccountMeta::new(to_address(ata), false))
        .collect();

    let mut ix = DistributeBuilder::new()
        .channel(to_address(&channel_addr))
        .payer(to_address(&payer))
        .rent_payer(to_address(&rent_payer))
        .channel_token_account(to_address(&channel_ata))
        .payer_token_account(to_address(&payer_ata))
        .payee_token_account(to_address(&payee_ata))
        .treasury_token_account(to_address(&treasury_ata))
        .mint(to_address(&mint))
        .token_program(to_address(token_program))
        .event_authority(to_address(&find_event_authority_pda(program_id).0))
        .self_program(to_address(program_id))
        .distribute_args(DistributeArgs {
            recipients: preimage.recipients.clone(),
        })
        .add_remaining_accounts(&remaining)
        .instruction();
    ix.program_id = to_address(program_id);

    (
        ix,
        DistributeAccounts {
            channel_ata,
            payer_ata,
            payee_ata,
            treasury_ata,
            recipient_atas,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_fixture() -> (SessionConfig, Vec<u8>) {
        use pay_kit::core::payment_channels_v2::{ChannelHooks, HookDescriptor};
        let hooks = ChannelHooks {
            post_distribute: Some(HookDescriptor {
                program_id: Pubkey::new_unique().to_string(),
                entrypoint: *b"refund!!",
                accounts: vec![],
                static_data: vec![7],
            }),
            ..Default::default()
        };
        let config = SessionConfig {
            network: "devnet".into(),
            channel_program: Some(
                pay_kit::core::payment_channels_v2::PAYMENT_CHANNELS_PROGRAM_ID
                    .parse()
                    .unwrap(),
            ),
            channel_version: Some(2),
            channel_hooks: Some(hooks.clone()),
            ..Default::default()
        };
        let mut bytes = vec![0; pay_kit::core::payment_channels_v2::CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 2;
        bytes[265..297].copy_from_slice(&hooks.config_hash().unwrap());
        bytes[297..].copy_from_slice(&hooks.encode_fixed().unwrap());
        (config, bytes)
    }

    #[test]
    fn session_decoder_checks_owner_abi_and_persisted_hooks() {
        let (config, bytes) = v2_fixture();
        let address = Pubkey::new_unique();
        let owner = config.channel_program.unwrap();
        assert_eq!(
            decode_session_channel(&address, &owner, &bytes, &config)
                .unwrap()
                .channel
                .version,
            2
        );
        assert!(decode_session_channel(&address, &default_program_id(), &bytes, &config).is_err());
        let mut missing_hooks = config.clone();
        missing_hooks.channel_hooks = None;
        assert!(decode_session_channel(&address, &owner, &bytes, &missing_hooks).is_err());
        let mut mismatch = config.clone();
        mismatch
            .channel_hooks
            .as_mut()
            .unwrap()
            .post_distribute
            .as_mut()
            .unwrap()
            .static_data
            .push(8);
        assert!(decode_session_channel(&address, &owner, &bytes, &mismatch).is_err());
        let mut missing_version = config;
        missing_version.channel_version = None;
        assert!(decode_session_channel(&address, &owner, &bytes, &missing_version).is_err());
        assert!(decode_v1_channel(&address, &bytes).is_err());
        // Batch/fleet reconciliation must report unsupported rather than
        // treating a V2-owned account as absent and deleting its durable row.
        assert!(decode_v1_account(&address, &owner.to_string(), &bytes).is_err());
        assert!(decode_v1_account(&address, &owner.to_string(), &[0]).is_err());
    }

    #[test]
    fn unhooked_v2_requires_matching_explicit_deployment_not_some_empty_hooks() {
        let (mut config, mut bytes) = v2_fixture();
        config.channel_hooks = None;
        let empty = pay_kit::core::payment_channels_v2::ChannelHooks::default();
        bytes[265..297].copy_from_slice(&empty.config_hash().unwrap());
        bytes[297..].copy_from_slice(&empty.encode_fixed().unwrap());
        assert!(
            decode_session_channel(
                &Pubkey::new_unique(),
                &config.channel_program.unwrap(),
                &bytes,
                &config
            )
            .is_ok()
        );
        config.channel_program = None;
        assert!(session_deployment(&config).is_err());
    }

    #[tokio::test]
    async fn v1_recovery_refuses_v2_before_rpc_even_for_empty_plan() {
        let (config, bytes) = v2_fixture();
        let mut channel = decode_session_channel(
            &Pubkey::new_unique(),
            &config.channel_program.unwrap(),
            &bytes,
            &config,
        )
        .unwrap();
        channel.channel.distribution_hash = Sha256::digest([0; 4]).into();
        let rpc = RpcClient::new(std::time::Duration::from_millis(10)).unwrap();
        assert!(matches!(
            recover_distribution_preimage(&rpc, "http://127.0.0.1:1", &channel).await,
            Err(JobError::Config(_))
        ));
        channel.channel.version = 1;
        assert!(
            recover_distribution_preimage(&rpc, "http://127.0.0.1:1", &channel)
                .await
                .unwrap()
                .recipients
                .is_empty()
        );
    }

    #[test]
    fn default_session_keeps_v1_account_layout() {
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let address = Pubkey::new_unique();
        assert_eq!(
            session_deployment(&SessionConfig::default()).unwrap(),
            Deployment::default()
        );
        assert!(
            decode_session_channel(
                &address,
                &default_program_id(),
                &bytes,
                &SessionConfig::default()
            )
            .is_ok()
        );
        assert!(decode_v1_channel(&address, &bytes).unwrap().is_some());
        assert!(decode_v1_channel(&address, &[0]).unwrap().is_none());
    }

    async fn serve_account(
        slot: u64,
        account: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, routing::post};
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let account = account.clone();
                async move {
                    assert_eq!(request["method"], "getMultipleAccounts");
                    assert_eq!(request["params"][1]["commitment"], "finalized");
                    Json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {"context": {"slot": slot}, "value": [account]}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    #[tokio::test]
    async fn only_a_finalized_distributed_read_finishes_a_close() {
        use base64::Engine;
        let rpc = RpcClient::new(std::time::Duration::from_secs(5)).unwrap();
        let address = Pubkey::new_unique();
        let program = default_program_id();
        let account = |status: u8| {
            let mut data = vec![0; CHANNEL_ACCOUNT_SIZE];
            data[0] = 1;
            data[1] = 1;
            data[3] = status;
            serde_json::json!({
                "owner": program.to_string(),
                "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"]
            })
        };
        for (value, expected) in [
            (account(STATUS_DISTRIBUTED), true),
            // A close rolled back by a fork reads open or closing again.
            (account(STATUS_OPEN), false),
            (account(STATUS_CLOSING), false),
            (account(STATUS_SEALED), false),
            (serde_json::Value::Null, false),
        ] {
            let (url, server) = serve_account(1, value).await;
            let distributed = finalized_distributed(&rpc, &url, &address, &program)
                .await
                .unwrap();
            server.abort();
            assert_eq!(distributed, expected);
        }
    }

    #[tokio::test]
    async fn absence_requires_finalized_proof_past_the_open_window() {
        use base64::Engine;
        let rpc = RpcClient::new(std::time::Duration::from_secs(5)).unwrap();
        let address = Pubkey::new_unique();
        let program = default_program_id();
        let open_slot = 10_000;
        let window = pay_kit::core::payment_channels::OPEN_SLOT_WINDOW;
        let encoded = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);
        let mut live = vec![0; CHANNEL_ACCOUNT_SIZE];
        live[0] = 1;
        live[1] = 1;
        let cases = [
            (open_slot + window, serde_json::Value::Null, false),
            (open_slot + window + 1, serde_json::Value::Null, true),
            (
                0,
                serde_json::json!({"owner": program.to_string(), "data": [encoded(&[2]), "base64"]}),
                true,
            ),
            (
                u64::MAX,
                serde_json::json!({"owner": Pubkey::new_unique().to_string(), "data": [encoded(&[2]), "base64"]}),
                false,
            ),
            (
                u64::MAX,
                serde_json::json!({"owner": program.to_string(), "data": [encoded(&live), "base64"]}),
                false,
            ),
        ];
        for (slot, account, expected) in cases {
            let (url, server) = serve_account(slot, account.clone()).await;
            let proven = channel_absence_proven(&rpc, &url, &address, &program, open_slot)
                .await
                .unwrap();
            server.abort();
            assert_eq!(proven, expected, "slot {slot} account {account}");
        }
    }

    #[test]
    fn only_the_payer_ever_funds_its_refund_account() {
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let mut channel = decode_v1_channel(&Pubkey::new_unique(), &bytes)
            .unwrap()
            .unwrap();
        channel.channel.payer = to_address(&Pubkey::new_unique());
        channel.channel.mint = to_address(&Pubkey::new_unique());
        for token_program in [SPL_TOKEN_PROGRAM_ID, TOKEN_2022_PROGRAM_ID] {
            let create = payer_funded_refund_account_ix(&channel, &token_program);
            let (distribute, accounts) = build_distribute_ix(
                &channel,
                &Pubkey::new_unique(),
                &token_program,
                &DistributionPreimage {
                    preimage_bytes: vec![0; 4],
                    recipients: vec![],
                },
            );
            // CreateIdempotent for the exact refund account, funded by its owner.
            assert_eq!(create.data, vec![1]);
            assert_eq!(
                create.program_id,
                to_address(&pay_api_core::ata::ATA_PROGRAM_ID)
            );
            assert_eq!(create.accounts[0].pubkey, channel.channel.payer);
            assert!(create.accounts[0].is_signer);
            assert_eq!(create.accounts[1].pubkey, to_address(&accounts.payer_ata));
            assert_eq!(distribute.accounts[4].pubkey, create.accounts[1].pubkey);
            assert_eq!(create.accounts[2].pubkey, channel.channel.payer);
            assert_eq!(create.accounts[5].pubkey, to_address(&token_program));
        }
    }

    #[tokio::test]
    async fn a_closed_refund_account_is_reported_closed() {
        use axum::{Json, Router, routing::post};
        let rpc = RpcClient::new(std::time::Duration::from_secs(5)).unwrap();
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let mut channel = decode_v1_channel(&Pubkey::new_unique(), &bytes)
            .unwrap()
            .unwrap();
        channel.channel.payer = to_address(&Pubkey::new_unique());
        channel.channel.mint = to_address(&Pubkey::new_unique());
        let refund =
            associated_token_address(&channel.payer(), &channel.mint(), &SPL_TOKEN_PROGRAM_ID);
        let system = Pubkey::default().to_string();
        let cases = [
            (Some(SPL_TOKEN_PROGRAM_ID.to_string()), RefundAccount::Live),
            (None, RefundAccount::Closed),
            (Some(system), RefundAccount::Closed),
            (
                Some(TOKEN_2022_PROGRAM_ID.to_string()),
                RefundAccount::Closed,
            ),
        ];
        for (owner, expected) in cases {
            let app = Router::new().route(
                "/",
                post(move |Json(request): Json<serde_json::Value>| {
                    let owner = owner.clone();
                    async move {
                        assert_eq!(request["method"], "getMultipleAccounts");
                        assert_eq!(request["params"][0][0], refund.to_string());
                        let account = owner.map(
                            |owner| serde_json::json!({"owner": owner, "data": ["", "base64"]}),
                        );
                        Json(serde_json::json!({
                            "jsonrpc": "2.0", "id": 1,
                            "result": {"context": {"slot": 1}, "value": [account]}
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let read = payer_refund_account(&rpc, &url, &channel, &SPL_TOKEN_PROGRAM_ID)
                .await
                .unwrap();
            server.abort();
            assert_eq!(read, expected);
        }
        // An unreachable RPC never reads as closed.
        assert!(
            payer_refund_account(&rpc, "http://127.0.0.1:1", &channel, &SPL_TOKEN_PROGRAM_ID)
                .await
                .is_err()
        );
    }

    #[test]
    fn a_foreign_owned_account_is_never_reported_absent() {
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let address = Pubkey::new_unique();
        let custom = Pubkey::new_unique().to_string();
        assert!(decode_v1_account(&address, &custom, &bytes).is_err());
        assert!(decode_v1_account(&address, &custom, &[2]).is_err());
        let canonical = default_program_id().to_string();
        assert!(
            decode_v1_account(&address, &canonical, &bytes)
                .unwrap()
                .is_some()
        );
        assert!(
            decode_v1_account(&address, &canonical, &[2])
                .unwrap()
                .is_none()
        );
    }

    fn recipients_channel(count: usize) -> (DecodedChannel, DistributionPreimage) {
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let mut channel = decode_v1_channel(&Pubkey::new_unique(), &bytes)
            .unwrap()
            .unwrap();
        channel.channel.status = STATUS_CLOSING;
        for key in [
            &mut channel.channel.payer,
            &mut channel.channel.payee,
            &mut channel.channel.rent_payer,
            &mut channel.channel.mint,
            &mut channel.channel.authorized_signer,
        ] {
            *key = to_address(&Pubkey::new_unique());
        }
        let recipients = (0..count)
            .map(|_| pay_kit::core::payment_channels::Distribution {
                recipient: Pubkey::new_unique(),
                bps: 1,
            })
            .collect::<Vec<_>>();
        channel.channel.distribution_hash =
            pay_kit::core::payment_channels::distribution_hash(&recipients);
        let preimage =
            distribution_preimage_from_snapshot(&recipients, &channel.channel.distribution_hash)
                .unwrap();
        (channel, preimage)
    }

    fn payout(channel: &DecodedChannel, preimage: &DistributionPreimage) -> Vec<Instruction> {
        vec![
            payer_funded_refund_account_ix(channel, &SPL_TOKEN_PROGRAM_ID),
            build_distribute_ix(
                channel,
                &Pubkey::new_unique(),
                &SPL_TOKEN_PROGRAM_ID,
                preimage,
            )
            .0,
        ]
    }

    #[test]
    fn the_largest_accepted_plan_distributes_in_one_transaction() {
        use pay_kit::core::tx::{TxVersion, build_unsigned};
        // A fee payer distinct from every channel account is the worst case.
        let fee_payer = Pubkey::new_unique();
        let (channel, preimage) = recipients_channel(MAX_DISTRIBUTION_RECIPIENTS);
        build_unsigned(
            TxVersion::V1,
            &fee_payer,
            &payout(&channel, &preimage),
            solana_hash::Hash::default(),
            None,
        )
        .expect("the maximum plan must fit");

        // One more recipient no longer fits, and is refused at recovery
        // rather than retried forever.
        let recipients = (0..=MAX_DISTRIBUTION_RECIPIENTS)
            .map(|_| pay_kit::core::payment_channels::Distribution {
                recipient: Pubkey::new_unique(),
                bps: 1,
            })
            .collect::<Vec<_>>();
        let hash = pay_kit::core::payment_channels::distribution_hash(&recipients);
        assert!(distribution_preimage_from_snapshot(&recipients, &hash).is_err());
        let mut preimage = (recipients.len() as u32).to_le_bytes().to_vec();
        for entry in &recipients {
            preimage.extend_from_slice(entry.recipient.as_ref());
            preimage.extend_from_slice(&entry.bps.to_le_bytes());
        }
        assert!(decode_recipients(&preimage).is_err());
    }

    #[test]
    fn a_close_that_cannot_carry_its_distribution_lands_alone() {
        use pay_kit::core::tx::TxVersion;
        let fee_payer = Pubkey::new_unique();
        let (channel, preimage) = recipients_channel(MAX_DISTRIBUTION_RECIPIENTS);
        // Settling the final voucher adds the payee, the instructions sysvar
        // and the signature-verify program to the distribution's keys.
        let close = pay_kit::core::payment_channels::build_settle_and_seal_instructions(
            &channel.payee(),
            &channel.address,
            &Pubkey::from(channel.channel.authorized_signer.to_bytes()),
            Some(&[3; 64]),
            10,
            0,
            &default_program_id(),
        )
        .unwrap();
        let mut combined = close.clone();
        combined.extend(payout(&channel, &preimage));
        assert!(defer_oversized_distribution(
            &mut combined,
            &fee_payer,
            TxVersion::V1
        ));
        assert_eq!(combined, close);
        // Without the payer's own create, the distribution alone is split off.
        let mut bare = close.clone();
        bare.push(payout(&channel, &preimage).pop().unwrap());
        assert!(defer_oversized_distribution(
            &mut bare,
            &fee_payer,
            TxVersion::V1
        ));
        assert_eq!(bare, close);

        // A distribution alone is never split, and a fitting pair is kept.
        let mut alone = payout(&channel, &preimage);
        assert!(!defer_oversized_distribution(
            &mut alone,
            &fee_payer,
            TxVersion::V1
        ));
        assert_eq!(alone.len(), 2);
        let (small, small_preimage) = recipients_channel(2);
        let mut fitting = vec![build_seal_ix(&small.address)];
        fitting.extend(payout(&small, &small_preimage));
        assert!(!defer_oversized_distribution(
            &mut fitting,
            &fee_payer,
            TxVersion::V0
        ));
        assert_eq!(fitting.len(), 3);
    }

    #[test]
    fn empty_plan_preimage_hashes_to_zero_count() {
        // count = 0 → preimage = the 4 bytes 00 00 00 00.
        let preimage = 0u32.to_le_bytes();
        let hash = Sha256::digest(preimage);
        let recipients = decode_recipients(&preimage).unwrap();
        assert!(recipients.is_empty());
        // Sanity: the hash is deterministic and 32 bytes.
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn decode_recipients_parses_entries() {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(&2u32.to_le_bytes());
        // entry 0
        preimage.extend_from_slice(&[1u8; 32]);
        preimage.extend_from_slice(&4000u16.to_le_bytes());
        // entry 1
        preimage.extend_from_slice(&[2u8; 32]);
        preimage.extend_from_slice(&6000u16.to_le_bytes());
        let recipients = decode_recipients(&preimage).unwrap();
        assert_eq!(recipients.len(), 2);
        assert_eq!(recipients[0].bps, 4000);
        assert_eq!(recipients[1].bps, 6000);
        assert_eq!(recipients[0].recipient.to_bytes(), [1u8; 32]);
    }

    #[test]
    fn decode_recipients_rejects_truncated() {
        let mut preimage = Vec::new();
        preimage.extend_from_slice(&1u32.to_le_bytes());
        preimage.extend_from_slice(&[1u8; 10]); // too short for a 34-byte entry
        assert!(decode_recipients(&preimage).is_err());
    }

    #[test]
    fn decode_recipients_rejects_unbounded_count_before_allocating() {
        assert!(decode_recipients(&u32::MAX.to_le_bytes()).is_err());
    }

    fn wire_transaction(fee_payer: &Pubkey, instructions: &[Instruction]) -> String {
        use base64::Engine;
        let message = solana_message::legacy::Message::new(instructions, Some(fee_payer));
        let transaction = solana_transaction::versioned::VersionedTransaction {
            signatures: vec![
                solana_signature::Signature::default();
                usize::from(message.header.num_required_signatures)
            ],
            message: solana_message::VersionedMessage::Legacy(message),
        };
        base64::engine::general_purpose::STANDARD.encode(wincode::serialize(&transaction).unwrap())
    }

    /// Serve an address history and its transactions: `(signature, slot,
    /// failed, base64 transaction)`, newest first.
    async fn serve_history(
        history: Vec<(String, u64, bool, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, routing::post};
        let app = Router::new().route(
            "/",
            post(move |Json(request): Json<serde_json::Value>| {
                let history = history.clone();
                async move {
                    let result = match request["method"].as_str().unwrap() {
                        "getSignaturesForAddress" => serde_json::Value::Array(
                            history
                                .iter()
                                .map(|(signature, slot, failed, _)| {
                                    serde_json::json!({
                                        "signature": signature,
                                        "slot": slot,
                                        "err": if *failed { serde_json::json!({"x": 1}) } else { serde_json::Value::Null },
                                    })
                                })
                                .collect(),
                        ),
                        "getTransaction" => {
                            let wanted = request["params"][0].as_str().unwrap();
                            history
                                .iter()
                                .find(|(signature, ..)| signature == wanted)
                                .map(|(.., tx)| serde_json::json!({"transaction": [tx, "base64"]}))
                                .unwrap_or(serde_json::Value::Null)
                        }
                        other => panic!("unexpected {other}"),
                    };
                    Json(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    #[tokio::test]
    async fn recovery_skips_pre_open_mentions_and_finds_the_real_open() {
        use pay_kit::core::payment_channels::{
            Distribution, OpenChannelParams, build_open_instruction, derive_channel_addresses,
            distribution_hash,
        };
        let recipients = vec![Distribution {
            recipient: Pubkey::new_unique(),
            bps: 10_000,
        }];
        let params = OpenChannelParams {
            payer: Pubkey::new_unique(),
            rent_payer: Pubkey::new_unique(),
            payee: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            authorized_signer: Pubkey::new_unique(),
            salt: 9,
            open_slot: 5_000,
            deposit: 1_000,
            grace_period: 900,
            recipients: recipients.clone(),
            token_program: SPL_TOKEN_PROGRAM_ID,
            program_id: default_program_id(),
        };
        let address = derive_channel_addresses(&params).channel;
        let mut bytes = vec![0; CHANNEL_ACCOUNT_SIZE];
        bytes[0] = 1;
        bytes[1] = 1;
        let mut channel = decode_v1_channel(&address, &bytes).unwrap().unwrap();
        channel.channel.open_slot = params.open_slot;
        channel.channel.distribution_hash = distribution_hash(&recipients);

        // A readonly mention of the future PDA, and an open-shaped spoof whose
        // accounts do not derive this channel.
        let marker = Instruction {
            program_id: to_address(&Pubkey::new_unique()),
            accounts: vec![solana_instruction::AccountMeta::new_readonly(
                to_address(&address),
                false,
            )],
            data: vec![],
        };
        let mut spoof = build_open_instruction(&OpenChannelParams {
            salt: 10,
            ..params.clone()
        });
        spoof.accounts[5].pubkey = to_address(&address);
        let open = build_open_instruction(&params);
        let payer = params.payer;
        let tx = |instructions: &[Instruction]| wire_transaction(&payer, instructions);
        let rpc = RpcClient::new(std::time::Duration::from_secs(5)).unwrap();

        let history = vec![
            (
                "settle".to_string(),
                5_900,
                false,
                tx(std::slice::from_ref(&marker)),
            ),
            (
                "open".to_string(),
                5_100,
                false,
                tx(std::slice::from_ref(&open)),
            ),
            (
                "failed-open".to_string(),
                5_050,
                true,
                tx(std::slice::from_ref(&open)),
            ),
            ("spoof".to_string(), 5_020, false, tx(&[spoof])),
            (
                "marker".to_string(),
                5_010,
                false,
                tx(std::slice::from_ref(&marker)),
            ),
            (
                "before-window".to_string(),
                4_000,
                false,
                tx(std::slice::from_ref(&marker)),
            ),
        ];
        let (url, server) = serve_history(history.clone()).await;
        let preimage = recover_distribution_preimage(&rpc, &url, &channel)
            .await
            .unwrap();
        server.abort();
        assert_eq!(preimage.recipients.len(), 1);
        assert_eq!(
            preimage.recipients[0].recipient.to_bytes(),
            recipients[0].recipient.to_bytes()
        );

        // Mentions alone are never an open: exhausted history is not found.
        let without_open = history
            .into_iter()
            .filter(|(signature, ..)| !signature.ends_with("open"))
            .collect();
        let (url, server) = serve_history(without_open).await;
        let error = recover_distribution_preimage(&rpc, &url, &channel)
            .await
            .err()
            .unwrap();
        server.abort();
        assert!(matches!(error, JobError::OpenTxNotFound), "{error}");
    }

    #[test]
    fn non_transactions_are_not_opens() {
        let channel = Pubkey::new_unique();
        assert!(open_preimage_in("not base64!!!", &channel, &default_program_id()).is_err());
        assert!(
            open_preimage_in("AAAA", &channel, &default_program_id())
                .unwrap()
                .is_none()
        );
    }
}
